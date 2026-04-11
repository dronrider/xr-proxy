# LLD-04 — Onboarding: QR / URI / одноразовые инвайты

**Статус:** Draft
**Область:** `xr-android` (новые экраны, deep link, QR-сканер, применение инвайта)
**Зависимости:** [LLD-01](01-control-plane.md) — использует `Invite`, `InvitePayload`, эндпоинт `/api/v1/invite/:token`. [LLD-02](02-android-reliability.md) — работает поверх `VpnViewModel` / `XrVpnService` binder-архитектуры. [LLD-06](06-android-visual.md) — использует `XrTheme` и общие компоненты.
**Связанные документы:** [ARCHITECTURE.md §4.6](../ARCHITECTURE.md)

Убираем ручной ввод server-address / port / ключа / salt. Подключение
нового устройства — за 10 секунд: тап «Сканировать QR» → отсканировать код,
показанный админом → экран подтверждения → Apply → Connect.

---

## 1. Текущее состояние

- При первом запуске Android-приложения пользователь попадает в Settings
  [MainActivity.kt:295](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L295)
  с шестью пустыми полями: server address, port, obfuscation key (base64),
  modifier, salt, routing preset.
- Все поля вводятся вручную. Ошибка в одном поле — Connect молча ничего
  не делает (после LLD-02 — Snackbar «заполните поля», но всё равно ручная
  работа).
- Нет ни QR-сканера, ни поддержки deep link, ни paste URI.
- Admin UI в LLD-01 умеет выдавать одноразовые инвайты и рендерить QR, но
  на Android-клиенте нет ничего, что с ними делать.

## 2. Целевое поведение

### 2.1 Приход пользователя в приложение

Три входных точки, все приводят к одинаковому финалу — настроенное
подключение, готовое к `Connect`:

1. **Тап по иконке приложения, ничего не настроено.** → Welcome-экран с
   тремя кнопками: «Сканировать QR», «Вставить ссылку», «Настроить
   вручную».
2. **Тап по HTTPS-ссылке инвайта** в любом приложении (Telegram, SMS,
   Email, браузер). Android показывает chooser «Открыть в…», пользователь
   выбирает XR Proxy → сразу экран подтверждения инвайта, минуя Welcome.
3. **Существующая настройка.** Приложение открывается в обычном главном
   экране — Welcome больше не показывается.

### 2.2 Экран подтверждения инвайта

После получения `InvitePayload` пользователь видит экран с:

- Сервером (`server_address:server_port`)
- Именем пресета (`preset`)
- Комментарием от администратора (`comment` из `Invite`)
- TTL: «действителен ещё 58 минут»
- Иконкой «щит со стрелой» из LLD-06, anim=Idle
- Двумя кнопками: **Отмена** и **Применить**

Кнопка Apply сохраняет конфиг в prefs, инициализирует `[hub]` секцию
(url + preset) и переходит на главный экран. Connect пользователь
запускает сам.

### 2.3 Форматы, которые мы принимаем

**Основной** — HTTPS URL от xr-hub:
```
https://<xr-hub-host>/invite/<token>
```

**Альтернативный** (для pro-пользователей, гарантированный deep link) —
кастомная схема:
```
xr://invite/<token>?hub=<xr-hub-host>
```

QR-коды, которые раздаёт `xr-hub`, всегда содержат HTTPS URL — это даёт
корректное поведение при отсутствии установленного клиента (пользователь
получает осмысленную страницу в браузере). Кастомная схема — только для
ручных случаев.

### 2.4 Безопасность

- **Одноразовость** — серверная (LLD-01 §3.3), клиенту доверять нечему:
  второй GET по тому же токену получит 410 Gone, приложение покажет
  ошибку.
- **Короткий TTL** — серверный (LLD-01 §3.1), дефолт 24 часа.
- **HTTPS с доверенным CA** — rustls в `reqwest` (использует webpki-roots),
  никаких кастомных trust anchor'ов. Если сертификат невалиден — отказ.
- **TOFU для подписи пресета** — при первом успешном Apply клиент делает
  `GET /api/v1/public-key`, сохраняет как доверенный якорь в prefs. Если
  при следующих обновлениях пресета подпись не сойдётся — отказ. Этот
  механизм описан в LLD-01 §3.5, здесь только фиксируем точку сохранения:
  момент Apply инвайта.
