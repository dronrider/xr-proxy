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
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.xrproxy.app.R
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
 * [reclaimable] это потреблённый инвайт, который потребила эта же установка:
 * применить его повторно можно, хаб узнаёт клиента по ключу (XR-216).
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
    reclaimable: Boolean = false,
    willReplaceExisting: Boolean = false,
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
                stringResource(R.string.invite_confirm_title),
                style = MaterialTheme.typography.headlineSmall,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Spacer(Modifier.height(24.dp))

            InviteField(stringResource(R.string.invite_field_hub), hubUrl)
            InviteField(stringResource(R.string.invite_field_preset), preset)
            if (comment.isNotBlank()) {
                InviteField(stringResource(R.string.invite_field_from), comment)
            }
            InviteField(stringResource(R.string.invite_field_expires), ttlLabel(expiresAt))

            if (reclaimable) {
                // Инвайт потрачен нами же, и хаб отдаст настройки повторно
                // (XR-216). Это не отказ, поэтому и не красным.
                Spacer(Modifier.height(16.dp))
                Text(
                    stringResource(R.string.invite_reclaimable_note),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    style = MaterialTheme.typography.bodyMedium,
                    textAlign = TextAlign.Center,
                )
            } else if (status != "active") {
                Spacer(Modifier.height(16.dp))
                val statusText = when (status) {
                    "consumed" -> stringResource(R.string.invite_status_consumed)
                    "expired" -> stringResource(R.string.invite_status_expired)
                    else -> stringResource(R.string.invite_status_unavailable)
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
                    stringResource(R.string.invite_replace_warning),
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
                ) { Text(stringResource(R.string.invite_cancel)) }

                Button(
                    onClick = onApply,
                    enabled = !applyInProgress && (status == "active" || reclaimable),
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
                    Text(stringResource(R.string.invite_apply_button))
                }
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
    // Разбор даты держим отдельно от текста: вокруг вызова composable
    // try/catch не ставится, а неразобранную метку показываем как есть.
    val left = try {
        Duration.between(OffsetDateTime.now(), OffsetDateTime.parse(expiresAt))
    } catch (_: DateTimeParseException) {
        null
    }
    return when {
        left == null -> expiresAt
        left.isNegative -> stringResource(R.string.invite_ttl_expired)
        else -> formatDuration(left)
    }
}

@Composable
private fun formatDuration(d: Duration): String {
    val totalMinutes = d.toMinutes()
    return when {
        totalMinutes < 1 -> stringResource(R.string.invite_ttl_lt_1min)
        totalMinutes < 60 -> stringResource(R.string.invite_ttl_minutes, totalMinutes.toInt())
        totalMinutes < 24 * 60 -> {
            val h = totalMinutes / 60
            val m = totalMinutes % 60
            if (m == 0L) stringResource(R.string.invite_ttl_hours, h.toInt())
            else stringResource(R.string.invite_ttl_hours_minutes, h.toInt(), m.toInt())
        }
        else -> {
            val days = totalMinutes / (24 * 60)
            val hours = (totalMinutes % (24 * 60)) / 60
            if (hours == 0L) stringResource(R.string.invite_ttl_days, days.toInt())
            else stringResource(R.string.invite_ttl_days_hours, days.toInt(), hours.toInt())
        }
    }
}
