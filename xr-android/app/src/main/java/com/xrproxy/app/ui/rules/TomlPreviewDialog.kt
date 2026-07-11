package com.xrproxy.app.ui.rules

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties

/** Порог отображения (LLD-05 §5.6): огромный пресет не рендерим целиком,
 *  полный текст доступен через «Скопировать». */
private const val DISPLAY_LIMIT = 50_000

/**
 * Полноэкранный просмотр собранного `[routing]`-блока: то, что движок
 * скомпилирует при следующем подключении. Read-only, моноширинный,
 * «Скопировать» кладёт в буфер полный текст (готов для client.toml роутера).
 */
@Composable
fun TomlPreviewDialog(
    toml: String,
    onDismiss: () -> Unit,
    onCopied: () -> Unit,
) {
    val clipboard = LocalClipboardManager.current
    val shown = remember(toml) {
        if (toml.length <= DISPLAY_LIMIT) toml
        else {
            val cut = toml.take(DISPLAY_LIMIT)
            val rest = toml.substring(DISPLAY_LIMIT).count { it == '\n' }
            "$cut\n… и ещё $rest строк (скопируйте для полного текста)"
        }
    }

    Dialog(
        onDismissRequest = onDismiss,
        properties = DialogProperties(usePlatformDefaultWidth = false),
    ) {
        Surface(
            modifier = Modifier
                .fillMaxSize()
                .padding(16.dp),
            shape = MaterialTheme.shapes.large,
            color = MaterialTheme.colorScheme.surface,
        ) {
            Column {
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 16.dp, vertical = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        "TOML",
                        style = MaterialTheme.typography.titleMedium,
                        modifier = Modifier.weight(1f),
                    )
                    TextButton(onClick = {
                        clipboard.setText(AnnotatedString(toml))
                        onCopied()
                    }) { Text("Скопировать") }
                }
                HorizontalDivider()
                Text(
                    shown,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 12.sp,
                    lineHeight = 16.sp,
                    modifier = Modifier
                        .weight(1f)
                        .fillMaxWidth()
                        .verticalScroll(rememberScrollState())
                        .padding(16.dp),
                )
                HorizontalDivider()
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 16.dp, vertical = 4.dp),
                ) {
                    Text("", modifier = Modifier.weight(1f))
                    TextButton(onClick = onDismiss) { Text("Закрыть") }
                }
            }
        }
    }
}
