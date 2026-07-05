package com.xrproxy.app.ui.servers

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.ArrowUpward
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.ContentPaste
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Visibility
import androidx.compose.material.icons.filled.VisibilityOff
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import com.xrproxy.app.data.ProfileEndpoint
import com.xrproxy.app.data.ServerProfile
import com.xrproxy.app.data.ServerSource
import com.xrproxy.app.ui.VpnViewModel
import java.time.OffsetDateTime
import java.util.UUID

@OptIn(ExperimentalMaterial3Api::class, ExperimentalLayoutApi::class)
@Composable
fun ServerEditScreen(
    initial: ServerProfile?,
    onSave: (ServerProfile) -> Unit,
    onCancel: () -> Unit,
) {
    val isCreate = initial == null
    val base = initial ?: ServerProfile(
        id = UUID.randomUUID().toString(),
        name = "",
        serverAddress = "",
        createdAt = OffsetDateTime.now().toString(),
        source = ServerSource.Manual,
    )

    var name by remember { mutableStateOf(base.name) }
    // Пул адресов (LLD-10): порядок в списке и есть приоритет, первый это
    // primary. Легаси-профиль разворачивается в одну строку.
    val endpoints = remember {
        val initial = base.effectiveEndpoints.ifEmpty {
            listOf(ProfileEndpoint(address = "", port = 8443))
        }
        mutableStateListOf(*initial.map { EndpointDraft(it.name, it.address, it.port.toString()) }
            .toTypedArray())
    }
    var key by remember { mutableStateOf(base.obfuscationKey) }
    var showKey by remember { mutableStateOf(false) }
    var modifier by remember { mutableStateOf(base.modifier) }
    var salt by remember { mutableStateOf(base.salt.toString()) }
    var preset by remember { mutableStateOf(base.routingPreset) }
    var customDomains by remember { mutableStateOf(base.customDomains) }
    var customIpRanges by remember { mutableStateOf(base.customIpRanges) }
    var hubUrl by remember { mutableStateOf(base.hubUrl) }
    // Once the user edits the hub field (or it was inherited from an invite),
    // stop auto-deriving it from the server address.
    var hubTouched by remember { mutableStateOf(base.hubUrl.isNotBlank()) }

    var nameError by remember { mutableStateOf(false) }
    var endpointsError by remember { mutableStateOf(false) }
    var keyError by remember { mutableStateOf(false) }
    var saltError by remember { mutableStateOf(false) }

    val clipboardManager = LocalClipboardManager.current

    fun validate(): Boolean {
        nameError = name.isBlank()
        // Минимум один адрес, пустых строк в пуле нет: та же валидация,
        // что у [[servers]] на роутере.
        endpointsError = endpoints.isEmpty() || endpoints.any { it.address.isBlank() }
        keyError = key.isBlank()
        saltError = parseSalt(salt) == null
        return !nameError && !endpointsError && !keyError && !saltError
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(if (isCreate) "Новый сервер" else "Изменить сервер") },
                navigationIcon = {
                    IconButton(onClick = onCancel) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, "Back")
                    }
                },
            )
        },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(horizontal = 24.dp)
                .verticalScroll(rememberScrollState()),
        ) {
            OutlinedTextField(
                value = name, onValueChange = { name = it; nameError = false },
                label = { Text("Имя сервера") },
                placeholder = { Text("Home VPS") },
                modifier = Modifier.fillMaxWidth(), singleLine = true,
                isError = nameError,
                supportingText = if (nameError) {{ Text("Обязательное поле") }} else null,
            )
            Spacer(Modifier.height(8.dp))

            Text("Серверы", style = MaterialTheme.typography.titleSmall)
            Spacer(Modifier.height(4.dp))
            Text(
                "Первый адрес основной, остальные резервные: при падении " +
                    "основного трафик сам переключится на следующий.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(8.dp))

            endpoints.forEachIndexed { idx, draft ->
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    OutlinedTextField(
                        value = draft.address,
                        onValueChange = {
                            endpoints[idx] = draft.copy(address = it)
                            endpointsError = false
                            // Default the hub URL to the primary address (https)
                            // until the user overrides it (see the Hub field below).
                            if (idx == 0 && !hubTouched) {
                                hubUrl = if (it.isBlank()) "" else "https://${it.trim()}"
                            }
                        },
                        label = { Text(if (idx == 0) "Адрес сервера" else "Резерв ${idx}") },
                        placeholder = { Text("1.2.3.4") },
                        modifier = Modifier.weight(1f),
                        singleLine = true,
                        isError = endpointsError && draft.address.isBlank(),
                        supportingText = if (endpointsError && draft.address.isBlank()) {
                            { Text("Обязательное поле") }
                        } else null,
                    )
                    Spacer(Modifier.width(8.dp))
                    OutlinedTextField(
                        value = draft.port,
                        onValueChange = { endpoints[idx] = draft.copy(port = it) },
                        label = { Text("Порт") },
                        modifier = Modifier.width(96.dp),
                        singleLine = true,
                        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                    )
                    if (endpoints.size > 1) {
                        IconButton(
                            onClick = {
                                if (idx > 0) {
                                    // Вверх по списку = выше приоритет.
                                    val tmp = endpoints[idx - 1]
                                    endpoints[idx - 1] = endpoints[idx]
                                    endpoints[idx] = tmp
                                }
                            },
                            enabled = idx > 0,
                        ) {
                            Icon(Icons.Default.ArrowUpward, "Выше приоритет")
                        }
                        IconButton(onClick = { endpoints.removeAt(idx) }) {
                            Icon(Icons.Default.Delete, "Удалить адрес")
                        }
                    }
                }
                Spacer(Modifier.height(4.dp))
            }
            OutlinedButton(
                onClick = { endpoints.add(EndpointDraft("", "", "8443")) },
                modifier = Modifier.fillMaxWidth(),
            ) {
                Icon(Icons.Default.Add, null, Modifier.size(18.dp))
                Spacer(Modifier.width(8.dp))
                Text("Добавить резервный адрес")
            }
            Spacer(Modifier.height(16.dp))

            Text("Обфускация", style = MaterialTheme.typography.titleSmall)
            Spacer(Modifier.height(8.dp))

            OutlinedTextField(
                value = key, onValueChange = { key = it; keyError = false },
                label = { Text("Ключ (base64)") },
                modifier = Modifier.fillMaxWidth(), singleLine = true,
                visualTransformation = if (showKey) VisualTransformation.None
                else PasswordVisualTransformation(),
                isError = keyError,
                supportingText = if (keyError) {{ Text("Обязательное поле") }} else null,
                trailingIcon = {
                    IconButton(onClick = { showKey = !showKey }) {
                        Icon(
                            if (showKey) Icons.Default.VisibilityOff else Icons.Default.Visibility,
                            "Toggle",
                        )
                    }
                },
            )
            Spacer(Modifier.height(8.dp))

            Text("Модификатор", style = MaterialTheme.typography.bodyMedium)
            Spacer(Modifier.height(4.dp))
            FlowRow(modifier = Modifier.fillMaxWidth()) {
                for ((value, label) in MODIFIERS) {
                    FilterChip(
                        selected = modifier == value,
                        onClick = { modifier = value },
                        label = { Text(label) },
                        leadingIcon = if (modifier == value) {
                            { Icon(Icons.Default.Check, null, Modifier.size(16.dp)) }
                        } else null,
                        modifier = Modifier.padding(end = 8.dp),
                    )
                }
            }
            Spacer(Modifier.height(8.dp))

            OutlinedTextField(
                value = salt, onValueChange = { salt = it; saltError = false },
                label = { Text("Salt") },
                placeholder = { Text("0xDEADBEEF или 3735928559") },
                modifier = Modifier.fillMaxWidth(), singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Ascii),
                isError = saltError,
                supportingText = if (saltError) {
                    { Text("Число: десятичное или 0x…, диапазон 0…4294967295") }
                } else null,
            )
            Spacer(Modifier.height(16.dp))

            Text("Маршрутизация", style = MaterialTheme.typography.titleSmall)
            Spacer(Modifier.height(8.dp))

            Row(modifier = Modifier.fillMaxWidth()) {
                for ((value, label) in listOf(
                    "russia" to "Russia",
                    "proxy_all" to "Proxy all",
                    "custom" to "Custom",
                )) {
                    FilterChip(
                        selected = preset == value,
                        onClick = { preset = value },
                        label = { Text(label) },
                        leadingIcon = if (preset == value) {
                            { Icon(Icons.Default.Check, null, Modifier.size(16.dp)) }
                        } else null,
                        modifier = Modifier.padding(end = 8.dp),
                    )
                }
            }

            if (preset == "custom") {
                Spacer(Modifier.height(8.dp))
                OutlinedButton(
                    onClick = {
                        val text = clipboardManager.getText()?.text ?: ""
                        if (text.isNotBlank()) {
                            val (d, r) = VpnViewModel.parseTomlDomains(text)
                            if (d.isNotEmpty() || r.isNotEmpty()) {
                                customDomains = d.joinToString("\n")
                                customIpRanges = r.joinToString("\n")
                            }
                        }
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Icon(Icons.Default.ContentPaste, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text("Import TOML from clipboard")
                }
                Spacer(Modifier.height(8.dp))
                OutlinedTextField(
                    value = customDomains, onValueChange = { customDomains = it },
                    label = { Text("Domains to proxy") },
                    placeholder = { Text("youtube.com\n*.google.com") },
                    modifier = Modifier.fillMaxWidth().height(120.dp), maxLines = 8,
                )
                Spacer(Modifier.height(8.dp))
                OutlinedTextField(
                    value = customIpRanges, onValueChange = { customIpRanges = it },
                    label = { Text("IP ranges to proxy") },
                    placeholder = { Text("91.108.56.0/22") },
                    modifier = Modifier.fillMaxWidth().height(100.dp), maxLines = 6,
                )
            }

            Spacer(Modifier.height(16.dp))
            Text("Хаб", style = MaterialTheme.typography.titleSmall)
            Spacer(Modifier.height(8.dp))
            OutlinedTextField(
                value = hubUrl,
                onValueChange = { hubUrl = it; hubTouched = true },
                label = { Text("Адрес хаба") },
                placeholder = { Text("https://hub.example.com") },
                modifier = Modifier.fillMaxWidth(), singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                supportingText = {
                    Text(
                        "Централизованный сервер конфигурации (HTTPS): хранит правила " +
                            "маршрутизации (пресеты) и обновления приложения. По умолчанию — " +
                            "адрес сервера; можно оставить пустым.",
                    )
                },
            )
            Spacer(Modifier.height(4.dp))
            if (base.hubPreset.isNotBlank()) {
                Text(
                    "Правила маршрутизации берутся из хаба (пресет: ${base.hubPreset}).",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            } else {
                Text(
                    "У этого сервера правила берутся из пресета выше; адрес хаба " +
                        "используется для проверки обновлений.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            Spacer(Modifier.height(24.dp))
            Button(
                onClick = {
                    if (!validate()) return@Button
                    val newEndpoints = endpoints.map {
                        ProfileEndpoint(
                            name = it.name,
                            address = it.address.trim(),
                            port = it.port.toIntOrNull() ?: 8443,
                        )
                    }
                    val primary = newEndpoints.first()
                    val autoName = name.ifBlank { primary.address }
                    val profile = base.copy(
                        name = autoName,
                        // Legacy-поля зеркалят primary (LLD-10 §5.7).
                        serverAddress = primary.address,
                        serverPort = primary.port,
                        endpoints = newEndpoints,
                        obfuscationKey = key.trim(),
                        modifier = modifier,
                        salt = parseSalt(salt) ?: DEFAULT_SALT,
                        routingPreset = preset,
                        customDomains = if (preset == "custom") customDomains else "",
                        customIpRanges = if (preset == "custom") customIpRanges else "",
                        // hubPreset intentionally left as-is: a manual server keeps
                        // it blank, so the hub URL drives only the update check
                        // (preset refresh needs both hubUrl AND hubPreset set).
                        hubUrl = normalizeHubUrl(hubUrl),
                    )
                    onSave(profile)
                },
                modifier = Modifier.fillMaxWidth(),
            ) {
                Icon(Icons.Default.Check, null, Modifier.size(18.dp))
                Spacer(Modifier.width(8.dp))
                Text("Сохранить")
            }
            Spacer(Modifier.height(8.dp))
            OutlinedButton(onClick = onCancel, modifier = Modifier.fillMaxWidth()) {
                Text("Отмена")
            }
            Spacer(Modifier.height(16.dp))
        }
    }
}

/**
 * Нормализует введённый адрес хаба к URL: пустой → "" (хаб не задан), без
 * схемы → префиксуем `https://`. Хвостовой `/` убираем — Rust-клиент его
 * добавляет сам при сборке `/api/v1/...`.
 */
internal fun normalizeHubUrl(input: String): String {
    val t = input.trim().trimEnd('/')
    if (t.isEmpty()) return ""
    return if (t.contains("://")) t else "https://$t"
}

/**
 * Черновик строки пула в редакторе: порт как текст (пока набирается), имя
 * не редактируется, но сохраняется. Оно приходит из инвайта и питает
 * статусную строку «через X (резерв)».
 */
private data class EndpointDraft(
    val name: String,
    val address: String,
    val port: String,
)

private const val DEFAULT_SALT = 0xDEADBEEFL

/** Должен совпадать с server.toml → [obfuscation].modifier. */
private val MODIFIERS = listOf(
    "positional_xor_rotate" to "Positional XOR",
    "rotating_salt" to "Rotating salt",
    "substitution_table" to "Substitution",
)

/**
 * Парсит salt из десятичной или hex-строки (`0x…`). Возвращает null, если ввод
 * нечисловой или не влезает в u32 — именно столько использует ядро обфускации.
 */
internal fun parseSalt(input: String): Long? {
    val t = input.trim()
    val value = when {
        t.startsWith("0x", ignoreCase = true) -> t.substring(2).toLongOrNull(16)
        else -> t.toLongOrNull()
    } ?: return null
    return if (value in 0L..0xFFFFFFFFL) value else null
}
