package com.xrproxy.app.ui.components

import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import com.xrproxy.app.R
import com.xrproxy.app.ui.ConnectPhase

/**
 * Main screen shield icon — renders the provided brand art as a bitmap
 * with phase-dependent animations overlaid (LLD-06 §3.5).
 */
@Composable
fun ShieldArrowIcon(phase: ConnectPhase, modifier: Modifier = Modifier) {
    val isAnimating = phase == ConnectPhase.Preparing ||
            phase == ConnectPhase.Connecting ||
            phase == ConnectPhase.Finalizing

    val isConnected = phase == ConnectPhase.Connected
    val isDimmed = phase == ConnectPhase.Idle || phase == ConnectPhase.NeedsPermission

    val transition = rememberInfiniteTransition(label = "shield")

    // Pulse animation during connecting phases
    val pulseAlpha by transition.animateFloat(
        initialValue = if (isAnimating) 0.5f else 1f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(
            animation = tween(800, easing = FastOutSlowInEasing),
            repeatMode = RepeatMode.Reverse,
        ),
        label = "pulseAlpha",
    )

    // Glow ring for Connected state
    val glowAlpha by transition.animateFloat(
        initialValue = 0f,
        targetValue = if (isConnected) 0.5f else 0f,
        animationSpec = infiniteRepeatable(
            animation = tween(2000, easing = FastOutSlowInEasing),
            repeatMode = RepeatMode.Reverse,
        ),
        label = "glowAlpha",
    )

    val compositeAlpha = when {
        isDimmed -> 0.6f
        isAnimating -> pulseAlpha
        else -> 1f
    }

    val glowColor = Color(0xFF22D3EE) // primary cyan

    Box(
        contentAlignment = Alignment.Center,
        modifier = modifier
            .drawBehind {
                // Glow ring behind icon when Connected
                if (isConnected && glowAlpha > 0.01f) {
                    val radius = size.minDimension / 2f + 4.dp.toPx()
                    drawCircle(
                        color = glowColor.copy(alpha = glowAlpha),
                        radius = radius,
                        center = Offset(size.width / 2f, size.height / 2f),
                        style = Stroke(width = 3.dp.toPx()),
                    )
                }
            },
    ) {
        Image(
            painter = painterResource(R.drawable.xr_shield_icon),
            contentDescription = "XR Proxy",
            modifier = Modifier
                .matchParentSize()
                .alpha(compositeAlpha),
        )
    }
}
