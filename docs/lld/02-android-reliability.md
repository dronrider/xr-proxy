# LLD-02 — Android reliability

**Статус:** Draft
**Область:** `xr-android`, `xr-core` (не затрагивается)
**Связанные документы:** [ARCHITECTURE.md §4.6](../ARCHITECTURE.md), [ARCHITECTURE.md §7.2](../ARCHITECTURE.md)

Устраняем четыре независимых дефекта в Android-клиенте, которые обнаружены
пользователем: кнопка Connect срабатывает со второго раза, UI рассинхронизируется
с реально работающим туннелем после возврата в приложение, бадж ошибок на
вкладке Log показывает число, которому не соответствует ни одна WARN-запись в
самом журнале, foreground-уведомление плохо видно в шторке.

---

## 1. Проблемы

### 1.1 Connect срабатывает со второго раза

**Симптом.** Первый тап по кнопке Connect визуально ни к чему не приводит;
второй тап подключает.

**Текущий код.** [MainActivity.kt:35-51](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L35):

```
launcher { result -> if (RESULT_OK) viewModel.connect() }
onConnect = { val intent = prepareVpn(); if (intent != null) launcher.launch(intent) else connect() }
```

Причины:

1. **Нет немедленной визуальной реакции.** [VpnViewModel.connect()](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt#L168)
   молча возвращается, если `serverAddress.isBlank() || obfuscationKey.isBlank()`.
   Никаких сообщений пользователю.
2. **Непрозрачная ветка разрешения.** Если `VpnService.prepare()` вернул Intent
   (после reboot, после очистки данных, первое подключение), пользователь видит
   системный диалог — а если закрыл его (home, back, cancel), `result.resultCode
   != RESULT_OK` → `viewModel.connect()` НЕ вызывается, UI остаётся в состоянии
   Disconnected, никаких сообщений. Следующий тап уже попадает в ветку
   `prepare() == null` и подключается — отсюда «со второго раза».
3. **Гонка `connecting=true` ↔ launcher.** При первом тапе UI сразу не
   переключается в `Connecting` — переключение происходит уже внутри `connect()`,
   который на ветке разрешения вообще не вызывается. Пользователь не видит
   «приложение что-то делает».

### 1.2 UI рассинхронизирован с живым туннелем

**Симптом.** Пользователь нажал Connect, ушёл смотреть YouTube (трафик шёл
через VPS), вернулся в приложение — на экране «Disconnected», хотя туннель
работает.

**Причины.**

1. **ViewModel не знает о живом сервисе.** `startStatsPolling()` запускается
   только изнутри `connect()`. Если приложение уходит в фон, система пересоздаёт
   Activity/ViewModel, а свежесозданная VM стартует с
   `VpnUiState(connected=false, connecting=false)` и не запускает polling —
   потому что с её точки зрения никто не звал `connect()`.
2. **Нет источника правды на стороне сервиса.** `XrVpnService` не публикует
   своё состояние наружу. VM вынуждена догадываться по побочным каналам.
3. **`onStartCommand(null, ...)` после process death.** При рестарте по
   `START_STICKY` система вызывает `onStartCommand` с `intent=null`. Текущий
   `when (intent?.action) { ... }` молча проваливается — получаем зомби-сервис
   в foreground без живого движка.
4. **`NativeBridge.vpnService` — изменяемый singleton.** При повторном создании
   Activity ссылка на `vpnService` может быть `null`, пока сервис не пересоздан
   — а Rust-колбэк `protectSocket` в это время может не сработать.

### 1.3 Бадж ошибок не соответствует содержимому журнала

**Симптом.** На вкладке Log в бадже «8», но ни одной WARN-записи в журнале
не видно (либо журнал вообще пустой).

**Семантика, которую ожидает пользователь.** Бадж — это счётчик ОШИБОК, а не
всех строк лога. Сколько показывает бадж — столько WARN-записей должно быть
в журнале прямо сейчас. Если обнулили журнал — обнулился бадж.

**Текущий код.**
- Бадж: [MainActivity.kt:73-76](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L73) —
  `if (state.relayErrors > 0) Badge { Text("${state.relayErrors}") }`.
- Заголовок вкладки: [MainActivity.kt:222](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L222) —
  `"Log (${state.relayErrors} errors)"`.
- Содержимое: [MainActivity.kt:266](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L266) —
  `colorizeLog(state.errorLog)`.
- `errorLog` обновляется только в `refreshLog()`, а НЕ в polling loop
  ([VpnViewModel.kt:107](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt#L107)).
- Polling обновляет `recentErrors: List<String>` и `relayErrors: Long`
  ([VpnViewModel.kt:222-230](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt#L222)),
  но UI их не читает.
- В Rust [stats.rs:135-138](../../xr-core/src/stats.rs#L135): `add_relay_error`
  одновременно инкрементит счётчик `relay_errors` И добавляет WARN-строку в
  `recent_errors`. `add_log` (INFO) пишет строку без инкремента счётчика.

**Причины расхождения.**

1. **Бадж и журнал смотрят в разные поля.** Бадж читает `state.relayErrors`
   (cumulative счётчик за жизнь текущего engine-сеанса, приходит полленгом),
   журнал читает `state.errorLog` (строка, обновляется только ручным тапом
   Refresh). В результате бадж может быть «свежим», а содержимое журнала —
   устаревшим из предыдущего тапа Refresh или вообще пустым.
2. **`disconnect()` не сбрасывает ни одно из этих полей**
   ([VpnViewModel.kt:186-197](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt#L186)).
   После стопа и повторного Connect `errorLog` остаётся прежним, а
   `relayErrors` начинает считаться с нуля от нового движка — два поля с
   разным «временем жизни».
3. **`relay_errors` — это метрика за сеанс, а не видимый в UI счётчик WARN.**
   Даже если убрать расхождение с полем 1, бадж-как-cumulative-counter будет
   расходиться с видимым списком в момент, когда `append_entry` срежет старые
   записи через `entries.drain(0..50)` при переполнении: 50 строк пропало, а
   counter не уменьшился.

**Правильный инвариант.** Бадж должен равняться количеству WARN-записей,
которые прямо сейчас лежат в `recent_errors`. Тогда срабатывает естественное
правило «видишь список — видишь то же число в бадже».

**Примечание.** Rust-часть (`xr-core/src/stats.rs`) не требует изменений —
список `recent_errors` уже содержит всё нужное, расхождение возникает целиком
на уровне Kotlin-состояния.

### 1.4 Уведомления в шторке вообще нет

**Симптом.** Приложение подключено, `XrVpnService` работает, трафик идёт, —
но в шторке никакого уведомления от XR Proxy нет. Не «плохо видно», а
физически отсутствует.

**Главная причина.** **POST_NOTIFICATIONS не запрашивается runtime на API 33+.**
Разрешение объявлено в манифесте
[AndroidManifest.xml:7](../../xr-android/app/src/main/AndroidManifest.xml#L7),
но на Android 13+ манифестное объявление даёт только право запросить — без
тапа пользователя по системному диалогу разрешение считается отозванным. В
этом состоянии `startForeground(NOTIFICATION_ID, ...)` не падает (сервис
действительно стартует как foreground), но система молча не показывает само
уведомление. Именно это и наблюдается: процесс живёт, туннель работает,
шторка пустая. В `MainActivity.onCreate`
[MainActivity.kt:41-51](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L41)
никакого runtime-запроса нет.

**Сопутствующие проблемы, которые надо починить в том же заходе** (после
того как уведомление вообще начнёт появляться):

1. **Канал с `IMPORTANCE_LOW`** [XrVpnService.kt:152-161](../../xr-android/app/src/main/java/com/xrproxy/app/service/XrVpnService.kt#L152)
   — на Android 8+ это «тихое» уведомление без heads-up; на некоторых
   прошивках MIUI/HarmonyOS такие уведомления уходят в секцию «Тихие» и тоже
   малозаметны. Для постоянного «я подключён» лучше `IMPORTANCE_DEFAULT` с
   `setOnlyAlertOnce(true)`.
2. **`foregroundServiceType="specialUse"`** [AndroidManifest.xml:29](../../xr-android/app/src/main/AndroidManifest.xml#L29)
   — для Android 14+ у VPN правильный тип `systemExempted`. `specialUse`
   требует отдельной property-декларации в манифесте, которой нет, что на
   API 34+ формально некорректно и может триггерить рантайм-warning'и.
3. **Уведомление не информативное и не интерактивное** [XrVpnService.kt:163-177](../../xr-android/app/src/main/java/com/xrproxy/app/service/XrVpnService.kt#L163).
   Фиксированный текст «Connecting...» / «Connected». Нет action «Отключить»,
   нет живой статистики, нет `setCategory`, нет цвета/моно-иконки. Даже когда
   уведомление начнёт показываться, выглядеть оно будет безлико.

Порядок фиксов важен: пункт «главная причина» решает симптом «ничего нет»,
пункты 1-3 — отдельно, и без них смысла исправлять только их нет.

---

## 2. Целевое поведение

### 2.1 Connect

- Тап по кнопке **всегда** даёт визуальный отклик ≤ 1 кадра: переход в
  `Connecting` и крутилка.
- Если не заполнены поля — показываем Snackbar с понятным сообщением, состояние
  возвращается в `Idle` через 2 сек.
- Если требуется системный диалог разрешения — запускаем его; `Connecting`
  держится, пока пользователь не ответит.
- `RESULT_OK` → реальный старт движка. `RESULT_CANCELED` / любой другой код →
  Snackbar «VPN-разрешение не получено» и возврат в `Idle`.
- Повторный тап во время `Connecting` ничего не делает (onClick — no-op);
  отмена доступна отдельным путём (кнопка Cancel, она уже есть).

### 2.2 State hydration

- При старте Activity (или onResume) UI за одну итерацию догоняет реальное
  состояние туннеля.
- Источник правды — живой `XrVpnService`, к которому VM подключена через
  `bindService`. Если сервис жив — VM немедленно знает это и запускает polling
  без ожидания действий пользователя.
- Если процесс был убит и сервис рестартуется системой без intent — сервис
  чисто останавливается и не висит зомби. UI показывает Disconnected честно.

### 2.3 Лог и бадж

- Ровно один источник данных лога: `recentErrors: List<String>` — то, что
  приходит из `StatsSnapshot.recent_errors` polling'ом.
- Бадж показывает число WARN-записей в `recentErrors`, а не суммарный счётчик
  и не общий `size`. Правило: «столько, сколько WARN-строк видно в журнале».
- Заголовок вкладки Log — такой же счётчик WARN'ов.
- Рендер строк — `recentErrors.joinToString("\n")`, прямо на лету, без
  отдельного поля `errorLog` в state.
- `disconnect()` очищает `recentErrors`, чтобы не тащить журнал через сеансы.
- `refreshLog()` удаляется — больше не нужен.
- `state.relayErrors: Long` остаётся как debug-метрика (используется только
  в Debug-секции статистики), но ни бадж, ни заголовок Log его не читают.

### 2.4 Notification

Главный результат — **уведомление действительно появляется в шторке сразу
после подключения**. Всё остальное — как оно должно выглядеть.

- Запрашиваем `POST_NOTIFICATIONS` при первом запуске активити на API 33+,
  до первого подключения. Если отказано — Snackbar с объяснением, что без
  этого разрешения VPN-сессия не будет отображаться в шторке; тоннель при
  этом всё равно работает.
- Канал с `IMPORTANCE_DEFAULT` (видимый в основной секции шторки, но без
  heads-up — под `setOnlyAlertOnce(true)`).
- `foregroundServiceType="systemExempted"`.
- Уведомление показывает live-статус: «Connecting…» → «Connected • ↑2 MB ↓15 MB •
  12m», обновляется раз в секунду из того же источника, что и UI.
- Есть action «Отключить» с `PendingIntent` на `XrVpnService.ACTION_STOP`.
- `setCategory(CATEGORY_SERVICE)`, `setOnlyAlertOnce(true)`,
  `setVisibility(VISIBILITY_PUBLIC)`, `setColor(...)`, `setOngoing(true)`.
- Собственная моно-иконка `ic_notification` (белая силуэтная) в `drawable/`.

---

## 3. Дизайн решения

### 3.1 Сервис как источник правды — `bindService`

Добавляем в `XrVpnService`:

- `inner class LocalBinder : Binder() { fun service() = this@XrVpnService }`
- `override fun onBind(intent: Intent): IBinder?` — если
  `intent.action == ACTION_BIND_INTERNAL` → вернуть `LocalBinder()`. Для
  штатного VPN-биндинга (`SERVICE_INTERFACE`) — делегируем `super.onBind()`.
- `val stateFlow: StateFlow<ServiceState>` — хранит
  `ServiceState { phase: Phase, snapshot: StatsSnapshot? }`, где
  `Phase = Idle | Preparing | Connecting | Connected | Stopping | Error(msg)`.
  Обновляется из внутреннего цикла сервиса (см. §3.3).
- `fun stopFromUi()` — публичная команда стоп (вместо Intent). Используется VM
  напрямую через binder.

`VpnViewModel` держит nullable `XrVpnService?` и подписку на `stateFlow`:

- `bindService(Intent(app, XrVpnService::class.java).apply { action = ACTION_BIND_INTERNAL }, conn, BIND_AUTO_CREATE or BIND_IMPORTANT)` в `init`.
- `ServiceConnection.onServiceConnected` → сохраняем ссылку на сервис, запускаем
  `viewModelScope.launch { service.stateFlow.collect { applyServiceState(it) } }`.
- `ServiceConnection.onServiceDisconnected` → ссылка null, UI переводим в
  Disconnected только если мы это ожидали (VM инициировала стоп).
- `onCleared()` → `unbindService`.

`BIND_AUTO_CREATE` здесь критичен: если сервис не запущен, связывание НЕ
поднимет его автоматически — это верно, мы хотим видеть «Disconnected», когда
сервиса нет. Используем `BIND_AUTO_CREATE` только когда реально стартуем
туннель (`startForegroundService` → сервис создаётся → onServiceConnected).

**Важно:** `bindService` без `BIND_AUTO_CREATE` и на незапущенный сервис
возвращает `false` — значит `onServiceConnected` не придёт. Логика VM тогда:
— сразу выставить UI в `Idle`. Это и есть «сервиса нет».

`NativeBridge.vpnService` превращаем в computed:

```kotlin
@Volatile var current: XrVpnService? = null
@JvmStatic fun protectSocket(fd: Int): Boolean = current?.protect(fd) ?: false
```

`XrVpnService` в `onCreate` пишет `NativeBridge.current = this`, в `onDestroy` —
`null`. Это устраняет ссылочную путаницу.

### 3.2 Connect-флоу с единым источником состояния

Вводим enum:

```kotlin
enum class ConnectPhase { Idle, NeedsPermission, Starting, Connecting, Connected, Stopping }
```

`VpnUiState.phase: ConnectPhase` заменяет пару `connected/connecting` (их
оставляем computed-свойствами для совместимости рендера).

`VpnViewModel`:

- `private val _permissionRequest = MutableSharedFlow<Intent>(extraBufferCapacity = 1)`
- `val permissionRequest: SharedFlow<Intent> = _permissionRequest`
- `private val _messages = MutableSharedFlow<String>()` — Snackbar-сообщения.
- `fun onConnectClicked()` — единственный entry point:
  1. Если `phase != Idle` → no-op.
  2. Если `serverAddress.isBlank() || obfuscationKey.isBlank()` → `_messages.emit("Заполните сервер и ключ в Settings")`, phase остаётся Idle.
  3. `phase = Starting` (UI сразу видит крутилку).
  4. Вызываем `VpnService.prepare(app)`:
     - `null` → `actuallyStart()`.
     - non-null → `phase = NeedsPermission`, `_permissionRequest.emit(intent)`.
- `fun onPermissionResult(granted: Boolean)`:
  - `granted` → `actuallyStart()`.
  - иначе → `phase = Idle`, `_messages.emit("VPN-разрешение не получено")`.
- `private fun actuallyStart()` — тело текущего `connect()` (сборка JSON и
  `startForegroundService`), но состояние UI не меняет — ждёт update через
  binder-stateFlow.

`MainActivity`:

- Подписка: `LaunchedEffect(Unit) { viewModel.permissionRequest.collect { vpnPermissionLauncher.launch(it) } }`.
- `vpnPermissionLauncher` вызывает `viewModel.onPermissionResult(result.resultCode == RESULT_OK)` вне зависимости от кода — гарантируется ответ.
- Snackbar: `val snackbarHostState = remember { SnackbarHostState() }`,
  `LaunchedEffect(Unit) { viewModel.messages.collect { snackbarHostState.showSnackbar(it) } }`.
- `onConnect = { viewModel.onConnectClicked() }` — никакой логики в лямбде.

### 3.3 Внутренний цикл сервиса и state publishing

Сейчас `XrVpnService.startVpn` — императивная последовательность без
завершаемой корутины. Перестраиваем минимально:

- В `XrVpnService` создаётся `val scope = CoroutineScope(Dispatchers.Default + SupervisorJob())` в `onCreate`, отменяется в `onDestroy`.
- `startVpn` становится `suspend fun startVpn(configJson: String)`, вызывается
  через `scope.launch { ... }`.
- Внутри — последовательность `phase = Preparing → Connecting → Connected`,
  каждый переход публикуется в `stateFlow`.
- После успешного `nativeStart` запускается корутина, которая раз в секунду
  читает `StatsSnapshot` + `nativeGetState()` и публикует в `stateFlow`.
  Это тот же цикл, что сейчас в VM — переносим его в сервис.
- `VpnViewModel.applyServiceState` просто маппит `ServiceState` в
  `VpnUiState`. Никакого native-вызова из VM больше нет.

Обработка `onStartCommand(intent=null, ...)` — `stopSelf(); return START_NOT_STICKY`.
Это предотвращает зомби-сервис после process death. START_STICKY при штатной
работе оставляем.

### 3.4 Унификация лога

Критерий «это WARN» — строка содержит ровно ` WARN ` (с пробелами с обеих
сторон), как это делает текущий `colorizeLog`
[MainActivity.kt:283](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L283).
Никакой регэкспы: формат фиксирован в
[stats.rs:143](../../xr-core/src/stats.rs#L143) —
`format!("{} {:>4} {}", timestamp(), level, msg)`, и уровень всегда
выровнен пробелами по четыре символа. Вводим в Kotlin одно помогалочное
свойство:

```kotlin
val List<String>.warnCount: Int get() = count { it.contains(" WARN ") }
```

`VpnUiState`:

- удаляем `errorLog: String`;
- `recentErrors: List<String>` остаётся единственным источником;
- `relayErrors: Long` оставляем как debug-метрику в статистике, но UI бейдж
  его не использует.

`LogSection`:

- заголовок: `"Log (${state.recentErrors.warnCount} errors)"`;
- содержимое: `colorizeLog(state.recentErrors.joinToString("\n"))`, пустой
  текст — «No entries»;
- кнопка Refresh удаляется (данные приходят через polling сервиса);
- кнопки Copy / Share / Clear работают через тот же список, Share через
  FileProvider — как сейчас, но источник `recentErrors`, а не `errorLog`.

`MainScreen`:

- бейдж: `val warn = state.recentErrors.warnCount; if (warn > 0) Badge { Text("$warn") }`.

Поведение при ротации списка в Rust (`entries.drain(0..50)` при переполнении)
получается корректным автоматически: вместе со срезанными WARN-строками
уменьшается и бадж — что именно то, чего мы хотели.

`VpnViewModel`:

- `clearLog()` — через binder: `service?.clearLog()`, который вызывает
  `NativeBridge.nativeClearErrorLog()` и обновляет свой stateFlow.
- `disconnect()` — просто вызывает `service?.stopFromUi()`. Сброс полей UI
  происходит через binder-stateFlow при переходе сервиса в Idle.

### 3.5 Notification

`XrVpnService.createNotificationChannel`:

- `IMPORTANCE_DEFAULT`.
- `setShowBadge(false)` — бейдж на иконке приложения не дублируется.
- `lockscreenVisibility = Notification.VISIBILITY_PUBLIC`.

`buildNotification(state: ServiceState)`:

- `setCategory(Notification.CATEGORY_SERVICE)`.
- `setOngoing(true)`, `setOnlyAlertOnce(true)`.
- `setColorized(true)`, `setColor(ContextCompat.getColor(this, R.color.brand_primary))`.
- `setSmallIcon(R.drawable.ic_notification)` — новая моно-иконка (white silhouette,
  создаётся в `drawable/ic_notification.xml`, см. §4.7).
- `setContentTitle("XR Proxy")`.
- `setContentText` — в зависимости от фазы:
  - `Connecting` → «Подключение…»;
  - `Connected` → «↑{bytesUp} ↓{bytesDown} • {uptime}» (формат как в UI).
- `addAction(R.drawable.ic_stop, "Отключить", stopPendingIntent)`:
  - `stopPendingIntent = PendingIntent.getService(this, 0, Intent(this, XrVpnService::class.java).apply { action = ACTION_STOP }, FLAG_UPDATE_CURRENT or FLAG_IMMUTABLE)`.
- `setContentIntent(mainActivityPendingIntent)` — как сейчас.

Обновление текста: внутри `scope` у сервиса крутится цикл-обновляющий, который
по каждому новому `ServiceState` вызывает `nm.notify(NOTIFICATION_ID, buildNotification(state))`.
Rate-limit не нужен — `setOnlyAlertOnce(true)` подавляет лишний шум.

### 3.6 POST_NOTIFICATIONS runtime

`MainActivity.onCreate`:

- `if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(POST_NOTIFICATIONS) != GRANTED)` →
  `notificationPermissionLauncher.launch(POST_NOTIFICATIONS)`.
- `val notificationPermissionLauncher = registerForActivityResult(RequestPermission()) { granted -> if (!granted) showRationaleSnackbar() }`.
- Запрос делаем один раз при первом запуске, до первого тапа Connect.
- Если отказано — Connect всё равно работает (туннель поднимется), но сервис
  показывает Snackbar «уведомление не будет видно, включите разрешение в
  настройках системы».

### 3.7 `foregroundServiceType`

`AndroidManifest.xml`: меняем `android:foregroundServiceType="specialUse"` на
`android:foregroundServiceType="systemExempted"` для `.service.XrVpnService`.
Убираем пермишен `android.permission.FOREGROUND_SERVICE_SPECIAL_USE`, добавляем
`android.permission.FOREGROUND_SERVICE_SYSTEM_EXEMPTED`.

---

## 4. Изменения в коде

| Файл | Что меняется |
|---|---|
| [xr-android/app/src/main/java/com/xrproxy/app/service/XrVpnService.kt](../../xr-android/app/src/main/java/com/xrproxy/app/service/XrVpnService.kt) | Добавить `LocalBinder`, `ServiceState` + `stateFlow`, `stopFromUi()`, `clearLog()`. Перевести `startVpn` на корутины в `scope`. Перенести polling-цикл из VM в сервис. Обработать `onStartCommand(null)` → `stopSelf()`. Новый `buildNotification(state)` с action «Отключить», цветом, моно-иконкой, категорией. Канал → `IMPORTANCE_DEFAULT`. |
| [xr-android/app/src/main/java/com/xrproxy/app/jni/NativeBridge.kt](../../xr-android/app/src/main/java/com/xrproxy/app/jni/NativeBridge.kt) | `vpnService: VpnService?` → `current: XrVpnService?` (имя и тип); пишется из `XrVpnService.onCreate/onDestroy`, а не из `startVpn/stopVpn`. |
| [xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt) | Новое `ConnectPhase`, `phase` в `VpnUiState`, удалить `errorLog`, удалить `refreshLog()`. `bindService` в `init`, `ServiceConnection`, коллекция `stateFlow` → `applyServiceState`. `onConnectClicked()` + `onPermissionResult(granted)`. `_permissionRequest: SharedFlow<Intent>`, `_messages: SharedFlow<String>`. `disconnect()` → `service?.stopFromUi()`. `clearLog()` → `service?.clearLog()`. Убрать весь JNI-polling из VM. |
| [xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt) | Установить `SnackbarHost`. `LaunchedEffect` подписки на `permissionRequest` и `messages`. `onConnect = { viewModel.onConnectClicked() }`. `vpnPermissionLauncher` всегда вызывает `viewModel.onPermissionResult(...)`. Runtime-запрос POST_NOTIFICATIONS на API 33+. Бейдж и заголовок Log через `recentErrors.size`. Рендер лога через `recentErrors.joinToString("\n")`. Убрать `refreshLog()` со всех вызовов. |
| [xr-android/app/src/main/AndroidManifest.xml](../../xr-android/app/src/main/AndroidManifest.xml) | `foregroundServiceType="systemExempted"`, пермишен `FOREGROUND_SERVICE_SYSTEM_EXEMPTED` вместо `FOREGROUND_SERVICE_SPECIAL_USE`. |
| [xr-android/app/src/main/res/drawable/ic_notification.xml](../../xr-android/app/src/main/res/drawable/ic_notification.xml) | Новая vector-drawable, белый силуэт (щит или ключ-замок), 24dp. Используется только в статус-баре — без цветной заливки. |
| [xr-android/app/src/main/res/drawable/ic_stop.xml](../../xr-android/app/src/main/res/drawable/ic_stop.xml) | Новая vector-drawable для action «Отключить» в уведомлении (простой квадрат stop). |
| [xr-android/app/src/main/res/values/colors.xml](../../xr-android/app/src/main/res/values/colors.xml) | Добавить `brand_primary` (если файла нет — создать). Используется `setColor` в уведомлении. |

Rust/JNI не затрагиваем — `xr-core` и `xr-android-jni` остаются как есть.

---

## 5. Риски и edge-кейсы

1. **`bindService` без `BIND_AUTO_CREATE` на незапущенном сервисе.** Ожидание:
   возвращает `false`, `onServiceConnected` не придёт. VM должна в этом случае
   просто остаться в `Idle`. Тест: свежий запуск активити без запущенного
   сервиса — UI должен сразу показать Disconnected, ничего не зависнуть.
2. **Гонка `startForegroundService` → `onServiceConnected`.** Между моментом
   старта сервиса и моментом, когда binder реально доступен, могут пройти
   десятки миллисекунд. В это время VM находится в `Starting`. Если
   `onServiceConnected` пришёл позже, чем сервис уже перешёл в `Connected`,
   первый же snapshot подхватит реальное состояние — это нормально.
3. **Process death.** Foreground service с корректным `foregroundServiceType`
   обычно выживает, но OOM killer может убить всё. После этого система вызовет
   `onStartCommand(null)`; мы `stopSelf()`. Активити, если была восстановлена,
   получит `onServiceDisconnected` и перейдёт в Disconnected. Живой туннель не
   восстанавливаем автоматически — это вне скоупа LLD-02.
4. **RESULT_CANCELED от диалога разрешения.** Покрыто: `onPermissionResult(false)`
   возвращает в Idle и показывает Snackbar.
5. **Одновременный тап по Connect несколько раз.** `onConnectClicked` — no-op
   при `phase != Idle`.
6. **POST_NOTIFICATIONS отказано.** Туннель продолжает работать, но `startForeground`
   может быть приглушён на некоторых прошивках. Android всё равно требует вызов
   `startForeground` — делаем его всегда, независимо от разрешения.
7. **Миграция `foregroundServiceType` на старых API.** `systemExempted` существует
   с API 34. На API 29–33 атрибут игнорируется, но FOREGROUND_SERVICE_SYSTEM_EXEMPTED
   доступен как пермишен с API 34. Для API 29–33 он безвреден (система не знает
   и пропускает). Проверить `manifestmerger` на warning.
8. **VpnService.onBind vs LocalBinder.** Система вызывает `onBind` с
   `intent.action == SERVICE_INTERFACE` для авторизации VPN. Мы должны вернуть
   `super.onBind(intent)` для этого случая, и `LocalBinder()` — только для
   нашего `ACTION_BIND_INTERNAL`. Порядок проверки: сначала action, потом
   fallback в super.
9. **Polling в сервисе vs lifecycle.** Polling крутится, пока сервис жив.
   `stopVpn` → `scope.cancel()` → polling останавливается. Это проще и
   надёжнее текущего флага `statsPolling` в VM.

---

## 6. План проверки

Проверка ручная. Автотесты в Android-слое сознательно не заводим: бизнес-логика
живёт в Rust и покрыта `cargo test --workspace` + запретом на warnings, а
Android-слой — тонкая склейка с платформенными API (VpnService, bindService,
launcher, notifications), где unit-тест проверял бы свой же mock, а не
реальное поведение системы. Instrumentation-инфраструктура (Robolectric /
Espresso) как разовая инвестиция на четыре бага не окупается.

1. **Connect с нуля (разрешение уже было).** Запустить приложение, тап Connect
   → сразу крутилка → через ~1 с «Connected». Не должно требоваться второго тапа.
2. **Connect с запросом разрешения.** Отозвать разрешение VPN в настройках →
   запустить приложение, тап Connect → сразу крутилка → системный диалог → OK
   → «Connected». Снова тап Connect не нужен.
3. **Отмена диалога.** Тот же сценарий, но в диалоге нажать «Отмена» →
   вернуться в Idle, увидеть Snackbar «VPN-разрешение не получено».
4. **Пустые поля.** Очистить Server Address → тап Connect → Snackbar
   «Заполните сервер и ключ в Settings», состояние Idle.
5. **Фон → возврат.** Connect → свернуть приложение на 3-5 минут, убедиться в
   браузере через 2ip.ru, что трафик идёт через VPS → вернуться в приложение
   → сразу «Connected» с живой статистикой, никаких «Disconnected».
6. **Process death.** `adb shell am kill com.xrproxy.app` после Connect →
   запустить приложение. Ожидаемо: «Disconnected». Убедиться, что в шторке
   нет зомби-уведомления, и что сервис не висит (`adb shell dumpsys activity services | grep XrVpn`).
7. **Бадж лога.** Вызвать несколько relay-ошибок (отключить сервер → Connect →
   подождать 10 с). Бадж показывает число, равное числу строк в списке.
   Clear → бадж исчезает, список пустеет.
8. **Disconnect → Connect.** После сценария 7 — Disconnect → Connect → бадж
   сразу `0`, список пустой (новая сессия).
9. **Уведомление в шторке.** После Connect опустить шторку → видно «XR Proxy»
   с цветом, текстом «↑X ↓Y • T», кнопкой «Отключить». Кнопка отключает VPN,
   статус меняется и в UI, и в шторке.
10. **POST_NOTIFICATIONS запрос.** Стереть данные приложения → первый запуск →
    система спрашивает разрешение на уведомления до любого тапа Connect.
11. **Warnings/тесты.** `cargo test --workspace` должен пройти без warnings.
    `./gradlew :app:assembleDebug` без warnings.

---

## 7. Вне скоупа

- Восстановление живого туннеля после process death (требует bind-service
  reconnect-логики и хранения последнего config в prefs) — отдельный LLD, если
  понадобится.
- Улучшение иконки приложения (не уведомления) — LLD-06.
- Поиск и auto-refresh в логах, скачивание — LLD-03.
- Новые экраны анимации коннекта, стилизация статистики — LLD-06.
