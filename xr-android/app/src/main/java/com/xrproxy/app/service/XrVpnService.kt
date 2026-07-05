package com.xrproxy.app.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.graphics.drawable.Icon
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.VpnService
import android.net.wifi.WifiInfo
import android.os.Binder
import android.os.Build
import android.os.IBinder
import android.os.ParcelFileDescriptor
import androidx.core.content.ContextCompat
import com.xrproxy.app.R
import com.xrproxy.app.data.TrustedNetworksRepository
import com.xrproxy.app.jni.NativeBridge
import com.xrproxy.app.model.HealthLevel
import com.xrproxy.app.model.HealthTracker
import com.xrproxy.app.ui.MainActivity
import java.io.FileInputStream
import java.io.FileOutputStream
import org.json.JSONObject
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull

/**
 * Foreground VPN service. Single source of truth for connection state and
 * live statistics — `VpnViewModel` binds to this service and mirrors its
 * `stateFlow` into UI state. Connection start/stop and the stats poll loop
 * live here so the UI can recover state via `bindService` after Activity
 * recreation or returning from background.
 *
 * Also owns the **trusted-network auto-pause** (task 3b-2): on a Wi-Fi the
 * user marked as trusted (home network already behind an xr-client router),
 * the tunnel pauses — engine stopped, TUN torn down, traffic falls through to
 * the router — while the service stays foreground watching the uplink, and
 * resumes itself when the phone leaves that network.
 */
class XrVpnService : VpnService() {

    companion object {
        const val ACTION_START = "com.xrproxy.app.START"
        const val ACTION_STOP = "com.xrproxy.app.STOP"
        const val ACTION_RESUME_OVERRIDE = "com.xrproxy.app.RESUME_OVERRIDE"
        const val ACTION_BIND_INTERNAL = "com.xrproxy.app.BIND_INTERNAL"
        const val EXTRA_CONFIG_JSON = "config_json"
        private const val CHANNEL_ID = "xr_vpn_channel"
        private const val NOTIFICATION_ID = 1

        // How long startVpn waits for the first capabilities callback before
        // deciding the initial network is untrusted and bringing the tunnel
        // up. Capabilities for the already-connected default network are
        // delivered almost immediately on registration, so this only bites
        // on a genuinely slow uplink — and the post-Connected backstop in
        // bringTunnelUp() still catches a late "trusted" verdict.
        private const val INITIAL_TRUST_TIMEOUT_MS = 1500L

        // While paused on a trusted network, re-run the restriction probe on
        // this cadence so a transient false "restricted" (cold proxy path, a
        // one-off timeout) self-heals — and a network that genuinely degrades
        // later still gets flagged. The probe only runs while paused (engine
        // down), so the cost is a few TLS connects per interval.
        private const val PROBE_INTERVAL_MS = 90_000L
        // Confirm a "restricted" verdict across two consecutive probes before
        // raising the warning, and recheck quickly while it is unconfirmed. Kills
        // the transient banner flash when the first probe runs on a not-yet-warm
        // uplink right after a pause (notably one that fires on screen-wake,
        // XR-021/XR-022); any reachable host still clears the flag at once.
        private const val RESTRICT_CONFIRM_PROBES = 2
        private const val RESTRICT_CONFIRM_DELAY_MS = 8_000L
    }

    enum class Phase { Idle, Preparing, Connecting, Finalizing, Connected, Paused, Stopping, Error }

    data class StatsSnapshot(
        val bytesUp: Long,
        val bytesDown: Long,
        val activeConnections: Int,
        val uptime: Long,
        val dnsQueries: Long,
        val tcpSyns: Long,
        val smolRecv: Long,
        val smolSend: Long,
        /** Cumulative WARN count (policy drops: fake IP, private IP, blocked DoT). */
        val relayWarnings: Long,
        /** Cumulative ERROR count (real I/O failures: mux open fail, timeouts). */
        val relayErrors: Long,
        val debugMsg: String,
        val recentErrors: List<String>,
        /** Имя активного сервера пула (LLD-10); пустое до старта движка. */
        val activeServer: String = "",
        /** Активен резервный сервер, на главном экране показывается
         *  «через [activeServer] (резерв)». */
        val backupActive: Boolean = false,
    )

    data class ServiceState(
        val phase: Phase = Phase.Idle,
        val errorMessage: String? = null,
        val snapshot: StatsSnapshot? = null,
        val speedUp: Long = 0,
        val speedDown: Long = 0,
        val health: HealthLevel = HealthLevel.Healthy,
        /** SSID display name when [phase] is [Phase.Paused], else null. */
        val pausedSsid: String? = null,
        /** While paused: the trusted network failed the restriction probe —
         *  blocked resources aren't reachable direct, so the pause risks
         *  cutting access (task 3b-2 §2). */
        val restrictedNetwork: Boolean = false,
        /** Diagnostic lines from the restriction probe (per-host DNS/connect/TLS
         *  outcome + verdict), surfaced in the app Log tab. The probe runs while
         *  paused with the engine stopped, so these can't go through the native
         *  error log — they ride here instead. */
        val probeLog: List<String> = emptyList(),
    )

    inner class LocalBinder : Binder() {
        fun service(): XrVpnService = this@XrVpnService
    }

    private val localBinder = LocalBinder()

    private val _stateFlow = MutableStateFlow(ServiceState())
    val stateFlow: StateFlow<ServiceState> = _stateFlow

    private val scope = CoroutineScope(Dispatchers.Default + SupervisorJob())

    // Serializes tunnel up/down transitions (connect, pause, resume, stop) so
    // overlapping network callbacks can't bring the tunnel up and tear it down
    // at the same time.
    private val transitionMutex = Mutex()

    private var vpnInterface: ParcelFileDescriptor? = null
    private var tunReadThread: Thread? = null
    private var tunWriteThread: Thread? = null
    @Volatile private var running = false

