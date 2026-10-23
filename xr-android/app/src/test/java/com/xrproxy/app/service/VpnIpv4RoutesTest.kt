package com.xrproxy.app.service

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class VpnIpv4RoutesTest {
    private val routes = VpnIpv4Routes.routes()

    private fun number(address: String): Long = address.split(".")
        .fold(0L) { total, octet -> (total shl 8) or octet.toLong() }

    private fun captured(address: String): Boolean {
        val value = number(address)
        return routes.any {
            val start = number(it.address)
            value >= start && value < start + (1L shl (32 - it.prefix))
        }
    }

    @Test fun homeRoutersAndLanDiscoveryBypassVpn() {
        listOf(
            "10.0.0.1", "10.0.0.2", "10.255.255.255",
            "172.16.0.1", "172.31.255.255", "192.168.1.1", "192.168.255.255",
            "100.64.0.1", "100.127.255.255", "169.254.10.20", "127.0.0.1",
            "224.0.0.251", "239.255.255.250", "255.255.255.255",
        ).forEach { assertFalse("LAN попал в TUN: $it", captured(it)) }
    }

    @Test fun publicAddressesAtExclusionBoundariesStayInVpn() {
        listOf(
            "1.1.1.1", "8.8.8.8", "9.255.255.255", "11.0.0.0",
            "100.63.255.255", "100.128.0.0", "126.255.255.255", "128.0.0.0",
            "169.253.255.255", "169.255.0.0", "172.15.255.255", "172.32.0.0",
            "192.167.255.255", "192.169.0.0", "223.255.255.255", "240.0.0.0",
        ).forEach { assertTrue("Адрес выпал из TUN: $it", captured(it)) }
    }

    @Test fun virtualEndpointsAndEntireFakeDnsPoolStayInVpnWithoutCollisions() {
        assertEquals("198.18.0.0", VpnIpv4Routes.TUN_ADDRESS)
        assertEquals("198.19.255.255", VpnIpv4Routes.DNS_ADDRESS)
        listOf(
            VpnIpv4Routes.TUN_ADDRESS, "198.18.0.1", "198.18.255.255",
            "198.19.0.0", "198.19.255.254", VpnIpv4Routes.DNS_ADDRESS,
        ).forEach { assertTrue("Служебный адрес выпал из TUN: $it", captured(it)) }
        val domainPool = number("198.18.0.1")..number("198.19.255.254")
        assertFalse(number(VpnIpv4Routes.TUN_ADDRESS) in domainPool)
        assertFalse(number(VpnIpv4Routes.DNS_ADDRESS) in domainPool)
    }

    @Test fun routesHaveNoOverlapsAndCoverExactlyTheNonLocalAddressSpace() {
        val sorted = routes.sortedBy { number(it.address) }
        sorted.zipWithNext().forEach { (left, right) ->
            assertTrue(number(left.address) + (1L shl (32 - left.prefix)) <= number(right.address))
        }
        val excluded = 3 * (1L shl 24) + (1L shl 22) + 2 * (1L shl 16) +
            (1L shl 20) + (1L shl 28) + 1
        assertEquals((1L shl 32) - excluded, routes.sumOf { 1L shl (32 - it.prefix) })
        assertTrue(routes.size < 128)
    }
}