- **Экран подтверждения** — явное согласие пользователя перед перезаписью
  существующих настроек. Без подтверждения нельзя «тихо» подменить чей-то
  VPN одной открытой ссылкой.

### 2.5 Что происходит с существующими настройками

Если пользователь принимает инвайт, а у него уже был настроен другой
сервер — новые значения **перезаписывают** старые. Предупреждение на
экране подтверждения: «Существующие настройки подключения будут
заменены». Никакого мультипрофиля в первой версии (он — отдельный LLD).

---

## 3. Дизайн решения

### 3.1 Формат URL и парсинг

`xr-proto` получает новый модуль `invite_url.rs` с чистыми функциями:

```rust
pub enum InviteLink {
    Https { hub_url: String, token: String }, // https://host/invite/TOKEN
    Custom { hub_url: String, token: String }, // xr://invite/TOKEN?hub=HOST
}

pub fn parse_invite_link(s: &str) -> Result<InviteLink, InviteLinkError>;
pub fn build_https_url(hub_url: &str, token: &str) -> String;
```

Парсинг — через `url` crate (маленькая, уже транзитивно в дереве через
`reqwest`). Тесты — на круг-трип и на отказ от невалидных форм.

Валидация:
- хост не пустой, не IP-литерал вида `10.*`/`127.*` (только чтобы случайно
  не принять чей-то LAN-хаб за production);
- токен — `[A-Za-z0-9_-]{22}` (base64url 16 байт без padding, см.
  [LLD-01 §3.1](01-control-plane.md));
- путь `/invite/<token>` или `xr://invite/<token>` — никаких query
  параметров кроме `hub=`;
- схема — `https` или `xr` (строго).

Эти функции работают и в Android (через `xr-android-jni`, см. §3.6), и в
будущем desktop-клиенте, если появится.

### 3.2 Google Code Scanner

**Зависимость:**
```kotlin
implementation("com.google.android.gms:play-services-code-scanner:16.1.0")
```

**Использование** (`ui/onboarding/QrScanner.kt`, новый):

```kotlin
suspend fun scanInviteQr(activity: Activity): String? =
    suspendCoroutine { cont ->
        val options = GmsBarcodeScannerOptions.Builder()
            .setBarcodeFormats(Barcode.FORMAT_QR_CODE)
            .enableAutoZoom()
            .build()
        GmsBarcodeScanning.getClient(activity, options)
            .startScan()
            .addOnSuccessListener { cont.resume(it.rawValue) }
            .addOnFailureListener { cont.resume(null) }
            .addOnCanceledListener { cont.resume(null) }
    }
```

- `CAMERA` permission **не добавляем** в манифест — Google Code Scanner
  использует system UI, не требует permission у встраивающего приложения.
- Модуль устанавливается по требованию через Play Services. Если не
  установлен — `startScan` сам инициирует скачивание, показывает
  стандартный UI.
- Результат — `rawValue: String?`. Если пользователь нажал back или
  отказался — `null`, UI переходит обратно на Welcome с Snackbar
  «Сканирование отменено».

Обработчик UI (во Welcome):

```kotlin
onClick = {
    scope.launch {
        val raw = scanInviteQr(activity) ?: return@launch
        viewModel.onInviteLinkReceived(raw)
    }
}
```

### 3.3 Welcome-экран

Новый composable `ui/onboarding/WelcomeScreen.kt`. Показывается, если в
prefs **нет** ни `server_address`, ни `hub.url` (значит никакой инвайт
ранее не применялся и ручная настройка не проводилась).

Компоновка:

- Сверху — `ShieldArrowIcon(phase = Idle)` 128dp (компонент из LLD-06).
- Под иконкой — заголовок «XR Proxy», подзаголовок «Безопасное подключение
  к интернету».
