package com.xrproxy.app.ui

import android.app.Activity
import android.content.Intent
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
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp

class MainActivity : ComponentActivity() {

    private val viewModel: VpnViewModel by viewModels()

    private val vpnPermissionLauncher = registerForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == Activity.RESULT_OK) {
            viewModel.connect()
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                MainScreen(
                    viewModel = viewModel,
                    onConnect = {
                        val intent = viewModel.prepareVpn()
                        if (intent != null) {
                            vpnPermissionLauncher.launch(intent)
                        } else {
                            viewModel.connect()
                        }
                    }
                )
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MainScreen(viewModel: VpnViewModel, onConnect: () -> Unit) {
    val state by viewModel.uiState.collectAsState()
    var showSettings by remember { mutableStateOf(false) }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("XR Proxy") },
                actions = {
                    IconButton(onClick = { showSettings = !showSettings }) {
                        Icon(
                            if (showSettings) Icons.Default.Close else Icons.Default.Settings,
                            contentDescription = "Settings"
                        )
                    }
                }
            )
        }
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(16.dp)
                .verticalScroll(rememberScrollState()),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            if (showSettings) {
                SettingsSection(state, viewModel)
            } else {
                ConnectionSection(state, onConnect, viewModel::disconnect)
            }
        }
    }
}

@Composable
fun ConnectionSection(
    state: VpnUiState,
    onConnect: () -> Unit,
    onDisconnect: () -> Unit
) {
    Spacer(Modifier.height(32.dp))

    // Status indicator.
    val (statusColor, statusText) = when {
        state.connected -> MaterialTheme.colorScheme.primary to "Connected"
        state.connecting -> MaterialTheme.colorScheme.tertiary to "Connecting..."
        else -> MaterialTheme.colorScheme.outline to "Disconnected"
    }

    Icon(
        imageVector = if (state.connected) Icons.Default.Lock else Icons.Default.LockOpen,
        contentDescription = null,
        tint = statusColor,
        modifier = Modifier.size(64.dp)
    )

    Spacer(Modifier.height(8.dp))
    Text(statusText, style = MaterialTheme.typography.headlineSmall, color = statusColor)

    Spacer(Modifier.height(32.dp))

    // Connect / Disconnect button.
    Button(
        onClick = { if (state.connected || state.connecting) onDisconnect() else onConnect() },
        modifier = Modifier.fillMaxWidth(0.6f),
        colors = ButtonDefaults.buttonColors(
            containerColor = if (state.connected) MaterialTheme.colorScheme.error
            else MaterialTheme.colorScheme.primary
        ),
        enabled = !state.connecting || state.connected,
    ) {
        Text(
            if (state.connected) "Disconnect" else if (state.connecting) "Connecting..." else "Connect",
            style = MaterialTheme.typography.titleMedium,
        )
    }

    // Stats.
    if (state.connected) {
        Spacer(Modifier.height(32.dp))
        Card(modifier = Modifier.fillMaxWidth()) {
            Column(modifier = Modifier.padding(16.dp)) {
                Text("Statistics", style = MaterialTheme.typography.titleSmall)
                Spacer(Modifier.height(8.dp))
                StatRow("Upload", formatBytes(state.bytesUp))
                StatRow("Download", formatBytes(state.bytesDown))
                StatRow("Connections", "${state.activeConnections}")
                StatRow("Uptime", formatUptime(state.uptime))
            }
        }
    }

    // Warning if not configured.
    if (!state.connected && state.serverAddress.isBlank()) {
        Spacer(Modifier.height(32.dp))
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.errorContainer)
        ) {
            Text(
                "Configure server address and key in Settings",
                modifier = Modifier.padding(16.dp),
                color = MaterialTheme.colorScheme.onErrorContainer,
            )
        }
    }
}

