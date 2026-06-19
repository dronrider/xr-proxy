package com.xrproxy.app.update

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageInstaller
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import com.xrproxy.app.BuildConfig
import com.xrproxy.app.jni.NativeBridge
import org.json.JSONObject
import java.io.File
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

/**
 * APK self-update orchestration (LLD-12 §2.3, §4).
 *
 * Splits the work along the trust boundary: the security-critical parts —
 * verifying the manifest signature with the **pinned** release key and the
 * downloaded APK's SHA-256 — live in Rust ([NativeBridge.nativeCheckUpdate] /
 * [NativeBridge.nativeVerifyApk]). Kotlin only does what Rust cannot: download
 * the APK to `cacheDir` with progress and drive the system [PackageInstaller].
 *
 * A compromised VPS can serve a tampered manifest/APK, but it fails
 * verification in Rust, so nothing here is ever asked to install it.
 */
class UpdateManager(private val context: Context) {

    /** A verified-available release, parsed from the signed manifest. */
    data class Release(
        val versionCode: Long,
        val versionName: String,
        val apkUrl: String,
        val sha256: String,
        val sizeBytes: Long,
        val notes: String,
        val releasedAt: String,
    )

    sealed interface CheckResult {
        data class Available(val release: Release) : CheckResult
        object UpToDate : CheckResult
        /** [error] is a short code (no_hub / no_release_key / network / verify: …). */
        data class Failed(val error: String) : CheckResult
    }

    sealed interface InstallStatus {
        object Success : InstallStatus
        data class Failed(val message: String) : InstallStatus
    }

    /** Set by the owner (ViewModel) to learn the final install outcome. */
    @Volatile
    var onInstallStatus: ((InstallStatus) -> Unit)? = null

    // ── Check ────────────────────────────────────────────────────────

    /**
     * Query the hub and verify in Rust. Blocking — call off the main thread.
     * The pinned release public key comes from the build (never the network);
     * an empty key means this build has self-update disabled.
     */
    fun check(hubUrl: String): CheckResult {
        if (BuildConfig.RELEASE_PUBLIC_KEY.isBlank()) {
            return CheckResult.Failed("no_release_key")
        }
        val json = NativeBridge.nativeCheckUpdate(
            hubUrl,
            currentVersionCode(),
            BuildConfig.RELEASE_PUBLIC_KEY,
            CHECK_TIMEOUT_MS,
        )
        val obj = runCatching { JSONObject(json) }.getOrNull()
            ?: return CheckResult.Failed("parse")

        if (!obj.optBoolean("available", false)) {
            val err = obj.optString("error", "")
            return if (err.isBlank() || err == "null") CheckResult.UpToDate
            else CheckResult.Failed(err)
        }
        val m = obj.optJSONObject("manifest") ?: return CheckResult.Failed("no_manifest")
        return CheckResult.Available(
            Release(
                versionCode = m.optLong("version_code"),
                versionName = m.optString("version_name"),
                apkUrl = m.optString("apk_url"),
                sha256 = m.optString("apk_sha256"),
                sizeBytes = m.optLong("size_bytes"),
                notes = m.optString("release_notes"),
                releasedAt = m.optString("released_at"),
            )
        )
    }