- Три крупные кнопки pill-формы (высота 56dp, `fillMaxWidth(0.85f)`),
  расстояние 12dp между ними:
  1. **«Сканировать QR-код»** — primary (cyan). Иконка `ic_qr_scan` 20dp
     слева от текста.
  2. **«Вставить ссылку»** — outlined. Иконка `ic_paste` 20dp.
  3. **«Настроить вручную»** — text button, без иконки, мелкий текст
     `on_surface_variant` — это «escape hatch», не привлекаем внимания.
- Внизу версия приложения, как в `ConnectionSection` сейчас.

**Вставить ссылку** — открывает `Dialog` с полем `OutlinedTextField`
(multiline=false), placeholder'ом `https://hub.example.com/invite/...`,
кнопкой «Вставить из буфера» (заполняет поле `clipboardManager.getText()`)
и кнопкой «Применить». На Apply — те же действия, что при сканировании.

**Настроить вручную** — переход в Settings (существующий экран, после
LLD-02 он остаётся). При сохранении настроек — возврат на главный.

### 3.4 Deep link

`AndroidManifest.xml` для `MainActivity` получает два intent-filter:

```xml
<!-- HTTPS deep link: общий, без autoVerify. -->
<intent-filter>
    <action android:name="android.intent.action.VIEW" />
    <category android:name="android.intent.category.DEFAULT" />
    <category android:name="android.intent.category.BROWSABLE" />
    <data android:scheme="https" />
    <data android:pathPrefix="/invite/" />
</intent-filter>

<!-- Кастомная схема для гарантированного перехвата. -->
<intent-filter>
    <action android:name="android.intent.action.VIEW" />
    <category android:name="android.intent.category.DEFAULT" />
    <category android:name="android.intent.category.BROWSABLE" />
    <data android:scheme="xr" android:host="invite" />
</intent-filter>
```

Первый intent-filter перехватывает **любые** `https://*/invite/*`, не
только конкретный хост. Это намеренно — хаб self-hosted, домены у
каждого пользователя свои. Android на первом нажатии покажет chooser
(«Открыть в…»), пользователь выберет XR Proxy, дальше Android запомнит.
`autoVerify` не используем — он требует `assetlinks.json` на известном
хосте, которого у self-hosted варианта нет.

**Побочный эффект:** chooser будет появляться для любой ссылки с
`/invite/` в пути. Фильтрация по known-hub домену — невозможна без
autoVerify и без списка доменов в manifest. Принимаем это как известный
минус self-hosted модели.

`MainActivity.onCreate` / `onNewIntent`:

```kotlin
override fun onCreate(savedInstanceState: Bundle?) {
    super.onCreate(...)
    handleIntent(intent)
    setContent { ... }
}

override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    handleIntent(intent)
}

private fun handleIntent(intent: Intent?) {
    val data = intent?.data ?: return
    if (intent.action != Intent.ACTION_VIEW) return
    viewModel.onInviteLinkReceived(data.toString())
}
```

### 3.5 Экран подтверждения инвайта

Новый composable `ui/onboarding/InviteConfirmScreen.kt`. Отображается,
когда в VM в состоянии `onboardingState = OnboardingState.ConfirmInvite(payload, invite_meta)`.

Компоновка:

```
┌─────────────────────────────────────────┐
│         [ShieldArrowIcon, 96dp]         │
│                                         │
│       Настройка подключения             │
│                                         │
│   Сервер       vpn.example.com:8443    │
│   Пресет       Russia                  │
│   От кого      admin@example.com       │
│   Действителен ещё 58 минут             │
│                                         │
│   ⚠ Существующие настройки будут       │
│     заменены.                           │
│                                         │
│   [ Отмена ]        [ Применить ]      │
└─────────────────────────────────────────┘
```

- Значения — из `InvitePayload` (LLD-01 §3.1).
- «От кого» — `comment` из `Invite`. Если пусто — строка скрывается.
- TTL countdown обновляется раз в минуту, через `LaunchedEffect + delay(60_000)`,
  читая `expires_at` относительно `server_time` из payload (см. LLD-01 §5.6 —
  используем серверное время, не локальное, чтобы не зависеть от часов
  устройства).
- Предупреждение «настройки будут заменены» — только если в prefs уже
  есть `server_address` или `hub.url`. При первом онбординге — скрыто.
- **Отмена** — возврат на Welcome (если открыто оттуда) или закрытие
  приложения (если открыто по deep link).
