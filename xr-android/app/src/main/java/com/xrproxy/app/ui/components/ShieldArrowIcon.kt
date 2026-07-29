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
import androidx.compose.ui.graphics.Brush
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
        drawShield(cutout = cutout)
    }
}

/** Ближняя и дальняя границы колец. Дальняя подобрана так, чтобы кольцо
 *  успевало погаснуть, не дойдя до строки статуса под иконкой: Canvas не
 *  обрезает рисование по своим границам. */
private const val RING_NEAR = 0.9f
private const val RING_FAR = 1.22f

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

/** Оттенки щита: свет сверху, тень снизу. Держим рядом с геометрией, потому
 *  что это материал самой иконки, а не роль палитры; те же значения зашиты в
 *  `ic_launcher_foreground.xml`. Канон в LLD-06, раздел 3.2. */
private val ShieldLight = Color(0xFF53E6F8)
private val ShieldMid = Color(0xFF22D3EE)
private val ShieldDeep = Color(0xFF0E9FBF)
private val ShieldGloss = Color(0x38FFFFFF)

/**
 * Щит с молнией по пропорциям из раздела 3.2: верх мягко скруглён, боковины
 * выпуклые, низ круглый, а не остриём. Круглое дно повторяет кривизну колец
 * вокруг иконки, поэтому «остриё против круга» больше не спорит.
 *
 * Геометрия задана в квадратной системе 120 на 120 и вписывается с одинаковым
 * масштабом по осям. Фигура в этой системе стоит по оптическому центру, а не
 * по геометрическому: центр масс щита ниже середины бокса, и без поправки
 * иконка казалась бы съехавшей вверх.
 *
 * Объём даёт вертикальный градиент со светом сверху плюс блик по плечам,
 * никакого глянца и растровых теней (XR-233).
 */
private fun DrawScope.drawShield(cutout: Color) {
    val scale = minOf(size.width, size.height) / 120f
    val originX = (size.width - 120f * scale) / 2f
    val originY = (size.height - 120f * scale) / 2f
    fun px(v: Float) = originX + v * scale
    fun py(v: Float) = originY + v * scale

    val shield = Path().apply {
        moveTo(px(60f), py(14f))
        cubicTo(px(74f), py(22f), px(88f), py(26f), px(98f), py(27f))
        lineTo(px(98f), py(56f))
        cubicTo(px(98f), py(82f), px(83f), py(100f), px(60f), py(108f))
        cubicTo(px(37f), py(100f), px(22f), py(82f), px(22f), py(56f))
        lineTo(px(22f), py(27f))
        cubicTo(px(32f), py(26f), px(46f), py(22f), px(60f), py(14f))
        close()
    }
    // Блик по плечам: та же дуга, что у верхнего края, отведённая вниз.
    val gloss = Path().apply {
        moveTo(px(60f), py(19f))
        cubicTo(px(72f), py(26f), px(84f), py(29.5f), px(93f), py(30.7f))
        lineTo(px(93f), py(42f))
        cubicTo(px(82f), py(40f), px(70f), py(36f), px(60f), py(30f))
        cubicTo(px(50f), py(36f), px(38f), py(40f), px(27f), py(42f))
        lineTo(px(27f), py(30.7f))
        cubicTo(px(36f), py(29.5f), px(48f), py(26f), px(60f), py(19f))
        close()
    }
    val bolt = Path().apply {
        moveTo(px(67f), py(30f))
        lineTo(px(42f), py(66f))
        lineTo(px(55f), py(66f))
        lineTo(px(51f), py(90f))
        lineTo(px(78f), py(52f))
        lineTo(px(63f), py(52f))
        close()
    }
    drawPath(
        shield,
        brush = Brush.verticalGradient(
            0f to ShieldLight,
            0.55f to ShieldMid,
            1f to ShieldDeep,
            startY = py(14f),
            endY = py(108f),
        ),
    )
    drawPath(gloss, color = ShieldGloss)
    drawPath(bolt, color = cutout)
}
