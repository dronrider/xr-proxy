package com.xrproxy.app.ui.update

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Error
import androidx.compose.material.icons.filled.SystemUpdate
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.xrproxy.app.ui.UpdateUiState
import com.xrproxy.app.ui.components.formatBytes

/**
 * Actionable update banner (LLD-12 §2.3). Renders nothing for the passive
 * states (Idle / Checking / UpToDate) so it can be dropped at the top of a
 * tab and only appears when there is something to act on.
 */
@Composable
fun UpdateBanner(
    state: UpdateUiState,
    onUpdate: () -> Unit,
    onInstall: () -> Unit,
    onDismiss: () -> Unit,
    onRetry: () -> Unit,
    modifier: Modifier = Modifier,
    deferred: Boolean = false,
) {
    // «Позже» прячет баннер с главной до следующей сессии или более нового
    // релиза; предложение обновиться остаётся на «Серверах» (XR-041).
    if (deferred &&
        (state is UpdateUiState.Available || state is UpdateUiState.ReadyToInstall)
    ) {
        return
    }
    when (state) {
        is UpdateUiState.Available -> Card(
            modifier = modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(
                containerColor = MaterialTheme.colorScheme.primaryContainer,
            ),
        ) {
            Column(Modifier.padding(16.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(Icons.Default.SystemUpdate, null, tint = MaterialTheme.colorScheme.primary)
                    Spacer(Modifier.width(12.dp))
                    Column(Modifier.weight(1f)) {
                        Text("Доступно обновление", style = MaterialTheme.typography.titleSmall)
                        Text(
                            "Версия ${state.release.versionName} · ${formatBytes(state.release.sizeBytes)}",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
                if (state.release.notes.isNotBlank()) {
                    Spacer(Modifier.height(8.dp))
                    Text(state.release.notes, style = MaterialTheme.typography.bodySmall)
                }
                Spacer(Modifier.height(12.dp))
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Button(onClick = onUpdate, modifier = Modifier.weight(1f)) { Text("Обновить") }
                    TextButton(onClick = onDismiss) { Text("Позже") }
                }
            }
        }

        is UpdateUiState.Downloading -> Card(
            modifier = modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(
                containerColor = MaterialTheme.colorScheme.surfaceVariant,
            ),
        ) {
            Column(Modifier.padding(16.dp)) {
                Text(
                    "Загрузка обновления ${state.release.versionName}…",
                    style = MaterialTheme.typography.titleSmall,
                )
                Spacer(Modifier.height(12.dp))
                if (state.progress >= 0f) {
                    LinearProgressIndicator(
                        progress = state.progress,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    Spacer(Modifier.height(4.dp))
                    Text(
                        "${(state.progress * 100).toInt()}%",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
                }
            }
        }

        is UpdateUiState.ReadyToInstall -> Card(
            modifier = modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(
                containerColor = MaterialTheme.colorScheme.primaryContainer,
            ),
        ) {
            Column(Modifier.padding(16.dp)) {
                Text(
                    "Обновление ${state.release.versionName} готово",
                    style = MaterialTheme.typography.titleSmall,
                )
                Text(
                    "Подпись и контрольная сумма проверены",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(12.dp))
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Button(onClick = onInstall, modifier = Modifier.weight(1f)) { Text("Установить") }
                    TextButton(onClick = onDismiss) { Text("Позже") }
                }
            }
        }

        is UpdateUiState.Error -> Card(
            modifier = modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(
                containerColor = MaterialTheme.colorScheme.errorContainer,
            ),
        ) {
            Column(Modifier.padding(16.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(Icons.Default.Error, null, tint = MaterialTheme.colorScheme.error)
                    Spacer(Modifier.width(12.dp))
                    Text(state.message, style = MaterialTheme.typography.bodyMedium)
                }
                Spacer(Modifier.height(12.dp))
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedButton(onClick = onRetry) { Text("Повторить") }
                    TextButton(onClick = onDismiss) { Text("Закрыть") }
                }
            }
        }

        else -> {} // Idle / Checking / UpToDate — nothing actionable.
    }
}

/**
 * "Проверить обновления" control + inline status for the Servers tab. The
 * actionable states (download/install) are handled by [UpdateBanner] rendered
 * alongside; this is just the trigger + a one-line status.
 */
@Composable
fun UpdateCheckControls(
    state: UpdateUiState,
    currentVersionName: String,
    onCheck: () -> Unit,
    onUpdate: () -> Unit,
    onInstall: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val busy = state is UpdateUiState.Checking || state is UpdateUiState.Downloading
    Column(modifier.fillMaxWidth()) {
        Text(
            "Обновление приложения",
            style = MaterialTheme.typography.titleMedium,
            modifier = Modifier.padding(vertical = 8.dp),
        )
        if (currentVersionName.isNotBlank()) {
            Text(
                "Текущая версия: $currentVersionName",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }

        // The actionable result is rendered INLINE here, directly under the
        // header — so "Обновить" / "Установить" are visible right where the
        // user tapped, without scrolling past the check button below.
        when (state) {
            is UpdateUiState.Available -> {
                Spacer(Modifier.height(8.dp))
                Text(
                    "Доступна версия ${state.release.versionName} · ${formatBytes(state.release.sizeBytes)}",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.primary,
                )
                if (state.release.notes.isNotBlank()) {
                    Spacer(Modifier.height(4.dp))
                    Text(state.release.notes, style = MaterialTheme.typography.bodySmall)
                }
                Spacer(Modifier.height(8.dp))
                // Без «Позже»: предложение на этой вкладке живёт, пока
                // обновление не поставлено (XR-041), прятать его нечем.
                Button(onClick = onUpdate, modifier = Modifier.fillMaxWidth()) { Text("Обновить") }
            }
            is UpdateUiState.Downloading -> {
                Spacer(Modifier.height(8.dp))
                Text(
                    "Загрузка ${state.release.versionName}…",
                    style = MaterialTheme.typography.bodyMedium,
                )
                Spacer(Modifier.height(8.dp))
                if (state.progress >= 0f) {
                    LinearProgressIndicator(progress = state.progress, modifier = Modifier.fillMaxWidth())
                    Spacer(Modifier.height(4.dp))
                    Text(
                        "${(state.progress * 100).toInt()}%",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
                }
            }
            is UpdateUiState.ReadyToInstall -> {
                Spacer(Modifier.height(8.dp))
                Text(
                    "Обновление ${state.release.versionName} готово",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.primary,
                )
                Spacer(Modifier.height(8.dp))
                Button(onClick = onInstall, modifier = Modifier.fillMaxWidth()) { Text("Установить") }
            }
            is UpdateUiState.UpToDate -> {
                Spacer(Modifier.height(4.dp))
                Text(
                    "У вас актуальная версия",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
            is UpdateUiState.Error -> {
                Spacer(Modifier.height(4.dp))
                Text(
                    state.message,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.error,
                )
            }
            else -> {}
        }

        Spacer(Modifier.height(8.dp))
        OutlinedButton(
            onClick = onCheck,
            enabled = !busy,
            modifier = Modifier.fillMaxWidth(),
        ) {
            if (busy) {
                CircularProgressIndicator(
                    modifier = Modifier.height(18.dp).width(18.dp),
                    strokeWidth = 2.dp,
                )
                Spacer(Modifier.width(8.dp))
                Text(if (state is UpdateUiState.Downloading) "Загрузка…" else "Проверка…")
            } else {
                Text("Проверить обновления")
            }
        }
    }
}