    // Config of the active session, kept so resumeFromPause() can rebuild the
    // tunnel with the same parameters after a trusted-network pause.
    @Volatile private var lastConfigJson: String? = null

    // Tracks the underlying (non-VPN) network so xr-core can bypass the
    // tunnel for direct-mode DNS lookups. Registered at VPN start, torn
    // down on stop. See NativeBridge.resolveDomain for the consumer side.
    private var networkCallback: ConnectivityManager.NetworkCallback? = null

    // Separate callback that watches only the DEFAULT (active) uplink so we
    // can (a) re-bind the tunnel when it switches (LTE↔Wi-Fi, task 3b-1) and
    // (b) detect trusted-SSID transitions for auto-pause (task 3b-2). Kept
    // apart from the resolver callback above, which reports *all* matching
    // networks — that one can't tell which uplink traffic actually uses.
    private var defaultNetworkCallback: ConnectivityManager.NetworkCallback? = null
    // The default network we last saw. Used to debounce re-bind (only on a
    // real switch) and to ignore stale capabilities callbacks for a replaced
    // network.
    @Volatile private var currentDefaultNetwork: Network? = null
    // Raw WifiInfo.getSSID() of the current default network (quotes and all),
    // or null when the uplink is non-Wi-Fi or the SSID is unavailable.
    @Volatile private var currentRawSsid: String? = null
    // The default network we were on when we paused. Auto-resume keys on a
    // change of THIS network — never on a transient SSID read glitch on the
    // same network (which previously caused spurious resumes).
    @Volatile private var pausedNetwork: Network? = null
    // Set when the default network changed and we still owe a re-bind — but
    // only if the new network turns out NOT to be trusted (a trusted network
    // pauses instead of re-binding; we must not do both — see maybeEvaluate).
    @Volatile private var pendingSwitch = false
    // The trusted network the user chose to keep the tunnel running on ("use
    // anyway"). While the default network equals this, auto-pause is skipped.
    // Cleared as soon as the default network changes.
    @Volatile private var overrideNetwork: Network? = null
    // Completed by the first capabilities callback of a session, so startVpn
    // can wait briefly for the initial SSID before deciding to pause.
    @Volatile private var firstCapsSignal: CompletableDeferred<Unit>? = null
    // Rotates which hosts the restriction probe picks across pauses.
    @Volatile private var probeSeed = 0
    // Whether the current default uplink is Wi-Fi. Lets the poll-loop trusted
    // re-check skip pausing on cellular when a stale Wi-Fi is still associated.
    @Volatile private var currentDefaultIsWifi = false

    // Re-evaluates the trusted-network decision when the screen turns on. The
    // event-driven auto-pause can be missed while the device is idle (network
    // callbacks coalesced in Doze, the poll-loop's delay() frozen with the CPU
    // asleep), leaving the tunnel up on a trusted Wi-Fi until something wakes
    // it. Screen-on is a reliable wake signal — re-run the same evaluation the
    // default-network callback would, so the pause lands as soon as the user
    // looks at the phone instead of only after they open the app.
    private var screenOnReceiver: android.content.BroadcastReceiver? = null

    // Bounded buffer of restriction-probe diagnostic lines (guarded by itself).
    // Surfaced in ServiceState.probeLog → app Log tab.
    private val probeLogBuf = ArrayDeque<String>()
    private val probeLogTime = java.text.SimpleDateFormat("HH:mm:ss", java.util.Locale.US)
    // The running restriction-probe loop (one per pause). Cancelled when the
    // pause ends or a re-probe restarts it.
    @Volatile private var probeJob: Job? = null

    private val trustedRepo: TrustedNetworksRepository by lazy {
        TrustedNetworksRepository(getSharedPreferences("xr_proxy", Context.MODE_PRIVATE))
    }

    // Speed tracking: previous snapshot for delta computation
    private var prevBytesUp: Long = 0
    private var prevBytesDown: Long = 0
    private var prevTickMs: Long = 0

    // Health tracking (LLD-06 §3.5a)
    private val healthTracker = HealthTracker()

    // ── Lifecycle ──────────────────────────────────────────────────────

    override fun onCreate() {
        super.onCreate()
        NativeBridge.current = this
        createNotificationChannel()
        registerScreenOnReceiver()
    }

    override fun onDestroy() {
        // Conditional null: если уже создан новый instance (Disconnect→Connect
        // race), не трогаем — иначе onDestroy старого обнулит ссылку нового,
        // и protectSocket() вернёт false → все сокеты полезут через TUN
        // петлёй (VPN→TUN→VPN→…) → connect timeout на всё.
        if (NativeBridge.current === this) {
            NativeBridge.current = null
        }
        screenOnReceiver?.let {
            try { unregisterReceiver(it) } catch (_: Exception) {}
            screenOnReceiver = null
        }
        scope.cancel()
        super.onDestroy()
    }

    override fun onBind(intent: Intent): IBinder? {
        // Internal binder used by VpnViewModel; the VPN framework itself
        // still gets the default BIND_VPN_SERVICE path via super.onBind().
        if (intent.action == ACTION_BIND_INTERNAL) return localBinder
        return super.onBind(intent)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Process death + START_STICKY restart delivers a null intent.
        // Do not silently resurrect a zombie — just go away.
        if (intent == null) {
            stopSelf()
            return START_NOT_STICKY
        }
        when (intent.action) {
            ACTION_START -> {
                val configJson = intent.getStringExtra(EXTRA_CONFIG_JSON)
                if (configJson == null) {
                    stopSelf()
                    return START_NOT_STICKY
                }
                scope.launch { startVpn(configJson) }
            }
            ACTION_STOP -> stopFromUi()
            ACTION_RESUME_OVERRIDE -> resumeOverride()
        }
        return START_STICKY
    }

    override fun onRevoke() {
        stopFromUi()
        super.onRevoke()
    }

    // ── Public API used by VpnViewModel through the binder ────────────

