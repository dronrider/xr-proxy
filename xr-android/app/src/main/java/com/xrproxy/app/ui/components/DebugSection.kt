package com.xrproxy.app.ui.components

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.expandVertically
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Divider
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.xrproxy.app.ui.VpnUiState
import kotlinx.coroutines.launch
import org.json.JSONObject

/**
 * Collapsible debug section with grouped metrics (LLD-06 §3.7).
 */
@Composable
fun DebugSection(
    state: VpnUiState,
    expanded: Boolean,
    onToggle: () -> Unit,
    snackbarHostState: SnackbarHostState,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()

    Column(modifier = modifier.fillMaxWidth()) {
        // Accordion header
        Card(
            modifier = Modifier
                .fillMaxWidth()
                .clickable { onToggle() },
            colors = CardDefaults.cardColors(
                containerColor = MaterialTheme.colorScheme.surfaceVariant,
            ),
        ) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 16.dp, vertical = 12.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(
                        Icons.Default.Settings,
                        contentDescription = null,
                        modifier = Modifier.size(20.dp),
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.padding(start = 8.dp))
                    Text("Debug", style = MaterialTheme.typography.titleSmall)
                }
                Icon(
                    if (expanded) Icons.Default.ExpandLess else Icons.Default.ExpandMore,
                    contentDescription = if (expanded) "Collapse" else "Expand",
                    modifier = Modifier.size(20.dp),
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }

        // Expandable content
        AnimatedVisibility(
            visible = expanded,
            enter = expandVertically(),
            exit = shrinkVertically(),
        ) {
            Card(
                modifier = Modifier.fillMaxWidth(),
                colors = CardDefaults.cardColors(
                    containerColor = MaterialTheme.colorScheme.surface,
                ),
            ) {
                Column(modifier = Modifier.padding(16.dp)) {
                    DebugGroup("Network") {
                        DebugRow("DNS queries", "${state.dnsQueries}")
                        DebugRow("TCP SYNs", "${state.tcpSyns}")
                    }

                    Spacer(Modifier.height(12.dp))

                    DebugGroup("smoltcp") {
                        DebugRow("Recv", formatBytes(state.smolRecv))
                        DebugRow("Send", formatBytes(state.smolSend))
                    }

                    Spacer(Modifier.height(12.dp))

                    DebugGroup("Relay") {
                        DebugRow("Warnings", "${state.relayWarnings}")
                        DebugRow("Errors", "${state.relayErrors}")
                        if (state.debugMsg.isNotBlank()) {
                            Spacer(Modifier.height(4.dp))
                            Text(
                                state.debugMsg,
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }

                    Spacer(Modifier.height(16.dp))

                    // Copy all button
                    Button(
                        onClick = {
                            val json = buildDebugJson(state)
                            val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                            cm.setPrimaryClip(ClipData.newPlainText("XR Proxy Debug", json))
                            scope.launch { snackbarHostState.showSnackbar("Скопировано") }
                        },
                        modifier = Modifier.align(Alignment.CenterHorizontally),
                        colors = ButtonDefaults.buttonColors(
                            containerColor = MaterialTheme.colorScheme.surfaceVariant,
                            contentColor = MaterialTheme.colorScheme.onSurface,
                        ),
                    ) {
                        Icon(Icons.Default.ContentCopy, null, modifier = Modifier.size(16.dp))
                        Spacer(Modifier.padding(start = 8.dp))
                        Text("Copy all (JSON)")
                    }
                }
            }
        }
    }
}

@Composable
private fun DebugGroup(title: String, content: @Composable () -> Unit) {
    Text(
        title,
        style = MaterialTheme.typography.labelMedium,
        color = MaterialTheme.colorScheme.primary,
    )
    HorizontalDivider(
        modifier = Modifier.padding(vertical = 4.dp),
        color = MaterialTheme.colorScheme.outline,
    )
    content()
}

@Composable
private fun DebugRow(label: String, value: String) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Text(label, style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant)
        Text(value, style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onBackground)
    }
}

private fun buildDebugJson(state: VpnUiState): String {
    return JSONObject().apply {
        put("bytes_up", state.bytesUp)
        put("bytes_down", state.bytesDown)
        put("speed_up", state.speedUp)
        put("speed_down", state.speedDown)
        put("active_connections", state.activeConnections)
        put("uptime", state.uptime)
        put("dns_queries", state.dnsQueries)
        put("tcp_syns", state.tcpSyns)
        put("smol_recv", state.smolRecv)
        put("smol_send", state.smolSend)
        put("relay_warnings", state.relayWarnings)
        put("relay_errors", state.relayErrors)
        put("debug_msg", state.debugMsg)
        put("health", state.health.name)
    }.toString(2)
}
