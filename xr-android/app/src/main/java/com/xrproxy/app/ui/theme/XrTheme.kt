package com.xrproxy.app.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

// XR Proxy brand palette (LLD-06 §3.1)
val Background      = Color(0xFF0B1220)
val Surface         = Color(0xFF121A2B)
val SurfaceVariant  = Color(0xFF1B2540)
val OnBackground    = Color(0xFFE6EDF7)
val OnSurfaceVariant = Color(0xFF94A3B8)
val Primary         = Color(0xFF22D3EE)
val OnPrimary       = Color(0xFF0B1220)
val Tertiary        = Color(0xFF7C8BFF)
val Error           = Color(0xFFF87171)
val OnError         = Color(0xFF0B1220)
val Outline         = Color(0xFF334155)

private val XrColorScheme = darkColorScheme(
    background = Background,
    surface = Surface,
    surfaceVariant = SurfaceVariant,
    onBackground = OnBackground,
    onSurface = OnBackground,
    onSurfaceVariant = OnSurfaceVariant,
    primary = Primary,
    onPrimary = OnPrimary,
    tertiary = Tertiary,
    onTertiary = OnPrimary,
    error = Error,
    onError = OnError,
    outline = Outline,
    surfaceContainerLow = Surface,
    surfaceContainer = Surface,
    surfaceContainerHigh = SurfaceVariant,
)

@Composable
fun XrTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = XrColorScheme,
        content = content,
    )
}
