package com.xrproxy.app.model

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Раскладка имени ступени из ядра в то, что рисует экран (XR-271). Сами окна
 * и пороги живут в `xr_core::health` и проверяются там; здесь закреплён стык:
 * имена приходят строкой через мост, и молчаливое расхождение показало бы
 * спокойную мордочку на упавшем туннеле.
 */
class HealthLevelTest {

    @Test
    fun `имена ступеней ядра раскладываются в уровни экрана`() {
        assertEquals(HealthLevel.Healthy, healthLevelOf("healthy"))
        assertEquals(HealthLevel.Good, healthLevelOf("good"))
        assertEquals(HealthLevel.Watching, healthLevelOf("watching"))
        assertEquals(HealthLevel.Hurt, healthLevelOf("hurt"))
        assertEquals(HealthLevel.Critical, healthLevelOf("critical"))
    }

    @Test
    fun `незнакомое имя не пугает пользователя`() {
        // Ядро новее приложения: неизвестная ступень это повод показать
        // спокойную мордочку, а не выдумывать тревогу.
        assertEquals(HealthLevel.Healthy, healthLevelOf("новая_ступень"))
        assertEquals(HealthLevel.Healthy, healthLevelOf(""))
    }
}
