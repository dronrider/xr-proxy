package com.xrproxy.app.ui.onboarding

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.xrproxy.app.ui.ConnectPhase
import com.xrproxy.app.ui.components.ShieldArrowIcon
import kotlinx.coroutines.delay
import java.time.Duration
import java.time.OffsetDateTime
import java.time.format.DateTimeParseException

/**
 * Подтверждение инвайта (LLD-04 §3.5). Данные — `InviteInfo` из
 * GET /api/v1/invite/:token; секретов нет, инвайт ещё не consume'нут.
 *
 * Кнопка «Применить» запускает фазу 2 (claim + preset fetch). Пока она в
 * полёте, индикация через `applyInProgress` — блокируем кнопку со
 * спиннером, чтобы не было двойных claim'ов.
 */
@Composable
fun InviteConfirmScreen(
    hubUrl: String,
    preset: String,
    comment: String,
    status: String,
    expiresAt: String,
    willReplaceExisting: Boolean = false,
    applyEnabled: Boolean,
    applyInProgress: Boolean,
    onApply: () -> Unit,
    onCancel: () -> Unit,
) {
    Box(
        modifier = Modifier
            .fillMaxSize()
            .padding(horizontal = 24.dp),
        contentAlignment = Alignment.Center,
    ) {
        Column(
            horizontalAlignment = Alignment.CenterHorizontally,
            modifier = Modifier.fillMaxWidth(),
        ) {
            ShieldArrowIcon(phase = ConnectPhase.Idle, modifier = Modifier.size(96.dp))
            Spacer(Modifier.height(16.dp))
            Text(
                "Настройка подключения",
                style = MaterialTheme.typography.headlineSmall,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Spacer(Modifier.height(24.dp))

            InviteField("Хаб", hubUrl)
            InviteField("Пресет", preset)
            if (comment.isNotBlank()) {
                InviteField("От кого", comment)
            }
            InviteField("Действителен", ttlLabel(expiresAt))

            if (status != "active") {
                Spacer(Modifier.height(16.dp))
                val statusText = when (status) {
                    "consumed" -> "Этот инвайт уже использован"
                    "expired" -> "Срок действия инвайта истёк"
                    else -> "Инвайт недоступен"
                }
                Text(
                    statusText,
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodyMedium,
                    textAlign = TextAlign.Center,
                )
            }

            if (willReplaceExisting && status == "active") {
                Spacer(Modifier.height(16.dp))
                Text(
                    "⚠ Существующие настройки подключения будут заменены.",
                    color = MaterialTheme.colorScheme.tertiary,
                    style = MaterialTheme.typography.bodyMedium,
                    textAlign = TextAlign.Center,
                )
            }

            Spacer(Modifier.height(32.dp))
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                OutlinedButton(
                    onClick = onCancel,
                    shape = RoundedCornerShape(28.dp),
                    modifier = Modifier.weight(1f).height(56.dp),
                ) { Text("Отмена") }

                Button(
                    onClick = onApply,
                    enabled = applyEnabled && !applyInProgress && status == "active",
                    shape = RoundedCornerShape(28.dp),
                    colors = ButtonDefaults.buttonColors(
                        containerColor = MaterialTheme.colorScheme.primary,
                        contentColor = MaterialTheme.colorScheme.onPrimary,
                    ),
                    modifier = Modifier.weight(1f).height(56.dp),
                ) {
                    if (applyInProgress) {
                        CircularProgressIndicator(
                            modifier = Modifier.size(20.dp),
                            strokeWidth = 2.dp,
                            color = MaterialTheme.colorScheme.onPrimary,
                        )
                        Spacer(Modifier.width(8.dp))
                    }
                    Text("Добавить")
                }
            }

            if (!applyEnabled && status == "active") {
                Spacer(Modifier.height(8.dp))
                Text(
                    "Сначала отключите VPN",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}

@Composable
private fun InviteField(label: String, value: String) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Text(
            label,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(
            value,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurface,
        )
    }
}

/**
 * Обновляется раз в минуту через `LaunchedEffect`, чтобы пользователь видел
 * живой обратный отсчёт. На локальное время полагаемся за неимением
 * server_time в `InviteInfo` — для пятиминутного окна подтверждения
 * достаточно.
 */
@Composable
private fun ttlLabel(expiresAt: String): String {
    var tick by remember { mutableStateOf(0) }
    LaunchedEffect(expiresAt) {
        while (true) {
            delay(60_000)
            tick++
        }
    }
    // Read `tick` so recomposition picks up new state.
    @Suppress("UNUSED_EXPRESSION") tick
    return try {
        val expires = OffsetDateTime.parse(expiresAt)
        val now = OffsetDateTime.now()
        val left = Duration.between(now, expires)
        if (left.isNegative) "истёк" else formatDuration(left)
    } catch (_: DateTimeParseException) {
        expiresAt
    }
}

private fun formatDuration(d: Duration): String {
    val totalMinutes = d.toMinutes()
    return when {
        totalMinutes < 1 -> "< 1 мин"
        totalMinutes < 60 -> "ещё $totalMinutes мин"
        totalMinutes < 24 * 60 -> {
            val h = totalMinutes / 60
            val m = totalMinutes % 60
            if (m == 0L) "ещё $h ч" else "ещё $h ч $m мин"
        }
        else -> {
            val days = totalMinutes / (24 * 60)
            val hours = (totalMinutes % (24 * 60)) / 60
            if (hours == 0L) "ещё $days д" else "ещё $days д $hours ч"
        }
    }
}
