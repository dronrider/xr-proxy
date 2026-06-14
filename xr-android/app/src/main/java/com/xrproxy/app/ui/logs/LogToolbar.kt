package com.xrproxy.app.ui.logs

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.FileDownload
import androidx.compose.material.icons.filled.Search
import androidx.compose.material.icons.filled.Share
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconToggleButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp

/**
 * Sticky Log-tab toolbar (LLD-03 §3.4): title with match counter, the
 * Copy/Download/Share/Delete actions (always operate on the full log), and a
 * search field with a regex `.*` toggle and a clear button. Lives outside the
 * LazyColumn so it stays fixed while the log scrolls.
 */
@Composable
fun LogToolbar(
    matchedWarn: Int,
    totalWarn: Int,
    query: String,
    regexMode: Boolean,
    invalidRegex: Boolean,
    onQueryChange: (String) -> Unit,
    onToggleRegex: () -> Unit,
    onCopy: () -> Unit,
    onDownload: () -> Unit,
    onShare: () -> Unit,
    onClear: () -> Unit,
) {
    Column(Modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            val header = if (query.isBlank()) {
                "Log ($totalWarn errors)"
            } else {
                "Log ($matchedWarn/$totalWarn errors)"
            }
            Text(
                header,
                style = MaterialTheme.typography.titleMedium,
                modifier = Modifier.weight(1f),
            )
            IconButton(onClick = onCopy) { Icon(Icons.Default.ContentCopy, "Copy") }
            IconButton(onClick = onDownload) { Icon(Icons.Default.FileDownload, "Download") }
            IconButton(onClick = onShare) { Icon(Icons.Default.Share, "Share") }
            IconButton(onClick = onClear) {
                Icon(Icons.Default.Delete, "Clear", tint = MaterialTheme.colorScheme.error)
            }
        }

        OutlinedTextField(
            value = query,
            onValueChange = onQueryChange,
            leadingIcon = { Icon(Icons.Default.Search, null) },
            trailingIcon = {
                Row(horizontalArrangement = Arrangement.spacedBy(0.dp)) {
                    if (query.isNotEmpty()) {
                        IconButton(onClick = { onQueryChange("") }) {
                            Icon(Icons.Default.Close, "Очистить поиск")
                        }
                    }
                    IconToggleButton(checked = regexMode, onCheckedChange = { onToggleRegex() }) {
                        Text(
                            ".*",
                            fontWeight = FontWeight.Bold,
                            color = if (regexMode) MaterialTheme.colorScheme.primary
                            else MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
            },
            singleLine = true,
            isError = invalidRegex,
            placeholder = { Text("Поиск…") },
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
        )
    }
}
