package com.xrproxy.app.ui

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.ActivityResultLauncher
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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.withStyle
import java.io.File
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * Счётчики строк по уровням в `recent_errors`. Критерии совпадают с
 * разметкой в `colorizeLog`, чтобы бадж и визуал журнала видели одно и то же.
 * Формат сообщения из Rust: `"TS LVL msg"`, где LVL — INFO/WARN/ERROR,
 * всегда обёрнут пробелами (ширина поля 5, выравнивание вправо).
 */
private val List<String>.errorCount: Int
    get() = count { it.contains(" ERROR ") }

private val List<String>.warnCount: Int
    get() = count { it.contains(" WARN ") }

private val List<String>.infoCount: Int
    get() = size - errorCount - warnCount

class MainActivity : ComponentActivity() {
    private val viewModel: VpnViewModel by viewModels()

    private lateinit var vpnPermissionLauncher: ActivityResultLauncher<Intent>
    private lateinit var notificationPermissionLauncher: ActivityResultLauncher<String>

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        vpnPermissionLauncher = registerForActivityResult(
            ActivityResultContracts.StartActivityForResult()
        ) { result ->
            viewModel.onPermissionResult(result.resultCode == Activity.RESULT_OK)
        }

        notificationPermissionLauncher = registerForActivityResult(
            ActivityResultContracts.RequestPermission()
        ) { /* no-op — если отказано, туннель всё равно работает */ }

        // Runtime POST_NOTIFICATIONS request on API 33+. Manifest permission
        // alone is not enough — without the user tapping Allow on the system
        // dialog, startForeground() silently shows nothing in the shade.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            val granted = checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
                    PackageManager.PERMISSION_GRANTED
            if (!granted) {
                notificationPermissionLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
            }
        }

        setContent {
            MaterialTheme {
                MainScreen(
                    viewModel = viewModel,
                    launchVpnPermission = { intent -> vpnPermissionLauncher.launch(intent) },
                )
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MainScreen(viewModel: VpnViewModel, launchVpnPermission: (Intent) -> Unit) {
    val state by viewModel.uiState.collectAsState()
    var currentTab by remember { mutableIntStateOf(0) } // 0=VPN, 1=Log, 2=Settings

    val snackbarHostState = remember { SnackbarHostState() }

    LaunchedEffect(Unit) {
        viewModel.permissionRequest.collect { intent -> launchVpnPermission(intent) }
    }
    LaunchedEffect(Unit) {
        viewModel.messages.collect { msg -> snackbarHostState.showSnackbar(msg) }
    }

    Scaffold(
        snackbarHost = { SnackbarHost(snackbarHostState) },
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
                    onClick = { currentTab = 1 },
                    icon = {
                        BadgedBox(badge = {
                            val infos = state.recentErrors.infoCount
                            val warns = state.recentErrors.warnCount
                            val errs = state.recentErrors.errorCount
                            if (infos + warns + errs > 0) {
                                // Трёхцветный бадж: <info>/<warn>/<error>.
                                // Цвета подобраны на нейтральном surface-фоне, чтобы
                                // все три части читались контрастно.
                                val infoColor = Color(0xFF4CAF50)    // green 500
                                val warnColor = Color(0xFFFFA726)    // orange 400
                                val errColor = MaterialTheme.colorScheme.error
                                val label = buildAnnotatedString {
                                    withStyle(SpanStyle(color = infoColor)) { append("$infos") }
                                    append("/")
                                    withStyle(SpanStyle(color = warnColor)) { append("$warns") }
                                    append("/")
                                    withStyle(SpanStyle(color = errColor)) { append("$errs") }
                                }
                                Badge(
                                    containerColor = MaterialTheme.colorScheme.surfaceContainerHighest,
                                    contentColor = MaterialTheme.colorScheme.onSurface,
                                ) {
                                    Text(label, fontSize = 10.sp)
                                }
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
                0 -> ConnectionSection(
                    state = state,
                    onConnect = { viewModel.onConnectClicked() },
                    onDisconnect = viewModel::disconnect,
                )
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
                StatRow("Warnings / Errors", "${state.relayWarnings} / ${state.relayErrors}")
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
    val context = LocalContext.current

    val logText = state.recentErrors.joinToString("\n")
    val warns = state.recentErrors.warnCount
    val errs = state.recentErrors.errorCount

    Row(modifier = Modifier.fillMaxWidth().padding(vertical = 8.dp),
        horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
        val header = when {
            errs > 0 && warns > 0 -> "Log ($errs errors, $warns warnings)"
            errs > 0 -> "Log ($errs errors)"
            warns > 0 -> "Log ($warns warnings)"
            else -> "Log"
        }
        Text(header, style = MaterialTheme.typography.titleMedium)
        Row {
            IconButton(onClick = {
                clipboardManager.setText(AnnotatedString(logText))
            }) {
                Icon(Icons.Default.ContentCopy, "Copy")
            }
            IconButton(onClick = {
                // Save log to cache and share via Intent.
                try {
                    val file = File(context.cacheDir, "xr-proxy.log")
                    file.writeText(logText)
                    val uri = androidx.core.content.FileProvider.getUriForFile(
                        context, "${context.packageName}.fileprovider", file
                    )
                    val intent = Intent(Intent.ACTION_SEND).apply {
                        type = "text/plain"
                        putExtra(Intent.EXTRA_STREAM, uri)
                        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                    }
                    context.startActivity(Intent.createChooser(intent, "Share log"))
                } catch (_: Exception) {
                    // Fallback: copy to clipboard.
                    clipboardManager.setText(AnnotatedString(logText))
                }
            }) {
                Icon(Icons.Default.Share, "Share")
            }
            IconButton(onClick = { viewModel.clearLog() }) {
                Icon(Icons.Default.Delete, "Clear")
            }
        }
    }

    if (state.recentErrors.isEmpty()) {
        Spacer(Modifier.height(32.dp))
        Text("No entries", style = MaterialTheme.typography.bodyLarge,
            color = MaterialTheme.colorScheme.onSurfaceVariant)
    } else {
        Card(modifier = Modifier.fillMaxWidth()) {
            Text(
                text = colorizeLog(logText),
                modifier = Modifier.padding(12.dp),
                style = MaterialTheme.typography.bodySmall,
                fontSize = 11.sp,
                lineHeight = 16.sp,
            )
        }
    }
    Spacer(Modifier.height(16.dp))
}

/** Colour log lines by level: ERROR red, WARN orange, INFO default. */
@Composable
fun colorizeLog(log: String): AnnotatedString {
    val errColor = MaterialTheme.colorScheme.error
    val warnColor = Color(0xFFFFA726)
    return buildAnnotatedString {
        for (line in log.lines()) {
            when {
                line.contains(" ERROR ") -> withStyle(SpanStyle(color = errColor)) { append(line) }
                line.contains(" WARN ") -> withStyle(SpanStyle(color = warnColor)) { append(line) }
                else -> append(line)
            }
            append("\n")
        }
    }
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
