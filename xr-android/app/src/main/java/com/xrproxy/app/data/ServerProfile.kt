package com.xrproxy.app.data

enum class ServerSource { Manual, Invite }

/**
 * Один адрес в пуле профиля (LLD-10). Приоритет задаётся порядком в списке
 * `endpoints`: первый это primary, остальные резервы. Ключ/salt/modifier
 * общие на профиль, per-endpoint ключей в приложении нет by design.
 */
data class ProfileEndpoint(
    val name: String = "",
    val address: String,
    val port: Int = 8443,
) {
    val displayName: String
        get() = name.ifBlank { address }
}

data class ServerProfile(
    val id: String,
    val name: String,
    /** Legacy-поля с primary-сервером. Пишутся зеркально с `endpoints[0]`,
     *  чтобы откат на старую версию приложения не терял адрес (LLD-10 §5.7). */
    val serverAddress: String,
    val serverPort: Int = 8443,
    val obfuscationKey: String = "",
    val modifier: String = "positional_xor_rotate",
    val salt: Long = 0xDEADBEEFL,
    val routingPreset: String = "russia",
    val customDomains: String = "",
    val customIpRanges: String = "",
    val hubUrl: String = "",
    val hubPreset: String = "",
    val trustedPublicKey: String = "",
    /** Invite this server was onboarded with (XR-031): the durable access anchor
     *  the Files screen presents to list the shares attached to it. Empty for
     *  manual servers. Будущее: заменится на JWT (XR-030). */
    val inviteToken: String = "",
    /** Пул адресов профиля (LLD-10): failover движок делает сам, без
     *  пересоздания VPN-сервиса. Пустой список значит легаси-профиль,
     *  реальный пул тогда даёт [effectiveEndpoints] из legacy-полей. */
    val endpoints: List<ProfileEndpoint> = emptyList(),
    val createdAt: String,
    val source: ServerSource,
) {
    /** Итоговый пул: `endpoints`, либо одиночный legacy-адрес. */
    val effectiveEndpoints: List<ProfileEndpoint>
        get() = endpoints.ifEmpty {
            if (serverAddress.isBlank()) emptyList()
            else listOf(ProfileEndpoint(address = serverAddress, port = serverPort))
        }

    val displaySubtitle: String
        get() {
            val eps = effectiveEndpoints
            return when {
                eps.isEmpty() -> "$serverAddress:$serverPort"
                eps.size == 1 -> "${eps[0].address}:${eps[0].port}"
                else -> "${eps[0].address}:${eps[0].port} (+${eps.size - 1} резерв)"
            }
        }

    val presetLabel: String
        get() = when (routingPreset) {
            "russia" -> "Russia"
            "proxy_all" -> "Proxy all"
            "custom" -> "Custom"
            else -> routingPreset
        }
}
