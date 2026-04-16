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
