package com.xrproxy.app.ui.logs

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SmallFloatingActionButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.launch

/**
 * Virtualized, auto-following log list (LLD-03 §3.2, §3.7). New entries stick
 * to the bottom while the user is at the bottom; if they scroll up to read,
 * following pauses and a "↓" FAB appears to jump back and re-enable it.
 */
@Composable
fun LogList(
    entries: List<String>,
    queryActive: Boolean,
    modifier: Modifier = Modifier,
) {
    if (entries.isEmpty()) {
        Box(modifier.fillMaxSize(), contentAlignment = Alignment.TopCenter) {
            Text(
                if (queryActive) "Ничего не найдено" else "No entries",
                style = MaterialTheme.typography.bodyLarge,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(top = 32.dp),
            )
        }
        return
    }

    val listState = rememberLazyListState()
    val scope = rememberCoroutineScope()
    var autoFollow by remember { mutableStateOf(true) }

    val isAtBottom by remember {
        derivedStateOf {
            val lastVisible = listState.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: 0
            lastVisible >= entries.lastIndex.coerceAtLeast(0)
        }
    }

    // When a user scroll settles, follow only if they parked at the bottom.
    LaunchedEffect(listState.isScrollInProgress) {
        if (!listState.isScrollInProgress) autoFollow = isAtBottom
    }
    // Stick to the tail as new entries arrive (only while following).
    LaunchedEffect(entries.size) {
        if (autoFollow && entries.isNotEmpty()) listState.scrollToItem(entries.lastIndex)
    }

    Box(modifier.fillMaxSize()) {
        LazyColumn(state = listState, modifier = Modifier.fillMaxSize()) {
            items(
                count = entries.size,
                // index + content: stable enough across 1s refreshes for our
                // ≤200-line buffer; avoids duplicate-key crashes on identical
                // same-second entries (LLD-03 §5.1).
                key = { index -> "${index}_${entries[index]}" },
            ) { index ->
                Text(
                    colorizeLine(entries[index]),
                    style = MaterialTheme.typography.bodySmall,
                    fontSize = 11.sp,
                    lineHeight = 16.sp,
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 12.dp, vertical = 1.dp),
                )
            }
        }

        if (!autoFollow) {
            SmallFloatingActionButton(
                onClick = {
                    autoFollow = true
                    scope.launch { listState.scrollToItem(entries.lastIndex) }
                },
                modifier = Modifier.align(Alignment.BottomEnd).padding(16.dp),
            ) {
                Icon(Icons.Default.KeyboardArrowDown, "Прокрутить вниз")
            }
        }
    }
}

@Composable
private fun colorizeLine(line: String): AnnotatedString {
    val errColor = MaterialTheme.colorScheme.error
    val warnColor = Color(0xFFFFA726)
    return buildAnnotatedString {
        when {
            line.contains(" ERROR ") -> withStyle(SpanStyle(color = errColor)) { append(line) }
            line.contains(" WARN ") -> withStyle(SpanStyle(color = warnColor)) { append(line) }
            else -> append(line)
        }
    }
}
