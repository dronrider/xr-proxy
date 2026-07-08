package com.xrproxy.app.ui

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.core.content.ContextCompat
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.view.WindowCompat
import com.xrproxy.app.data.ServerProfile
import com.xrproxy.app.model.HealthLevel
import com.xrproxy.app.ui.components.DebugSection
import com.xrproxy.app.ui.components.ShieldArrowIcon
import com.xrproxy.app.ui.components.StatsGrid
import com.xrproxy.app.ui.components.XrSnackbarHost
import com.xrproxy.app.ui.components.formatBytes
import com.xrproxy.app.ui.components.formatUptime
import com.xrproxy.app.ui.files.FilesScreen
import com.xrproxy.app.ui.onboarding.InviteConfirmScreen
import com.xrproxy.app.ui.onboarding.PasteLinkDialog
import com.xrproxy.app.ui.onboarding.WelcomeScreen
import com.xrproxy.app.ui.onboarding.scanInviteQr
import com.xrproxy.app.ui.servers.AddServerDialog
import com.xrproxy.app.ui.servers.ServerEditScreen
import com.xrproxy.app.ui.servers.ServerSwitcherChip
import com.xrproxy.app.ui.servers.ServerSwitcherSheet
import com.xrproxy.app.ui.logs.LogList
import com.xrproxy.app.ui.logs.LogToolbar
import com.xrproxy.app.ui.logs.filterLog
import com.xrproxy.app.ui.servers.FailurePolicySection
import com.xrproxy.app.ui.servers.JournalSection
import com.xrproxy.app.ui.servers.ServersSection
import com.xrproxy.app.ui.theme.XrTheme
import com.xrproxy.app.ui.trusted.TrustedNetworksSection
import com.xrproxy.app.ui.update.UpdateBanner
import com.xrproxy.app.ui.update.UpdateCheckControls
import kotlinx.coroutines.launch

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

        WindowCompat.setDecorFitsSystemWindows(window, false)

        vpnPermissionLauncher = registerForActivityResult(
            ActivityResultContracts.StartActivityForResult()
        ) { result ->
            viewModel.onPermissionResult(result.resultCode == Activity.RESULT_OK)
        }

        notificationPermissionLauncher = registerForActivityResult(
            ActivityResultContracts.RequestPermission()
        ) {}

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            val granted = checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
                    PackageManager.PERMISSION_GRANTED
            if (!granted) {
                notificationPermissionLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
            }
        }

        handleIntent(intent)

        setContent {
            XrTheme {
                MainScreen(
                    viewModel = viewModel,
                    launchVpnPermission = { intent -> vpnPermissionLauncher.launch(intent) },
                    finishActivity = { finish() },
                )
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleIntent(intent)
    }

    /**
     * HTTPS-ссылки `/invite/...` и `xr://invite` — две точки входа по deep link.
     * Парсинг/сетевая часть живут в VpnViewModel, здесь — только роутинг URI.
     */
    private fun handleIntent(intent: Intent?) {
        if (intent == null) return
        if (intent.action != Intent.ACTION_VIEW) return
        val data = intent.data ?: return
        viewModel.onInviteLinkReceived(data.toString())
    }
}

// ── Navigation state for server editing ─────────────────────────────

private sealed interface EditMode {
    data class Create(val fromWelcome: Boolean = false) : EditMode
    data class Edit(val profile: ServerProfile) : EditMode
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MainScreen(
    viewModel: VpnViewModel,
    launchVpnPermission: (Intent) -> Unit,
    finishActivity: () -> Unit,
) {
    val state by viewModel.uiState.collectAsState()
    val onboarding by viewModel.onboardingState.collectAsState()
    val servers by viewModel.repo.servers.collectAsState()
    val activeId by viewModel.repo.activeId.collectAsState()
    val activeServer = remember(servers, activeId) {
        servers.firstOrNull { it.id == activeId }
    }
    val trustedNetworks by viewModel.trustedRepo.networks.collectAsState()
    val trustedEnabled by viewModel.trustedRepo.enabled.collectAsState()
    val failClosed by viewModel.failClosed.collectAsState()
    val journalMaxKb by viewModel.journalMaxKb.collectAsState()
    val journalMaxFiles by viewModel.journalMaxFiles.collectAsState()
    val updateState by viewModel.updateState.collectAsState()

    // Location permission for reading the current Wi-Fi SSID (auto-pause,
    // task 3b-2). FINE_LOCATION is the cross-version path that unredacts the
    // SSID in NetworkCapabilities; NEARBY_WIFI_DEVICES is added on 33+. The
    // feature degrades gracefully without it (never pauses).
    val permissionContext = LocalContext.current
    var permissionEpoch by remember { mutableIntStateOf(0) }
    val hasSsidPermission = remember(permissionEpoch) {
        ContextCompat.checkSelfPermission(
            permissionContext, Manifest.permission.ACCESS_FINE_LOCATION,
        ) == PackageManager.PERMISSION_GRANTED
    }
    val ssidPermissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions()
    ) { permissionEpoch++ }