    fun stopFromUi() {
        val current = _stateFlow.value.phase
        if (current == Phase.Idle || current == Phase.Stopping) return
        scope.launch {
            transitionMutex.withLock {
                if (_stateFlow.value.phase == Phase.Idle) return@withLock
                publish(Phase.Stopping)
                updateNotification()
                stopInternal()
                stopForeground(STOP_FOREGROUND_REMOVE)
                // Публикуем Idle ПОСЛЕ stopForeground: VM увидит Idle через
                // stateFlow и сделает `unbindService`, что наконец-то позволит
                // сервису реально умереть (stopSelf на bound-сервисе — no-op).
                // Без этого следующий Connect приходит на тот же instance,
                // где native engine уже остановлен, и туннель не поднимается.
                publish(Phase.Idle, snapshot = null)
                stopSelf()
            }
        }
    }

    fun clearLog() {
        NativeBridge.nativeClearErrorLog()
        synchronized(probeLogBuf) { probeLogBuf.clear() }
        _stateFlow.value = _stateFlow.value.copy(snapshot = readSnapshot(), probeLog = emptyList())
    }

    /** Raw SSID of the current uplink as seen by the default-network callback
     *  (non-redacted when the app holds location permission). Used by the UI
     *  to pre-fill "add current network". Null when unknown / non-Wi-Fi. */
    fun currentRawSsidOrNull(): String? = currentRawSsid

    /** Notification "use anyway" action: resume from a trusted-network pause
     *  and keep the tunnel up on this network until it changes. */
    private fun resumeOverride() {
        overrideNetwork = currentDefaultNetwork
        scope.launch { requestResume() }
    }

    // ── Connection flow ───────────────────────────────────────────────

    private suspend fun startVpn(configJson: String) {
        lastConfigJson = configJson

        publish(Phase.Preparing)
        startForegroundWithLocationType()

        // Arm the first-capabilities signal BEFORE registering the callback so
        // we can't miss the very first onCapabilitiesChanged (which lands almost
        // immediately for the already-connected default network).
        val capsSignal = CompletableDeferred<Unit>()
        firstCapsSignal = capsSignal

        // Register BEFORE establish() so the underlying network is known by
        // the time xr-core starts resolving. registerNetworkCallback gives
        // us the non-VPN default network directly — we explicitly exclude
        // VPN-capable networks, otherwise we'd pick up our own tunnel after
        // it's up and get a resolve loop.
        registerUnderlyingNetworkCallback()

        transitionMutex.withLock {
            // Wait briefly for the first SSID verdict so we can pause
            // pre-emptively on a trusted network instead of flickering through
            // a full connect just to tear it down. Completes immediately on
            // cellular (caps arrive with no WifiInfo) — no penalty there.
            withTimeoutOrNull(INITIAL_TRUST_TIMEOUT_MS) { capsSignal.await() }
            firstCapsSignal = null

            val raw = currentRawSsid
            if (raw != null && isTrusted(raw)) {
                doPause(raw)
                return
            }
            bringTunnelUp()
        }
    }

    /**
     * Establish the TUN, start the native engine and the I/O threads, and
     * publish Connected. Returns false (after publishing Error + stopSelf) on
     * failure. MUST be called holding [transitionMutex].
     */
    private suspend fun bringTunnelUp(): Boolean {
        val configJson = lastConfigJson ?: run {
            publish(Phase.Error, errorMessage = "Нет конфигурации")
            stopSelf()
            return false
        }

        // Reset speed and health tracking for the (re)started session.
        prevBytesUp = 0; prevBytesDown = 0; prevTickMs = 0
        healthTracker.reset()

        val iface = Builder()
            .setSession("XR Proxy")
            .addAddress("10.0.0.2", 32)
            .addRoute("0.0.0.0", 0)
            .addDnsServer("10.0.0.1")
            // 1280 = IPv6 minimum MTU. Mobile IPv6/NAT64 uplinks run MTU ~1300
            // and tunnel stacking (VPN over a router that itself proxies) shrinks
            // it further; a 1500 TUN advertised an MSS the underlay couldn't carry,
            // so re-originated streams stalled (bug 3c). Pairs with the MSS clamp
            // on outbound sockets in xr-core session.rs.
            .setMtu(1280)
            .setBlocking(true)
            .establish()

        if (iface == null) {
            publish(Phase.Error, errorMessage = "TUN establish failed")
            updateNotification()
            stopInternal()
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return false
        }
        vpnInterface = iface

        publish(Phase.Connecting)
        updateNotification()

        val startError = NativeBridge.nativeStart(iface.fd, configJson)
        if (startError != null) {
            iface.close()
            vpnInterface = null
            publish(Phase.Error, errorMessage = startError)
            updateNotification()
            stopInternal()
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return false
        }

        running = true
        pendingSwitch = false

        tunReadThread = Thread {
            val input = FileInputStream(iface.fileDescriptor)
            val buf = ByteArray(1500)
            try {
                while (running) {
                    val n = input.read(buf)
                    if (n > 0) NativeBridge.nativePushPacket(buf.copyOf(n))
                }
            } catch (_: Exception) {
                // TUN closed.
            }
        }.apply { name = "tun-read"; isDaemon = true; start() }

        tunWriteThread = Thread {
            val output = FileOutputStream(iface.fileDescriptor)
            try {
                while (running) {
                    val packet = NativeBridge.nativePopPacket()
                    if (packet != null) output.write(packet)
                    else Thread.sleep(1)
                }
            } catch (_: Exception) {
                // TUN closed.
            }
        }.apply { name = "tun-write"; isDaemon = true; start() }

        // Transition through Finalizing before Connected (LLD-06 §3.6)
        publish(Phase.Finalizing)
        updateNotification()

        publish(Phase.Connected, snapshot = readSnapshot())
        updateNotification()

        // Backstop: capabilities may have arrived after the initial-trust wait
        // timed out, or a trusted network may have appeared during connect.
        // Re-check now while we hold the lock — if trusted, pause instead of
        // running. (Skipped if the user chose "use anyway" on this network.)
        val raw = currentRawSsid
        if (raw != null && currentDefaultNetwork != overrideNetwork && isTrusted(raw)) {
            doPause(raw)
            return true
        }

        scope.launch { pollLoop() }
        return true
    }

