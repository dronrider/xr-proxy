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
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.List
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
import androidx.core.view.WindowCompat
import com.xrproxy.app.ui.components.DebugSection
import com.xrproxy.app.ui.components.HealthFace
import com.xrproxy.app.ui.components.ShieldArrowIcon
import com.xrproxy.app.ui.components.StatsGrid
import com.xrproxy.app.ui.components.XrSnackbarHost
import com.xrproxy.app.ui.components.formatBytes
import com.xrproxy.app.ui.components.formatUptime
import com.xrproxy.app.ui.theme.XrTheme

/**
 * Счётчики событий по уровням в `recent_errors`. Критерии совпадают с
 * разметкой в `colorizeLog`, чтобы бадж и визуал журнала видели одно и то же.
 *
 * Rust-сторона сворачивает подряд идущие дубликаты в пределах одной
 * секунды и дописывает суффикс `(×N)`. Бадж должен показывать число
 * СОБЫТИЙ, а не строк — поэтому каждая запись даёт свой N в сумму.
 */
private val COUNT_SUFFIX_RE = Regex(" \\(\u00D7(\\d+)\\)\$")

private fun String.repeatCount(): Int =
    COUNT_SUFFIX_RE.find(this)?.groupValues?.get(1)?.toIntOrNull() ?: 1

private val List<String>.errorCount: Int
    get() = filter { it.contains(" ERROR ") }.sumOf { it.repeatCount() }

private val List<String>.warnCount: Int
    get() = filter { it.contains(" WARN ") }.sumOf { it.repeatCount() }

private val List<String>.infoCount: Int
    get() = filter { !it.contains(" ERROR ") && !it.contains(" WARN ") }
        .sumOf { it.repeatCount() }

class MainActivity : ComponentActivity() {
    private val viewModel: VpnViewModel by viewModels()

    private lateinit var vpnPermissionLauncher: ActivityResultLauncher<Intent>
    private lateinit var notificationPermissionLauncher: ActivityResultLauncher<String>

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Edge-to-edge (LLD-06 §3.1)
        WindowCompat.setDecorFitsSystemWindows(window, false)

        vpnPermissionLauncher = registerForActivityResult(
            ActivityResultContracts.StartActivityForResult()
        ) { result ->
            viewModel.onPermissionResult(result.resultCode == Activity.RESULT_OK)
        }

