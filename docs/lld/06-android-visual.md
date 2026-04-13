# LLD-06 — Android visual

**Статус:** Implemented
**Область:** `xr-android` (UI, ресурсы, Compose)
**Зависимости:** [LLD-02](02-android-reliability.md) — использует и расширяет `ConnectPhase`
**Связанные документы:** [ARCHITECTURE.md §4.6](../ARCHITECTURE.md)

Превращаем приложение из утилитарного в узнаваемое и живое: собственная
иконка (щит, пробитый стрелой-молнией), серьёзная-но-современная тёмная
палитра, анимация на этапе подключения (прогресс по фазам + моргание стрелы
в центральной иконке), перекомпоновка статистики (основной блок с акцентом
на скорость, Debug-секция за явной кнопкой).

---

## 1. Текущее состояние

- **Иконка приложения** — временный плейсхолдер: синий треугольник,
  `@drawable/ic_launcher.xml`. Не передаёт ни функцию, ни бренд.
- **Тема** — дефолтная Material3 без кастомизации палитры. Primary color —
  системный синий, всё остальное автоматически подбирается.
- **Главный экран** [MainActivity.kt:112-211](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L112):
  - На idle — неподвижная иконка Lock/LockOpen 64dp, подпись статуса, кнопка.
  - На connecting — та же иконка внутри `CircularProgressIndicator` (стандартная
    круговая крутилка), текст «Connecting...», никакого понимания «что
    сейчас происходит», сколько ещё ждать, не застряло ли.
  - Нет никакой индивидуальности, экран выглядит как окно отладки.
- **Статистика** [MainActivity.kt:174-197](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L174) —
  одна плоская колонка с восемью строками вперемешку: Upload, Download,
  Connections, Uptime, DNS, SYNs, smol recv/send, Relay errors, debug msg.
  Значимое (сколько реально качается) перемешано с диагностикой (smoltcp
  внутренние счётчики). Под заголовком `Debug` скрыта только треть строк —
  остальные в том же блоке выше.
- **Скорости (KB/s)** нет совсем — только накопленные байты.
- **Уведомление в шторке** — тема отдельно в LLD-02, визуальную часть (цвет,
  иконка) доведём здесь в соответствии с общей палитрой.

## 2. Цели

- Чёткий узнаваемый бренд: иконка, палитра, моно-иконка для статус-бара.
- На этапе подключения — осмысленный прогресс, а не молчаливая крутилка.
- Центральная иконка экрана связывается с иконкой приложения (один и тот же
  щит-и-стрела), на этапе connecting стрела пульсирует.
- Статистика: крупные карточки для значимых метрик (включая live-скорость),
  отдельный раздел Debug разложен по группам и снабжён кнопкой «Copy all».
- Минимум добавленных зависимостей. Никаких сторонних анимационных
  библиотек (Lottie, MotionLayout) — всё на Compose-примитивах.

---

## 3. Дизайн-решения

### 3.1 Цветовая палитра

**Концепция.** Тёмный navy как основа, электрический cyan как акцент.
Ассоциативно — «ночной щит с электрическим импульсом», соответствует иконке.
Палитра читается как «серьёзный инструмент», но не скучная — cyan-акцент
даёт современный tech-vibe (похожие решения: Linear, Vercel, Raycast).

Приложение всегда в тёмной теме. Светлую не поддерживаем (VPN-инструмент,
чаще всего открывается в момент, когда что-то пошло не так — тёмный фон
комфортнее; плюс это упрощает дизайн одного набора ассетов).

| Роль | Hex | Где используется |
|---|---|---|
| `background` | `#0B1220` | Экранный фон, `Scaffold` background |
| `surface` | `#121A2B` | Карточки статистики, Settings-поля |
| `surface_variant` | `#1B2540` | Toolbar, `NavigationBar`, граница карточек |
| `on_background` | `#E6EDF7` | Основной текст |
| `on_surface_variant` | `#94A3B8` | Второстепенный текст, подписи полей |
| `primary` | `#22D3EE` | Accent: активная кнопка Connect, выбранные FilterChip, статус Connected |
| `on_primary` | `#0B1220` | Текст на `primary` (инверсия для контраста) |
| `tertiary` | `#7C8BFF` | Статус Connecting, прогресс-линии |
| `error` | `#F87171` | WARN-записи лога, Disconnect-кнопка, ошибки валидации |
| `on_error` | `#0B1220` | Текст на error |
| `outline` | `#334155` | Границы, разделители |

Все цвета задаются в `values/colors.xml` как ресурсы. Compose `ColorScheme`
строится в `ui/theme/XrTheme.kt` (новый файл) из этих ресурсов, а не
автоматически от seed-color. `MainActivity.setContent { XrTheme { ... } }`.

Статус-бар и navigation-bar системы — `background` (через
`WindowCompat.setDecorFitsSystemWindows(false)` + `ColorScheme.background`
для edge-to-edge). `Window.statusBarColor` и `navigationBarColor` — тоже
`background` + светлые иконки (иконки статус-бара в тёмной теме — white).

### 3.2 Иконка приложения — дизайн-бриф

**Концепт.** Щит, пробитый стрелой-молнией. Щит = защита (VPN), стрела-молния
= скорость + «пробивает барьеры» (обход блокировок). Один элемент несёт оба
смысла без текста.