- **Применить** — кнопка блокируется с маленьким спиннером на 2-3 секунды,
  пока выполняется Apply (§3.6).

### 3.6 Применение инвайта

Логика живёт в `VpnViewModel.applyInvite(payload: InvitePayload)`:

```
1. Сохранить туннельные параметры в prefs:
   - server_address, server_port
   - obfuscation_key, modifier, salt
2. Сохранить hub-секцию:
   - hub.url = payload.hub_url
   - hub.preset = payload.preset
3. TOFU public key:
   - GET {hub_url}/api/v1/public-key, timeout 3 сек
   - При 200 → сохранить как trusted_public_key (base64)
   - При 404 → подписи не используются, поле остаётся пустым
   - При сетевой ошибке → WARN в лог, trusted_public_key пустой;
     пользователь увидит Snackbar с советом «хаб недоступен, подпись
     пресета не будет проверяться»
4. Первый фетч пресета:
   - GET {hub_url}/api/v1/presets/{preset}, timeout 5 сек
   - При успехе → кэшировать на диск (xr-core/presets/<name>.json)
   - При ошибке → WARN в лог, пресет подтянется при первом Connect
5. Обновить VpnUiState:
   - serverAddress, serverPort, obfuscationKey, salt, routingPreset ← payload
   - onboardingState ← Completed
6. Навигация: экран подтверждения → главный экран в состоянии Idle
```

Шаги 3 и 4 не блокируют UI больше чем на 3+5=8 секунд; если хаб тупит —
выходим с Snackbar и всё равно завершаем Apply. Пресет тогда скачается
при первом реальном Connect.

**Шаги 3 и 4 вызываются через `xr-android-jni`**, потому что reqwest
живёт в Rust:
- Новая JNI-функция `nativeFetchPresetAndKey(hub_url: String, preset: String, timeout_ms: u64) -> String` → возвращает JSON-результат `{ public_key: String?, preset: Preset?, errors: [String] }`. Однократный вызов, без side effects, без участия в активном движке. Реализация — `tokio::runtime::Runtime::new()` + два `reqwest::get` с таймаутом. Runtime создаётся на один вызов, чтобы не мешать основному движку.

### 3.7 Обработка ошибок при применении

| Ошибка | Сообщение пользователю | Действие |
|---|---|---|
| URL невалиден | «Неправильный формат приглашения» | Snackbar, остаётся на Welcome |
| Сеть недоступна (DNS / TCP) | «Хаб недоступен. Проверьте интернет» | Snackbar, остаётся на Welcome |
| Сертификат xr-hub невалиден | «Небезопасное соединение с хабом» | Snackbar, остаётся на Welcome |
| 404 на `/invite/:token` | «Приглашение не найдено» | Snackbar |
| 410 Gone (consumed/expired) | «Приглашение уже использовано или истекло» | Snackbar |
| 4xx/5xx прочие | «Ошибка хаба: HTTP <code>» | Snackbar |
| Подтверждение пользователя — Отмена | (нет сообщения) | Возврат |

Все сообщения — через `VpnViewModel.messages: SharedFlow<String>` (ввели в
LLD-02). UI рендерит в `SnackbarHost` из `Scaffold`.

### 3.8 OnboardingState в ViewModel

Новый sealed interface в VM:

```kotlin
sealed interface OnboardingState {
    object NotStarted : OnboardingState              // первый запуск, ничего не настроено
    object ShowingWelcome : OnboardingState           // Welcome открыт
    object Loading : OnboardingState                  // fetching invite
    data class ConfirmInvite(
        val payload: InvitePayload,
        val meta: InviteMeta,
    ) : OnboardingState
    data class Error(val message: String) : OnboardingState
    object Completed : OnboardingState                // в prefs валидные настройки, главный экран
}
```

Стартовое значение вычисляется в `VpnViewModel.init`:

```kotlin
private fun initialOnboardingState(): OnboardingState =
    if (prefs.getString("server_address", "").isNullOrBlank() &&
        prefs.getString("hub_url", "").isNullOrBlank())
        OnboardingState.ShowingWelcome
    else
        OnboardingState.Completed
```

