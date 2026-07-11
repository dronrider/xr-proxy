package com.xrproxy.app.ui.rules

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.xrproxy.app.data.UserRule
import com.xrproxy.app.jni.NativeBridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import org.json.JSONObject

/** Итог классификации паттерна Rust-стороной (nativeClassifyPattern). */
private data class PatternCheck(
    val kind: String,
    val normalized: String,
    val error: String,
)

private val PatternCheck.valid: Boolean
    get() = kind != "invalid" && kind.isNotBlank()

/** Подпись распознанного типа под полем ввода (LLD-05 §3.5). */
private fun kindLabel(kind: String, normalized: String): String = when (kind) {
    "domain" -> "Домен"
    "wildcard" -> if (normalized == "*") "Любой домен" else "Домен с подстановкой"
    "cidr4" -> "IP-диапазон (IPv4)"
    "cidr6" -> "IP-диапазон (IPv6)"
    else -> ""
}

/**
 * Диалог добавления/правки правила: паттерн с живой классификацией (тип
 * определяется автоматически, без переключателей) и действие Proxy/Direct.
 * [initial] null значит новое правило.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun RuleEditDialog(
    initial: UserRule?,
    onDismiss: () -> Unit,
    onSave: (UserRule) -> Unit,
) {
    var pattern by remember { mutableStateOf(initial?.pattern ?: "") }
    var action by remember { mutableStateOf(initial?.action ?: "proxy") }
    var check by remember { mutableStateOf<PatternCheck?>(null) }

    // Классификация с дебаунсом: ввод каждого символа перезапускает эффект,
    // JNI дёргается не чаще раза в 150 мс и красная рамка не мигает.
    LaunchedEffect(pattern) {
        if (pattern.isBlank()) {
            check = null
            return@LaunchedEffect
        }
        delay(150)
        check = withContext(Dispatchers.IO) {
            val json = runCatching { JSONObject(NativeBridge.nativeClassifyPattern(pattern)) }
                .getOrNull()
            PatternCheck(
                kind = json?.optString("kind") ?: "invalid",
                normalized = json?.optString("normalized") ?: "",
                error = json?.optString("error") ?: "Некорректный формат",
            )
        }
    }

    val current = check
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (initial == null) "Добавить правило" else "Изменить правило") },
        text = {
            Column {
                OutlinedTextField(
                    value = pattern,
                    onValueChange = { pattern = it },
                    label = { Text("Домен или IP-диапазон") },
                    placeholder = { Text("*.example.com или 10.0.0.0/8") },
                    modifier = Modifier.fillMaxWidth(),
                    singleLine = true,
                    isError = current != null && !current.valid,
                    keyboardOptions = KeyboardOptions(
                        keyboardType = KeyboardType.Uri,
                        capitalization = KeyboardCapitalization.None,
                        autoCorrect = false,
                    ),
                    supportingText = {
                        when {
                            current == null -> {}
                            current.valid -> Text(kindLabel(current.kind, current.normalized))
                            else -> Text(current.error, color = MaterialTheme.colorScheme.error)
                        }
                    },
                )
                Spacer(Modifier.height(12.dp))
                SingleChoiceSegmentedButtonRow(modifier = Modifier.fillMaxWidth()) {
                    SegmentedButton(
                        selected = action == "proxy",
                        onClick = { action = "proxy" },
                        shape = SegmentedButtonDefaults.itemShape(index = 0, count = 2),
                    ) { Text("Через прокси") }
                    SegmentedButton(
                        selected = action == "direct",
                        onClick = { action = "direct" },
                        shape = SegmentedButtonDefaults.itemShape(index = 1, count = 2),
                    ) { Text("Напрямую") }
                }
            }
        },
        confirmButton = {
            TextButton(
                enabled = current?.valid == true,
                onClick = {
                    val normalized = current?.normalized?.ifBlank { pattern.trim() }
                        ?: pattern.trim()
                    onSave(
                        UserRule(
                            id = initial?.id ?: java.util.UUID.randomUUID().toString(),
                            action = action,
                            pattern = normalized,
                        )
                    )
                },
            ) { Text("Сохранить") }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Отмена") }
        },
    )
}
