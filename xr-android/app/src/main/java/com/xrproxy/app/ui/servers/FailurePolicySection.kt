package com.xrproxy.app.ui.servers

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/**
 * Переключатель политики на случай, когда недоступен весь пул серверов.
 *
 * fail-closed (block, дефолт): проксируемый трафик режется. Домены попадают в
 * прокси не просто так. Напрямую они либо не открываются, либо светят реальный
 * IP (риск блокировки аккаунта), поэтому «либо через прокси, либо никак».
 * fail-open (direct): проксируемое уходит напрямую. Мягче деградирует, но
 * засвечивает реальный IP на заблокированных ресурсах. failover primary ->
 * резерв работает при любом значении; переключатель влияет только на случай,
 * когда лёг весь пул.
 */
@Composable
fun FailurePolicySection(
    failClosed: Boolean,
    onToggle: (Boolean) -> Unit,
) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(top = 24.dp, bottom = 4.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text("Если все серверы недоступны", style = MaterialTheme.typography.titleMedium)
        Switch(checked = failClosed, onCheckedChange = onToggle)
    }

    Text(
        if (failClosed) {
            "Проксируемые ресурсы блокируются (не идут напрямую). Реальный IP не " +
                "засветится, но при отказе всех серверов такие сайты просто не " +
                "откроются. Остальной трафик работает как обычно."
        } else {
            "Проксируемые ресурсы уходят напрямую в обход прокси. Открываются, но " +
                "светят реальный IP на заблокированных сайтах (риск блокировки " +
                "аккаунта)."
        },
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
    Spacer(Modifier.height(8.dp))
}