    private suspend fun pollLoop() {
        while (running && scope.isActive) {
            val native = NativeBridge.nativeGetState()

            // Engine reported a fatal error (health check failed, event loop
            // crashed, etc.). Shut down the tunnel and publish Error so the VM
            // can show the message and unbind.
            if (native.startsWith("Error:")) {
                val errorMsg = native.removePrefix("Error: ").trim()
                publish(Phase.Error, errorMessage = errorMsg)
                updateNotification()
                stopInternal()
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
                return
            }

            // Periodic trusted-network re-check. The event-driven auto-pause in
            // maybeEvaluate can miss a return to a trusted Wi-Fi: while the
            // tunnel is up the network-callback SSID is often redacted, and once
            // the re-bind for the switch has fired nothing re-evaluates. Re-read
            // the SSID via WifiManager (VPN-independent) each tick and pause if
            // it resolves to a trusted network. Gated on the default uplink being
            // Wi-Fi so a stale association can't pause us on cellular.
            if (_stateFlow.value.phase == Phase.Connected && currentDefaultIsWifi &&
                currentDefaultNetwork != overrideNetwork
            ) {
                val wifiSsid = usableSsid(currentWifiSsidRaw())
                if (wifiSsid != null && isTrusted(wifiSsid)) {
                    requestPause(wifiSsid)
                    return
                }
            }

            val phase = when (native) {
                "Connected" -> Phase.Connected
                "Connecting" -> Phase.Connecting
                "Disconnecting" -> Phase.Stopping
                else -> _stateFlow.value.phase
            }

            // Don't clobber a pause that landed between ticks.
            if (_stateFlow.value.phase == Phase.Paused || !running) return

            val snap = readSnapshot()

            // Speed computation: bytes/sec delta since previous tick
            val now = System.currentTimeMillis()
            var speedUp = 0L
            var speedDown = 0L
            if (prevTickMs > 0) {
                val dtSec = ((now - prevTickMs).coerceAtLeast(1)) / 1000.0
                speedUp = ((snap.bytesUp - prevBytesUp) / dtSec).toLong().coerceAtLeast(0)
                speedDown = ((snap.bytesDown - prevBytesDown) / dtSec).toLong().coerceAtLeast(0)
            }
            prevBytesUp = snap.bytesUp
            prevBytesDown = snap.bytesDown
            prevTickMs = now

            // Health tracking
            val health = healthTracker.update(snap.relayErrors, snap.relayWarnings)

            _stateFlow.value = _stateFlow.value.copy(
                phase = phase,
                snapshot = snap,
                speedUp = speedUp,
                speedDown = speedDown,
                health = health,
            )
            updateNotification()
            delay(1000)
        }
    }

    /** Tear the tunnel down (engine + TUN + I/O threads) but KEEP the network
     *  callbacks registered — used for trusted-network pause, where we must
     *  keep watching the uplink to resume. */
    private fun tearTunnelDown() {
        probeJob?.cancel()
        probeJob = null
        running = false
        NativeBridge.nativeStop()
        tunReadThread?.interrupt()
        tunWriteThread?.interrupt()
        tunReadThread = null
        tunWriteThread = null
        vpnInterface?.close()
        vpnInterface = null
    }

    /** Full stop: tear the tunnel down and unregister network callbacks. */
    private fun stopInternal() {
        tearTunnelDown()
        unregisterUnderlyingNetworkCallback()
    }

    // ── Trusted-network pause / resume (task 3b-2) ────────────────────

    /** Enter the paused state. MUST be called holding [transitionMutex].
     *  Safe whether or not a tunnel is currently up (tearTunnelDown is a
     *  no-op when nothing is running). */
    private fun doPause(rawSsid: String?) {
        tearTunnelDown()
        pausedNetwork = currentDefaultNetwork
        val display = rawSsid?.let { NativeBridge.nativeNormalizeSsid(it) }
        publish(Phase.Paused, snapshot = null, pausedSsid = display)
        updateNotification()
        launchRestrictionProbe()
    }

    /**
     * While paused, repeatedly check whether blocked resources are actually
     * reachable on this network (tunnel is down → probe goes direct over the
     * uplink) and flag it so the UI/notification can warn that pausing here cuts
     * access (task 3b-2 §2). Runs as a LOOP, not a one-shot: a transient failure
     * (cold proxy path, one-off timeout) would otherwise pin the "restricted"
     * warning for the whole pause — re-probing every [PROBE_INTERVAL_MS] lets it
     * self-heal (and catches a network that degrades later). Restarts on each
     * call (cancels the previous loop), so a re-target or a foreground re-probe
     * re-evaluates immediately.
     */
    private fun launchRestrictionProbe() {
        probeJob?.cancel()
        probeJob = scope.launch {
            // Let routing settle after the TUN teardown — and give the router's
            // transparent-proxy path a moment to warm — before the first probe.
            delay(2000)
            var restrictedStreak = 0
            while (isActive && _stateFlow.value.phase == Phase.Paused) {
                val seed = probeSeed++
                val net = NativeBridge.underlyingNetwork
                val ssid = _stateFlow.value.pausedSsid
                logProbe("── Доверенная сеть «${ssid ?: "?"}» — проверка доступности ресурсов")
                val result = withContext(Dispatchers.IO) { RestrictionProbe.probe(net, seed, ::logProbe) }
                // Network may have changed during the probe — only apply if still paused.
                if (_stateFlow.value.phase != Phase.Paused) break
                restrictedStreak = if (result.restricted) restrictedStreak + 1 else 0
                val confirmed = restrictedStreak >= RESTRICT_CONFIRM_PROBES
                if (_stateFlow.value.restrictedNetwork != confirmed) {
                    _stateFlow.value = _stateFlow.value.copy(restrictedNetwork = confirmed)
                    updateNotification()
                }
                if (result.restricted && !confirmed) {
                    logProbe("  одиночный сбой, баннер не показываю — перепроверю через ${RESTRICT_CONFIRM_DELAY_MS / 1000}с")
                }
                // Recheck soon while a lone restricted result is unconfirmed (cold
                // uplink self-heals); otherwise settle to the normal interval.
                delay(if (result.restricted && !confirmed) RESTRICT_CONFIRM_DELAY_MS else PROBE_INTERVAL_MS)
            }
        }
    }