`MainActivity` рендерит соответствующий экран через `when (onboardingState)`.
`Scaffold` с нижним TabBar виден только в `Completed`.

### 3.9 Интеграция с xr-hub Admin UI

LLD-01 §3.8 уже описывает диалог создания инвайта с QR и кнопкой Copy.
В рамках LLD-04 добавляем в тот же диалог:

- **Deeplink-версия URL** (та, что в QR) — единый формат HTTPS.
- **QR рендерится через `qrcode` npm-пакет**, uploaded content = HTTPS URL.
- **Кнопка «Поделиться»** — на мобильных браузерах вызывает `navigator.share({url})`, на десктопе копирует.
- **Countdown TTL** — таймер раз в секунду, после истечения — плашка «Истёк», QR затемняется.

Эти изменения — минорная правка Vue-компонента `InvitesList.vue` /
`InviteCreatedDialog.vue` в LLD-01. Не требует серверных изменений.

---

## 4. Изменения в коде

| Файл | Что меняется |
|---|---|
| `xr-proto/src/invite_url.rs` (новый) | `InviteLink` enum, `parse_invite_link`, `build_https_url`. Тесты. |
| `xr-proto/src/lib.rs` | `pub mod invite_url;` |
| [xr-android-jni/src/lib.rs](../../xr-android-jni/src/lib.rs) | Новая JNI-функция `nativeFetchPresetAndKey(hub_url, preset, timeout_ms) -> String`. Использует разовый `tokio::runtime::Runtime`. Возвращает JSON-результат. |
| [xr-android-jni/src/lib.rs](../../xr-android-jni/src/lib.rs) | Новая JNI-функция `nativeParseInviteLink(raw: String) -> String`. Под капотом — `xr_proto::invite_url::parse_invite_link`, возвращает JSON с полями `hub_url` / `token` или `error`. |
| [NativeBridge.kt](../../xr-android/app/src/main/java/com/xrproxy/app/jni/NativeBridge.kt) | Объявить `external fun nativeFetchPresetAndKey(...)` и `external fun nativeParseInviteLink(...)`. |
| [AndroidManifest.xml](../../xr-android/app/src/main/AndroidManifest.xml) | Два новых `<intent-filter>` на `MainActivity` (§3.4). |
| [build.gradle.kts](../../xr-android/app/build.gradle.kts) | `implementation("com.google.android.gms:play-services-code-scanner:16.1.0")`. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/onboarding/WelcomeScreen.kt` (новый) | §3.3. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/onboarding/InviteConfirmScreen.kt` (новый) | §3.5. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/onboarding/QrScanner.kt` (новый) | §3.2, suspend-обёртка над Google Code Scanner. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/onboarding/PasteLinkDialog.kt` (новый) | §3.3, диалог «Вставить ссылку». |
| [VpnViewModel.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt) | `onboardingState: StateFlow<OnboardingState>`. `fun onInviteLinkReceived(raw: String)`. `fun onInviteConfirmed()`. `fun onInviteCancelled()`. `fun applyInvite(payload)`. Инициализация `onboardingState` в `init { }` (§3.8). |
| [MainActivity.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt) | `handleIntent(intent)` в `onCreate` + `onNewIntent`. Рендер: `when (onboardingState)` → Welcome / Loading / Confirm / MainScaffold. |
| `res/drawable/ic_qr_scan.xml` (новый) | Material Symbol «qr_code_scanner» 20dp. |
| `res/drawable/ic_paste.xml` (новый) | Material Symbol «content_paste» 20dp. |
| [configs/client.toml](../../configs/client.toml) | Комментарий-пример с секцией `[hub]`, как онбординг заполняет её (на OpenWRT после приём инвайта в отдельном LLD — сейчас только пример в документации). |

Rust-ядро (`xr-core`) не меняется — применение инвайта живёт в
`xr-android-jni` как отдельный вызов, не через `VpnEngine`.

---

## 5. Риски и edge-кейсы

1. **Chooser на каждой `/invite/`-ссылке.** Без `autoVerify` Android показывает
   chooser при первом нажатии любой HTTPS-ссылки, путь которой начинается с
   `/invite/`. Для обычного пользователя это разовая мелочь («всегда
   открывать в XR Proxy»). Для гика — ожидаемое поведение self-hosted.
