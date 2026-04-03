package com.xrproxy.app.ui

import android.app.Activity
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

class MainActivity : ComponentActivity() {
    private val viewModel: VpnViewModel by viewModels()

    private val vpnPermissionLauncher = registerForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == Activity.RESULT_OK) viewModel.connect()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                MainScreen(viewModel = viewModel, onConnect = {
                    val intent = viewModel.prepareVpn()
                    if (intent != null) vpnPermissionLauncher.launch(intent) else viewModel.connect()
                })
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MainScreen(viewModel: VpnViewModel, onConnect: () -> Unit) {
    val state by viewModel.uiState.collectAsState()
    var currentTab by remember { mutableIntStateOf(0) } // 0=VPN, 1=Log, 2=Settings

    Scaffold(
        bottomBar = {
            NavigationBar {
                NavigationBarItem(
                    selected = currentTab == 0,
                    onClick = { currentTab = 0 },
                    icon = { Icon(Icons.Default.Lock, null) },
                    label = { Text("VPN") },
                )
                NavigationBarItem(
                    selected = currentTab == 1,
                    onClick = { currentTab = 1; viewModel.refreshLog() },
                    icon = {
                        BadgedBox(badge = {
                            if (state.relayErrors > 0) {
                                Badge { Text("${state.relayErrors}") }
                            }
                        }) {
                            Icon(Icons.Default.List, null)
                        }
                    },
                    label = { Text("Log") },
                )
                NavigationBarItem(
                    selected = currentTab == 2,
                    onClick = { currentTab = 2 },
                    icon = { Icon(Icons.Default.Settings, null) },
                    label = { Text("Settings") },
                )
            }
        }
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(horizontal = 16.dp)
                .verticalScroll(rememberScrollState()),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            when (currentTab) {
                0 -> ConnectionSection(state, onConnect, viewModel::disconnect)
                1 -> LogSection(state, viewModel)
                2 -> SettingsSection(state, viewModel)
            }
        }
    }
}

// ── VPN Connection tab ──────────────────────────────────────────────

