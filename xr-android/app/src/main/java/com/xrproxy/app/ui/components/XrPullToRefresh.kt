package com.xrproxy.app.ui.components

import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier

/**
 * Единый жест обновления по всему приложению (XR-181): потягивание списка вниз
 * перезапрашивает его источник. Обёртка над [PullToRefreshBox] нужна как одна
 * точка стиля и как явная конвенция: любой экран с сетевым списком заворачивает
 * контент сюда, а не изобретает жест заново и не ограничивается кнопкой в шапке.
 *
 * [content] должен нести свой скролл-контейнер (LazyColumn или verticalScroll),
 * иначе жест некуда цеплять. Индикатор показывается, пока [refreshing] истинно,
 * поэтому тот же флаг загрузки, что гасит кнопку «Обновить», гасит и его: жест
 * и кнопка ведут к одному состоянию, дублировать спиннер не нужно.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun XrPullToRefresh(
    refreshing: Boolean,
    onRefresh: () -> Unit,
    modifier: Modifier = Modifier,
    content: @Composable () -> Unit,
) {
    PullToRefreshBox(
        isRefreshing = refreshing,
        onRefresh = onRefresh,
        modifier = modifier,
    ) {
        content()
    }
}
