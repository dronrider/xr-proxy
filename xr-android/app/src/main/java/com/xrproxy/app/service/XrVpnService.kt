package com.xrproxy.app.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.graphics.drawable.Icon
import android.net.VpnService
import android.os.Binder
import android.os.IBinder
import android.os.ParcelFileDescriptor
import androidx.core.content.ContextCompat
import com.xrproxy.app.R
import com.xrproxy.app.jni.NativeBridge
import com.xrproxy.app.model.HealthLevel
import com.xrproxy.app.model.HealthTracker
import com.xrproxy.app.ui.MainActivity
import java.io.FileInputStream
import java.io.FileOutputStream
import org.json.JSONObject
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

/**
 * Foreground VPN service. Single source of truth for connection state and
 * live statistics — `VpnViewModel` binds to this service and mirrors its
 * `stateFlow` into UI state. Connection start/stop and the stats poll loop
 * live here so the UI can recover state via `bindService` after Activity
 * recreation or returning from background.
 */
class XrVpnService : VpnService() {

    companion object {
        const val ACTION_START = "com.xrproxy.app.START"
        const val ACTION_STOP = "com.xrproxy.app.STOP"
        const val ACTION_BIND_INTERNAL = "com.xrproxy.app.BIND_INTERNAL"
        const val EXTRA_CONFIG_JSON = "config_json"
        private const val CHANNEL_ID = "xr_vpn_channel"
        private const val NOTIFICATION_ID = 1
    }

    enum class Phase { Idle, Preparing, Connecting, Finalizing, Connected, Stopping, Error }

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
    )

    data class ServiceState(
        val phase: Phase = Phase.Idle,
        val errorMessage: String? = null,
        val snapshot: StatsSnapshot? = null,
        val speedUp: Long = 0,
        val speedDown: Long = 0,
        val health: HealthLevel = HealthLevel.Healthy,
    )

    inner class LocalBinder : Binder() {
        fun service(): XrVpnService = this@XrVpnService
    }

    private val localBinder = LocalBinder()

    private val _stateFlow = MutableStateFlow(ServiceState())
    val stateFlow: StateFlow<ServiceState> = _stateFlow

    private val scope = CoroutineScope(Dispatchers.Default + SupervisorJob())

    private var vpnInterface: ParcelFileDescriptor? = null
    private var tunReadThread: Thread? = null
    private var tunWriteThread: Thread? = null
    @Volatile private var running = false

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
    }

    override fun onDestroy() {
        // Conditional null: если уже создан новый instance (Disconnect→Connect
        // race), не трогаем — иначе onDestroy старого обнулит ссылку нового,
        // и protectSocket() вернёт false → все сокеты полезут через TUN
        // петлёй (VPN→TUN→VPN→…) → connect timeout на всё.
        if (NativeBridge.current === this) {
            NativeBridge.current = null
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

    fun clearLog() {
        NativeBridge.nativeClearErrorLog()
        _stateFlow.value = _stateFlow.value.copy(snapshot = readSnapshot())
    }

    // ── Connection flow ───────────────────────────────────────────────

    private suspend fun startVpn(configJson: String) {
        // Reset speed and health tracking for new session
        prevBytesUp = 0; prevBytesDown = 0; prevTickMs = 0
        healthTracker.reset()

        publish(Phase.Preparing)
        startForeground(NOTIFICATION_ID, buildNotification(_stateFlow.value))

        val iface = Builder()
            .setSession("XR Proxy")
            .addAddress("10.0.0.2", 32)
            .addRoute("0.0.0.0", 0)
            .addDnsServer("10.0.0.1")
            .setMtu(1500)
            .setBlocking(true)
            .establish()

        if (iface == null) {
            publish(Phase.Error, errorMessage = "TUN establish failed")
            updateNotification()
            stopSelf()
            return
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
            stopSelf()
            return
        }

        running = true

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

        scope.launch { pollLoop() }
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

            val phase = when (native) {
                "Connected" -> Phase.Connected
                "Connecting" -> Phase.Connecting
                "Disconnecting" -> Phase.Stopping
                else -> _stateFlow.value.phase
            }

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

    private fun stopInternal() {
        running = false
        NativeBridge.nativeStop()
        tunReadThread?.interrupt()
        tunWriteThread?.interrupt()
        tunReadThread = null
        tunWriteThread = null
        vpnInterface?.close()
        vpnInterface = null
    }

    // ── State publishing helpers ──────────────────────────────────────

    private fun publish(
        phase: Phase,
        snapshot: StatsSnapshot? = _stateFlow.value.snapshot,
        errorMessage: String? = null,
    ) {
        _stateFlow.value = _stateFlow.value.copy(
            phase = phase,
            snapshot = snapshot,
            errorMessage = errorMessage,
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
        )
    }

    // ── Notification ──────────────────────────────────────────────────

    private fun updateNotification() {
        val nm = getSystemService(NotificationManager::class.java)
        nm.notify(NOTIFICATION_ID, buildNotification(_stateFlow.value))
    }

    private fun createNotificationChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID,
            "XR Proxy VPN",
            NotificationManager.IMPORTANCE_DEFAULT,
        ).apply {
            description = "VPN connection status"
            setShowBadge(false)
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
            Phase.Stopping -> "Отключение…"
            Phase.Error -> state.errorMessage ?: "Ошибка"
        }

        val stopAction = Notification.Action.Builder(
            Icon.createWithResource(this, R.drawable.ic_notification_stop),
            "Отключить",
            stopIntent,
        ).build()

        return Notification.Builder(this, CHANNEL_ID)
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
            .addAction(stopAction)
            .build()
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