**Композиция (adaptive icon, safe zone 66dp из 108dp):**

- **Щит.** Классический heater-shield силуэт:
  - симметричный, верхний край — ровная горизонталь с лёгкими закруглениями
    углов (радиус 6% от ширины щита),
  - боковины — плавные выпуклые дуги,
  - низ — острая точка по центру с закруглением кончика (радиус 4%).
  - Пропорции: высота щита 100%, ширина в самой широкой части (верх) 80%.
  - Stroke-free, **сплошная заливка** `primary` (#22D3EE).
  - Щит занимает safe-zone целиком (66dp в adaptive icon foreground).

- **Стрела-молния.** Пересекает щит по диагонали сверху-справа вниз-влево:
  - Углы излома — зигзаг из трёх сегментов (как знак молнии ⚡), но
    вытянутый: верхний сегмент длиннее, средний короткий, нижний длиннее.
  - Пропорции: длина ~1.3× высоты щита (выступает за границы сверху-справа
    и снизу-слева), ширина молнии ~12% высоты щита.
  - **Заливка** — `background` (#0B1220), то есть молния «вырезана» из щита
    как отверстие. За пределами щита молния продолжается той же формой,
    но заливка `primary` (как бы одна непрерывная форма, где часть внутри
    щита — отверстие, а снаружи — видимая).
  - Визуальный эффект: стрела прошла сквозь щит, часть её видна снаружи,
    «пробоина» в щите точно повторяет форму проходящей молнии.

- **Background слой adaptive icon** — сплошной `background_variant` gradient
  от `#0F172A` в верхнем-левом углу к `#0B1220` в нижнем-правом (линейный,
  45°). Это даёт глубину без сложного декора.

**Файлы:**
- `res/mipmap-anydpi-v26/ic_launcher.xml` — adaptive icon с `<foreground>` и
  `<background>`, где foreground — vector с щитом и молнией, background —
  vector с градиентом.
- `res/drawable/ic_launcher_foreground.xml` — vector drawable со всей
  композицией (щит + молния) в 108dp viewport.
- `res/drawable/ic_launcher_background.xml` — vector drawable с градиентом.
- Legacy `res/mipmap-*/ic_launcher.png` (48/72/96/144/192 dp) —
  рендер через Android Studio Image Asset Studio или `vd-tool`.
  Запекается при сборке, отдельных рук-артов не делаем.

Фактическое создание SVG/vector — задача этапа реализации LLD. В LLD
описаны только параметры (форма, позиции, цвета), которых достаточно,
чтобы повторить однозначно.

### 3.3 Иконка уведомления

Android требует для статус-бара **silhouette-only** иконку (только alpha-канал,
система сама красит в белый). Значит — отдельный файл, а не тот же adaptive
icon.

`res/drawable/ic_notification.xml` — 24dp vector:
- Щит и стрела в той же композиции, но:
  - Щит — контур толщиной 1.5dp (stroke), без заливки.
  - Стрела — сплошная заливка, пересекает щит.
  - Всё в чистом чёрном (`#000000`), Android инвертирует в белый в
    статус-баре.
- `android:tint="?android:attr/textColorPrimary"` — на всякий случай для
  прошивок, которые не инвертируют автоматически.

### 3.4 Главный экран: компоновка и иерархия

Сверху вниз:

1. **Центральная иконка `ShieldArrowIcon`** — 128dp, см. §3.5, анимируется
   по фазам.
1a. **Health HUD** — 32dp (§3.5a), только в фазе Connected. По умолчанию
    под `ShieldArrowIcon`, по центру; альтернативное размещение — внутри
    самой иконки, если отдельный HUD перегружает экран.
2. **Строка статуса** — одна крупная строка (headlineMedium, 28sp):
   - Idle → «Disconnected»
   - Preparing → «Подготовка…»
   - Connecting → «Подключение…»
   - Finalizing → «Проверка маршрутов…»
   - Connected → «Подключено»
   - Stopping → «Отключение…»
   - Error(msg) → «Ошибка»
3. **Подстрока шагов** (только в фазах Preparing / Connecting / Finalizing):
   «1/3 · Подготовка» / «2/3 · Установка туннеля» / «3/3 · Проверка
   маршрутов». bodyMedium, `on_surface_variant`. В Idle и Connected этой
   строки нет.
4. **Кнопка Connect / Disconnect / Cancel** — прежняя логика из LLD-02, но
   стилизованная:
   - `Button` с фиксированной высотой 56dp, `fillMaxWidth(0.7f)`.
   - `shape = RoundedCornerShape(28.dp)` (pill).
   - В состоянии Idle — `primary` фон, `on_primary` текст.
   - В Connecting/Preparing/Finalizing — `tertiary` фон, текст «Cancel».
   - В Connected — `error` фон, текст «Disconnect».
5. **Preset-подсказка** (как сейчас, только в Idle) — «Preset: Russia»
   мелким текстом `on_surface_variant`.
6. **Карточка статистики** — §3.7, только в фазе Connected.
7. **Баннер «Configure server»** — если обязательные поля пусты (тоже из
   LLD-02), со стилизацией под палитру (`errorContainer` → `#2A1818`,
   текст `error`).

Все вертикальные отступы — кратные 8dp. Боковые поля экрана — 24dp.

### 3.5 Компонент `ShieldArrowIcon`

Новый composable в `ui/components/ShieldArrowIcon.kt`.

```kotlin
@Composable
fun ShieldArrowIcon(phase: ConnectPhase, modifier: Modifier = Modifier)
```

Внутри:
- `Canvas(modifier.size(128.dp))` — отрисовка целиком на Canvas через
  `drawPath`. Используются те же формы, что в ассете, но в `Path`-ах
  Compose, с параметрами (ширина viewport) — `Size(128f, 128f)`, чтобы
  обойтись без импорта векторного ресурса.
- **Статичный режим** (Idle, Error, Stopping, Disconnected): щит и стрела
  отрисованы один в один, без анимации. Цвета — `primary` для щита,
  `background` для «пробоины», `primary` для выступающих концов стрелы.
- **Моргание стрелы** (Preparing, Connecting, Finalizing): отдельный
  `Animatable<Float>` для alpha-канала стрелы, анимация через
  `LaunchedEffect(phase)`:
  ```
  animate(
    initialValue = 0.4f, targetValue = 1.0f,
    animationSpec = infiniteRepeatable(
      animation = tween(800, easing = FastOutSlowInEasing),
      repeatMode = RepeatMode.Reverse,
    )
  )
  ```
  Alpha применяется к цветам заливок стрелы (и «пробоины», и выступающих
  концов). Щит остаётся полностью непрозрачным.
- **Connected** — отдельная тонкая пульсация щита (glow): внешний edge щита
  раз в 2 секунды меняет `strokeWidth` с 0 до 3dp и обратно, цвет stroke —
  `primary` с alpha 0.4. Это даёт ощущение «живой защиты», без отвлечения.
  Стрела в Connected — статичная с alpha 1.0.
- **Disconnected** — чуть приглушённые цвета (alpha 0.6 у всей композиции)
  — показывает что «пока неактивно».

Всё через Compose `animate*AsState` / `rememberInfiniteTransition`. Никакой
`Lottie`, никаких сторонних зависимостей.

### 3.5a Индикатор здоровья сессии (Health HUD)

Счётчики `warn/error` в бадже и в Debug-секции отвечают на вопрос «сколько
раз оно сломалось?», но не на «как дела прямо сейчас». Пользователь хочет
периферийно видеть состояние: если туннель работает гладко — успокоительный
зелёный, если сыплются ошибки — тревожный красный, и сразу понятно, стоит
лезть в Log или нет. Без индикатора единственный способ это узнать —
переключиться на вкладку Log и вглядываться в поток строк.

Референс — HUD Doomguy'а из классического Doom: маленький портрет, который
меняет выражение в зависимости от текущего состояния здоровья и последних
полученных повреждений. Визуально считываемо за 100 мс, даёт персонажу
игры характер, хорошо работает как декоративный элемент.

**Где на экране.** Под `ShieldArrowIcon`, над строкой статуса. Размер
32dp, по центру. Не на месте иконки — `ShieldArrowIcon` остаётся главным
визуальным якорем и показывает *фазу* подключения, health HUD показывает
*состояние* уже работающего подключения. Эти две метрики ортогональны:
`ShieldArrowIcon=Connected + HealthHUD=green` ≠ `ShieldArrowIcon=Connected
+ HealthHUD=red`. Альтернативный вариант размещения (если отдельный HUD
перегружает экран) — вписать моську прямо в центр `ShieldArrowIcon`,
между щитом и стрелой. Решаем при прототипировании.

Скрыт во всех фазах кроме `Connected`: в Idle/Connecting/Stopping показывать
«здоровье» бессмысленно.

**Шкала здоровья.** Четыре состояния:

| Уровень | Цвет | Выражение | Триггер |
|---|---|---|---|
| `Healthy`   | зелёный  | улыбка / `sentiment_satisfied`        | 0 ERROR и 0 WARN в последние 30 секунд |
| `Watching`  | зелёный  | нейтральное / `sentiment_neutral`      | 0 ERROR, но ≥ 1 WARN в последние 30 секунд |
| `Hurt`      | оранжевый | гримаса / `sentiment_dissatisfied`     | ≥ 1 ERROR в окне 30 секунд, но < 5 ошибок в 5 секунд |
| `Critical`  | красный  | паника / `sentiment_very_dissatisfied` | ≥ 5 ERROR в последние 5 секунд (шквал отказов) |

Переход в более «плохое» состояние — мгновенный. Переход в более «хорошее»
— с задержкой ≥ 5 секунд, чтобы HUD не прыгал между Critical ↔ Hurt при
редких всплесках.

**Логика вычисления.** Не нужна полная история `recent_errors` — достаточно
двух скользящих счётчиков по времени. В `XrVpnService` (или в VM) держим:

```kotlin
data class HealthTracker(
    var lastSeenErrors: Long = 0,   // snapshot.relayErrors на прошлом тике
    var lastSeenWarns:  Long = 0,   // snapshot.relayWarnings на прошлом тике
    val errorBurst: ArrayDeque<Long> = ArrayDeque(),  // timestamps последних ERROR
    val warnBurst:  ArrayDeque<Long> = ArrayDeque(),  // timestamps последних WARN
)
```

На каждый snapshot (pollLoop раз в секунду):

1. `deltaErr = snapshot.relayErrors - lastSeenErrors`, `deltaWarn = snapshot.relayWarnings - lastSeenWarns`.
2. Если `deltaErr > 0`, добавить `deltaErr` меток `now` в `errorBurst`.
   Аналогично с `deltaWarn → warnBurst`.
3. Вычистить из `errorBurst` метки старше `now - 30 s`, из `warnBurst` — тоже.
4. `healthLevel = when { errorBurst в окне 5s ≥ 5 → Critical; errorBurst.isNotEmpty() → Hurt; warnBurst.isNotEmpty() → Watching; else → Healthy }`.
5. Плавный downshift: если предыдущий уровень был хуже, держим его ещё
   `min(5s, timeSinceLastBadEvent)` прежде чем отпустить в healthier.

Этот счётчик переживает ротацию `recent_errors` (которую делает smart
drain в `stats.rs`) — мы считаем дельты cumulative counters
`relay_warns`/`relay_errors`, а они монотонны и не уменьшаются при drain.

**Где держать.** Предпочтительно в `XrVpnService`, чтобы state переживал
recreate `VpnViewModel` (как и весь остальной state после LLD-02). Поле
`ServiceState.health: HealthLevel` публикуется в `stateFlow` рядом с
`snapshot`. VM только мапит в UI.

**Визуализация — v1 (без нового арта).**

Material Icons Extended (уже в deps — см. `material-icons-extended`)
содержит `Icons.Filled.SentimentVerySatisfied / SentimentSatisfied /
SentimentNeutral / SentimentDissatisfied / SentimentVeryDissatisfied`.
Берём их напрямую, tint — цвет из палитры (`accent_ok` / `warning` / `error`),
лёгкая анимация масштаба `scaleX/Y = 0.9..1.0` когда уровень ухудшается
(pulse, 300 мс).

Это не Doomguy, но даёт узнаваемый индикатор «доволен / недоволен /
паника». Реализуется за час и не блокирует релиз.

**Визуализация — v2 (Doomguy-style моська).**

Кастомная `HealthFace` composable рисует стилизованное лицо на Canvas —
квадратный помятый портрет в духе Doom HUD, но в стилистике бренда
(тёмный navy-фон, cyan-контур, короткие штрихи). 5 кадров: `Happy`,
`Calm`, `Watching`, `Hurt`, `Critical`. Каждый кадр — несколько примитивов
(круг-голова, глаза-точки, линия-рот), никакого растрового арта.

Переход между кадрами — crossfade 150 мс. Дополнительно «shake» на
переходе в Critical (`offset.x` модулируется синусоидой ±2dp, 200 мс).

v2 — это красивая часть, необязательная для первого релиза LLD-06.
Порядок работы: сначала v1 (функциональность), потом при наличии
времени/желания v2 (стилистика). v1 и v2 взаимозаменяемы — интерфейс
компонента один и тот же: `HealthFace(level: HealthLevel, modifier: Modifier)`.

**Что НЕ делаем в этом LLD.** Push-уведомление при переходе в Critical,
звук, вибрация, история уровней здоровья, графики — всё это можно
добавить позже отдельным LLD, если окажется нужным. Сейчас — только
визуальный пассивный индикатор.

### 3.6 Детализация `ConnectPhase`

LLD-02 вводит enum:
```
ConnectPhase { Idle, NeedsPermission, Starting, Connecting, Connected, Stopping }
```

Для анимации с шагами «1/3 · 2/3 · 3/3» нужны три разные суб-фазы.
Расширяем enum **в рамках LLD-06**, сохраняя совместимость с LLD-02:

```
ConnectPhase {
    Idle,
    NeedsPermission,
    Preparing,     // был Starting в LLD-02
    Connecting,    // от startForeground до nativeStart == 0
    Finalizing,    // от nativeStart success до первого Connected от native
    Connected,
    Stopping,
    Error(msg),
}
```

Маппинг на реальные события (в `XrVpnService`, который после LLD-02 — источник
правды):

| Переход | Когда | Длительность |
|---|---|---|
| Idle → Preparing | `onConnectClicked()` вызвал `actuallyStart()` | моментально |
| Preparing → Connecting | `startForeground` успешен, TUN создан, вызываем `nativeStart` | ~50-200 ms |
| Connecting → Finalizing | `nativeStart` вернул 0, tun-read/write потоки запущены | 100-1500 ms (чтение mux handshake) |
| Finalizing → Connected | Первый snapshot от `nativeGetState()` вернул `"Connected"` | 200-1500 ms |

Каждый переход публикуется в `stateFlow: StateFlow<ServiceState>` (тот же,
что из LLD-02 §3.3), VM читает и мапит на `VpnUiState.phase`.

**Fallback на случай быстрого старта.** Если реальный переход Preparing→
Connected занимает < 900 ms, пользователь увидит всего один кадр каждой
фазы и ощущение прогресса не будет осмысленным. Решение: ViewModel
применяет **минимальное время показа** каждой суб-фазы — 300 ms. Если
native движок уже в Connected, а UI ещё в Connecting, VM держит
Connecting ещё 300 ms и только потом переходит. Это не обманывает
пользователя — движок уже готов, ответ на кнопку Disconnect моментальный,
просто визуальный rollover не «мигает».

Реализация — обёртка `phaseFlow.transform { phase -> emit(phase); delay(300) }`
в VM (грубый набросок, точная форма — в рамках реализации).

### 3.7 Статистика

**Основной блок** (карточки в Grid 2×2 + одна широкая снизу):

```
┌──────────────┬──────────────┐
│ ↑ Upload     │ ↓ Download   │
│ 2.4 MB       │ 15 MB        │
├──────────────┼──────────────┤
│ ↑ 125 KB/s   │ ↓ 890 KB/s   │
│ Speed up     │ Speed down   │
├──────────────┴──────────────┤
│ ⏱ Uptime       🔗 Connections│
│ 12m 34s        17 active    │
└─────────────────────────────┘
```

- Каждая карточка — `Card(modifier.padding(4.dp))` с `containerColor = surface`,
  внутри `Column(padding = 16.dp)`.
- Первая строка карточки — иконка (16dp) + подпись (labelSmall,
  `on_surface_variant`).
- Вторая строка — значение (headlineSmall, 22sp, `on_background`).
- Скорости и Uptime/Connections обновляются каждую секунду в такт с
  polling'ом сервиса. Байтовые значения форматируются через существующий
  `formatBytes`, скорости — через новый `formatSpeed` (bytes/sec → «125 KB/s»).

**Скорость.** Вычисляется как разность между двумя последовательными
snapshot'ами: `speed_up = (bytes_up_now - bytes_up_prev) / (now - prev) в секундах`.
Логика живёт в `XrVpnService` (которая с LLD-02 — источник stats):
`ServiceState.snapshot` расширяется полями `speedUp: Long`, `speedDown: Long`
(bytes/sec, целые). Сервис хранит prev snapshot и вычисляет разность при
публикации. Никакого EMA, никакого сглаживания — сырое значение за последнюю
секунду; мигание на 1 знак в секунду не мешает.

**Debug-секция.** По умолчанию свёрнута. Появляется за кнопкой-аккордеоном
внизу экрана:

```
┌─────────────────────────────────────────┐
│ ⚙ Debug                           ⌄    │
└─────────────────────────────────────────┘
```

На тап — раскрывается `Column` с группированными метриками:

```
Network
  DNS queries           1,234
  TCP SYNs              56

smoltcp
  Recv                  12 MB
  Send                  3.4 MB

Relay
  Errors                2
  Last msg              mux open fail: connection refused
                        at 2026-04-11 12:34:56 UTC

              ┌──────────────────┐
              │  Copy all (JSON) │
              └──────────────────┘
```

- Группы — `Text(groupTitle, style = labelMedium, color = primary)` +
  `Divider` под заголовком.
- Строки внутри группы — два `Text`а в `Row(SpaceBetween)`, как текущий
  `StatRow`, но в `surface_variant`.
- Кнопка «Copy all (JSON)» — собирает из `ServiceState.snapshot` полный
  JSON со всеми debug-полями, копирует в буфер через `ClipboardManager`,
  показывает Snackbar «Скопировано».
- Состояние «раскрыто / свёрнуто» хранится в VM как `debugExpanded: Boolean`,
  не персистится в prefs — для каждой сессии заново. Большую часть времени
  пользователь не должен этого видеть.

Компонент: `ui/components/StatsCard.kt` (основной блок) + `ui/components/DebugSection.kt`
(аккордеон).

### 3.8 Notification — стилизация по палитре

Дополняет §2.4 и §3.5 LLD-02 конкретными цветами/иконкой:

- `setSmallIcon(R.drawable.ic_notification)` — из §3.3 этого LLD.
- `setColor(ContextCompat.getColor(this, R.color.primary))` — `#22D3EE`,
  используется как accent и `setColorized(true)`.
- `setContentTitle("XR Proxy")`, текст как в LLD-02.
- Action «Отключить» — иконка `ic_notification_stop` (маленький квадрат-stop,
  монохром), см. §3.9.

### 3.9a Snackbar-и

LLD-02 ввёл Snackbar как канал быстрых сообщений от `VpnViewModel` к UI:
«Заполните сервер и ключ в Settings», «VPN-разрешение не получено»,
«Сервер X:Y недоступен». Дефолтный Material3 Snackbar — тёмный прямоугольник
снизу во всю ширину, без скруглений, без иконки, без цветового разделения
по смыслу. Эмпирически он воспринимается как «что-то серьёзно сломалось»
даже когда по сути это просто валидация формы или pre-connect probe.

Целевой вид:

- Закруглённая пилюля (`RoundedCornerShape(12.dp)`) с внутренними отступами
  `16.dp` по горизонтали и `12.dp` по вертикали, не во всю ширину —
  `padding(horizontal = 16.dp)` + `widthIn(max = 400.dp)`, центровка по
  нижнему краю с отступом `24.dp` от bottom bar.
- Фон — `surface_alt` из §3.1 (чуть светлее, чем базовый surface), тонкая
  рамка `1.dp` цветом `divider`. Текст — `text_primary`, размер 14 sp,
  Medium.
- Слева 20 dp моно-иконка, отражающая категорию сообщения:
  - **info** (заполните поля, разрешение не получено) — `ic_info` (кружок
    с «i»), tint `text_secondary`;
  - **warning** (probe failed — «Сервер недоступен») — `ic_warning_round`
    (кружок с «!»), tint `warning` из палитры;
  - **error** (резерв, пока не используется) — `ic_error`, tint `error`.
  Категория приходит вместе с сообщением; см. ниже про `UiMessage`.
- Плавное появление — `AnimatedVisibility` с `slideInVertically + fadeIn`
  (250 мс), такое же исчезновение. Без «резкого выскока» стандартного
  Material3 Snackbar.
- Без action-кнопки по умолчанию. Если в будущем понадобится «Повторить» —
  добавим как текст-кнопку справа тем же `primary`-цветом.

Реализация — кастомный composable `XrSnackbar(message: UiMessage)`,
передаваемый в `Scaffold(snackbarHost = { XrSnackbarHost(snackbarHostState) })`.
`XrSnackbarHost` слушает `SnackbarHostState.currentSnackbarData` и
переводит `visuals.message` в `UiMessage` через префикс-парсинг
(`"warn:..."`, `"info:..."`) — чтобы не менять сигнатуру
`SnackbarHostState.showSnackbar` и не ломать LLD-02 API.

Альтернатива — расширить `VpnViewModel._messages: SharedFlow<String>` до
`SharedFlow<UiMessage>`, где `UiMessage(text: String, severity: Severity)`.
Это чище, но меняет контракт LLD-02 §3.2. Решаем при имплементации
LLD-06: если в тот момент ещё никто не опирается на
`_messages: SharedFlow<String>`, мигрируем на `UiMessage`; иначе делаем
префикс-парсинг как временное решение с TODO на миграцию.

В VpnViewModel тексты становятся:

- `_messages.emit("info:Заполните сервер и ключ в Settings")`;
- `_messages.emit("info:VPN-разрешение не получено")`;
- `_messages.emit("warn:Сервер ${s.serverAddress}:${s.serverPort} недоступен")`.

Префикс снимается в `XrSnackbarHost` и определяет категорию/иконку/tint.
Если сообщение пришло без префикса — дефолт `info`.

### 3.9 Дополнительные мелкие иконки

Список новых vector-ресурсов (все monochrome, без заливок-градиентов, чтобы
работали с `tint`):

| Файл | Что |
|---|---|
| `drawable/ic_upload.xml` | Стрелка вверх 16dp для карточки Upload |
| `drawable/ic_download.xml` | Стрелка вниз 16dp для Download |
| `drawable/ic_speed_up.xml` | Двойная стрелка вверх 16dp |
| `drawable/ic_speed_down.xml` | Двойная стрелка вниз 16dp |
| `drawable/ic_uptime.xml` | Часы 16dp |
| `drawable/ic_connections.xml` | Сеть/ноды 16dp |
| `drawable/ic_debug.xml` | Шестерня 20dp |
| `drawable/ic_expand.xml` | Шеврон вниз 20dp (ротация при раскрытии) |
| `drawable/ic_notification.xml` | 24dp, §3.3 |
| `drawable/ic_notification_stop.xml` | 24dp квадрат для action «Отключить» |
| `drawable/ic_info.xml` | 20dp кружок с «i» для info-Snackbar, §3.9a |
| `drawable/ic_warning_round.xml` | 20dp кружок с «!» для warning-Snackbar, §3.9a |
| `drawable/ic_error.xml` | 20dp крестик/восклик для error-Snackbar (резерв), §3.9a |

Все создаются как простые vector-drawables, можно брать готовые shapes из
Material Symbols (они имеют лицензию Apache-2.0) — это самый быстрый
способ получить согласованный набор, без ручного рисования.

---

## 4. Изменения в коде

| Файл | Что меняется |
|---|---|
| `xr-android/app/src/main/res/values/colors.xml` | Палитра из §3.1 (12 цветов + `gradient_top/bottom` для icon background). |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/theme/XrTheme.kt` (новый) | `darkColorScheme(...)` на основе ресурсов. `@Composable fun XrTheme(content: @Composable () -> Unit)`. |
| [MainActivity.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt) | `setContent { XrTheme { ... } }`. Edge-to-edge: `WindowCompat.setDecorFitsSystemWindows(window, false)`, стиль status bar — light icons. Переработать `ConnectionSection`: центральная `ShieldArrowIcon` вместо Lock/LockOpen, строка статуса + подстрока шагов, pill-кнопка, карточки статистики через `StatsCard`, `DebugSection` вместо плоской колонки метрик. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/components/ShieldArrowIcon.kt` (новый) | Canvas-композит щита и стрелы, анимация по `ConnectPhase` (§3.5). |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/components/HealthFace.kt` (новый) | Индикатор здоровья сессии (§3.5a). v1 — обёртка над `Icons.Filled.Sentiment*` с tint по уровню; v2 — кастомный Canvas-рисунок Doomguy-style. Интерфейс: `HealthFace(level: HealthLevel, modifier: Modifier)`. |
| `xr-android/app/src/main/java/com/xrproxy/app/model/HealthLevel.kt` (новый) | `enum HealthLevel { Healthy, Watching, Hurt, Critical }` + `HealthTracker` логика rolling-window, публикация в `ServiceState.health` (§3.5a). |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/components/StatsCard.kt` (новый) | Карточки Upload/Download/Speed/Uptime/Connections, сетка 2×2 + широкая снизу. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/components/DebugSection.kt` (новый) | Аккордеон с группами Network / smoltcp / Relay + кнопка Copy all. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/components/XrSnackbar.kt` (новый) | Кастомный `XrSnackbar` + `XrSnackbarHost` (§3.9a), префикс-парсинг `info:/warn:/error:`. Используется в `Scaffold(snackbarHost = ...)` вместо дефолтного. |
| [VpnViewModel.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt) | `ConnectPhase` расширен до 8 вариантов (§3.6). Обёртка `stateFlow.transform { ... delay(300) }` для минимального времени показа фаз. Поля `speedUp`, `speedDown` в `VpnUiState`. Поле `debugExpanded: Boolean`, toggle-функция. Тексты в `_messages.emit(...)` префиксуются `info:`/`warn:`/`error:` (§3.9a); либо — если контракт LLD-02 ещё можно менять — `_messages` мигрирует с `SharedFlow<String>` на `SharedFlow<UiMessage>`. |
| [XrVpnService.kt](../../xr-android/app/src/main/java/com/xrproxy/app/service/XrVpnService.kt) | В polling-цикле хранить prev snapshot, вычислять delta → `speedUp/speedDown` в `ServiceState`. Публиковать Preparing/Connecting/Finalizing переходы в правильных точках (см. таблицу §3.6). Добавить `HealthTracker`, обновлять на каждом тике, публиковать `ServiceState.health` (§3.5a). Notification — добавить `setColor` из `R.color.primary` и моно-иконку `ic_notification`. |
| `res/mipmap-anydpi-v26/ic_launcher.xml` | Adaptive icon с foreground+background, §3.2. |
| `res/drawable/ic_launcher_foreground.xml` | Vector со щитом и стрелой, §3.2. |
| `res/drawable/ic_launcher_background.xml` | Vector с градиентом, §3.2. |
| `res/mipmap-*/ic_launcher.png` (legacy) | PNG-рендеры через Image Asset Studio, 48/72/96/144/192 dp. |
| `res/drawable/ic_notification.xml` (новый) | §3.3. Можно будет использовать для LLD-02 (там она тоже указана). |
| `res/drawable/ic_upload.xml`, `ic_download.xml`, `ic_speed_up.xml`, `ic_speed_down.xml`, `ic_uptime.xml`, `ic_connections.xml`, `ic_debug.xml`, `ic_expand.xml`, `ic_notification_stop.xml`, `ic_info.xml`, `ic_warning_round.xml`, `ic_error.xml` | Новые моно-vector'ы, §3.9 и §3.9a. |
| [AndroidManifest.xml](../../xr-android/app/src/main/AndroidManifest.xml) | `android:theme` — убрать `Theme.Material.Light.NoActionBar`, заменить на `@style/Theme.XrProxy` (наш, parent — `Theme.Material3.DynamicDark.NoActionBar` либо `Theme.Material3.DarkNoActionBar`). |
| `res/values/themes.xml` (новый или дополнить) | `Theme.XrProxy` — parent `Theme.Material3.DarkNoActionBar`, статус-бар `background`, navigation-bar `background`, `windowLightStatusBar=false`. |

Итог: ~6 новых Kotlin-файлов, ~12 новых vector-drawables, один удалённый
плейсхолдер `ic_launcher.xml`, минорные правки `MainActivity`, `VpnViewModel`,
`XrVpnService`.

---

## 5. Риски и edge-кейсы

1. **Canvas vs векторный ассет.** Мы рисуем `ShieldArrowIcon` на Canvas, а
   иконку приложения — через vector drawable. Если формы разойдутся, два
   «щита» будут выглядеть по-разному. Митигация: тестовый экран (dev-flag),
   который показывает оба рядом, — визуальная проверка при каждом изменении.
   После стабилизации экран удалить.
2. **Минимальное время показа фаз 300 ms.** На слабом устройстве реальный
   переход может быть медленнее, и «обёртка» не добавит ничего — это ок.
   На быстром — пользователь увидит короткий прогресс, но не будет
   чувствовать «задержку» (300 ms × 3 = 900 ms — это быстрый ответ, никто
   не заметит задержки). Если окажется, что даже 300 ms раздражают — можно
   снизить до 200 ms без пересборки дизайна.
3. **Анимация моргания садит батарею.** `infiniteRepeatable` + Compose
   invalidation перерисовывает экран каждый кадр. На фазе Connecting это
   максимум 2-3 секунды — пренебрежимо. На фазе Connected включена только
   тонкая пульсация раз в 2 сек, между тиками Compose ничего не перерисовывает.
4. **Adaptive icon на старых прошивках.** Android < 8 использует legacy PNG.
   Android 8+ — adaptive. Важно, чтобы PNG-рендер в `mipmap-*` визуально
   совпадал с adaptive icon (щит должен занимать те же пропорции, которые
   после обрезки safe zone получаются одинаковыми). Практически — это
   значит PNG рисуется на той же канве, что foreground adaptive, без
   background gradient, с явным margin под обрезку.
5. **Цветовая слепота.** `primary` (cyan) и `error` (red) различимы при
   обоих типах цветовой слепоты (deuteranopia/protanopia), но для
   перестраховки в статусных сообщениях рядом с цветом всегда есть текст
   («Connected» / «Error»). Не полагаться на цвет как единственный
   индикатор.
6. **Material Symbols лицензия.** Apache-2.0, совместима с проектом. Sha1
   vector-файлов коммитятся в git. Источник — официальный
   `material-symbols` репозиторий Google, версия закреплена в комментарии
   к каждому файлу.
7. **Динамические цвета Material You.** Отключаем явно — используем наш
   ColorScheme, а не `dynamicDarkColorScheme(LocalContext.current)`. На
   Android 12+ пользователь не увидит свой обойный цвет в XR Proxy, и это
   сознательное решение: бренд важнее системных настроек.
8. **Скорость = 0 на коротких интервалах.** Если polling обновляется раз в
   секунду, а между тиками не прилетели пакеты, `speed = 0`. Для
   пользователя это ок: реально в момент измерения ничего не качалось. Не
   сглаживаем.

---

## 6. План проверки

Ручная (согласно правилу из LLD-02 §6 — автотестов в Android-слое не
заводим):

1. **Собрать и установить APK.** Иконка в лаунчере — щит со стрелой,
   узнаваемая издалека. На Android 8+ — adaptive, на 7- — legacy PNG,
   оба варианта проверить в эмуляторе.
2. **Тёмная тема.** После установки открыть приложение — экран полностью
   тёмный (#0B1220), нет белых flash'ей при старте, статус-бар в цвет фона.
3. **Idle state.** Центральная иконка `ShieldArrowIcon` нарисована
   статично, щит + стрела, слегка приглушённые. Строка статуса
   «Disconnected». Кнопка pill-формы, primary accent.
4. **Connect press.** Тап Connect:
   - Сразу переход в Preparing: иконка — стрела начинает моргать.
   - Через ~50-200 ms — Connecting: та же анимация, но подстрока
     «2/3 · Установка туннеля».
   - Через ~300-1500 ms — Finalizing: «3/3 · Проверка маршрутов».
   - Через ещё 200-1500 ms — Connected: стрела перестаёт моргать, щит
     начинает тонко пульсировать раз в 2 сек.
   Минимум по 300 ms на каждую фазу даже при быстром старте.
5. **Статистика в Connected.** 4 карточки (Upload / Download / Speed↑ /
   Speed↓) + одна широкая (Uptime · Connections). Значения обновляются раз
   в секунду, скорости считаются как delta.
6. **Скорость.** Запустить скачивание большого файла — speed_down должен
   расти до реальной пропускной. После остановки загрузки — упасть в 0 за
   секунду.
7. **Debug аккордеон.** Свёрнут по умолчанию. Тап → плавно раскрывается,
   видны три группы (Network, smoltcp, Relay), кнопка Copy all. Тап на
   Copy all → Snackbar «Скопировано», в буфере — JSON со всеми debug-полями.
8. **Уведомление в шторке.** Шторка → видно XR Proxy в цвете `primary`
   (cyan tint), моно-иконка щита, текст с байтами и uptime, action
   «Отключить». Тап на action → UI переходит в Stopping → Idle.
9. **Повторный Connect → Disconnect → Connect.** Состояние анимации и
   цвета корректно обнуляются между сеансами.
10. **Цветовая слепота (ручной smoke).** Включить в Android симулятор
    deuteranopia (Developer options → Simulate color space → Deuteranomaly).
    Убедиться, что Connected / Error / Connecting всё ещё различимы по
    тексту, а не только по цвету.
11. **Warnings.** `./gradlew :app:assembleDebug` без warnings. При попытке
    использовать удалённый `ic_launcher.xml` — error на этапе сборки.

---

## 7. Вне скоупа

- **Светлая тема.** Не поддерживаем принципиально, см. §3.1.
- **Динамические цвета (Material You).** Отключены в пользу бренда.
- **Lottie / сложные анимации** — не тащим зависимости ради одной иконки.
- **Splash screen с анимацией.** На старте Android 12+ используется
  системный SplashScreen API, который подхватит иконку приложения и цвет
  `windowBackground` автоматически. Отдельная анимация splash — отдельный
  LLD, если вообще понадобится.
- **Живое обновление цен/курса/etc.** Статистика показывает только то, что
  есть в `StatsSnapshot`, никаких внешних источников.
- **Notification с графиком байтов.** Один текст на одну строку,
  `BigTextStyle` не используем.
- **Анимация перехода между экранами.** Оставляем дефолтное Compose-поведение
  `AnimatedContent` только внутри `ConnectionSection` (при смене фазы).
  Полный `NavHost` с transitions — если появится LLD по навигации.
- **Шрифты.** Material3 default (Roboto). Если захочется Inter или JetBrains
  Mono для debug-секции — отдельный минорный ответвлённый LLD, но, скорее
  всего, не нужно.