        notificationPermissionLauncher = registerForActivityResult(
            ActivityResultContracts.RequestPermission()
        ) { /* no-op — если отказано, туннель всё равно работает */ }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            val granted = checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
                    PackageManager.PERMISSION_GRANTED
            if (!granted) {
                notificationPermissionLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
            }
        }

        setContent {
            XrTheme {
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
    var currentTab by remember { mutableIntStateOf(0) }

    val snackbarHostState = remember { SnackbarHostState() }
    var lastSeverity by remember { mutableStateOf(UiSeverity.Info) }

    LaunchedEffect(Unit) {
        viewModel.permissionRequest.collect { intent -> launchVpnPermission(intent) }
    }
    LaunchedEffect(Unit) {
        viewModel.messages.collect { msg ->
            lastSeverity = msg.severity
            snackbarHostState.showSnackbar(msg.text)
        }
    }

    Scaffold(
        snackbarHost = { XrSnackbarHost(snackbarHostState, lastSeverity) },
        containerColor = MaterialTheme.colorScheme.background,
        bottomBar = {
            NavigationBar(
                containerColor = MaterialTheme.colorScheme.surfaceVariant,
            ) {
                NavigationBarItem(
                    selected = currentTab == 0,
                    onClick = { currentTab = 0 },
                    icon = { Icon(Icons.Default.Shield, null) },
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
                                val infoColor = Color(0xFF4CAF50)
                                val warnColor = Color(0xFFFFA726)
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
                            Icon(Icons.AutoMirrored.Filled.List, null)
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
                .padding(horizontal = 24.dp)
                .verticalScroll(rememberScrollState()),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            when (currentTab) {
                0 -> ConnectionSection(
                    state = state,
                    onConnect = { viewModel.onConnectClicked() },
                    onDisconnect = viewModel::disconnect,
                    onToggleDebug = viewModel::toggleDebug,
                    snackbarHostState = snackbarHostState,
                )
                1 -> LogSection(state, viewModel)
                2 -> SettingsSection(state, viewModel)
            }
        }
    }
}

// ── VPN Connection tab ──────────────────────────────────────────────

@Composable
fun ConnectionSection(
    state: VpnUiState,
    onConnect: () -> Unit,
    onDisconnect: () -> Unit,
    onToggleDebug: () -> Unit,
    snackbarHostState: SnackbarHostState,
) {
    Spacer(Modifier.height(32.dp))

    // 1. Central shield icon (LLD-06 §3.5)
    ShieldArrowIcon(
        phase = state.phase,
        modifier = Modifier.size(128.dp),
    )

    // 1a. Health HUD — only in Connected (LLD-06 §3.5a)
    if (state.connected) {
        Spacer(Modifier.height(8.dp))
        HealthFace(level = state.health)
    }

    Spacer(Modifier.height(16.dp))

    // 2. Status line (LLD-06 §3.4)
    val statusText = when (state.phase) {
        ConnectPhase.Idle, ConnectPhase.NeedsPermission -> "Disconnected"
        ConnectPhase.Preparing -> "Подготовка…"
        ConnectPhase.Connecting -> "Подключение…"
        ConnectPhase.Finalizing -> "Проверка маршрутов…"
        ConnectPhase.Connected -> "Подключено"
        ConnectPhase.Stopping -> "Отключение…"
    }
    val statusColor = when (state.phase) {
        ConnectPhase.Connected -> MaterialTheme.colorScheme.primary
        ConnectPhase.Preparing, ConnectPhase.Connecting, ConnectPhase.Finalizing ->
            MaterialTheme.colorScheme.tertiary
        else -> MaterialTheme.colorScheme.onSurfaceVariant
    }
    Text(
        statusText,
        style = MaterialTheme.typography.headlineMedium,
        color = statusColor,
    )

    // 3. Phase substep line (LLD-06 §3.4)
    val substep = when (state.phase) {
        ConnectPhase.Preparing -> "1/3 · Подготовка"
        ConnectPhase.Connecting -> "2/3 · Установка туннеля"
        ConnectPhase.Finalizing -> "3/3 · Проверка маршрутов"
        else -> null
    }
    if (substep != null) {
        Spacer(Modifier.height(4.dp))
        Text(
            substep,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }

    // Version
    val context = LocalContext.current
    val versionName = remember {
        try { context.packageManager.getPackageInfo(context.packageName, 0).versionName ?: "" }
        catch (_: Exception) { "" }
    }
    if (versionName.isNotBlank() && !state.connected && !state.connecting) {
        Spacer(Modifier.height(4.dp))
        Text("v$versionName", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.outline)
    }

    // 5. Preset hint (LLD-06 §3.4)
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

    // 4. Connect / Disconnect / Cancel button — pill shape (LLD-06 §3.4)
    val btnColor = when {
        state.connected -> MaterialTheme.colorScheme.error
        state.connecting -> MaterialTheme.colorScheme.tertiary
        else -> MaterialTheme.colorScheme.primary
    }
    val btnTextColor = when {
        state.connected -> MaterialTheme.colorScheme.onError
        else -> MaterialTheme.colorScheme.onPrimary
    }

    Button(
        onClick = { if (state.connected || state.connecting) onDisconnect() else onConnect() },
        modifier = Modifier
            .fillMaxWidth(0.7f)
            .height(56.dp),
        shape = RoundedCornerShape(28.dp),
        colors = ButtonDefaults.buttonColors(
            containerColor = btnColor,
            contentColor = btnTextColor,
        ),
    ) {
        val btnText = when {
            state.connecting -> "Cancel"
            state.connected -> "Disconnect"
            else -> "Connect"
        }
        Text(btnText, style = MaterialTheme.typography.titleMedium)
    }

    // 6. Statistics cards — only in Connected (LLD-06 §3.7)
    if (state.connected) {
        Spacer(Modifier.height(24.dp))
        StatsGrid(state = state)
        Spacer(Modifier.height(8.dp))
        DebugSection(
            state = state,
            expanded = state.debugExpanded,
            onToggle = onToggleDebug,
            snackbarHostState = snackbarHostState,
        )
    }

    // 7. Configure server banner (LLD-06 §3.4)
    if (!state.connected && !state.connecting && state.serverAddress.isBlank()) {
        Spacer(Modifier.height(24.dp))
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = Color(0xFF2A1818)),
        ) {
            Row(modifier = Modifier.padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Default.Warning, null, tint = MaterialTheme.colorScheme.error)
                Spacer(Modifier.width(12.dp))
                Text("Configure server in Settings tab", color = MaterialTheme.colorScheme.error)
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
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
        ) {
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