    fun currentVersionCode(): Long =
        try {
            val pi = context.packageManager.getPackageInfo(context.packageName, 0)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) pi.longVersionCode
            else @Suppress("DEPRECATION") pi.versionCode.toLong()
        } catch (_: Exception) {
            BuildConfig.VERSION_CODE.toLong()
        }

    // ── Download + verify ────────────────────────────────────────────

    /**
     * Download the APK to `cacheDir/updates`, then verify its SHA-256 in Rust
     * against the (already signature-verified) manifest value. Blocking — call
     * off the main thread. Returns the verified file; throws [IOException] on a
     * network error or a SHA mismatch (truncated/swapped download, cf. C2),
     * leaving no partial file behind.
     */
    fun download(release: Release, onProgress: (Float) -> Unit): File {
        val dir = File(context.cacheDir, "updates").apply { mkdirs() }
        // Drop any stale downloads (we keep only the current target).
        dir.listFiles()?.forEach { it.delete() }

        val target = File(dir, "${release.versionName}.apk")
        val tmp = File(dir, "${release.versionName}.apk.part")

        val conn = (URL(release.apkUrl).openConnection() as HttpURLConnection).apply {
            connectTimeout = 15_000
            readTimeout = 30_000
            instanceFollowRedirects = true
        }
        try {
            conn.connect()
            if (conn.responseCode !in 200..299) {
                throw IOException("http_${conn.responseCode}")
            }
            val total = if (release.sizeBytes > 0) release.sizeBytes else conn.contentLengthLong
            conn.inputStream.use { input ->
                tmp.outputStream().use { out ->
                    val buf = ByteArray(64 * 1024)
                    var read = 0L
                    while (true) {
                        val n = input.read(buf)
                        if (n < 0) break
                        out.write(buf, 0, n)
                        read += n
                        if (total > 0) onProgress((read.toFloat() / total).coerceIn(0f, 1f))
                        else onProgress(-1f)
                    }
                }
            }
        } finally {
            conn.disconnect()
        }

        // SHA-256 lives in Rust — single source of truth shared with the manifest.
        if (!NativeBridge.nativeVerifyApk(tmp.absolutePath, release.sha256)) {
            tmp.delete()
            throw IOException("sha_mismatch")
        }
        if (target.exists()) target.delete()
        if (!tmp.renameTo(target)) {
            tmp.delete()
            throw IOException("rename_failed")
        }
        return target
    }

    // ── Install ──────────────────────────────────────────────────────

    /** True if the user has granted "install unknown apps" to this app. */
    fun canRequestInstall(): Boolean =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O)
            context.packageManager.canRequestPackageInstalls()
        else true

    /**
     * Intent to the system "allow install from this source" screen. Used when
     * [canRequestInstall] is false so we can lead the user there instead of
     * silently failing (LLD-12 §5.4).
     */
    fun unknownSourcesSettingsIntent(): Intent =
        Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:${context.packageName}"))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)

    /**
     * Hand the verified APK to the system installer. Primary path is
     * [PackageInstaller] (the system shows its confirm dialog, and — if the
     * install permission is missing — its own "allow this source" gate via
     * `STATUS_PENDING_USER_ACTION`). Falls back to an `ACTION_VIEW` +
     * `FileProvider` intent if the session path errors.
     *
     * Note: install succeeds only if this APK is signed by the **same**
     * keystore as the installed app (LLD-12 §5.3); otherwise Android refuses
     * the update-in-place and the failure is surfaced via [onInstallStatus].
     */
    fun install(file: File) {
        try {
            installViaSession(file)
        } catch (e: Exception) {
            Log.w(TAG, "PackageInstaller failed, falling back to ACTION_VIEW: $e")
            try {
                installViaView(file)
            } catch (e2: Exception) {
                Log.e(TAG, "ACTION_VIEW install fallback failed: $e2")
                onInstallStatus?.invoke(InstallStatus.Failed(e2.message ?: "install failed"))
            }
        }
    }

    private fun installViaSession(file: File) {
        ensureReceiverRegistered()
        val installer = context.packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL)
        params.setAppPackageName(context.packageName)
        val sessionId = installer.createSession(params)
        installer.openSession(sessionId).use { session ->
            session.openWrite("xr-proxy", 0, file.length()).use { out ->
                file.inputStream().use { it.copyTo(out) }
                session.fsync(out)
            }
            val intent = Intent(INSTALL_ACTION).setPackage(context.packageName)
            var flags = PendingIntent.FLAG_UPDATE_CURRENT
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) flags = flags or PendingIntent.FLAG_MUTABLE
            val pending = PendingIntent.getBroadcast(context, sessionId, intent, flags)
            session.commit(pending.intentSender)
        }
    }

    private fun installViaView(file: File) {
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", file)
        val intent = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(uri, "application/vnd.android.package-archive")
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        context.startActivity(intent)
    }

    @Volatile
    private var receiverRegistered = false

    private val installReceiver = object : BroadcastReceiver() {
        override fun onReceive(ctx: Context, intent: Intent) {
            when (intent.getIntExtra(PackageInstaller.EXTRA_STATUS, Int.MIN_VALUE)) {
                PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                    // The system wants the user to confirm (and grant the
                    // install-source permission, if missing). Launch its UI.
                    val confirm = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU)
                        intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java)
                    else
                        @Suppress("DEPRECATION") intent.getParcelableExtra(Intent.EXTRA_INTENT)
                    confirm?.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    runCatching { ctx.startActivity(confirm) }
                        .onFailure { onInstallStatus?.invoke(InstallStatus.Failed("cannot_open_installer")) }
                }
                PackageInstaller.STATUS_SUCCESS ->
                    onInstallStatus?.invoke(InstallStatus.Success)
                else -> {
                    val msg = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)
                    onInstallStatus?.invoke(InstallStatus.Failed(msg ?: "install_failed"))
                }
            }
        }
    }

    private fun ensureReceiverRegistered() {
        if (receiverRegistered) return
        ContextCompat.registerReceiver(
            context,
            installReceiver,
            IntentFilter(INSTALL_ACTION),
            ContextCompat.RECEIVER_NOT_EXPORTED,
        )
        receiverRegistered = true
    }

    /** Unregister the install-result receiver. Call from the owner's teardown. */
    fun release() {
        if (receiverRegistered) {
            runCatching { context.unregisterReceiver(installReceiver) }
            receiverRegistered = false
        }
    }

    companion object {
        private const val TAG = "UpdateManager"
        private const val CHECK_TIMEOUT_MS = 8_000L
        private const val INSTALL_ACTION = "com.xrproxy.app.INSTALL_RESULT"
    }
}
