package com.xrproxy.app.data

enum class ServerSource { Manual, Invite }

data class ServerProfile(
    val id: String,
    val name: String,
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
    val createdAt: String,
    val source: ServerSource,
) {
    val displaySubtitle: String
        get() = "$serverAddress:$serverPort"

    val presetLabel: String
        get() = when (routingPreset) {
            "russia" -> "Russia"
            "proxy_all" -> "Proxy all"
            "custom" -> "Custom"
            else -> routingPreset
        }
}
