package com.xrproxy.app.ui.servers

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.xrproxy.app.data.JournalSettings

/**
 * Настройки ротации единого журнала (XR-042): размер одного файла и сколько
 * файлов держать на диске. Журнал append-only и переживает перезапуски, так
 * что его объём ограничивается только этими двумя параметрами.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun JournalSection(
    maxKb: Int,
    maxFiles: Int,
    onChange: (maxKb: Int, maxFiles: Int) -> Unit,
) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(top = 24.dp, bottom = 4.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text("Журнал", style = MaterialTheme.typography.titleMedium)
    }
    Text(
        "Лента на вкладке Log пишется в файл и переживает перезапуски. " +
            "Старые записи вытесняются по мере заполнения.",
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )

    Spacer(Modifier.height(12.dp))
    Text("Размер файла", style = MaterialTheme.typography.bodyMedium)
    Spacer(Modifier.height(4.dp))
    SingleChoiceSegmentedButtonRow(modifier = Modifier.fillMaxWidth()) {
        JournalSettings.SIZE_OPTIONS_KB.forEachIndexed { i, kb ->
            SegmentedButton(
                selected = maxKb == kb,
                onClick = { onChange(kb, maxFiles) },
                shape = SegmentedButtonDefaults.itemShape(
                    index = i, count = JournalSettings.SIZE_OPTIONS_KB.size,
                ),
            ) {
                Text(if (kb >= 1024) "${kb / 1024} МБ" else "$kb КБ")
            }
        }
    }

    Spacer(Modifier.height(12.dp))
    Text("Файлов на диске", style = MaterialTheme.typography.bodyMedium)
    Spacer(Modifier.height(4.dp))
    SingleChoiceSegmentedButtonRow(modifier = Modifier.fillMaxWidth()) {
        JournalSettings.FILES_OPTIONS.forEachIndexed { i, n ->
            SegmentedButton(
                selected = maxFiles == n,
                onClick = { onChange(maxKb, n) },
                shape = SegmentedButtonDefaults.itemShape(
                    index = i, count = JournalSettings.FILES_OPTIONS.size,
                ),
            ) {
                Text("$n")
            }
        }
    }
    Spacer(Modifier.height(8.dp))
}