    /** Re-run the restriction probe now if we're paused — called when the app
     *  returns to the foreground (the moment the warning is actually looked at),
     *  so a stale "restricted" flag doesn't linger until the next interval. */
    fun reprobeRestrictionsIfPaused() {
        if (_stateFlow.value.phase == Phase.Paused) launchRestrictionProbe()
    }

    /** Append a restriction-probe diagnostic line to the app log (see
     *  [ServiceState.probeLog]). Called from the probe's IO thread; the same
     *  read-modify-write on _stateFlow as the rest of the service. */
    private fun logProbe(line: String) {
        val stamped = "${probeLogTime.format(java.util.Date())} $line"
        synchronized(probeLogBuf) {
            probeLogBuf.addLast(stamped)
            while (probeLogBuf.size > 120) probeLogBuf.removeFirst()
            _stateFlow.value = _stateFlow.value.copy(probeLog = probeLogBuf.toList())
        }
    }

    /** UI ("Включить здесь") / notification action to keep the tunnel running
     *  on the current trusted network until it changes. */
    fun resumeOnTrustedNetwork() = resumeOverride()

    private suspend fun requestPause(rawSsid: String?) {
        transitionMutex.withLock {
            val ph = _stateFlow.value.phase
            if (ph != Phase.Connected && ph != Phase.Finalizing) return@withLock
            doPause(rawSsid)
        }
    }

    private suspend fun requestResume() {
        transitionMutex.withLock {
            if (_stateFlow.value.phase != Phase.Paused) return@withLock
            probeJob?.cancel()
            probeJob = null
            publish(Phase.Connecting)
            updateNotification()
            bringTunnelUp()
        }
    }

    // ── Network watching: SSID auto-pause + LTE↔Wi-Fi re-bind ─────────

    private fun isTrusted(rawSsid: String?): Boolean {
        if (rawSsid == null) return false
        val trusted = trustedRepo.activeTrustedSsids()
        if (trusted.isEmpty()) return false
        return NativeBridge.nativeSsidMatches(rawSsid, trusted)
    }

    private fun extractRawSsid(caps: NetworkCapabilities): String? {
        val info = caps.transportInfo
        return if (info is WifiInfo) info.ssid else null
    }

    /** Returns [raw] when it is a usable SSID, or null when it is absent or one
     *  of Android's redacted sentinels (`<unknown ssid>`, `0x`, empty). Mirrors
     *  the Rust `normalize_ssid` so Kotlin-level "no SSID" checks agree with the
     *  matcher. CRITICAL: while our own VPN is up, the caps SSID comes back as
     *  the literal string `"<unknown ssid>"` — non-null — so a plain `== null`
     *  check misreads it as a distinct, untrusted network and (a) skips the
     *  WifiManager fallback below and (b) defeats the "wait for SSID" guard in
     *  the paused branch, leaving the tunnel stuck up on a trusted network. */
    private fun usableSsid(raw: String?): String? =
        raw?.takeIf { NativeBridge.nativeNormalizeSsid(it) != null }

    /**
     * Raw SSID of the currently associated Wi-Fi via WifiManager. Unlike the
     * NetworkCapabilities path, this is independent of our own VPN being up —
     * which is exactly the case (return to a trusted network while the tunnel
     * is running) where the caps SSID comes back empty. Needs location
     * permission + services, like every SSID read; null otherwise.
     */
    @Suppress("DEPRECATION")
    private fun currentWifiSsidRaw(): String? {
        val wifi = getSystemService(Context.WIFI_SERVICE) as? android.net.wifi.WifiManager
            ?: return null
        return try {
            wifi.connectionInfo?.ssid
        } catch (_: Exception) {
            null
        }
    }

    /**
     * React to a default-network capabilities update. Decides between
     * auto-pause (entered trusted SSID), auto-resume (left trusted SSID), and
     * the LTE↔Wi-Fi re-bind from task 3b-1 — making sure pause and re-bind
     * never both fire for the same change.
     */
    private fun maybeEvaluate(network: Network, caps: NetworkCapabilities) {
        // usableSsid() folds the redacted "<unknown ssid>" literal (returned by
        // the caps path while our VPN is up) into null, so the fallback and the
        // paused-branch guards below behave correctly instead of treating it as
        // a real untrusted network.
        var raw = usableSsid(extractRawSsid(caps))
        // While our own VPN is up, the default-network caps come back without a
        // usable Wi-Fi SSID, which would hide a return to a trusted network
        // (tunnel stays up instead of pausing). WifiManager reports the
        // associated Wi-Fi regardless of VPN state — use it as a fallback when
        // the uplink is Wi-Fi but the caps SSID is unusable.
        if (raw == null && caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) {
            raw = usableSsid(currentWifiSsidRaw())
        }
        currentDefaultIsWifi = caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)
        currentRawSsid = raw
        // Unblock startVpn's initial-trust wait now that we have an SSID verdict.
        firstCapsSignal?.complete(Unit)