    // SAF document picker for downloading the log (LLD-03 §3.5). Writes the
    // full log to the user-chosen location; no storage permission needed.
    val downloadLogLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.CreateDocument("text/plain")
    ) { uri ->
        if (uri != null) viewModel.writeLogTo(uri, permissionContext.contentResolver)
    }
    val requestSsidPermission: () -> Unit = {
        val perms = buildList {
            add(Manifest.permission.ACCESS_FINE_LOCATION)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                add(Manifest.permission.NEARBY_WIFI_DEVICES)
            }
        }.toTypedArray()
        ssidPermissionLauncher.launch(perms)
    }

    var currentTab by remember { mutableIntStateOf(0) }
    val snackbarHostState = remember { SnackbarHostState() }
    var lastSeverity by remember { mutableStateOf(UiSeverity.Info) }
    var pasteDialogOpen by remember { mutableStateOf(false) }
    var addServerDialogOpen by remember { mutableStateOf(false) }
    var switcherSheetOpen by remember { mutableStateOf(false) }
    var editMode by remember { mutableStateOf<EditMode?>(null) }
    val activity = LocalContext.current as Activity
    val scope = rememberCoroutineScope()

    LaunchedEffect(Unit) {
        viewModel.permissionRequest.collect { intent -> launchVpnPermission(intent) }
    }
    LaunchedEffect(Unit) {
        viewModel.openIntent.collect { intent -> activity.startActivity(intent) }
    }
    LaunchedEffect(Unit) {
        viewModel.messages.collect { msg ->
            lastSeverity = msg.severity
            snackbarHostState.showSnackbar(msg.text)
        }
    }

    // ── ServerEditScreen overlay ────────────────────────────────────
    editMode?.let { mode ->
        ServerEditScreen(
            initial = when (mode) {
                is EditMode.Create -> null
                is EditMode.Edit -> mode.profile
            },
            onSave = { profile ->
                when (mode) {
                    is EditMode.Create -> {
                        viewModel.upsertServer(profile)
                        viewModel.repo.setActive(profile.id)
                        if (mode.fromWelcome) {
                            viewModel.onManualSetupChosen()
                        }
                    }
                    is EditMode.Edit -> viewModel.onServerEditSaved(profile)
                }
                editMode = null
            },
            onCancel = {
                if (mode is EditMode.Create && mode.fromWelcome &&
                    servers.isEmpty()) {
                    // Nothing to go back to — stay on Welcome
                }
                editMode = null
            },
        )
        return
    }

    // ── Onboarding overlays ────────────────────────────────────────
    if (onboarding != OnboardingState.Completed) {
        Box(modifier = Modifier.fillMaxSize()) {
            when (val ob = onboarding) {
                is OnboardingState.ShowingWelcome -> {
                    WelcomeScreen(
                        onScanClick = {
                            scope.launch {
                                try {
                                    val raw = scanInviteQr(activity) ?: return@launch
                                    viewModel.onInviteLinkReceived(raw)
                                } catch (_: Throwable) {
                                    snackbarHostState.showSnackbar(
                                        "Сканер QR недоступен, используйте \"Вставить ссылку\""
                                    )
                                }
                            }
                        },
                        onPasteClick = { pasteDialogOpen = true },
                        onManualClick = { editMode = EditMode.Create(fromWelcome = true) },
                    )
                    if (pasteDialogOpen) {
                        PasteLinkDialog(
                            onDismiss = { pasteDialogOpen = false },
                            onSubmit = { raw ->
                                pasteDialogOpen = false
                                viewModel.onInviteLinkReceived(raw)
                            },
                        )
                    }
                }
                is OnboardingState.Loading -> {
                    Box(Modifier.fillMaxSize(), Alignment.Center) {
                        CircularProgressIndicator()
                    }
                }
                is OnboardingState.ConfirmInvite -> {
                    InviteConfirmScreen(
                        hubUrl = ob.hubUrl,
                        preset = ob.preset,
                        comment = ob.comment,
                        status = ob.status,
                        expiresAt = ob.expiresAt,
                        willReplaceExisting = false,
                        applyEnabled = state.phase == ConnectPhase.Idle,
                        applyInProgress = ob.applyInProgress,
                        onApply = { viewModel.onInviteConfirmed() },
                        onCancel = {
                            viewModel.onInviteCancelled()
                            if (servers.isEmpty()) finishActivity()
                        },
                    )
                }
                is OnboardingState.Completed -> {}
            }
            XrSnackbarHost(
                snackbarHostState, lastSeverity,
                modifier = Modifier.align(Alignment.BottomCenter),
            )
        }
        return
    }

    // ── Add-server dialog ──────────────────────────────────────────
    if (addServerDialogOpen) {
        AddServerDialog(
            onScanQr = {
                addServerDialogOpen = false
                scope.launch {
                    try {
                        val raw = scanInviteQr(activity) ?: return@launch
                        viewModel.onInviteLinkReceived(raw)
                    } catch (_: Throwable) {
                        snackbarHostState.showSnackbar(
                            "Сканер QR недоступен, используйте \"Вставить ссылку\""
                        )
                    }
                }
            },
            onPasteLink = {
                addServerDialogOpen = false
                pasteDialogOpen = true
            },
            onManual = {
                addServerDialogOpen = false
                editMode = EditMode.Create()
            },
            onDismiss = { addServerDialogOpen = false },
        )
    }
    if (pasteDialogOpen) {
        PasteLinkDialog(
            onDismiss = { pasteDialogOpen = false },
            onSubmit = { raw ->
                pasteDialogOpen = false
                viewModel.onInviteLinkReceived(raw)
            },
        )
    }

    // ── Server switcher BottomSheet ────────────────────────────────
    if (switcherSheetOpen) {
        ServerSwitcherSheet(
            servers = servers,
            activeId = activeId,
            onSelect = { id -> viewModel.selectServer(id) },
            onEdit = { server ->
                switcherSheetOpen = false
                editMode = EditMode.Edit(server)
            },
            onAddServer = { addServerDialogOpen = true },
            onDismiss = { switcherSheetOpen = false },
        )
    }

    // ── Main scaffold ──────────────────────────────────────────────
    Scaffold(
        snackbarHost = { XrSnackbarHost(snackbarHostState, lastSeverity) },
        containerColor = MaterialTheme.colorScheme.background,
        bottomBar = {
            NavigationBar(containerColor = MaterialTheme.colorScheme.surfaceVariant) {
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
                            val infos = state.logLines.infoCount
                            val warns = state.logLines.warnCount
                            val errs = state.logLines.errorCount
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
                                ) { Text(label, fontSize = 10.sp) }
                            }
                        }) { Icon(Icons.AutoMirrored.Filled.List, null) }
                    },
                    label = { Text("Log") },
                )
                NavigationBarItem(
                    selected = currentTab == 2,
                    onClick = { currentTab = 2 },
                    icon = { Icon(Icons.Default.Dns, null) },
                    label = { Text("Servers") },
                )
                NavigationBarItem(
                    selected = currentTab == 3,
                    onClick = { currentTab = 3 },
                    icon = { Icon(Icons.Default.Folder, null) },
                    label = { Text("Files") },
                )
            }
        }
    ) { padding ->
        // The Log tab hosts its own LazyColumn (sticky toolbar + virtualized
        // list, LLD-03), so it must NOT sit inside a verticalScroll parent —
        // each tab picks the container it needs.
        when (currentTab) {
            0 -> Column(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(padding)
                    .padding(horizontal = 24.dp)
                    .verticalScroll(rememberScrollState()),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                UpdateBanner(
                    state = updateState,
                    onUpdate = { viewModel.startUpdateDownload() },
                    onInstall = { viewModel.installReadyUpdate() },
                    onDismiss = { viewModel.dismissUpdate() },
                    onRetry = { viewModel.checkForUpdates(manual = true) },
                    modifier = Modifier.padding(top = 16.dp),
                )
                ConnectionSection(
                    state = state,
                    activeServer = activeServer,
                    onConnect = { viewModel.onConnectClicked() },
                    onDisconnect = viewModel::disconnect,
                    onResumeHere = { viewModel.resumeOnTrustedNetwork() },
                    onPauseHere = { viewModel.pauseOnTrustedNetwork() },
                    onToggleDebug = viewModel::toggleDebug,
                    onSwitcherClick = { switcherSheetOpen = true },
                    snackbarHostState = snackbarHostState,
                )
            }
            1 -> LogSection(
                state = state,
                viewModel = viewModel,
                onDownload = { name -> downloadLogLauncher.launch(name) },
                modifier = Modifier.fillMaxSize().padding(padding),
            )
            2 -> Column(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(padding)
                    .padding(horizontal = 24.dp)
                    .verticalScroll(rememberScrollState()),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                ServersSection(
                    servers = servers,
                    activeId = activeId,
                    isConnected = state.connected,
                    onSetActive = { viewModel.selectServer(it) },
                    onEdit = { editMode = EditMode.Edit(it) },
                    onDelete = { viewModel.deleteServer(it) },
                    onAddServer = { addServerDialogOpen = true },
                )
                TrustedNetworksSection(
                    networks = trustedNetworks,
                    enabled = trustedEnabled,
                    hasPermission = hasSsidPermission,
                    onToggleEnabled = { enabled ->
                        viewModel.setTrustedAutoPauseEnabled(enabled)
                        if (enabled && !hasSsidPermission) requestSsidPermission()
                    },
                    onAdd = { viewModel.addTrustedNetwork(it) },
                    onRemove = { viewModel.removeTrustedNetwork(it) },
                    onRequestPermission = requestSsidPermission,
                    availableSsids = { viewModel.availableSsids() },
                )
                FailurePolicySection(
                    failClosed = failClosed,
                    onToggle = { viewModel.setFailClosed(it) },
                )
                JournalSection(
                    maxKb = journalMaxKb,
                    maxFiles = journalMaxFiles,
                    onChange = { kb, files -> viewModel.setJournalRotation(kb, files) },
                )
                Spacer(Modifier.height(8.dp))
                val appVersion = remember {
                    try {
                        permissionContext.packageManager
                            .getPackageInfo(permissionContext.packageName, 0).versionName ?: ""
                    } catch (_: Exception) { "" }
                }
                UpdateCheckControls(
                    state = updateState,
                    currentVersionName = appVersion,
                    onCheck = { viewModel.checkForUpdates(manual = true) },
                    onUpdate = { viewModel.startUpdateDownload() },
                    onInstall = { viewModel.installReadyUpdate() },
                    onDismiss = { viewModel.dismissUpdate() },
                )
                Spacer(Modifier.height(16.dp))
            }
            3 -> FilesScreen(
                hubUrl = activeServer?.hubUrl,
                inviteToken = activeServer?.inviteToken,
                modifier = Modifier.fillMaxSize().padding(padding),
            )
        }
    }
}