@Composable
fun ConnectionSection(state: VpnUiState, onConnect: () -> Unit, onDisconnect: () -> Unit) {
    Spacer(Modifier.height(24.dp))

    val (statusColor, statusText) = when {
        state.connected -> MaterialTheme.colorScheme.primary to "Connected"
        state.connecting -> MaterialTheme.colorScheme.tertiary to "Connecting..."
        else -> MaterialTheme.colorScheme.outline to "Disconnected"
    }

    Box(contentAlignment = Alignment.Center) {
        if (state.connecting) {
            CircularProgressIndicator(modifier = Modifier.size(80.dp), color = statusColor, strokeWidth = 3.dp)
        }
        Icon(
            imageVector = if (state.connected) Icons.Default.Lock else Icons.Default.LockOpen,
            contentDescription = null, tint = statusColor,
            modifier = Modifier.size(if (state.connecting) 40.dp else 64.dp)
        )
    }

    Spacer(Modifier.height(4.dp))
    Text(statusText, style = MaterialTheme.typography.headlineSmall, color = statusColor)

    // Version.
    val context = androidx.compose.ui.platform.LocalContext.current
    val versionName = remember {
        try { context.packageManager.getPackageInfo(context.packageName, 0).versionName ?: "" }
        catch (_: Exception) { "" }
    }
    if (versionName.isNotBlank()) {
        Text("v$versionName", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.outline)
    }

    if (!state.connected && !state.connecting) {
        val presetLabel = when (state.routingPreset) {
            "russia" -> "Preset: Russia"
            "proxy_all" -> "Proxy all traffic"
            "custom" -> "Custom rules"
            else -> ""
        }
        Text(presetLabel, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }

    Spacer(Modifier.height(24.dp))

    Button(
        onClick = { if (state.connected || state.connecting) onDisconnect() else onConnect() },
        modifier = Modifier.fillMaxWidth(0.6f),
        colors = ButtonDefaults.buttonColors(
            containerColor = if (state.connected || state.connecting) MaterialTheme.colorScheme.error
            else MaterialTheme.colorScheme.primary
        ),
    ) {
        if (state.connecting) {
            CircularProgressIndicator(modifier = Modifier.size(18.dp), color = MaterialTheme.colorScheme.onError, strokeWidth = 2.dp)
            Spacer(Modifier.width(8.dp))
            Text("Cancel")
        } else {
            Text(if (state.connected) "Disconnect" else "Connect", style = MaterialTheme.typography.titleMedium)
        }
    }

    if (state.connected) {
        Spacer(Modifier.height(24.dp))
        Card(modifier = Modifier.fillMaxWidth()) {
            Column(modifier = Modifier.padding(16.dp)) {
                Text("Statistics", style = MaterialTheme.typography.titleSmall)
                Spacer(Modifier.height(8.dp))
                StatRow("Upload", formatBytes(state.bytesUp))
                StatRow("Download", formatBytes(state.bytesDown))
                StatRow("Connections", "${state.activeConnections}")
                StatRow("Uptime", formatUptime(state.uptime))
                Spacer(Modifier.height(8.dp))
                Text("Debug", style = MaterialTheme.typography.labelSmall)
                StatRow("DNS", "${state.dnsQueries}")
                StatRow("SYNs", "${state.tcpSyns}")
                StatRow("smol recv/send", "${formatBytes(state.smolRecv)} / ${formatBytes(state.smolSend)}")
                StatRow("Relay errors", "${state.relayErrors}")
                if (state.debugMsg.isNotBlank()) {
                    Spacer(Modifier.height(2.dp))
                    Text(state.debugMsg, style = MaterialTheme.typography.bodySmall, fontSize = 10.sp,
                        color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
        }
    }

    if (!state.connected && !state.connecting && state.serverAddress.isBlank()) {
        Spacer(Modifier.height(24.dp))
        Card(modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.errorContainer)) {
            Row(modifier = Modifier.padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Default.Warning, null, tint = MaterialTheme.colorScheme.error)
                Spacer(Modifier.width(12.dp))
                Text("Configure server in Settings tab", color = MaterialTheme.colorScheme.onErrorContainer)
            }
        }
    }
    Spacer(Modifier.height(16.dp))
}

// ── Log tab ─────────────────────────────────────────────────────────

@Composable
fun LogSection(state: VpnUiState, viewModel: VpnViewModel) {
    val clipboardManager = LocalClipboardManager.current

    Row(modifier = Modifier.fillMaxWidth().padding(vertical = 8.dp),
        horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
        Text("Error Log (${state.relayErrors})", style = MaterialTheme.typography.titleMedium)
        Row {
            IconButton(onClick = {
                clipboardManager.setText(AnnotatedString(state.errorLog))
            }) {
                Icon(Icons.Default.ContentCopy, "Copy log")
            }
            IconButton(onClick = { viewModel.clearLog() }) {
                Icon(Icons.Default.Delete, "Clear log")
            }
            IconButton(onClick = { viewModel.refreshLog() }) {
                Icon(Icons.Default.Refresh, "Refresh")
            }
        }
    }

    if (state.errorLog.isBlank()) {
        Spacer(Modifier.height(32.dp))
        Text("No errors", style = MaterialTheme.typography.bodyLarge,
            color = MaterialTheme.colorScheme.onSurfaceVariant)
    } else {
        Card(modifier = Modifier.fillMaxWidth()) {
            Text(
                text = state.errorLog,
                modifier = Modifier.padding(12.dp),
                style = MaterialTheme.typography.bodySmall,
                fontSize = 11.sp,
                lineHeight = 16.sp,
            )
        }
    }
    Spacer(Modifier.height(16.dp))
}

// ── Settings tab ────────────────────────────────────────────────────

@Composable
fun SettingsSection(state: VpnUiState, viewModel: VpnViewModel) {
    var showKey by remember { mutableStateOf(false) }
    val clipboardManager = LocalClipboardManager.current

    Spacer(Modifier.height(8.dp))
    Text("Server", style = MaterialTheme.typography.titleMedium)
    Spacer(Modifier.height(8.dp))

    OutlinedTextField(value = state.serverAddress, onValueChange = viewModel::updateServerAddress,
        label = { Text("Server address") }, placeholder = { Text("1.2.3.4") },
        modifier = Modifier.fillMaxWidth(), singleLine = true)
    Spacer(Modifier.height(8.dp))

    OutlinedTextField(value = state.serverPort, onValueChange = viewModel::updateServerPort,
        label = { Text("Port") }, modifier = Modifier.fillMaxWidth(), singleLine = true)
    Spacer(Modifier.height(16.dp))

    Text("Obfuscation", style = MaterialTheme.typography.titleMedium)
    Spacer(Modifier.height(8.dp))

    OutlinedTextField(value = state.obfuscationKey, onValueChange = viewModel::updateObfuscationKey,
        label = { Text("Key (base64)") }, modifier = Modifier.fillMaxWidth(), singleLine = true,
        visualTransformation = if (showKey) VisualTransformation.None else PasswordVisualTransformation(),
        trailingIcon = {
            IconButton(onClick = { showKey = !showKey }) {
                Icon(if (showKey) Icons.Default.VisibilityOff else Icons.Default.Visibility, "Toggle key visibility")
            }
        })
    Spacer(Modifier.height(8.dp))

    OutlinedTextField(value = state.salt, onValueChange = viewModel::updateSalt,
        label = { Text("Salt") }, modifier = Modifier.fillMaxWidth(), singleLine = true)
    Spacer(Modifier.height(16.dp))

    Text("Routing", style = MaterialTheme.typography.titleMedium)
    Spacer(Modifier.height(8.dp))

    Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        FilterChip(selected = state.routingPreset == "russia", onClick = { viewModel.updateRoutingPreset("russia") },
            label = { Text("Russia") },
            leadingIcon = if (state.routingPreset == "russia") {{ Icon(Icons.Default.Check, null, Modifier.size(16.dp)) }} else null)
        FilterChip(selected = state.routingPreset == "proxy_all", onClick = { viewModel.updateRoutingPreset("proxy_all") },
            label = { Text("Proxy all") },
            leadingIcon = if (state.routingPreset == "proxy_all") {{ Icon(Icons.Default.Check, null, Modifier.size(16.dp)) }} else null)
        FilterChip(selected = state.routingPreset == "custom", onClick = { viewModel.updateRoutingPreset("custom") },
            label = { Text("Custom") },
            leadingIcon = if (state.routingPreset == "custom") {{ Icon(Icons.Default.Check, null, Modifier.size(16.dp)) }} else null)
    }

    if (state.routingPreset == "custom") {
        Spacer(Modifier.height(8.dp))
        OutlinedButton(onClick = {
            val text = clipboardManager.getText()?.text ?: ""
            if (text.isNotBlank()) viewModel.importToml(text)
        }, modifier = Modifier.fillMaxWidth()) {
            Icon(Icons.Default.ContentPaste, null, modifier = Modifier.size(18.dp))
            Spacer(Modifier.width(8.dp))
            Text("Import TOML from clipboard")
        }
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(value = state.customDomains, onValueChange = viewModel::updateCustomDomains,
            label = { Text("Domains to proxy") }, placeholder = { Text("youtube.com\n*.google.com") },
            modifier = Modifier.fillMaxWidth().height(120.dp), maxLines = 8)
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(value = state.customIpRanges, onValueChange = viewModel::updateCustomIpRanges,
            label = { Text("IP ranges to proxy") }, placeholder = { Text("91.108.56.0/22") },
            modifier = Modifier.fillMaxWidth().height(100.dp), maxLines = 6)
    }

    if (state.routingPreset == "russia") {
        Spacer(Modifier.height(4.dp))
        Text("YouTube, Meta, Twitter/X, Telegram, Discord, Google, LinkedIn, AI, Dev tools, etc.",
            style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }

    Spacer(Modifier.height(24.dp))
    Button(onClick = { viewModel.saveSettings() }, modifier = Modifier.fillMaxWidth()) {
        Icon(Icons.Default.Check, null, modifier = Modifier.size(18.dp))
        Spacer(Modifier.width(8.dp))
        Text("Save")
    }
    if (state.settingsSaved) {
        Spacer(Modifier.height(4.dp))
        Text("Settings saved", color = MaterialTheme.colorScheme.primary, style = MaterialTheme.typography.bodySmall)
    }
    Spacer(Modifier.height(16.dp))
}

// ── Helpers ──────────────────────────────────────────────────────────

@Composable
fun StatRow(label: String, value: String) {
    Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
        Text(label, color = MaterialTheme.colorScheme.onSurfaceVariant, style = MaterialTheme.typography.bodySmall)
        Text(value, style = MaterialTheme.typography.bodySmall)
    }
}

fun formatBytes(bytes: Long): String = when {
    bytes < 1024 -> "$bytes B"
    bytes < 1024 * 1024 -> "${bytes / 1024} KB"
    bytes < 1024L * 1024 * 1024 -> "${"%.1f".format(bytes / 1024.0 / 1024.0)} MB"
    else -> "${"%.2f".format(bytes / 1024.0 / 1024.0 / 1024.0)} GB"
}

fun formatUptime(seconds: Long): String {
    val h = seconds / 3600; val m = (seconds % 3600) / 60; val s = seconds % 60
    return if (h > 0) "${h}h ${m}m" else "${m}m ${s}s"
}
