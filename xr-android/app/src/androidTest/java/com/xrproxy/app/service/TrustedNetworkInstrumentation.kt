package com.xrproxy.app.service

import android.app.Activity
import android.app.Instrumentation
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.ServiceInfo
import android.os.Bundle
import android.os.IBinder
import com.xrproxy.app.ui.MainActivity
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.flow.MutableStateFlow

/** Проверяет настоящий foreground-сервис на устройстве без доступа к VPS. */
class TrustedNetworkInstrumentation : Instrumentation() {
    private var checkLan = false

    override fun onCreate(arguments: Bundle?) {
        super.onCreate(arguments)
        checkLan = arguments?.getString("lan") == "true"
        start()
    }

    override fun onStart() {
        val result = Bundle()
        var activity: Activity? = null
        var service: XrVpnService? = null
        var connection: ServiceConnection? = null
        var resultCode = Activity.RESULT_CANCELED
        try {
            shell("pm grant ${targetContext.packageName} android.permission.ACCESS_COARSE_LOCATION")
            shell("pm grant ${targetContext.packageName} android.permission.ACCESS_FINE_LOCATION")
            activity = startActivitySync(Intent(targetContext, MainActivity::class.java).apply {
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            })
            targetContext.startService(Intent(targetContext, XrVpnService::class.java).setAction("instrumentation"))
            val connected = CountDownLatch(1)
            connection = object : ServiceConnection {
                override fun onServiceConnected(name: ComponentName, binder: IBinder) {
                    service = (binder as XrVpnService.LocalBinder).service()
                    connected.countDown()
                }
                override fun onServiceDisconnected(name: ComponentName) {}
            }
            check(targetContext.bindService(
                Intent(targetContext, XrVpnService::class.java).setAction(XrVpnService.ACTION_BIND_INTERNAL),
                connection,
                Context.BIND_AUTO_CREATE,
            ))
            check(connected.await(10, TimeUnit.SECONDS)) { "service bind timed out" }
            val svc = checkNotNull(service)
            runOnMainSync {
                // Восстановленная сессия уже держит specialUse. В тесте нет
                // TUN и движка: меняем лишь фазу, которая разрешает повышение.
                @Suppress("UNCHECKED_CAST")
                val state = XrVpnService::class.java.getDeclaredField("_stateFlow").apply {
                    isAccessible = true
                }.get(svc) as MutableStateFlow<XrVpnService.ServiceState>
                state.value = XrVpnService.ServiceState(phase = XrVpnService.Phase.Connected)
                XrVpnService::class.java.getDeclaredField("sessionFromUi").apply {
                    isAccessible = true
                }.setBoolean(svc, false)
                XrVpnService::class.java.getDeclaredMethod("startForegroundWithLocationType").apply {
                    isAccessible = true
                }.invoke(svc)
                check(svc.foregroundServiceType and ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION == 0) {
                    "background restore unexpectedly started location"
                }
                // На базе вызываем прежнюю перепроверку при открытии UI.
                // Так красный прогон проверяет поведение, а не наличие метода.
                val foreground = XrVpnService::class.java.methods.firstOrNull { it.name == "onAppForeground" }
                    ?: XrVpnService::class.java.getMethod("reevaluateTrustedNetwork")
                foreground.invoke(svc)
                check(svc.foregroundServiceType and ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION != 0) {
                    "opening UI left restored service without background SSID access"
                }
            }
            if (checkLan) {
                shell("appops set ${targetContext.packageName} ACTIVATE_VPN allow")
                // Отдельный режим позволяет гонять регрессию доверия на базе,
                // где нового набора маршрутов и его проверки ещё нет.
                val platformCheck = Class.forName("com.xrproxy.app.service.LanRoutePlatformCheck")
                platformCheck.getMethod("run", XrVpnService::class.java)
                    .invoke(platformCheck.getField("INSTANCE").get(null), svc)
            }
            result.putString("stream", "trusted network foreground promotion: ok\n" +
                if (checkLan) "Android LAN routes: ok\n" else "")
            result.putInt("passed", if (checkLan) 2 else 1)
            resultCode = Activity.RESULT_OK
        } catch (failure: Throwable) {
            result.putString("stream", "FAIL: ${failure.stackTraceToString()}\n")
        } finally {
            runOnMainSync {
                service?.stopForeground(android.app.Service.STOP_FOREGROUND_REMOVE)
                connection?.let { targetContext.unbindService(it) }
                targetContext.stopService(Intent(targetContext, XrVpnService::class.java))
                activity?.finish()
            }
        }
        finish(resultCode, result)
    }

    private fun shell(command: String) {
        uiAutomation.executeShellCommand(command).use { descriptor ->
            android.os.ParcelFileDescriptor.AutoCloseInputStream(descriptor).use { it.readBytes() }
        }
    }
}
