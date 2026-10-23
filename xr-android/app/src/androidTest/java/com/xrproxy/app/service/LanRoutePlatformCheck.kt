package com.xrproxy.app.service

import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.RouteInfo
import android.net.VpnService
import java.net.Inet4Address
import java.net.InetAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

internal object LanRoutePlatformCheck {
    fun run(service: XrVpnService) {
        check(VpnService.prepare(service) == null) {
            "VPN permission missing: grant ACTIVATE_VPN before the platform check"
        }
        val connectivity = service.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        val received = AtomicReference<LinkProperties>()
        val ready = CountDownLatch(1)
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
                if (properties.linkAddresses.any {
                        it.address.hostAddress == VpnIpv4Routes.TUN_ADDRESS && it.prefixLength == 32
                    }) {
                    received.set(properties)
                    ready.countDown()
                }
            }
        }
        val request = NetworkRequest.Builder()
            .clearCapabilities()
            .addTransportType(NetworkCapabilities.TRANSPORT_VPN)
            .build()
        connectivity.registerNetworkCallback(request, callback)
        try {
            val builder = service.Builder()
                .setSession("XR LAN platform regression")
                .addAddress(VpnIpv4Routes.TUN_ADDRESS, 32)
                .addDnsServer(VpnIpv4Routes.DNS_ADDRESS)
                .setMtu(1280)
            VpnIpv4Routes.routes().forEach { builder.addRoute(it.address, it.prefix) }
            checkNotNull(builder.establish()) { "Android rejected the LAN-safe VPN configuration" }.use {
                check(ready.await(10, TimeUnit.SECONDS)) { "VPN LinkProperties were not published" }
                val properties = checkNotNull(received.get())
                check(properties.dnsServers.map { it.hostAddress } == listOf(VpnIpv4Routes.DNS_ADDRESS)) {
                    "virtual DNS changed: ${properties.dnsServers}"
                }
                val forwarding = properties.routes.filter { it.type == RouteInfo.RTN_UNICAST }
                fun captured(address: String): Boolean = forwarding.any {
                    it.destination.contains(InetAddress.getByName(address))
                }
                listOf(
                    "10.0.0.1", "10.0.0.2", "192.168.1.1", "172.16.0.1",
                    "100.64.0.1", "169.254.10.20", "224.0.0.251", "255.255.255.255",
                ).forEach { address -> check(!captured(address)) { "Android routed LAN into TUN: $address" } }
                listOf(
                    "1.1.1.1", "8.8.8.8", "198.18.0.1", "198.19.255.254",
                    VpnIpv4Routes.DNS_ADDRESS,
                ).forEach { address -> check(captured(address)) { "Android lost a VPN route: $address" } }
                check(forwarding.all { it.destination.address is Inet4Address }) {
                    "IPv6 forwarding was enabled without an Android IPv6 tunnel"
                }
            }
        } finally {
            connectivity.unregisterNetworkCallback(callback)
        }
    }
}
