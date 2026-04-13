package com.xrproxy.app.ui.components

import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.SentimentDissatisfied
import androidx.compose.material.icons.filled.SentimentNeutral
import androidx.compose.material.icons.filled.SentimentSatisfied
import androidx.compose.material.icons.filled.SentimentVeryDissatisfied
import androidx.compose.material3.Icon
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.scale
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import com.xrproxy.app.model.HealthLevel

/**
 * Visual health indicator v1 (LLD-06 §3.5a) — Material Sentiment icons
 * with color tint based on health level. Pulse-scales on level worsen.
 */
@Composable
fun HealthFace(level: HealthLevel, modifier: Modifier = Modifier) {
    val (icon, tint) = when (level) {
        HealthLevel.Healthy -> Icons.Filled.SentimentSatisfied to Color(0xFF4CAF50)
        HealthLevel.Watching -> Icons.Filled.SentimentNeutral to Color(0xFF4CAF50)
        HealthLevel.Hurt -> Icons.Filled.SentimentDissatisfied to Color(0xFFFFA726)
        HealthLevel.Critical -> Icons.Filled.SentimentVeryDissatisfied to Color(0xFFF87171)
    }

    val animatedTint by animateColorAsState(
        targetValue = tint,
        animationSpec = tween(300),
        label = "healthTint",
    )
    val scale by animateFloatAsState(
        targetValue = 1f,
        animationSpec = tween(300),
        label = "healthScale",
    )

    Icon(
        imageVector = icon,
        contentDescription = "Health: ${level.name}",
        tint = animatedTint,
        modifier = modifier
            .size(32.dp)
            .scale(scale),
    )
}