2. **Пользователь кликает на инвайт, а приложение не установлено.** Браузер
   откроет страницу `https://hub/invite/TOKEN` — это обычный HTTPS
   эндпоинт xr-hub, который возвращает JSON. Плохой UX. Решение в LLD-01
   §3.3: эндпоинт детектит `Accept: text/html` и вместо JSON отдаёт
   landing-HTML с инструкцией «установите XR Proxy и откройте ссылку
   снова». Уточняю это здесь: LLD-01 §3.3 → эндпоинт `/api/v1/invite/:token`
   остаётся JSON-only (для программного использования), а параллельный
   эндпоинт `/invite/:token` (без `/api/v1/`) — HTML-landing с
   JavaScript-кодом, который пытается запустить deep link и, если не
   сработало за 2 секунды, показывает ссылку на APK. **Это изменение
   скоупа LLD-01** — добавляется в §3.3 как отдельный публичный эндпоинт.
3. **Двойное применение одного инвайта.** Пользователь отсканировал QR,
   получил `ConfirmInvite`, а между получением `InvitePayload` и
   нажатием Apply кто-то другой тоже отсканировал (если инвайт
   одноразовый, у второго будет 410). У первого всё ещё валидный payload
   — и Apply работает. Это корректно: consume происходит на сервере на
   шаге fetch, а не Apply.
4. **Истечение TTL во время экрана подтверждения.** Пользователь открыл
   screen, отвлёкся, вернулся через час. `expires_at` уже в прошлом.
   Решение: `LaunchedEffect` раз в минуту сравнивает текущее серверное
   время (`server_time` из payload + прошедшее локальное время) с
   `expires_at`. Если истекло — переводит в `OnboardingState.Error("Приглашение истекло")`.
5. **Приложение открыто по deep link, пользователь отказывается (Cancel).**
   Приложение закрывается (`finish()`) — настроек всё равно нет, главный
   экран показать нечего. При следующем открытии — Welcome.
6. **Deep link при наличии настроек.** Существующий пользователь открывает
   новую ссылку. Показывается `ConfirmInvite` с предупреждением
   «существующие настройки будут заменены». Кнопка Cancel возвращает на
   главный экран.
7. **Apply во время активного подключения.** Не разрешаем: если
   `ConnectPhase != Idle`, кнопка Apply отключена с tooltip «Сначала
   отключите VPN». Иначе менять server_address под живым туннелем —
   источник багов, которые дороже чем эта ручная проверка.
8. **xr-hub доступен только через VPN.** Если админ хостит xr-hub за VPN
   (т.е. его нельзя достать до подключения), onboarding невозможен. Это
   прямо сказано в LLD-01 §2.4: хаб должен быть reachable из non-VPN
   сети. В LLD-04 добавляем явное сообщение пользователю при таймауте
   fetch'а.
9. **Code Scanner недоступен на устройстве без Play Services.** На
   устройствах без GMS (Huawei, китайские ROMы) `GmsBarcodeScanning`
   недоступен. Fallback: при ошибке запуска scanner'а показываем Snackbar
   «Сканер QR недоступен на устройстве, используйте “Вставить ссылку”».
   Это ручная альтернатива, которая работает всегда.
10. **Обработка `xr://` deep link на устройстве без приложения.** Ничего
    не произойдёт (Android просто не знает, кому адресовать), пользователь
    увидит «нет приложения для этого действия». Это ожидаемо — кастомная
    схема и нужна именно тем, кто уже установил приложение.
11. **Копия QR на скриншоте.** Одноразовость защищает: первый, кто
    отсканировал, «сжёг» токен. Но если админ сам отсканировал для теста
    и потом отправил QR пользователю — пользователь получит 410. Решение
    — на стороне админа: не сканировать свои же QR. Предупреждение в
    Admin UI при создании («этот токен одноразовый»). Уточняю это как
    правку к тексту в LLD-01 §3.8.

---

## 6. План проверки

Ручная (автотесты в Android-слое не заводим — см. правило LLD-02 §6).

1. **Первый запуск без настроек.** Стираем данные приложения, запускаем.
   Видим Welcome с тремя кнопками и ShieldArrowIcon.
