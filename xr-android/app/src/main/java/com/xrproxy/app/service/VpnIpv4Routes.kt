package com.xrproxy.app.service

internal object VpnIpv4Routes {
    // Границы fake DNS /15 не выдаются доменам. Адреса вне RFC1918 позволяют
    // обращаться и к домашним сетям 10/8, включая роутер 10.0.0.1.
    const val TUN_ADDRESS = "198.18.0.0"
    const val DNS_ADDRESS = "198.19.255.255"

    data class Route(val address: String, val prefix: Int)

    private data class Block(val first: Long, val prefix: Int) {
        val size: Long get() = 1L shl (32 - prefix)
        val last: Long get() = first + size - 1
    }

    private val local = listOf(
        Block(0x00000000L, 8),
        Block(0x0a000000L, 8),
        Block(0x64400000L, 10),
        Block(0x7f000000L, 8),
        Block(0xa9fe0000L, 16),
        Block(0xac100000L, 12),
        Block(0xc0a80000L, 16),
        Block(0xe0000000L, 4),
        Block(0xffffffffL, 32),
    )

    // excludeRoute появился лишь в API 33. Положительные маршруты работают
    // и на minSdk 29; ни allowBypass, ни allowFamily здесь не нужны.
    fun routes(): List<Route> = buildList {
        fun include(block: Block) {
            if (local.any { block.first >= it.first && block.last <= it.last }) return
            if (local.none { block.first <= it.last && block.last >= it.first }) {
                val address = (3 downTo 0).joinToString(".") {
                    ((block.first shr (8 * it)) and 255).toString()
                }
                add(Route(address, block.prefix))
                return
            }
            val prefix = block.prefix + 1
            include(Block(block.first, prefix))
            include(Block(block.first + (1L shl (32 - prefix)), prefix))
        }
        include(Block(0, 0))
    }
}