@Composable
fun SettingsSection(state: VpnUiState, viewModel: VpnViewModel) {
    var showKey by remember { mutableStateOf(false) }

    Text("Server", style = MaterialTheme.typography.titleMedium)
    Spacer(Modifier.height(8.dp))

    OutlinedTextField(
        value = state.serverAddress,
        onValueChange = viewModel::updateServerAddress,
        label = { Text("Server address") },
        placeholder = { Text("1.2.3.4") },
        modifier = Modifier.fillMaxWidth(),
        singleLine = true,
    )
    Spacer(Modifier.height(8.dp))

    OutlinedTextField(
        value = state.serverPort,
        onValueChange = viewModel::updateServerPort,
        label = { Text("Port") },
        modifier = Modifier.fillMaxWidth(),
        singleLine = true,
    )
    Spacer(Modifier.height(16.dp))

    Text("Obfuscation", style = MaterialTheme.typography.titleMedium)
    Spacer(Modifier.height(8.dp))

    OutlinedTextField(
        value = state.obfuscationKey,
        onValueChange = viewModel::updateObfuscationKey,
        label = { Text("Key (base64)") },
        modifier = Modifier.fillMaxWidth(),
        singleLine = true,
        visualTransformation = if (showKey) VisualTransformation.None else PasswordVisualTransformation(),
        trailingIcon = {
            IconButton(onClick = { showKey = !showKey }) {
                Icon(
                    if (showKey) Icons.Default.VisibilityOff else Icons.Default.Visibility,
                    contentDescription = "Toggle key visibility"
                )
            }
        }
    )
    Spacer(Modifier.height(8.dp))

    OutlinedTextField(
        value = state.salt,
        onValueChange = viewModel::updateSalt,
        label = { Text("Salt") },
        modifier = Modifier.fillMaxWidth(),
        singleLine = true,
    )
    Spacer(Modifier.height(16.dp))

    Text("Routing", style = MaterialTheme.typography.titleMedium)
    Spacer(Modifier.height(8.dp))

    // Default action toggle.
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text("Default action: ")
        FilterChip(
            selected = state.defaultAction == "proxy",
            onClick = { viewModel.updateDefaultAction("proxy") },
            label = { Text("Proxy all") },
        )
        Spacer(Modifier.width(8.dp))
        FilterChip(
            selected = state.defaultAction == "direct",
            onClick = { viewModel.updateDefaultAction("direct") },
            label = { Text("Direct all") },
        )
    }
    Spacer(Modifier.height(8.dp))

    val rulesLabel = if (state.defaultAction == "direct") "Domains to proxy" else "Domains to bypass"
    OutlinedTextField(
        value = state.customDomains,
        onValueChange = viewModel::updateCustomDomains,
        label = { Text(rulesLabel) },
        placeholder = { Text("youtube.com\n*.google.com") },
        modifier = Modifier.fillMaxWidth().height(120.dp),
        maxLines = 8,
    )
    Spacer(Modifier.height(8.dp))

    val ipLabel = if (state.defaultAction == "direct") "IP ranges to proxy" else "IP ranges to bypass"
    OutlinedTextField(
        value = state.customIpRanges,
        onValueChange = viewModel::updateCustomIpRanges,
        label = { Text(ipLabel) },
        placeholder = { Text("91.108.56.0/22\n149.154.160.0/20") },
        modifier = Modifier.fillMaxWidth().height(100.dp),
        maxLines = 6,
    )
}

@Composable
fun StatRow(label: String, value: String) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Text(label, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Text(value)
    }
}

fun formatBytes(bytes: Long): String = when {
    bytes < 1024 -> "$bytes B"
    bytes < 1024 * 1024 -> "${bytes / 1024} KB"
    bytes < 1024 * 1024 * 1024 -> "${"%.1f".format(bytes / 1024.0 / 1024.0)} MB"
    else -> "${"%.2f".format(bytes / 1024.0 / 1024.0 / 1024.0)} GB"
}

fun formatUptime(seconds: Long): String {
    val h = seconds / 3600
    val m = (seconds % 3600) / 60
    val s = seconds % 60
    return if (h > 0) "${h}h ${m}m" else "${m}m ${s}s"
}
