package com.xrproxy.app.ui.components

import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.unit.dp
import com.xrproxy.app.ui.ConnectPhase

/**
 * Shield-and-lightning icon drawn on Compose Canvas (LLD-06 §3.5).
 * Animates differently based on the connection phase.
 */
@Composable
fun ShieldArrowIcon(phase: ConnectPhase, modifier: Modifier = Modifier) {
    val primary = MaterialTheme.colorScheme.primary
    val bg = MaterialTheme.colorScheme.background

    val isAnimating = phase == ConnectPhase.Preparing ||
            phase == ConnectPhase.Connecting ||
            phase == ConnectPhase.Finalizing

    val isConnected = phase == ConnectPhase.Connected
    val isDimmed = phase == ConnectPhase.Idle || phase == ConnectPhase.NeedsPermission

    // Arrow pulse animation during connecting phases
    val transition = rememberInfiniteTransition(label = "shield")
    val arrowAlpha by transition.animateFloat(
        initialValue = if (isAnimating) 0.4f else 1f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(
            animation = tween(800, easing = FastOutSlowInEasing),
            repeatMode = RepeatMode.Reverse,
        ),
        label = "arrowAlpha",
    )

    // Glow pulse for Connected state
    val glowStroke by transition.animateFloat(
        initialValue = 0f,
        targetValue = if (isConnected) 3f else 0f,
        animationSpec = infiniteRepeatable(
            animation = tween(2000, easing = FastOutSlowInEasing),
            repeatMode = RepeatMode.Reverse,
        ),
        label = "glowStroke",
    )

    val compositeAlpha = if (isDimmed) 0.6f else 1f
    val effectiveArrowAlpha = if (isAnimating) arrowAlpha * compositeAlpha else compositeAlpha

    Canvas(modifier = modifier) {
        val s = size.minDimension

        // Draw shield
        drawShield(primary.copy(alpha = compositeAlpha), s)

        // Draw lightning bolt "hole" through shield
        drawLightningHole(bg.copy(alpha = effectiveArrowAlpha), s)

        // Draw lightning extensions outside shield
        drawLightningExtensions(primary.copy(alpha = effectiveArrowAlpha), s)

        // Glow outline on Connected
        if (isConnected && glowStroke > 0.1f) {
            drawShield(
                color = primary.copy(alpha = 0.4f * (glowStroke / 3f)),
                size = s,
                style = Stroke(width = glowStroke.dp.toPx()),
            )
        }
    }
}

private fun DrawScope.drawShield(color: Color, size: Float, style: Stroke? = null) {
    val path = Path().apply {
        // Shield shape matching the app icon
        val cx = size / 2f
        val top = size * 0.12f
        val bottom = size * 0.88f
        val left = size * 0.2f
        val right = size * 0.8f
        val midY = size * 0.5f

        moveTo(cx, top)
        // Top right corner
        lineTo(right - size * 0.02f, top)
        quadraticBezierTo(right, top, right, top + size * 0.04f)
        // Right side curve
        quadraticBezierTo(right + size * 0.02f, midY, cx + size * 0.15f, bottom - size * 0.12f)
        // Bottom point
        quadraticBezierTo(cx + size * 0.04f, bottom - size * 0.03f, cx, bottom)
        quadraticBezierTo(cx - size * 0.04f, bottom - size * 0.03f, cx - size * 0.15f, bottom - size * 0.12f)
        // Left side curve
        quadraticBezierTo(left - size * 0.02f, midY, left, top + size * 0.04f)
        quadraticBezierTo(left, top, left + size * 0.02f, top)
        close()
    }
    if (style != null) {
        drawPath(path, color, style = style)
    } else {
        drawPath(path, color)
    }
}

private fun DrawScope.drawLightningHole(color: Color, size: Float) {
    val path = Path().apply {
        moveTo(size * 0.58f, size * 0.18f)
        lineTo(size * 0.42f, size * 0.46f)
        lineTo(size * 0.54f, size * 0.46f)
        lineTo(size * 0.36f, size * 0.78f)
        lineTo(size * 0.46f, size * 0.54f)
        lineTo(size * 0.34f, size * 0.54f)
        lineTo(size * 0.50f, size * 0.18f)
        close()
    }
    drawPath(path, color)
}

private fun DrawScope.drawLightningExtensions(color: Color, size: Float) {
    // Top extension
    val topPath = Path().apply {
        moveTo(size * 0.64f, size * 0.08f)
        lineTo(size * 0.58f, size * 0.18f)
        lineTo(size * 0.50f, size * 0.18f)
        lineTo(size * 0.55f, size * 0.08f)
        close()
    }
    drawPath(topPath, color)

    // Bottom extension
    val bottomPath = Path().apply {
        moveTo(size * 0.36f, size * 0.78f)
        lineTo(size * 0.30f, size * 0.92f)
        lineTo(size * 0.40f, size * 0.92f)
        lineTo(size * 0.42f, size * 0.78f)
        close()
    }
    drawPath(bottomPath, color)
}
