package com.xrproxy.app.ui.onboarding

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.ErrorOutline
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.xrproxy.app.R

/** Хост хаба для подписи под статусом: схема и хвостовой слэш это шум,
 *  человек сверяет именно адрес. */
private fun hostLabel(hubUrl: String): String =
    hubUrl.removePrefix("https://").removePrefix("http://").trimEnd('/')

/**
 * Ожидание проверки инвайта (XR-234). Раньше тут был голый спиннер: до пяти
 * секунд пустого экрана на плохой сети читались как зависание. Адрес хаба
 * известен ещё до сети (ссылка разбирается локально), поэтому экран говорит,
 * что происходит и к кому идёт запрос. Пока ссылка не разобрана, [hubUrl]
 * пуст, и подпись не рисуется, как и на стартовой заглушке приложения.
 */
@Composable
fun InviteLoadingScreen(hubUrl: String) {
    Column(
        modifier = Modifier.fillMaxSize().padding(horizontal = 24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        CircularProgressIndicator()
        if (hubUrl.isNotBlank()) {
            Spacer(Modifier.height(24.dp))
            Text(
                stringResource(R.string.invite_loading_title),
                style = MaterialTheme.typography.headlineSmall,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Spacer(Modifier.height(4.dp))
            Text(
                hostLabel(hubUrl),
                style = MaterialTheme.typography.bodySmall,
                fontFamily = FontFamily.Monospace,
                color = MaterialTheme.colorScheme.outline,
            )
        }
    }
}

/**
 * Отказ проверки инвайта (XR-234). Прежде причина мелькала снекбаром, а
 * экран сбрасывался на приветствие: человек оказывался там, откуда начал,
 * и не знал почему. Теперь причина стоит перед глазами, «Повторить» гоняет
 * ту же ссылку заново, а на приветствие уводит только «Назад».
 */
@Composable
fun InviteErrorScreen(
    title: String,
    detail: String,
    hubUrl: String,
    retryable: Boolean,
    onRetry: () -> Unit,
    onBack: () -> Unit,
) {
    BackHandler(onBack = onBack)
    Column(
        modifier = Modifier.fillMaxSize().padding(horizontal = 24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Icon(
            Icons.Outlined.ErrorOutline,
            contentDescription = null,
            tint = MaterialTheme.colorScheme.error,
            modifier = Modifier.size(56.dp),
        )
        Spacer(Modifier.height(20.dp))
        Text(
            title,
            style = MaterialTheme.typography.headlineSmall,
            color = MaterialTheme.colorScheme.onSurface,
            textAlign = TextAlign.Center,
        )
        Spacer(Modifier.height(6.dp))
        Text(
            detail,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
        if (hubUrl.isNotBlank()) {
            Spacer(Modifier.height(4.dp))
            Text(
                hostLabel(hubUrl),
                style = MaterialTheme.typography.bodySmall,
                fontFamily = FontFamily.Monospace,
                color = MaterialTheme.colorScheme.outline,
            )
        }
        Spacer(Modifier.height(28.dp))
        if (retryable) {
            Button(
                onClick = onRetry,
                modifier = Modifier.fillMaxWidth(0.7f).height(56.dp),
                shape = RoundedCornerShape(28.dp),
                colors = ButtonDefaults.buttonColors(
                    containerColor = MaterialTheme.colorScheme.primary,
                    contentColor = MaterialTheme.colorScheme.onPrimary,
                ),
            ) { Text(stringResource(R.string.invite_retry), style = MaterialTheme.typography.titleMedium) }
            Spacer(Modifier.height(12.dp))
        }
        OutlinedButton(
            onClick = onBack,
            modifier = Modifier.fillMaxWidth(0.7f).height(56.dp),
            shape = RoundedCornerShape(28.dp),
        ) { Text(stringResource(R.string.invite_back), style = MaterialTheme.typography.titleMedium) }
    }
}
