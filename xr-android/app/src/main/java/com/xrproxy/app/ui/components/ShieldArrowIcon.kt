package com.xrproxy.app.ui.components

import android.provider.Settings
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.StartOffset
import androidx.compose.animation.core.StartOffsetType
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.State
import androidx.compose.runtime.key
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import com.xrproxy.app.ui.ConnectPhase

/** Полный цикл расходящихся колец в Connected. */
private const val RADAR_CYCLE_MS = 2400

/** Цикл сходящихся колец, пока туннель поднимается. Короче радара: сборка
 *  связи должна читаться как более быстрое движение. */
private const val CONVERGE_CYCLE_MS = 1600

private const val RING_COUNT = 3

/**
 * Центральная иконка главного экрана (LLD-06, раздел 3.5): щит с вырезанной молнией,
 * нарисованный вектором прямо здесь, плюс кольца вокруг него.
 *
 * Рисуем, а не тянем растр: прежний PNG был серо-стальным глянцем в круглой
 * плашке, то есть мимо брендовой палитры (раздел 3.2), а его плашка спорила с
 * кольцом свечения поверх. Заодно иконка тянется на любой размер и следует
 * цветам темы (XR-186).
 *
 * Кольца несут состояние направлением, а не скоростью: в Connected они
 * расходятся от щита (защита работает), пока туннель поднимается, сходятся к
 * нему (связь собирается). Прежняя анимация была миганием alpha у одного
 * кольца и на глаз читалась как «что-то моргает».
 */
@Composable
fun ShieldArrowIcon(phase: ConnectPhase, modifier: Modifier = Modifier) {
    val isConnecting = phase == ConnectPhase.Preparing ||
        phase == ConnectPhase.Connecting ||
        phase == ConnectPhase.Finalizing
    val isConnected = phase == ConnectPhase.Connected
    val isDimmed = phase == ConnectPhase.Idle || phase == ConnectPhase.NeedsPermission

    // Системное «убрать анимацию» выключает и наши кольца: бесконечное
    // движение на главном экране это ровно то, что просят убрать.
    val context = LocalContext.current
    val motionEnabled = remember(context) {
        Settings.Global.getFloat(
            context.contentResolver,
            Settings.Global.ANIMATOR_DURATION_SCALE,
            1f,
        ) != 0f
    }
    val showRings = motionEnabled && (isConnected || isConnecting)

    val cycleMs = if (isConnected) RADAR_CYCLE_MS else CONVERGE_CYCLE_MS
    // key по длительности обязателен: InfiniteTransition читает animationSpec
    // только при заведении состояния и на смену одного лишь spec не реагирует.
    // Без этого цикл, пойманный при первой композиции (а это Idle), остался бы
    // навсегда и радар шёл бы со скоростью сходящихся колец.
    val ringProgress: List<State<Float>> = key(cycleMs) {
        val transition = rememberInfiniteTransition(label = "shield")
        // Кольца это одна и та же анимация со сдвигом старта, поэтому идут
        // вереницей, а не хором. FastForward, а не задержка: иначе при входе
        // на уже поднятый туннель все три кольца сначала стоят слипшимися.
        List(RING_COUNT) { i ->
            transition.animateFloat(
                initialValue = 0f,
                targetValue = 1f,
                animationSpec = infiniteRepeatable(
                    animation = tween(cycleMs, easing = LinearEasing),
                    initialStartOffset = StartOffset(
                        offsetMillis = i * cycleMs / RING_COUNT,
                        offsetType = StartOffsetType.FastForward,
                    ),
                ),
                label = "ring$i",
            )
        }
    }

    val primary = MaterialTheme.colorScheme.primary
    val cutout = MaterialTheme.colorScheme.background
    val shieldAlpha = if (isDimmed) 0.6f else 1f

    // Прозрачность вешаем на всю композицию, а не на каждый путь: иначе
    // «пробоина» молнии рисуется полупрозрачным фоном поверх полупрозрачного
    // щита и вместо дырки получается цветное пятно.
    Canvas(
        modifier = modifier
            .alpha(shieldAlpha)
            .semantics { contentDescription = "XR Proxy" },
    ) {
        if (showRings) {
            val base = size.minDimension / 2f
            val stroke = Stroke(width = 2.dp.toPx())
            ringProgress.forEach { p ->
                val scaleAndAlpha =
                    if (isConnected) radarRing(p.value) else convergeRing(p.value)
                val alpha = scaleAndAlpha.second
                if (alpha > 0.01f) {
                    drawCircle(
                        color = primary.copy(alpha = alpha),
                        radius = base * scaleAndAlpha.first,
                        style = stroke,
                    )
                }
            }
        }
        drawShield(primary = primary, cutout = cutout)
    }
}

/** Ближняя и дальняя границы колец. Дальняя подобрана так, чтобы кольцо
 *  успевало погаснуть, не дойдя до строки статуса под иконкой: Canvas не
 *  обрезает рисование по своим границам. */
private const val RING_NEAR = 0.9f
private const val RING_FAR = 1.3f

/** Расходящееся кольцо: растёт от щита наружу и гаснет к 70% пути. */
private fun radarRing(p: Float): Pair<Float, Float> {
    val scale = RING_NEAR + (RING_FAR - RING_NEAR) * p
    val alpha = (0.55f * (1f - p / 0.7f)).coerceAtLeast(0f)
    return scale to alpha
}

/** Сходящееся кольцо: приходит извне, ярче всего на трети пути. Те же
 *  границы, что у радара, только пройденные в обратную сторону. */
private fun convergeRing(p: Float): Pair<Float, Float> {
    val scale = RING_FAR - (RING_FAR - RING_NEAR) * p
    val alpha = if (p < 0.3f) 0.5f * (p / 0.3f) else 0.5f * (1f - (p - 0.3f) / 0.7f)
    return scale to alpha
}

/**
 * Щит с молнией по пропорциям из раздела 3.2: верх ровный со скруглёнными
 * углами, боковины выпуклые, низ сходится в точку. Молния вырезана цветом
 * фона. Геометрия задана в системе 100 на 120 и вписывается в иконку с
 * одинаковым масштабом по осям, иначе на квадратном холсте щит расплывался бы
 * в ширину и терял заданную разделом 3.2 пропорцию.
 */
private fun DrawScope.drawShield(primary: Color, cutout: Color) {
    val scale = minOf(size.width / 100f, size.height / 120f)
    val originX = (size.width - 100f * scale) / 2f
    val originY = (size.height - 120f * scale) / 2f
    fun px(v: Float) = originX + v * scale
    fun py(v: Float) = originY + v * scale

    val shield = Path().apply {
        moveTo(px(8f), py(14f))
        quadraticTo(px(8f), py(8f), px(14f), py(8f))
        lineTo(px(86f), py(8f))
        quadraticTo(px(92f), py(8f), px(92f), py(14f))
        lineTo(px(92f), py(54f))
        quadraticTo(px(92f), py(88f), px(50f), py(114f))
        quadraticTo(px(8f), py(88f), px(8f), py(54f))
        close()
    }
    val bolt = Path().apply {
        moveTo(px(58f), py(26f))
        lineTo(px(33f), py(68f))
        lineTo(px(47f), py(68f))
        lineTo(px(41f), py(98f))
        lineTo(px(68f), py(54f))
        lineTo(px(53f), py(54f))
        close()
    }
    drawPath(shield, color = primary)
    drawPath(bolt, color = cutout)
}
