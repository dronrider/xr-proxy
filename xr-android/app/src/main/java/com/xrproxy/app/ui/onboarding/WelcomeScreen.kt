package com.xrproxy.app.ui.onboarding

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
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
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import com.xrproxy.app.R
import com.xrproxy.app.ui.ConnectPhase
import com.xrproxy.app.ui.components.ShieldArrowIcon

/**
 * First-launch Welcome screen (LLD-04 §3.3). Shown when neither server
 * settings nor a hub invite have been applied yet. Three paths in, all
 * converge on a configured VpnUiState.
 */
@Composable
fun WelcomeScreen(
    onScanClick: () -> Unit,
    onPasteClick: () -> Unit,
    onManualClick: () -> Unit,
) {
    val context = LocalContext.current
    val versionName = remember {
        try {
            context.packageManager.getPackageInfo(context.packageName, 0).versionName ?: ""
        } catch (_: Exception) {
            ""
        }
    }

    Box(modifier = Modifier.fillMaxSize().padding(horizontal = 24.dp)) {
        Column(
            modifier = Modifier.fillMaxSize(),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            ShieldArrowIcon(phase = ConnectPhase.Idle, modifier = Modifier.size(128.dp))
            Spacer(Modifier.height(24.dp))
            Text(
                "XR Proxy",
                style = MaterialTheme.typography.headlineMedium,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Spacer(Modifier.height(8.dp))
            Text(
                "Безопасное подключение к интернету",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(32.dp))

            Button(
                onClick = onScanClick,
                modifier = Modifier.fillMaxWidth(0.85f).height(56.dp),
                shape = RoundedCornerShape(28.dp),
                colors = ButtonDefaults.buttonColors(
                    containerColor = MaterialTheme.colorScheme.primary,
                    contentColor = MaterialTheme.colorScheme.onPrimary,
                ),
            ) {
                Icon(
                    painter = painterResource(R.drawable.ic_qr_scan),
                    contentDescription = null,
                    modifier = Modifier.size(20.dp),
                )
                Spacer(Modifier.width(12.dp))
                Text("Сканировать QR-код", style = MaterialTheme.typography.titleMedium)
            }
            Spacer(Modifier.height(12.dp))

            OutlinedButton(
                onClick = onPasteClick,
                modifier = Modifier.fillMaxWidth(0.85f).height(56.dp),
                shape = RoundedCornerShape(28.dp),
            ) {
                Icon(
                    painter = painterResource(R.drawable.ic_paste),
                    contentDescription = null,
                    modifier = Modifier.size(20.dp),
                )
                Spacer(Modifier.width(12.dp))
                Text("Вставить ссылку", style = MaterialTheme.typography.titleMedium)
            }
            Spacer(Modifier.height(12.dp))

            TextButton(
                onClick = onManualClick,
                modifier = Modifier.fillMaxWidth(0.85f),
            ) {
                Text(
                    "Настроить вручную",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }

        if (versionName.isNotBlank()) {
            Text(
                "v$versionName",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.outline,
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .padding(bottom = 24.dp),
            )
        }
    }
}
