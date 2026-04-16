# LLD-08 — Android: мультисерверная модель

**Статус:** Draft
**Область:** `xr-android` — модель хранения подключений, переключатель
серверов на главном экране, вкладка Servers (CRUD), миграция со
single-server prefs, интеграция с onboarding'ом из LLD-04.
**Зависимости:** [LLD-02](02-android-reliability.md) — `VpnViewModel` /
`XrVpnService` binder-архитектура. [LLD-04](04-onboarding-qr-uri.md) —
приём инвайтов; теперь Apply добавляет новый сервер вместо перезаписи.
[LLD-06](06-android-visual.md) — `XrTheme`, компоненты, раскладка
главного экрана.
**Связанные документы:** [ARCHITECTURE.md §4.6](../ARCHITECTURE.md)

Превращаем приложение из «одно подключение, настроенное глобально» в
«список подключений, одно активно». Три пути добавления (ручной / QR /
вставленная ссылка), редактирование, удаление, выбор активного на
главном экране.

---

## 1. Текущее состояние

- `VpnUiState` хранит подключение как плоский набор полей
  ([VpnViewModel.kt:68-82](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt#L68))
  — server_address / port / obfuscation_key / modifier / salt / routing_*
  + `hub_url` / `hub_preset` / `trusted_public_key` (LLD-04).
- Настройки пишутся в `SharedPreferences "xr_proxy"` каждое отдельным
  ключом.
- Вкладка **Settings** редактирует ровно эти поля
  ([MainActivity.kt:SettingsSection](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt)).
- На главном экране под кнопкой Connect пишется `Preset: Russia` /
  `Proxy all traffic` / `Custom rules` — подсказка активного пресета.
- Apply инвайта (LLD-04 §3.6) перезаписывает эти же поля и TOFU'ит
  `trusted_public_key`.
- Переключить подключение можно только полной перезаписью настроек
  (через новый инвайт или ручным вводом). Несколько серверов — никак.

## 2. Целевое поведение

### 2.1 Модель «профиля сервера»

Подключение — самостоятельная сущность `ServerProfile` c полями, которые
раньше были плоскими в prefs:

| Поле | Источник |
|---|---|
| `id: String` | UUID, генерируется при создании |
| `name: String` | лейбл от пользователя (display-имя) |
| `serverAddress` / `serverPort` | ручной ввод или `InvitePayload` |
| `obfuscationKey` / `modifier` / `salt` | ручной ввод или `InvitePayload` |
| `routingPreset` (russia / proxy_all / custom) | по умолчанию russia; в custom добавляются `customDomains` / `customIpRanges` |
| `hubUrl` / `hubPreset` / `trustedPublicKey` | заполняется только для серверов, добавленных из инвайта; для ручных — пустые |
| `createdAt: String` | RFC 3339 |
| `source: Manual \| Invite` | как был добавлен; влияет на UI (у Invite-сервера нельзя ручно вытащить `obfuscation_key`, его и так знает хаб) |

Пользователь видит и редактирует все поля; `id` и `source` — read-only.

### 2.2 Имя сервера по умолчанию

- **Manual** — пользователь вводит `name` в том же диалоге, что и
  сервер/ключ. Пустое — автогенерация из `host`
  (`"vpn.example.com:8443"`).
- **Invite** — `name = invite.comment`; если пуст, берём `host` из
  `hub_url` (`"hub.example.com"`). Пользователь может переименовать в
  Edit.

### 2.3 Активный сервер

- В prefs хранится `active_server_id: String?`. На старте
  `VpnViewModel.init`:
  - если серверов ноль → `onboardingState = ShowingWelcome`
    (как в LLD-04 §3.3);
  - если активный id указывает на живой профиль → он активен;
  - если активный id мёртвый (профиль удалён) → активируем первый из
    списка и пишем id обратно;
  - если серверов ≥ 1, но active_id пуст → активируем первый.
- Connect всегда идёт к активному серверу. Переключение активного при
  уже запущенном туннеле → Disconnect, смена active_id, пользователь
  сам нажмёт Connect заново.

### 2.4 Главный экран: перекомпоновка

Переносим `HealthFace` (смайл-индикатор из LLD-06 §3.5a) из
отдельной строки под щитом **в строку статус-текста** — «Подключено 😊»
(inline, одна строка). Это экономит ~32dp по вертикали и даёт место
server-switcher'у без увеличения общей высоты колонки.

Раскладка `ConnectionSection` сверху вниз:

```
ShieldArrowIcon 128dp
  Spacer(16)
"Подключено 😊"          ← status + HealthFace inline
  Spacer(4)
"2/3 · Установка туннеля" ← substep (только в Connecting-фазах)
  Spacer(4)
v0.1.0-xxxxx              ← version (только в Idle)
  Spacer(16)
[🔀 Home VPS · Russia ▾]  ← ServerSwitcherChip
  Spacer(12)
[     Connect     ]        ← кнопка
  Spacer(24)
...StatsGrid...            ← только в Connected
```

**Server switcher chip:**

- **Вид:** pill-chip, высота 36dp, outline (tertiary/primary),
  центрированный, `fillMaxWidth(0.7f)` — та же ширина, что кнопка
  Connect, выглядит как связанная пара. Содержимое:
  `[icon_swap_horiz] · <name> · <preset-hint> · ▾`. Если имя длинное —
  `maxLines=1` + ellipsis.
- **Взаимодействие:** тап → `ModalBottomSheet` со списком всех
  серверов (радио-выбор по active_id) и кнопкой `+ Добавить сервер`
  внизу. Выбор другого сервера:
  - если туннель **Idle** — меняем `active_server_id`, закрываем sheet;
  - если **Connected/Connecting** — алерт «Подключение будет разорвано.
    Переключиться?» → `viewModel.disconnect()` → смена active → sheet
    закрывается → пользователь жмёт Connect уже с новым сервером.
- **Состояние `Connecting/Connected`:** chip видим, но disabled —
  показываем Snackbar «сначала отключите VPN» при тапе. Доступен
  только когда `phase == Idle`.
- **Один сервер:** chip всё равно показывается (pill с именем), но тап
  открывает тот же sheet — пользователь видит, что может добавить ещё.

### 2.5 Вкладка Servers (вместо Settings)

Переименование в нижней навигации: **Settings → Servers** (иконка
остаётся `Icons.Default.Settings` или заменяется на
`Icons.Default.Dns`; выбор — при реализации, главное консистентность).

Контент вкладки:

- Заголовок «Серверы» + число в скобках.
- Список карточек (см. §3.4), активный — с подсветкой primary (cyan
  outline по периметру).
- Снизу — primary-кнопка `+ Добавить сервер` (pill, full-width
  отступы 24dp).

Удалён текущий раздел «Settings» (все его поля теперь живут внутри
ServerEdit, §3.6).

### 2.6 CRUD флоу

**Добавить сервер.** Кнопка `+ Добавить` (в Servers-вкладке или в
bottom-sheet переключателя) открывает `AddServerDialog` с тремя
действиями:

1. **Сканировать QR** — делегирует в уже существующий `scanInviteQr`
   (LLD-04 §3.2). Результат → `onInviteLinkReceived(raw)` (LLD-04
   §3.5). Экран `ConfirmInvite` поверх текущего, после Apply —
   добавлен новый сервер и **становится активным** (см. §3.9).
2. **Вставить ссылку** — открывает `PasteLinkDialog` (LLD-04 §3.3), тот
   же pipeline.
3. **Заполнить вручную** — открывает `ServerEditScreen` в режиме
   `Mode.Create` с пустыми полями.

**Редактирование.** В карточке → меню `⋮` → «Изменить» → открывается
`ServerEditScreen` в режиме `Mode.Edit(id)`. Редактирование доступно и
для активного сервера при живом туннеле (см. §2.7).

**Удаление.** Меню `⋮` → «Удалить» → `AlertDialog`:
- Если удаляется **неактивный** сервер: «Удалить сервер X?» → Delete.
- Если **активный при Connected/Connecting**: «Соединение будет
  разорвано, сервер будет удалён.» → Disconnect → Delete → выбор
  нового активного по правилам §2.3 (первый из оставшихся, либо
  `null` и переход в `ShowingWelcome` если список опустел).
- Если **активный при Idle**: «Сервер X активен. Удалить?» → Delete,
  выбор нового активного.

**Выбор активным.** В меню карточки `⋮` → «Сделать активным». Та же
логика, что в bottom-sheet из §2.4.

### 2.7 Редактирование активного сервера

Редактируем активный **в любой фазе** — движку всё равно, он видит
только то, что мы передали в `nativeStart`. После сохранения:

- Если `phase == Idle` — просто обновляем профиль и `VpnUiState`.
- Если `phase == Connected | Connecting` и изменились любые поля,
  которые влияют на подключение (server_address/port, obfuscation_*,
  modifier, salt, hub_url, hub_preset, routingPreset,
  customDomains/Ranges, trustedPublicKey) — показываем Snackbar
  «Применяю новые настройки…», делаем Disconnect → Connect (цикл
  через `onConnectClicked` после `stopFromUi`).
- Если изменилось только `name` — никакого реконнекта, обновляем
  карточку.

### 2.8 Deep link при наличии серверов

Invite-link → `ConfirmInvite` поверх главного экрана. Предупреждение из
LLD-04 §3.5 «Существующие настройки будут заменены» **убираем** —
теперь мы не заменяем, а добавляем. Текст на экране: «Добавить этот
сервер и сделать активным». После Apply:

- Проверяем дубликат `(server_address, server_port)` в существующих
  серверах.
  - Если есть — показываем `AlertDialog` «Такой сервер уже есть в
    списке (имя: X). Добавить как отдельный?» → Add anyway / Cancel.
  - Если нет — добавляем сразу.
- Новый сервер сохраняется, становится активным.

### 2.9 Миграция со старой модели

Однократная, в `VpnViewModel.init` **до** вычисления onboardingState:

1. Если prefs ключа `servers` нет (или пуст), читаем старые плоские
   ключи `server_address`, `server_port`, …, `hub_url`, `hub_preset`,
   `trusted_public_key`, `routing_preset`, `custom_domains`,
   `custom_ip_ranges`.
2. Если `server_address` не пуст — конструируем один
   `ServerProfile(id = UUID, name = serverAddress, source = Manual)`
   (или `source = Invite`, если есть `hub_url`), сохраняем
   `servers = [profile]`, `active_server_id = profile.id`.
3. Если `server_address` пуст — `servers = []`, `active_server_id =
   null` (Welcome как обычно).
4. Старые плоские ключи **не удаляем** — оставляем как read-only
   legacy-артефакт. Код больше к ним не обращается после миграции;
   новая запись всегда через `ServerRepository`. Это убирает риск
   потери данных, если кто-то откатит APK на прошлую версию — он
   увидит старую настройку.

---

## 3. Дизайн решения

### 3.1 Данные и репозиторий

Новый файл `data/ServerProfile.kt`:

```kotlin
enum class ServerSource { Manual, Invite }

data class ServerProfile(
    val id: String,
    val name: String,
    val serverAddress: String,
    val serverPort: Int,
    val obfuscationKey: String,
    val modifier: String = "positional_xor_rotate",
    val salt: Long = 0xDEADBEEFL,
    val routingPreset: String = "russia",
    val customDomains: String = "",
    val customIpRanges: String = "",
    val hubUrl: String = "",
    val hubPreset: String = "",
    val trustedPublicKey: String = "",
    val createdAt: String,
    val source: ServerSource,
)
```

Новый файл `data/ServerRepository.kt` — тонкий фасад над
SharedPreferences, сериализация через `org.json` (избегаем лишней
зависимости на moshi/kotlinx.serialization ради одной модели):

```kotlin
class ServerRepository(private val prefs: SharedPreferences) {
    private val _servers = MutableStateFlow<List<ServerProfile>>(emptyList())
    private val _activeId = MutableStateFlow<String?>(null)

    val servers: StateFlow<List<ServerProfile>> = _servers
    val activeId: StateFlow<String?> = _activeId

    init { load(); migrateFromFlatPrefsIfNeeded() }

    fun activeServer(): ServerProfile? =
        _servers.value.firstOrNull { it.id == _activeId.value }

    fun upsert(profile: ServerProfile) { ... save ... }
    fun delete(id: String) { ... save, reassign active ... }
    fun setActive(id: String) { ... save ... }
}
```

Ключи prefs:
- `servers` — JSON-строка `[{...}, {...}]`.
- `active_server_id` — строка UUID или отсутствует.

Репозиторий НЕ занимается коннектом — только состоянием серверов.
Реакция на смену active живёт в `VpnViewModel`.

### 3.2 VpnViewModel: что меняется

- `VpnUiState` теряет плоские поля подключения (`serverAddress`,
  `serverPort`, `obfuscationKey`, `modifier`, `salt`, `routingPreset`,
  `customDomains`, `customIpRanges`, `hubUrl`, `hubPreset`,
  `trustedPublicKey`). Вместо них — `activeServer: ServerProfile?`.
- Текущие методы `updateServerAddress`/… удаляются — редактирование
  уехало в `ServerEditScreen`. `saveSettings()` → `upsertServer(profile)`.
- Новый набор:
  ```kotlin
  val servers: StateFlow<List<ServerProfile>>
  val activeId: StateFlow<String?>
  fun selectServer(id: String)        // §2.4 switcher
  fun upsertServer(profile: ServerProfile)
  fun deleteServer(id: String)
  fun onServerEditSaved(profile)      // + reconnect если active + connected (§2.7)
  ```
- `buildConfigJson` читает из `activeServer`, а не из `_uiState.value`.
- `applyInvite` (LLD-04 §3.6) **переписываем**: вместо записи плоских
  prefs создаёт `ServerProfile(source = Invite)` и вызывает
  `upsertServer` + `selectServer`. См. §3.9.

### 3.3 Главный экран: chip + ModalBottomSheet

В `ConnectionSection` заменяем текущий блок preset-hint
([MainActivity.kt:282-290](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L282))
на `ServerSwitcherChip`:

```kotlin
@Composable
fun ServerSwitcherChip(
    activeName: String,
    presetLabel: String,
    enabled: Boolean,
    onClick: () -> Unit,
)
```

- Pill высотой 36dp, outline `MaterialTheme.colorScheme.outline`
  (в disabled — `outlineVariant`).
- Текст: `"$activeName · $presetLabel"` (точка-разделитель). Если
  `presetLabel` пустой — только имя.
- Иконка слева (swap_horiz 16dp), иконка справа (arrow_drop_down 16dp).

`ModalBottomSheet` — отдельный composable `ServerSwitcherSheet`:

- Заголовок «Серверы».
- `LazyColumn` с `ServerRow(profile, isActive, onClick)`:
  - RadioButton слева, название крупным, под ним — `host:port`
    меньшим серым.
- В подвале кнопка `+ Добавить сервер` (OutlinedButton,
  `fillMaxWidth`). Тап → закрывает sheet, открывает AddServerDialog.

### 3.4 Вкладка Servers: список карточек

Заменяет текущий `SettingsSection`. Компоненты:

```
┌───────────────────────────────────┐
│ ○ Home VPS                      ⋮ │   ← radio (active: •)
│   1.2.3.4:8443 · Russia           │
│   Invite · 3 дня назад            │   ← source + createdAt (короткое)
└───────────────────────────────────┘
```

- Карточка — `OutlinedCard`, padding 12dp.
- Active: `border = BorderStroke(2.dp, primary)`.
- Меню `⋮` (DropdownMenu):
  - «Сделать активным» (скрыт у активного)
  - «Изменить»
  - «Удалить»

Тап по самой карточке (не по меню) — тоже «Сделать активным» как
shortcut. Радио-кнопка визуализирует состояние, но неклiкабельна как
самостоятельный контрол — не путаем.

Под списком — `Button("+ Добавить сервер")`, full-width.

### 3.5 AddServerDialog

`AlertDialog` с тремя pill-кнопками в колонку:

- **Сканировать QR** — иконка `ic_qr_scan`, primary-filled.
- **Вставить ссылку** — иконка `ic_paste`, outlined.
- **Заполнить вручную** — text button.

Первые две повторно используют pipeline LLD-04. Третья открывает
`ServerEditScreen(Mode.Create)`.

### 3.6 ServerEditScreen

Отдельный полноэкранный composable (не Dialog — полей много,
diaglog становится неудобным на маленьких экранах).

- TopAppBar: заголовок «Новый сервер» / «Изменить сервер»,
  стрелка-назад.
- Поля (все `OutlinedTextField`):
  - Имя (обязательно; `placeholder` = «Home VPS»)
  - Server address (`1.2.3.4` или `vpn.example.com`)
  - Port (`8443`, numeric keyboard)
  - Obfuscation key (base64, `PasswordVisualTransformation` с toggle)
  - Salt (numeric)
  - `FilterChip`-ряд: routing preset (russia / proxy_all / custom)
  - Если `custom` выбран — раскрываются два multiline-поля:
    `customDomains`, `customIpRanges` (как сейчас в Settings).
  - Hub (read-only, collapsible): `hubUrl`, `hubPreset`,
    `trustedPublicKey` — показываем только если они непустые
    (= source = Invite). Редактировать нельзя, только просмотр с
    кнопкой «Отвязать от хаба» (убирает hub_* поля и переводит
    source в Manual; trusted key стирается).
- Внизу: `Button("Сохранить")`, `OutlinedButton("Отмена")`.

Валидация перед сохранением:
- name непустой;
- server_address непустой, port ∈ [1, 65535];
- obfuscation_key непустой.

Ошибки — подсветка поля + `supportingText`.

### 3.7 Delete flow

`AlertDialog` с текстом по §2.6. Positive — «Удалить» (error color),
negative — «Отмена». Если active + connected, внутри handle'а сначала
`disconnect()`, потом `deleteServer(id)`.

### 3.8 Set-active flow

Вызов `selectServer(id)`:
1. Если `phase != Idle` и `id != currentActive` — алерт «Подключение
   будет разорвано». Positive → `disconnect()` затем `repo.setActive(id)`.
2. Если `phase == Idle` — сразу `repo.setActive(id)`.

Connect после смены — пользователь нажимает сам (не автоматически);
не хотим неожиданных переподключений.

### 3.9 Интеграция с LLD-04 onboarding

`VpnViewModel.onInviteConfirmed()` переписывается:

```
1. nativeApplyInvite(...) → payload + public_key + preset_cached
2. Построить ServerProfile:
   - id = UUID
   - name = invite.comment | hubUrl-host | server_address
   - source = Invite
   - hubUrl/hubPreset/trustedPublicKey из результата
   - serverAddress/port/key/salt/modifier из payload
   - routingPreset = payload.preset (если совпадает с одним из
     known-преsetов, иначе "russia" по умолчанию)
3. Проверить дубликат по (serverAddress, serverPort):
   - если есть → показать AlertDialog (§2.8), на "Add anyway" → upsert + setActive
   - если нет → upsert + setActive
4. onboardingState → Completed
```

Экран `ConfirmInvite` из LLD-04 §3.5 подчищаем: убираем блок «будут
заменены» (теперь неактуален), меняем текст кнопки на «Добавить»
вместо «Применить». Поведение TTL-countdown и обработки `status` —
без изменений.

### 3.10 Миграция — детали

Метод `ServerRepository.migrateFromFlatPrefsIfNeeded()`:

```kotlin
private fun migrateFromFlatPrefsIfNeeded() {
    if (prefs.contains("servers")) return
    val addr = prefs.getString("server_address", "") ?: ""
    if (addr.isBlank()) { save(); return }  // сохранить пустой список

    val hubUrl = prefs.getString("hub_url", "") ?: ""
    val profile = ServerProfile(
        id = UUID.randomUUID().toString(),
        name = if (hubUrl.isNotBlank()) hostOf(hubUrl) else addr,
        serverAddress = addr,
        serverPort = (prefs.getString("server_port", "8443")
            ?.toIntOrNull() ?: 8443),
        ...
        source = if (hubUrl.isNotBlank()) ServerSource.Invite else ServerSource.Manual,
        createdAt = OffsetDateTime.now().toString(),
    )
    _servers.value = listOf(profile)
    _activeId.value = profile.id
    save()
}
```

Миграция выполняется внутри `init`-блока `ServerRepository`, до
создания StateFlow-подписчиков — к моменту, когда `VpnViewModel`
читает `repo.activeId`, значение уже корректное.

---

## 4. Изменения в коде

| Файл | Что меняется |
|---|---|
| `xr-android/app/src/main/java/com/xrproxy/app/data/ServerProfile.kt` (новый) | `data class ServerProfile`, `enum ServerSource`. |
| `xr-android/app/src/main/java/com/xrproxy/app/data/ServerRepository.kt` (новый) | JSON сериализация через `org.json`, StateFlow-экспозиция, миграция из flat-prefs. |
| [VpnViewModel.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt) | Убрать плоские поля из `VpnUiState` + их сеттеры. Добавить `servers`, `activeId`, `activeServer`, `selectServer`, `upsertServer`, `deleteServer`, `onServerEditSaved`. `buildConfigJson` читает из `activeServer`. `applyInvite` → создание ServerProfile. `onConnectClicked` валидирует `activeServer != null`. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/servers/ServerSwitcherChip.kt` (новый) | Composable из §3.3. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/servers/ServerSwitcherSheet.kt` (новый) | `ModalBottomSheet` из §3.3. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/servers/ServersSection.kt` (новый) | Список карточек + «+ Добавить сервер». Заменяет SettingsSection в Nav. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/servers/AddServerDialog.kt` (новый) | §3.5. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/servers/ServerEditScreen.kt` (новый) | §3.6, полноэкранный. |
| [MainActivity.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt) | `ConnectionSection`: убрать отдельный `HealthFace` composable — смайл теперь inline в строке статуса (§2.4); убрать preset-hint → `ServerSwitcherChip`; выровнять Spacer'ы по новой раскладке. Навигация: 3-я вкладка «Servers» рендерит `ServersSection`. Удалить `SettingsSection`. Обработка навигации в `ServerEditScreen` (ещё один стейт на уровне MainScreen: `editingServer: ServerEditMode?`). Поддержка back-gesture — закрывает edit-экран. |
| `xr-android/app/src/main/res/values/strings.xml` (если используется; в репо сейчас встроенные строки) | — не трогаем, всё inline в Compose (как сейчас). |
| [LLD-04 §3.5 / §3.6 / §4 / §5 / §7](04-onboarding-qr-uri.md) | Снять «будут заменены», поправить applyInvite flow — добавление нового сервера, не overwrite. |
| [ARCHITECTURE.md §4.6](../ARCHITECTURE.md) | Новый под-раздел про `ServerRepository`, миграцию, обновлённую модель состояния (`activeServer` вместо плоских полей). |

---

## 5. Риски и edge-кейсы

1. **Повреждённый JSON в `servers`.** Если кто-то руками стёр
   половину строки, `JSONArray(data)` кинет `JSONException`. Падать
   нельзя — ловим, логируем WARN через существующий `recent_errors`,
   выставляем пустой список и запускаем миграцию с flat-prefs как
   fallback.
2. **Потеря active_id при удалении.** Уже описано в §2.3:
   переназначаем на первый из оставшихся; если опустело — Welcome.
   Тест: сценарий 13 в §6.
3. **Переименование вкладки в навигации.** Старые пользователи
   увидят другое имя и иконку — это ожидаемо. Никакого confirm-флоу
   не надо, изменение одноразовое.
4. **Переключение серверов под нагрузкой.** Между `disconnect()` и
   `setActive(newId)` есть окно, где `phase == Stopping` и тап по
   переключателю может подвисать. Решение: кнопка chip и элементы
   sheet выставляют `enabled = (phase == Idle || phase == Connected)`,
   в промежуточных фазах — disabled, Snackbar «дождитесь перехода в
   Idle».
5. **Одинаковый `host:port` у двух серверов.** Разрешаем (§2.8
   с диалогом-подтверждением), потому что user-case: один и тот же
   сервер с двумя разными пресетами — зачёт. Просто предупреждаем,
   чтобы не создать дубликат по невнимательности.
6. **Дубликат `id`.** UUID-collision не считаем. Если теоретически
   случится — `upsert` заменит существующий. Не проблема.
7. **Удаление активного при Connected.** Порядок важен:
   - сначала `disconnect()` (async, через binder, ждём Idle);
   - только затем `deleteServer(id)` и `setActive(...)`.
   Неверный порядок → движок получит `nativeStop` с уже удалённым
   профилем. Реализуется через `viewModelScope.launch { disconnect();
   waitForIdle(); delete() }`. `waitForIdle()` — collect на
   `uiState.phase == Idle`, `first()`.
8. **Редактирование активного в Connected.** Реконнект по §2.7.
   Если пользователь поменял поле и сразу нажал «Сохранить» пока
   движок в `Connecting` — откладываем реконнект до Idle (Snackbar
   «применится при следующем подключении»).
9. **Отвязка от хаба (§3.6).** После кнопки «Отвязать» теряется
   возможность автообновления пресета, но это явное действие
   пользователя — ок. Предупреждающий AlertDialog: «Сервер перестанет
   получать обновления пресета с хаба. Отвязать?».
10. **TOFU при повторном приёме инвайта.** Если пользователь
    принял инвайт, потом удалил этот сервер, потом принял снова —
    старый `trusted_public_key` уже стёрт, новое TOFU происходит с
    нуля. Документируем в справке (позже).
11. **Backup/restore.** `allowBackup=false` в манифесте, так что
    перенос конфигурации между устройствами — только через инвайты.
    Отдельная «экспорт/импорт JSON» — вне этого LLD.
12. **Concurrency.** `ServerRepository` живёт в главном потоке
    (VpnViewModel instantiates), все мутации через suspend / StateFlow
    атомарны. Блокировки не нужны — никто не пишет в prefs из других
    потоков.

---

## 6. План проверки

Автотестов в Android-слое нет (правило LLD-02 §6). Проверка —
ручная, на устройстве.

1. **Миграция со старой модели.** Установить предыдущий APK, задать
   server_address, обновиться до LLD-08 APK. Запустить — единственный
   сервер в списке, активен, имя = `server_address`.
2. **Миграция из инвайта.** Так же, но на предыдущем APK применить
   инвайт. После апгрейда — профиль с `source = Invite`, имя =
   hub-host или comment.
3. **Welcome на чистой установке.** Clear data → старт → Welcome из
   LLD-04 §3.3.
4. **Создание ручного сервера.** Welcome → Настроить вручную →
   заполнить поля → Сохранить → попадаем на главный, chip показывает
   введённое имя, Connect работает.
5. **Добавление второго сервера.** Через chip → sheet → + Добавить →
   QR. После Apply — в списке два сервера, новый активен.
6. **Переключение сервером.** Chip → sheet → выбрать первый →
   активный поменялся, preset-подсказка обновилась. Connect работает.
7. **Переключение при Connected.** Connect → chip → выбрать другой →
   диалог «разорвать соединение» → OK → Disconnected + новый active.
   Повторный Connect идёт к новому.
8. **Редактирование неактивного.** Servers → ⋮ → Изменить → поменять
   port → Сохранить. Touch на chip — соседний сервер, никаких
   реконнектов. Connect работает.
9. **Редактирование активного при Idle.** Аналогично, сохраняется,
   никаких реконнектов.
10. **Редактирование активного при Connected.** Change port →
    Сохранить → Snackbar «применяю…» → phase пробегает Stopping →
    Idle → Connecting → Connected с новым конфигом.
11. **Удаление неактивного.** ⋮ → Удалить → подтверждение → профиль
    исчезает, активный не меняется.
12. **Удаление активного при Idle.** ⋮ → Удалить → подтверждение →
    активный становится первый из оставшихся. Если список пуст —
    Welcome.
13. **Удаление активного при Connected.** → алерт, подтверждение →
    Disconnect → Delete → новый активный (или Welcome).
14. **Отвязка от хаба.** Invite-сервер → Edit → «Отвязать» →
    подтверждение → поля hub_* скрываются, source = Manual,
    trusted_public_key = "". Preset обновляться перестаёт.
15. **Дубликат host:port через инвайт.** Принять второй инвайт с тем
    же server_address → AlertDialog «сервер уже есть» → Add anyway →
    два профиля.
16. **Ручной дубликат.** Настроить вручную два сервера с одинаковым
    host:port → никаких предупреждений (ручной ввод — ответственность
    пользователя).
17. **Deep link при пустом списке.** Clear data → открыть invite-URL
    из Telegram → ConfirmInvite → Add → главный экран с одним
    сервером, активен.
18. **Deep link при заполненном списке.** Уже есть 2 сервера → invite
    → ConfirmInvite без блока про замену → Add → три сервера,
    последний активен.
19. **Preset в списке.** Chip показывает правильный `name ·
    preset_label`: "Russia" / "Proxy all" / "Custom".
20. **Повреждение JSON.** Вручную через Device File Explorer или
    `adb shell run-as` заменить `servers` на мусор → перезапуск → в
    логе WARN, список пустой, запускается миграция с flat-prefs (или
    Welcome, если flat-prefs тоже пусты).
21. **Warnings.** `./gradlew :app:assembleDebug` — без warnings.

---

## 7. Вне скоупа

- **Экспорт / импорт списка серверов.** Перенос между устройствами —
  через инвайты (если сервер добавлен вручную, остаётся на устройстве;
  при необходимости — новый ручной ввод на другом устройстве).
- **Группировка серверов / теги.** Плоский список в первой версии;
  если серверов станет > 10 — отдельный LLD.
- **Search/filter в списке.** Тоже пока не нужно.
- **Drag-to-reorder.** Порядок = по `createdAt` (старые сверху) либо
  по имени — выбор при реализации. Перетаскивать не даём — поведение
  избыточное для ожидаемого числа серверов.
- **Шифрованное хранение** (`EncryptedSharedPreferences`). Старые
  plaintext-prefs тоже не шифровались; менять модель хранения —
  отдельная история. `obfuscation_key` base64-читаемый, но на общем
  устройстве пользователь уже под своим аккаунтом — риск такой же,
  как в других VPN-клиентах.
- **Мульти-connect** (одновременно несколько активных туннелей). Одно
  активное подключение — по-прежнему инвариант.
- **Шаринг сервера между приложениями** (Intent SEND). Если понадобится
  — тривиально добавить кнопку «Поделиться ссылкой» для Invite-сервера,
  но не сейчас.