        val trusted = isTrusted(raw)
        when (_stateFlow.value.phase) {
            Phase.Paused -> {
                when {
                    network == pausedNetwork -> {
                        // Still on the network we paused on. NEVER resume on a
                        // transient capability update (incl. a momentary
                        // <unknown ssid>) — only an actual move resumes. This is
                        // the spurious-resume fix. Refresh the SSID label if it
                        // resolved/changed while staying trusted.
                        if (trusted) {
                            val display = raw?.let { NativeBridge.nativeNormalizeSsid(it) }
                            if (display != null && display != _stateFlow.value.pausedSsid) {
                                publish(Phase.Paused, snapshot = null, pausedSsid = display)
                                updateNotification()
                            }
                        }
                    }
                    trusted -> {
                        // Moved to a different but still trusted network — stay
                        // paused, retarget and re-probe restrictions.
                        pausedNetwork = network
                        val display = raw?.let { NativeBridge.nativeNormalizeSsid(it) }
                        publish(Phase.Paused, snapshot = null, pausedSsid = display)
                        updateNotification()
                        launchRestrictionProbe()
                    }
                    caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) && raw == null -> {
                        // New Wi-Fi but its SSID hasn't resolved yet — wait for
                        // the next caps update instead of resuming prematurely
                        // (avoids churn on a Wi-Fi reconnect).
                    }
                    else -> {
                        // Left to a non-trusted network (cellular / other Wi-Fi).
                        scope.launch { requestResume() }
                    }
                }
            }
            Phase.Connected, Phase.Finalizing -> {
                if (trusted && network != overrideNetwork) {
                    pendingSwitch = false
                    scope.launch { requestPause(raw) }
                } else if (!trusted && pendingSwitch) {
                    pendingSwitch = false
                    NativeBridge.nativeOnNetworkChanged()
                }
            }
            else -> {
                // Preparing/Connecting/Stopping/Idle/Error: don't act. The
                // connect path (initial-trust wait + Connected backstop)
                // owns the trusted decision during bring-up.
            }
        }
    }

    private fun registerUnderlyingNetworkCallback() {
        if (networkCallback != null) return
        val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager ?: return

        // Match any real uplink (WiFi/cellular/ethernet) but exclude our own
        // VPN transport. `NET_CAPABILITY_NOT_VPN` is exactly that filter and
        // has been available since API 21, unlike `removeTransportType`.
        val request = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
            .build()

        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                NativeBridge.underlyingNetwork = network
                // Inform the framework that traffic is metered/unmetered
                // through this uplink — it's also what SystemUI uses to
                // decide the signal-strength indicator above the VPN key.
                setUnderlyingNetworks(arrayOf(network))
            }
            override fun onLost(network: Network) {
                // Only clear if the lost one is what we were pointing at;
                // another network may already be Available.
                if (NativeBridge.underlyingNetwork == network) {
                    NativeBridge.underlyingNetwork = null
                }
            }
        }
        try {
            cm.registerNetworkCallback(request, callback)
            networkCallback = callback
        } catch (_: SecurityException) {
            // Some OEM ROMs deny registerNetworkCallback without
            // ACCESS_NETWORK_STATE edge cases. Swallow and let direct
            // mode fall back to UDP:53 probes — no regression vs before.
        }

        registerDefaultNetworkRebindCallback(cm)
    }

    /**
     * Watch the default uplink for re-bind (LTE↔Wi-Fi, task 3b-1) and trusted
     * SSID transitions (task 3b-2). `registerDefaultNetworkCallback` reports
     * exactly the network the OS routes through; for a VPN app that is the
     * underlying physical network, not our own tunnel — so there's no
     * resolve-loop concern here. SSID lives in the capabilities, so the actual
     * decision happens in onCapabilitiesChanged (where we know the network).
     */
    private fun registerDefaultNetworkRebindCallback(cm: ConnectivityManager) {
        if (defaultNetworkCallback != null) return
        // CRITICAL for SSID detection: on API 31+ the SSID inside
        // NetworkCapabilities.transportInfo is redacted to "<unknown ssid>"
        // unless the callback is registered with FLAG_INCLUDE_LOCATION_INFO
        // (even with ACCESS_FINE_LOCATION granted and location services on).
        // Without it, trusted-network matching silently never fires. The flag
        // constructor only exists on API 31+, so branch by SDK.
        val callback = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            object : ConnectivityManager.NetworkCallback(
                ConnectivityManager.NetworkCallback.FLAG_INCLUDE_LOCATION_INFO,
            ) {
                override fun onAvailable(network: Network) = onDefaultAvailable(network)
                override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) =
                    onDefaultCaps(network, caps)
            }
        } else {
            object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) = onDefaultAvailable(network)
                override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) =
                    onDefaultCaps(network, caps)
            }
        }
        try {
            cm.registerDefaultNetworkCallback(callback)
            defaultNetworkCallback = callback
        } catch (_: RuntimeException) {
            // Best-effort: without it, a network switch still recovers via the
            // slow native consecutive-timeout detector — just not instantly.
        }
    }

    private fun onDefaultAvailable(network: Network) {
        val previous = currentDefaultNetwork
        currentDefaultNetwork = network
        // A real switch — owe a re-bind (deferred until onCapabilities tells us
        // the new network isn't trusted; see maybeEvaluate).
        if (previous != null && previous != network) {
            pendingSwitch = true
            overrideNetwork = null
        }
    }

    private fun onDefaultCaps(network: Network, caps: NetworkCapabilities) {
        // Ignore stale callbacks for a network that's no longer default.
        if (network != currentDefaultNetwork) return
        maybeEvaluate(network, caps)
    }

    private fun registerScreenOnReceiver() {
        if (screenOnReceiver != null) return
        val receiver = object : android.content.BroadcastReceiver() {
            override fun onReceive(context: Context?, intent: Intent?) {
                if (intent?.action == Intent.ACTION_SCREEN_ON) reevaluateTrustedNetwork()
            }
        }
        try {
            registerReceiver(receiver, android.content.IntentFilter(Intent.ACTION_SCREEN_ON))
            screenOnReceiver = receiver
        } catch (_: Exception) {
            // Best-effort: without it, the trusted re-check still recovers on the
            // next event callback or poll tick once the device is fully awake.
        }
    }

    /**
     * Re-run the trusted-network decision now against the current default
     * uplink — pause if we're Connected on a trusted Wi-Fi, resume if we left
     * one. Called on a wake signal (screen-on, app-foreground) to recover the
     * pause/resume the idle event path can miss: while the device is dozing the
     * network callbacks are coalesced and the poll-loop's delay() is frozen with
     * the CPU asleep, so the tunnel can sit up on a trusted network until the
     * phone wakes. Reuses [maybeEvaluate] so the decision stays identical to the
     * callback path (incl. the WifiManager SSID fallback for the redacted caps
     * SSID while our VPN is up).
     */
    fun reevaluateTrustedNetwork() {
        val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager ?: return
        // CRITICAL (XR-021): a synchronous getNetworkCapabilities() returns the
        // SSID redacted to "<unknown ssid>" — FLAG_INCLUDE_LOCATION_INFO applies
        // only to a registered callback, never to a direct query — and the
        // WifiManager SSID fallback is blocked while we are backgrounded. So a
        // wake-time re-check fired from the SCREEN_ON receiver (screen on, app
        // still in background) could never read the SSID: the decision fell to
        // "untrusted" and the tunnel sat Connected on a trusted network until the
        // app was foregrounded, where WifiManager unblocks. That is exactly the
        // "shade updates only on app entry" symptom. Pull one fresh caps delivery
        // through a short-lived default callback carrying location info, so the
        // trusted decision sees the real SSID in the background too.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            val probe = object : ConnectivityManager.NetworkCallback(
                ConnectivityManager.NetworkCallback.FLAG_INCLUDE_LOCATION_INFO,
            ) {
                private var fired = false
                override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
                    if (fired) return
                    fired = true
                    try { cm.unregisterNetworkCallback(this) } catch (_: Exception) {}
                    currentDefaultNetwork = network
                    maybeEvaluate(network, caps)
                }
            }
            try {
                cm.registerDefaultNetworkCallback(probe)
                // Leak guard: drop the one-shot if no caps ever arrive (no default
                // network, or the platform never calls back).
                scope.launch {
                    delay(3000)
                    try { cm.unregisterNetworkCallback(probe) } catch (_: Exception) {}
                }
            } catch (_: RuntimeException) {
                reevaluateViaDirectQuery(cm)
            }
        } else {
            // Pre-31 the caps SSID isn't location-redacted the same way; the
            // direct query plus WifiManager fallback is the callback path's source.
            reevaluateViaDirectQuery(cm)
        }
    }

    private fun reevaluateViaDirectQuery(cm: ConnectivityManager) {
        val net = currentDefaultNetwork ?: return
        val caps = try { cm.getNetworkCapabilities(net) } catch (_: Exception) { null } ?: return
        maybeEvaluate(net, caps)
    }

    private fun unregisterUnderlyingNetworkCallback() {
        val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
        defaultNetworkCallback?.let { cb ->
            defaultNetworkCallback = null
            currentDefaultNetwork = null
            currentRawSsid = null
            currentDefaultIsWifi = false
            pausedNetwork = null
            pendingSwitch = false
            overrideNetwork = null
            try {
                cm?.unregisterNetworkCallback(cb)
            } catch (_: IllegalArgumentException) {
                // Already unregistered — benign race with Android-side cleanup.
            }
        }
        val cb = networkCallback ?: return
        networkCallback = null
        NativeBridge.underlyingNetwork = null
        if (cm == null) return
        try {
            cm.unregisterNetworkCallback(cb)
        } catch (_: IllegalArgumentException) {
            // Already unregistered — benign race with Android-side cleanup.
        }
    }

    // ── State publishing helpers ──────────────────────────────────────

    private fun publish(
        phase: Phase,
        snapshot: StatsSnapshot? = _stateFlow.value.snapshot,
        errorMessage: String? = null,
        pausedSsid: String? = if (phase == Phase.Paused) _stateFlow.value.pausedSsid else null,
        // Reset the restriction flag whenever we leave the paused state; the
        // probe re-sets it on the next pause.
        restrictedNetwork: Boolean = if (phase == Phase.Paused) _stateFlow.value.restrictedNetwork else false,
    ) {
        _stateFlow.value = _stateFlow.value.copy(
            phase = phase,
            snapshot = snapshot,
            errorMessage = errorMessage,
            pausedSsid = pausedSsid,
            restrictedNetwork = restrictedNetwork,
        )
    }

    private fun readSnapshot(): StatsSnapshot {
        val raw = NativeBridge.nativeGetStats()
        // Честный JSON через org.json — ручной парсер ломался на WARN-строках
        // формата `[domain] dst: error`, где квадратная скобка внутри
        // строкового литерала ошибочно считалась концом массива.
        val json = try {
            JSONObject(raw)
        } catch (_: Exception) {
            return StatsSnapshot(
                bytesUp = 0, bytesDown = 0, activeConnections = 0, uptime = 0,
                dnsQueries = 0, tcpSyns = 0, smolRecv = 0, smolSend = 0,
                relayWarnings = 0, relayErrors = 0, debugMsg = "", recentErrors = emptyList(),
            )
        }
        val errorsJson = json.optJSONArray("errors")
        val recentErrors: List<String> = if (errorsJson != null) {
            List(errorsJson.length()) { i -> errorsJson.optString(i, "") }
                .filter { it.isNotEmpty() }
        } else {
            emptyList()
        }
        return StatsSnapshot(
            bytesUp = json.optLong("bytes_up", 0),
            bytesDown = json.optLong("bytes_down", 0),
            activeConnections = json.optInt("active", 0),
            uptime = json.optLong("uptime", 0),
            dnsQueries = json.optLong("dns", 0),
            tcpSyns = json.optLong("syns", 0),
            smolRecv = json.optLong("smol_recv", 0),
            smolSend = json.optLong("smol_send", 0),
            relayWarnings = json.optLong("relay_warn", 0),
            relayErrors = json.optLong("relay_err", 0),
            debugMsg = json.optString("debug", ""),
            recentErrors = recentErrors,
            activeServer = json.optString("active_server", ""),
            backupActive = json.optBoolean("backup_active", false),
        )
    }

    // ── Notification ──────────────────────────────────────────────────

    /**
     * Go foreground, declaring the `location` FGS type when (and only when)
     * location permission is actually granted. A location-typed foreground
     * service keeps foreground-level location access for as long as the tunnel
     * runs, which is what lets the trusted-network check read the Wi-Fi SSID
     * while the app UI is backgrounded (XR-023): without it the SSID is redacted
     * off the foreground and auto-pause only fired after opening the app. The
     * type MUST be omitted when location is not granted — on Android 14+ starting
     * a location-typed FGS without the permission throws and would kill the VPN.
     */
    private fun startForegroundWithLocationType() {
        val notif = buildNotification(_stateFlow.value)
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, notif)
            return
        }
        var types = 0
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            types = types or android.content.pm.ServiceInfo.FOREGROUND_SERVICE_TYPE_SYSTEM_EXEMPTED
        }
        val locGranted = checkSelfPermission(android.Manifest.permission.ACCESS_FINE_LOCATION) ==
            android.content.pm.PackageManager.PERMISSION_GRANTED
        if (locGranted) {
            types = types or android.content.pm.ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION
        }
        if (types != 0) {
            startForeground(NOTIFICATION_ID, notif, types)
        } else {
            startForeground(NOTIFICATION_ID, notif)
        }
    }

    private fun updateNotification() {
        val nm = getSystemService(NotificationManager::class.java)
        nm.notify(NOTIFICATION_ID, buildNotification(_stateFlow.value))
    }

    private fun createNotificationChannel() {
        // IMPORTANCE_LOW — канал виден в статус-баре, но БЕЗ звука и без
        // heads-up pop-up'а. Для persistent foreground VPN-сервиса это
        // стандарт (так же делают Tailscale, WireGuard, ProtonVPN):
        // пользователь не должен слышать "дзынь" при каждом Connect,
        // тем более при внутренних рестартах сервиса.
        //
        // Важно: канал создаётся ОДИН раз на инсталляцию. Если до этой
        // правки приложение уже было установлено с IMPORTANCE_DEFAULT,
        // новый уровень сам по себе не применится — Android не позволяет
        // понижать importance существующего канала. Чтобы сброс
        // сработал для test-инсталла, пользователю нужно либо
        // переустановить приложение, либо вручную отключить звук в
        // Settings → Apps → XR Proxy → Notifications → "XR Proxy VPN"
        // → Sound: None.
        val channel = NotificationChannel(
            CHANNEL_ID,
            "XR Proxy VPN",
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = "VPN connection status"
            setShowBadge(false)
            setSound(null, null)
            enableVibration(false)
            lockscreenVisibility = Notification.VISIBILITY_PUBLIC
        }
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    private fun buildNotification(state: ServiceState): Notification {
        val contentIntent = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val stopIntent = PendingIntent.getService(
            this, 0,
            Intent(this, XrVpnService::class.java).apply { action = ACTION_STOP },
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )

        val text = when (state.phase) {
            Phase.Idle, Phase.Preparing -> "Запуск…"
            Phase.Connecting -> "Подключение…"
            Phase.Finalizing -> "Проверка маршрутов…"
            Phase.Connected -> state.snapshot?.let { s ->
                "↑${formatBytes(s.bytesUp)} ↓${formatBytes(s.bytesDown)} • ${formatUptime(s.uptime)}"
            } ?: "Подключено"
            Phase.Paused -> {
                val base = state.pausedSsid?.let { "На паузе · доверенная сеть «$it»" }
                    ?: "На паузе · доверенная сеть"
                if (state.restrictedNetwork) "$base · ⚠ в сети ограничения" else base
            }
            Phase.Stopping -> "Отключение…"
            Phase.Error -> state.errorMessage ?: "Ошибка"
        }

        val stopAction = Notification.Action.Builder(
            Icon.createWithResource(this, R.drawable.ic_notification_stop),
            "Отключить",
            stopIntent,
        ).build()

        val builder = Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("XR Proxy")
            .setContentText(text)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentIntent(contentIntent)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setCategory(Notification.CATEGORY_SERVICE)
            .setVisibility(Notification.VISIBILITY_PUBLIC)
            .setColor(ContextCompat.getColor(this, R.color.brand_primary))
            .setColorized(true)

        if (state.phase == Phase.Paused) {
            // Offer a one-tap override to keep proxying on this trusted network.
            val resumeIntent = PendingIntent.getService(
                this, 1,
                Intent(this, XrVpnService::class.java).apply { action = ACTION_RESUME_OVERRIDE },
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
            builder.addAction(
                Notification.Action.Builder(
                    Icon.createWithResource(this, R.drawable.ic_notification),
                    "Включить здесь",
                    resumeIntent,
                ).build()
            )
        }
        builder.addAction(stopAction)
        return builder.build()
    }

    private fun formatBytes(bytes: Long): String = when {
        bytes < 1024 -> "$bytes B"
        bytes < 1024 * 1024 -> "${bytes / 1024} KB"
        bytes < 1024L * 1024 * 1024 -> "${"%.1f".format(bytes / 1024.0 / 1024.0)} MB"
        else -> "${"%.2f".format(bytes / 1024.0 / 1024.0 / 1024.0)} GB"
    }

    private fun formatUptime(seconds: Long): String {
        val h = seconds / 3600
        val m = (seconds % 3600) / 60
        val s = seconds % 60
        return if (h > 0) "${h}h ${m}m" else "${m}m ${s}s"
    }
}
