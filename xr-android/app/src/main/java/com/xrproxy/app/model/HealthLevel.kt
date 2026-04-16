package com.xrproxy.app.model

/**
 * Session health indicator levels (LLD-06 §3.5a).
 * Mapped to visual states in HealthFace composable.
 */
enum class HealthLevel {
    /** 0 ERROR and 0 WARN in last 30 seconds. */
    Healthy,
    /** 0 ERROR, 1-10 WARN in last 30 seconds (background noise). */
    Good,
    /** 0 ERROR, >10 WARN in last 30 seconds. */
    Watching,
    /** >= 3 ERROR in 30s window, but < 5 errors in 5 seconds. */
    Hurt,
    /** >= 5 ERROR in last 5 seconds (burst of failures). */
    Critical,
}

/**
 * Rolling-window health tracker. Fed cumulative relay counters from
 * [StatsSnapshot] on each poll tick (~1s). Computes [HealthLevel] from
 * delta-based burst windows without needing the full error history.
 *
 * Downshift delay: transition to a healthier state is held for at least
 * [DOWNSHIFT_HOLD_MS] to prevent jitter between Critical <-> Hurt.
 */
class HealthTracker {

    private companion object {
        const val WINDOW_MS = 30_000L
        const val BURST_WINDOW_MS = 5_000L
        const val BURST_THRESHOLD = 5
        const val DOWNSHIFT_HOLD_MS = 5_000L
        const val WARN_NOISE_THRESHOLD = 10
        const val ERROR_HURT_THRESHOLD = 3
    }

    private var lastSeenErrors: Long = 0
    private var lastSeenWarns: Long = 0
    private val errorTimestamps = ArrayDeque<Long>()
    private val warnTimestamps = ArrayDeque<Long>()

    private var currentLevel: HealthLevel = HealthLevel.Healthy
    private var lastWorseTime: Long = 0L

    /**
     * Update with new cumulative counters. Call once per poll tick.
     * Returns the current [HealthLevel] after applying downshift hold.
     */
    fun update(relayErrors: Long, relayWarnings: Long): HealthLevel {
        val now = System.currentTimeMillis()

        val deltaErr = relayErrors - lastSeenErrors
        val deltaWarn = relayWarnings - lastSeenWarns
        lastSeenErrors = relayErrors
        lastSeenWarns = relayWarnings

        if (deltaErr > 0) {
            repeat(deltaErr.coerceAtMost(100).toInt()) { errorTimestamps.addLast(now) }
        }
        if (deltaWarn > 0) {
            repeat(deltaWarn.coerceAtMost(100).toInt()) { warnTimestamps.addLast(now) }
        }

        // Evict old timestamps
        val windowCutoff = now - WINDOW_MS
        while (errorTimestamps.firstOrNull()?.let { it < windowCutoff } == true) {
            errorTimestamps.removeFirst()
        }
        while (warnTimestamps.firstOrNull()?.let { it < windowCutoff } == true) {
            warnTimestamps.removeFirst()
        }

        // Compute raw level
        val burstCutoff = now - BURST_WINDOW_MS
        val recentBurstErrors = errorTimestamps.count { it >= burstCutoff }
        val rawLevel = when {
            recentBurstErrors >= BURST_THRESHOLD -> HealthLevel.Critical
            errorTimestamps.size >= ERROR_HURT_THRESHOLD -> HealthLevel.Hurt
            warnTimestamps.size > WARN_NOISE_THRESHOLD -> HealthLevel.Watching
            warnTimestamps.isNotEmpty() -> HealthLevel.Good
            else -> HealthLevel.Healthy
        }

        // Downshift hold: instant upgrade to worse, delayed downgrade to better
        if (rawLevel.ordinal >= currentLevel.ordinal) {
            // Same or worse — apply immediately
            currentLevel = rawLevel
            if (rawLevel.ordinal > HealthLevel.Healthy.ordinal) {
                lastWorseTime = now
            }
        } else {
            // Better — hold for DOWNSHIFT_HOLD_MS
            if (now - lastWorseTime >= DOWNSHIFT_HOLD_MS) {
                currentLevel = rawLevel
            }
        }

        return currentLevel
    }

    /** Reset tracker (e.g. on new connection). */
    fun reset() {
        lastSeenErrors = 0
        lastSeenWarns = 0
        errorTimestamps.clear()
        warnTimestamps.clear()
        currentLevel = HealthLevel.Healthy
        lastWorseTime = 0L
    }
}
