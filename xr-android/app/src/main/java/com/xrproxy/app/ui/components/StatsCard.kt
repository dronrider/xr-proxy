package com.xrproxy.app.ui.components

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.xrproxy.app.ui.VpnUiState

/**
 * Statistics cards grid: 2×2 top (Upload/Download, Speed Up/Down) +
 * one wide bottom (Uptime + Connections). LLD-06 §3.7.
 */
@Composable
fun StatsGrid(state: VpnUiState, modifier: Modifier = Modifier) {
    Column(modifier = modifier.fillMaxWidth()) {
        // Row 1: Upload / Download
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            SmallStatCard(
                label = "Upload",
                value = formatBytes(state.bytesUp),
                modifier = Modifier.weight(1f),
            )
            SmallStatCard(
                label = "Download",
                value = formatBytes(state.bytesDown),
                modifier = Modifier.weight(1f),
            )
        }
        Spacer(Modifier.height(8.dp))
        // Row 2: Speed Up / Speed Down
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            SmallStatCard(
                label = "Speed up",
                value = formatSpeed(state.speedUp),
                modifier = Modifier.weight(1f),
            )
            SmallStatCard(
                label = "Speed down",
                value = formatSpeed(state.speedDown),
                modifier = Modifier.weight(1f),
            )
        }
        Spacer(Modifier.height(8.dp))
        // Row 3: Wide card — Uptime + Connections
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
        ) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(16.dp),
                horizontalArrangement = Arrangement.SpaceEvenly,
            ) {
                Column(horizontalAlignment = Alignment.CenterHorizontally) {
                    Text("Uptime", style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant)
                    Text(formatUptime(state.uptime), style = MaterialTheme.typography.headlineSmall,
                        fontSize = 20.sp, color = MaterialTheme.colorScheme.onBackground)
                }
                Column(horizontalAlignment = Alignment.CenterHorizontally) {
                    Text("Connections", style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant)
                    Text("${state.activeConnections}", style = MaterialTheme.typography.headlineSmall,
                        fontSize = 20.sp, color = MaterialTheme.colorScheme.onBackground)
                }
            }
        }
    }
}

@Composable
private fun SmallStatCard(
    label: String,
    value: String,
    modifier: Modifier = Modifier,
) {
    Card(
        modifier = modifier,
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
    ) {
        Column(modifier = Modifier.padding(16.dp)) {
            Text(
                text = label,
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(4.dp))
            Text(
                text = value,
                style = MaterialTheme.typography.headlineSmall,
                fontSize = 20.sp,
                color = MaterialTheme.colorScheme.onBackground,
            )
        }
    }
}

fun formatBytes(bytes: Long): String = when {
    bytes < 1024 -> "$bytes B"
    bytes < 1024 * 1024 -> "${bytes / 1024} KB"
    bytes < 1024L * 1024 * 1024 -> "${"%.1f".format(bytes / 1024.0 / 1024.0)} MB"
    else -> "${"%.2f".format(bytes / 1024.0 / 1024.0 / 1024.0)} GB"
}

fun formatSpeed(bytesPerSec: Long): String = when {
    bytesPerSec < 1024 -> "$bytesPerSec B/s"
    bytesPerSec < 1024 * 1024 -> "${bytesPerSec / 1024} KB/s"
    else -> "${"%.1f".format(bytesPerSec / 1024.0 / 1024.0)} MB/s"
}

fun formatUptime(seconds: Long): String {
    val h = seconds / 3600; val m = (seconds % 3600) / 60; val s = seconds % 60
    return if (h > 0) "${h}h ${m}m" else "${m}m ${s}s"
}
