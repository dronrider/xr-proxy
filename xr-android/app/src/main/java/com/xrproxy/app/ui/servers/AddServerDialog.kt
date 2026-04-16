package com.xrproxy.app.ui.servers

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import com.xrproxy.app.R

@Composable
fun AddServerDialog(
    onScanQr: () -> Unit,
    onPasteLink: () -> Unit,
    onManual: () -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Добавить сервер") },
        text = {
            Column {
                Button(
                    onClick = { onDismiss(); onScanQr() },
                    modifier = Modifier.fillMaxWidth().height(48.dp),
                    shape = RoundedCornerShape(24.dp),
                    colors = ButtonDefaults.buttonColors(
                        containerColor = MaterialTheme.colorScheme.primary,
                        contentColor = MaterialTheme.colorScheme.onPrimary,
                    ),
                ) {
                    Icon(painterResource(R.drawable.ic_qr_scan), null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text("Сканировать QR")
                }
                Spacer(Modifier.height(8.dp))
                OutlinedButton(
                    onClick = { onDismiss(); onPasteLink() },
                    modifier = Modifier.fillMaxWidth().height(48.dp),
                    shape = RoundedCornerShape(24.dp),
                ) {
                    Icon(painterResource(R.drawable.ic_paste), null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text("Вставить ссылку")
                }
                Spacer(Modifier.height(8.dp))
                TextButton(
                    onClick = { onDismiss(); onManual() },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text(
                        "Заполнить вручную",
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        },
        confirmButton = {},
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Отмена") }
        },
    )
}