2. **Сканирование QR.** В xr-hub Admin UI создаём инвайт, показываем QR
   на другом устройстве / мониторе. В приложении «Сканировать QR» →
   системный сканер → наводим камеру → экран подтверждения с правильными
   данными.
3. **Apply.** «Применить» → через 1-3 секунды главный экран, статус
   Disconnected, поля в Settings заполнены из инвайта.
4. **Первый Connect после Apply.** Тап Connect → соединение поднимается,
   в логе `xr-core` видно `preset russia fetched v1`. Трафик на youtube
   идёт через VPS (через 2ip.ru в браузере).
5. **Вставить ссылку из буфера.** Admin UI → копируем URL в буфер →
   отправляем на Android → в приложении «Вставить ссылку» → «Вставить из
   буфера» → «Применить» → тот же результат, что при сканировании.
6. **Deep link из Telegram.** Отправляем ссылку себе в Saved Messages →
   тап → Android chooser → выбираем XR Proxy → экран подтверждения.
7. **Deep link на чистое устройство.** Приложение удалено, открываем
   ссылку в Chrome → видим landing-HTML от xr-hub с инструкцией (§5.2).
8. **Одноразовый инвайт: повторное сканирование.** Первый раз — Apply
   ок. Второй раз тот же QR → Snackbar «Приглашение уже использовано».
9. **Истечение TTL.** Создаём инвайт с TTL=1 минута, ждём 70 сек, открываем
   → Snackbar «Приглашение истекло».
10. **Невалидный URL.** В диалоге «Вставить ссылку» вводим мусор →
    Snackbar «Неправильный формат приглашения».
11. **Хаб недоступен.** Отключаем xr-hub, сканируем свежий QR → через 5
    сек Snackbar «Хаб недоступен».
12. **Сертификат невалиден.** Ставим self-signed сертификат без добавления
    в CA → Snackbar «Небезопасное соединение с хабом».
13. **Apply при активном VPN.** Connect → во время Connected возвращаемся
    к деплоям, сканируем новый QR → на экране подтверждения кнопка Apply
    отключена, tooltip «Сначала отключите VPN».
14. **Cancel из deep link.** Открыли приложение по deep link (первый
    запуск), на экране подтверждения нажали Отмена → приложение закрылось.
    Повторный запуск показывает Welcome, не Confirm.
15. **Устройство без Play Services.** На Huawei-эмуляторе или MicroG:
    «Сканировать QR» → Snackbar «Сканер недоступен, используйте Вставить
    ссылку».
16. **TOFU public key.** После Apply в prefs сохранён `trusted_public_key`.
    На хабе с отключёнными подписями — поле пустое, WARN в логе.
17. **Warnings.** `cargo test --workspace` и `./gradlew :app:assembleDebug`
    — без warnings.

---

## 7. Вне скоупа

- **Мультипрофиль** (несколько сохранённых подключений, переключение).
  Первая версия — ровно один набор настроек. Отдельный LLD, если
  понадобится.
- **Inline config в URL fragment** (`#fallback=base64(...)`) для
  недостижимого хаба — отложено. Если блокировка xr-hub станет реальной
  проблемой — добавляется отдельной правкой в формат URL.
- **OpenWRT-клиент с инвайтами.** В OpenWRT другой путь онбординга
  (SSH + конфиг), Android-специфика здесь. Если захочется `xr-client`
  тоже уметь инвайты — отдельный LLD (понадобится CLI-флаг
  `--apply-invite URL` и логика, аналогичная `applyInvite`).
- **Pinning хаба по хэшу ключа в URL.** TOFU с HTTPS считаем достаточным.
  Если модель угроз изменится — добавим `#pk=<sha256>` к URL.
- **Отзыв доверия к хабу** (стёртый `trusted_public_key`). Делается через
  ручной сброс настроек приложения. Отдельный UI для «поменять хаб» —
  когда появится мультипрофиль.
- **Анимация перехода Welcome → Confirm → Main.** Используется
  `AnimatedContent` из Compose с дефолтными переходами — fade, без
  slide. Если захочется красоты — LLD-06 расширение.
