package com.xrproxy.app.service

import android.content.Context
import android.util.Log
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import com.xrproxy.app.data.ShareRepository
import com.xrproxy.app.data.ShareStore
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.util.concurrent.TimeUnit

/**
 * Background one-way mirror for every sync-enabled share (LLD-19 §2.4, §5.4).
 * Runs periodically and on demand (app foreground / user toggle). Doze may delay
 * it; a foreground sync is guaranteed when the user opens the Files screen.
 *
 * Per share it runs one [ShareRepository.syncOnce] cycle — true mirror, so files
 * removed on the server are deleted locally. A failure on one share doesn't stop
 * the others; the run retries if any failed.
 */
class ShareSyncWorker(appContext: Context, params: WorkerParameters) :
    CoroutineWorker(appContext, params) {

    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        val store = ShareStore.create(applicationContext)
        val repo = ShareRepository(applicationContext)
        val enabled = store.enabledShares()
        if (enabled.isEmpty()) return@withContext Result.success()

        var anyFailed = false
        for (config in enabled) {
            val result = runCatching { repo.syncOnce(config) }
            val outcome = result.getOrNull()
            if (outcome == null) {
                Log.w(TAG, "sync '${config.name}' crashed: ${result.exceptionOrNull()}")
                anyFailed = true
                continue
            }
            if (outcome.ok) {
                Log.i(TAG, "synced '${config.name}': +${outcome.fetched} -${outcome.deleted}")
                if (outcome.failed > 0) anyFailed = true
            } else {
                Log.w(TAG, "sync '${config.name}' failed: ${outcome.error}")
                anyFailed = true
            }
        }
        if (anyFailed) Result.retry() else Result.success()
    }

    companion object {
        private const val TAG = "ShareSyncWorker"
    }
}

/** Schedules the mirror Worker (LLD-19 §5.4). */
object ShareSyncScheduler {
    private const val PERIODIC = "xr-share-sync-periodic"
    private const val ONESHOT = "xr-share-sync-now"

    private fun connected() =
        Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build()

    /** Enable periodic background mirror (~6h; WorkManager's floor is 15 min). */
    fun schedulePeriodic(context: Context) {
        val req = PeriodicWorkRequestBuilder<ShareSyncWorker>(6, TimeUnit.HOURS)
            .setConstraints(connected())
            .build()
        WorkManager.getInstance(context)
            .enqueueUniquePeriodicWork(PERIODIC, ExistingPeriodicWorkPolicy.UPDATE, req)
    }

    /** Run a mirror now (user opened Files / toggled sync on). */
    fun syncNow(context: Context) {
        val req = OneTimeWorkRequestBuilder<ShareSyncWorker>()
            .setConstraints(connected())
            .build()
        WorkManager.getInstance(context)
            .enqueueUniqueWork(ONESHOT, ExistingWorkPolicy.KEEP, req)
    }

    /** Stop periodic sync (no shares are enabled any more). */
    fun cancelPeriodic(context: Context) {
        WorkManager.getInstance(context).cancelUniqueWork(PERIODIC)
    }
}