// ── VPN Connection tab ──────────────────────────────────────────────

@Composable
fun ConnectionSection(
    state: VpnUiState,
    activeServer: ServerProfile?,
    onConnect: () -> Unit,
    onDisconnect: () -> Unit,
    onResumeHere: () -> Unit,
    onPauseHere: () -> Unit,
    onToggleDebug: () -> Unit,
    onSwitcherClick: () -> Unit,
    snackbarHostState: SnackbarHostState,
) {
    Spacer(Modifier.height(32.dp))

    ShieldArrowIcon(phase = state.phase, modifier = Modifier.size(128.dp))
    Spacer(Modifier.height(16.dp))

    // Status text with inline health emoji (LLD-08 §2.4)
    val healthEmoji = if (state.connected) {
        when (state.health) {
            HealthLevel.Healthy -> " \uD83D\uDE0A"
            HealthLevel.Good -> " \uD83D\uDE42"
            HealthLevel.Watching -> " \uD83D\uDE10"
            HealthLevel.Hurt -> " \uD83D\uDE1F"
            HealthLevel.Critical -> " \uD83D\uDE35"
        }
    } else ""
    val statusText = when (state.phase) {
        ConnectPhase.Idle, ConnectPhase.NeedsPermission -> "Disconnected"
        ConnectPhase.Preparing -> "Подготовка…"
        ConnectPhase.Connecting -> "Подключение…"
        ConnectPhase.Finalizing -> "Проверка маршрутов…"
        ConnectPhase.Connected -> "Подключено$healthEmoji"
        ConnectPhase.Paused -> "На паузе"
        ConnectPhase.Stopping -> "Отключение…"
    }
    val statusColor = when (state.phase) {
        ConnectPhase.Connected -> MaterialTheme.colorScheme.primary
        ConnectPhase.Preparing, ConnectPhase.Connecting, ConnectPhase.Finalizing ->
            MaterialTheme.colorScheme.tertiary
        else -> MaterialTheme.colorScheme.onSurfaceVariant
    }
    Text(statusText, style = MaterialTheme.typography.headlineMedium, color = statusColor)

    // Phase substep
    val substep = when (state.phase) {
        ConnectPhase.Preparing -> "1/3 · Подготовка"
        ConnectPhase.Connecting -> "2/3 · Установка туннеля"
        ConnectPhase.Finalizing -> "3/3 · Проверка маршрутов"
        else -> null
    }
    if (substep != null) {
        Spacer(Modifier.height(4.dp))
        Text(substep, style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant)
    }

    // Активен резервный сервер пула (LLD-10 §2.6): primary упал, движок
    // увёл трафик на backup. Строка исчезает после failback.
    if (state.connected && state.backupActive && state.activeServer.isNotBlank()) {
        Spacer(Modifier.height(4.dp))
        Text(
            "через ${state.activeServer} (резерв)",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.tertiary,
        )
    }

    // Контекст доверенной сети даём компактными строками под статусом, по
    // образцу строки «через X (резерв)»: без карточки метрики и кнопка
    // остаются на месте, а само действие живёт в главной кнопке (XR-049).
    if (state.paused) {
        Spacer(Modifier.height(4.dp))
        Text(
            state.pausedSsid?.let { "Доверенная сеть «$it»" } ?: "Доверенная сеть",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(
            "трафик идёт напрямую, VPN включится сам при уходе из сети",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.outline,
            textAlign = TextAlign.Center,
        )
        if (state.restrictedNetwork) {
            Spacer(Modifier.height(4.dp))
            Text(
                "В этой сети есть ограничения: включите VPN, если что-то не открывается",
                style = MaterialTheme.typography.bodySmall,
                color = Color(0xFFFFA726),
                textAlign = TextAlign.Center,
            )
        }
    }
    if (state.connected && state.overrideSsid != null) {
        Spacer(Modifier.height(4.dp))
        Text(
            "включено вручную · доверенная сеть «${state.overrideSsid}»",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.tertiary,
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
        Text("v$versionName", style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.outline)
    }

    Spacer(Modifier.height(16.dp))

    // Server switcher chip (LLD-08 §2.4)
    if (activeServer != null) {
        ServerSwitcherChip(
            activeName = activeServer.name,
            presetLabel = activeServer.presetLabel,
            enabled = state.phase == ConnectPhase.Idle,
            onClick = onSwitcherClick,
            modifier = Modifier.fillMaxWidth(0.7f),
        )
    }

    Spacer(Modifier.height(12.dp))

    // Главная кнопка меняет состояние по контексту (XR-049): на паузе в
    // доверенной сети это «Включить здесь» (override), при включённом вручную
    // туннеле «Выключить здесь» (возврат в авто-паузу). Полный стоп из этих
    // состояний остаётся в уведомлении («Отключить») и по уходу из сети.
    val trustedForcedOn = state.connected && state.overrideSsid != null
    val btnColor = when {
        state.paused -> MaterialTheme.colorScheme.primary
        state.connected -> MaterialTheme.colorScheme.error
        state.connecting -> MaterialTheme.colorScheme.tertiary
        else -> MaterialTheme.colorScheme.primary
    }
    val btnTextColor = when {
        state.connected -> MaterialTheme.colorScheme.onError
        else -> MaterialTheme.colorScheme.onPrimary
    }
    Button(
        onClick = {
            when {
                state.paused -> onResumeHere()
                trustedForcedOn -> onPauseHere()
                state.connected || state.connecting -> onDisconnect()
                else -> onConnect()
            }
        },
        modifier = Modifier.fillMaxWidth(0.7f).height(56.dp),
        shape = RoundedCornerShape(28.dp),
        colors = ButtonDefaults.buttonColors(containerColor = btnColor, contentColor = btnTextColor),
    ) {
        val btnText = when {
            state.connecting -> "Cancel"
            state.paused -> "Включить здесь"
            trustedForcedOn -> "Выключить здесь"
            state.connected -> "Disconnect"
            else -> "Connect"
        }
        Text(btnText, style = MaterialTheme.typography.titleMedium)
    }


    // Statistics
    if (state.connected) {
        Spacer(Modifier.height(24.dp))

        // No-traffic warning: данные уходят, но ответа нет — почти всегда
        // означает рассогласование ключа/salt/modifier с сервером (сервер не
        // может расшифровать запрос и закрывает соединение).
        if (state.uptime >= 8 && state.bytesUp > 8192 && state.bytesDown == 0L) {
            Card(
                modifier = Modifier.fillMaxWidth(),
                colors = CardDefaults.cardColors(containerColor = Color(0xFF2A2418)),
            ) {
                Row(
                    modifier = Modifier.padding(16.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(Icons.Default.Warning, null, tint = Color(0xFFFFA726))
                    Spacer(Modifier.width(12.dp))
                    Text(
                        "Трафик уходит, но ответа нет. Проверьте ключ, salt и " +
                            "modifier — они должны точно совпадать с сервером.",
                        color = Color(0xFFFFA726),
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
            }
            Spacer(Modifier.height(12.dp))
        }

        StatsGrid(state = state)
        Spacer(Modifier.height(8.dp))
        DebugSection(
            state = state, expanded = state.debugExpanded,
            onToggle = onToggleDebug, snackbarHostState = snackbarHostState,
        )
    }

    // No-server banner
    if (!state.connected && !state.connecting && activeServer == null) {
        Spacer(Modifier.height(24.dp))
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = Color(0xFF2A1818)),
        ) {
            Row(modifier = Modifier.padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Default.Warning, null, tint = MaterialTheme.colorScheme.error)
                Spacer(Modifier.width(12.dp))
                Text("Добавьте сервер во вкладке Servers", color = MaterialTheme.colorScheme.error)
            }
        }
    }
    Spacer(Modifier.height(16.dp))
}

// ── Log tab (LLD-03) ────────────────────────────────────────────────

@Composable
fun LogSection(
    state: VpnUiState,
    viewModel: VpnViewModel,
    onDownload: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val filter = remember(state.logLines, state.logQuery, state.logRegexMode) {
        filterLog(state.logLines, state.logQuery, state.logRegexMode)
    }
    val totalWarn = state.logLines.warnCount
    val matchedWarn = filter.entries.warnCount

    Column(modifier) {
        LogToolbar(
            matchedWarn = matchedWarn,
            totalWarn = totalWarn,
            query = state.logQuery,
            regexMode = state.logRegexMode,
            invalidRegex = filter.invalidRegex,
            onQueryChange = viewModel::updateLogQuery,
            onToggleRegex = viewModel::toggleLogRegexMode,
            onCopy = viewModel::copyLog,
            onDownload = { onDownload(defaultLogFileName()) },
            onShare = { viewModel.shareLog(context) },
            onClear = viewModel::clearLog,
        )
        LogList(
            entries = filter.entries,
            queryActive = state.logQuery.isNotBlank(),
            modifier = Modifier.weight(1f).fillMaxWidth(),
        )
    }
}

private fun defaultLogFileName(): String {
    val ts = java.time.LocalDateTime.now()
        .format(java.time.format.DateTimeFormatter.ofPattern("yyyy-MM-dd-HHmmss"))
    return "xr-proxy-log-$ts.txt"
}
