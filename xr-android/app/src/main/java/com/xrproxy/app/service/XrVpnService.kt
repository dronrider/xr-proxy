package com.xrproxy.app.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.net.VpnService
import android.os.ParcelFileDescriptor
import com.xrproxy.app.jni.NativeBridge
import com.xrproxy.app.ui.MainActivity
import java.io.FileInputStream
import java.io.FileOutputStream

class XrVpnService : VpnService() {

    companion object {
        const val ACTION_START = "com.xrproxy.app.START"
        const val ACTION_STOP = "com.xrproxy.app.STOP"
        const val EXTRA_CONFIG_JSON = "config_json"
        private const val CHANNEL_ID = "xr_vpn_channel"
        private const val NOTIFICATION_ID = 1
    }

    private var vpnInterface: ParcelFileDescriptor? = null
    private var tunReadThread: Thread? = null
    private var tunWriteThread: Thread? = null
    @Volatile private var running = false

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START -> {
                val configJson = intent.getStringExtra(EXTRA_CONFIG_JSON) ?: return START_NOT_STICKY
                startVpn(configJson)
            }
            ACTION_STOP -> stopVpn()
        }
        return START_STICKY
    }

    private fun startVpn(configJson: String) {
        // Create notification channel and start foreground.
        createNotificationChannel()
        startForeground(NOTIFICATION_ID, buildNotification("Connecting..."))

        // Create TUN interface.
        val builder = Builder()
            .setSession("XR Proxy")
            .addAddress("10.0.0.2", 32)
            .addRoute("0.0.0.0", 0)
            .addDnsServer("10.0.0.1")
            .setMtu(1500)
            .setBlocking(true)

        // Protect our own server connection from being routed through TUN.
        // (The app socket must bypass the VPN.)

        vpnInterface = builder.establish() ?: run {
            stopSelf()
            return
        }

        val tunFd = vpnInterface!!.fd

        // Start native engine.
        val result = NativeBridge.nativeStart(tunFd, configJson)
        if (result != 0) {
            vpnInterface?.close()
            vpnInterface = null
            stopSelf()
            return
        }

        running = true

        // TUN read thread: read packets from TUN → push to engine.
        tunReadThread = Thread {
            val input = FileInputStream(vpnInterface!!.fileDescriptor)
            val buf = ByteArray(1500)
            try {
                while (running) {
                    val n = input.read(buf)
                    if (n > 0) {
                        NativeBridge.nativePushPacket(buf.copyOf(n))
                    }
                }
            } catch (_: Exception) {
                // TUN closed.
            }
        }.apply {
            name = "tun-read"
            isDaemon = true
            start()
        }

        // TUN write thread: pop packets from engine → write to TUN.
        tunWriteThread = Thread {
            val output = FileOutputStream(vpnInterface!!.fileDescriptor)
            try {
                while (running) {
                    val packet = NativeBridge.nativePopPacket()
                    if (packet != null) {
                        output.write(packet)
                    } else {
                        Thread.sleep(1) // Avoid busy-wait.
                    }
                }
            } catch (_: Exception) {
                // TUN closed.
            }
        }.apply {
            name = "tun-write"
            isDaemon = true
            start()
        }

        // Update notification.
        val nm = getSystemService(NotificationManager::class.java)
        nm.notify(NOTIFICATION_ID, buildNotification("Connected"))
    }

    private fun stopVpn() {
        running = false
        NativeBridge.nativeStop()

        tunReadThread?.interrupt()
        tunWriteThread?.interrupt()
        tunReadThread = null
        tunWriteThread = null

        vpnInterface?.close()
        vpnInterface = null

        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    override fun onDestroy() {
        stopVpn()
        super.onDestroy()
    }

    override fun onRevoke() {
        stopVpn()
        super.onRevoke()
    }

    private fun createNotificationChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID,
            "XR Proxy VPN",
            NotificationManager.IMPORTANCE_LOW
        ).apply {
            description = "VPN connection status"
        }
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    private fun buildNotification(status: String): Notification {
        val intent = Intent(this, MainActivity::class.java)
        val pendingIntent = PendingIntent.getActivity(
            this, 0, intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )

        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("XR Proxy")
            .setContentText(status)
            .setSmallIcon(android.R.drawable.ic_lock_lock)
            .setContentIntent(pendingIntent)
            .setOngoing(true)
            .build()
    }
}
