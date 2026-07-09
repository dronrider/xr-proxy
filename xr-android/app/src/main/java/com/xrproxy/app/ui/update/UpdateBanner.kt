package com.xrproxy.app.ui.update

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.SystemUpdate
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.xrproxy.app.ui.UpdateUiState
import com.xrproxy.app.ui.components.formatBytes
import com.xrproxy.app.ui.updatePending

/**
 * Карточка обновления в блоке «Обновление приложения» на «Серверах»
 * (LLD-12 §2.3, XR-041): доступный релиз, прогресс загрузки, готовность к
 * установке. Предложение живёт, пока обновление не поставлено, поэтому
 * кнопок «скрыть» здесь нет; пассивные состояния не рендерятся.
 */
@Composable
fun UpdateBanner(
    state: UpdateUiState,
    onUpdate: () -> Unit,
    onInstall: () -> Unit,
    modifier: Modifier = Modifier,
) {
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
                Button(onClick = onUpdate, modifier = Modifier.fillMaxWidth()) { Text("Обновить") }
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
                Button(onClick = onInstall, modifier = Modifier.fillMaxWidth()) { Text("Установить") }
            }
        }

        else -> {} // Idle / UpToDate / Error: показываются в UpdateCheckControls.
    }
}

/**
 * Компактное уведомление на главной (XR-041, дизайн владельца): одна строка
 * вместо большого баннера, чтобы о новой версии было видно больше, чем точку,
 * но без дубля предложения. Тап ведёт на «Серверы» к самому предложению,
 * крестик закрывает уведомление для этой версии насовсем (точка остаётся).
 */
@Composable
fun UpdateNotice(
    state: UpdateUiState,
    onOpen: () -> Unit,
    onClose: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val text = when (state) {
        is UpdateUiState.Available -> "Доступно обновление ${state.release.versionName}"
        is UpdateUiState.ReadyToInstall -> "Обновление ${state.release.versionName} готово"
        else -> return
    }
    Surface(
        shape = RoundedCornerShape(12.dp),
        color = MaterialTheme.colorScheme.surfaceVariant,
        modifier = modifier.fillMaxWidth(),
    ) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .clickable(onClick = onOpen)
                .padding(start = 12.dp, top = 4.dp, bottom = 4.dp, end = 4.dp),
        ) {
            Icon(
                Icons.Default.SystemUpdate,
                null,
                tint = MaterialTheme.colorScheme.primary,
                modifier = Modifier.size(18.dp),
            )
            Spacer(Modifier.width(8.dp))
            Text(
                text,
                style = MaterialTheme.typography.bodyMedium,
                modifier = Modifier.weight(1f),
            )
            IconButton(onClick = onClose) {
                Icon(
                    Icons.Default.Close,
                    "Закрыть",
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}

/**
 * Блок «Обновление приложения» для вкладки «Серверы» (XR-041): текущая
 * версия и, пока висит обновление, закреплённый [UpdateBanner]. Кнопка
 * «Проверить обновления» в этот момент прячется: искать нечего, предложение
 * уже на экране. Без обновления остаются ручная проверка и её статусы
 * «актуально»/ошибка.
 */
@Composable
fun UpdateCheckControls(
    state: UpdateUiState,
    currentVersionName: String,
    buildInfo: String,
    checking: Boolean,
    onCheck: () -> Unit,
    onUpdate: () -> Unit,
    onInstall: () -> Unit,
    modifier: Modifier = Modifier,
) {
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
        // Ориентир, какая именно сборка установлена: дата и коммит, versionName
        // у релиза этой информации больше не несёт (XR-041).
        if (buildInfo.isNotBlank()) {
            Text(
                "Сборка: $buildInfo",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }

        if (state.updatePending) {
            Spacer(Modifier.height(8.dp))
            UpdateBanner(
                state = state,
                onUpdate = onUpdate,
                onInstall = onInstall,
            )
        } else {
            when (state) {
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
                enabled = !checking,
                modifier = Modifier.fillMaxWidth(),
            ) {
                if (checking) {
                    CircularProgressIndicator(
                        modifier = Modifier.height(18.dp).width(18.dp),
                        strokeWidth = 2.dp,
                    )
                    Spacer(Modifier.width(8.dp))
                    Text("Проверка…")
                } else {
                    Text("Проверить обновления")
                }
            }
        }
    }
}
