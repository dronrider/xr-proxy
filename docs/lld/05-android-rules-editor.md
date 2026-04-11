# LLD-05 — Android Rules Editor

**Статус:** Draft
**Область:** `xr-android` (новая вкладка Rules), `xr-core` (слияние правил с пресетом), `xr-proto` (структура `UserRule`)
**Зависимости:** [LLD-01](01-control-plane.md) — использует `Preset`, `PresetSummary`, модель merge из §3.10. [LLD-02](02-android-reliability.md) — VpnViewModel / bind-service архитектура. [LLD-06](06-android-visual.md) — `XrTheme`, `ShieldArrowIcon`, палитра.
**Связанные документы:** [ARCHITECTURE.md §6.2](../ARCHITECTURE.md)

Вводим полноценный редактор правил маршрутизации в Android: отдельная
вкладка Rules с read-only пресетом (скачивается из xr-hub) и упорядоченным
списком пользовательских правил, которые срабатывают первыми. Удаляем из
приложения последний хардкод (`PRESET_RUSSIA`) и три FilterChip'а
«Russia / Proxy all / Custom» из Settings.

---

## 1. Текущее состояние

- В Settings — три FilterChip'а выбора пресета:
  `russia / proxy_all / custom` [MainActivity.kt:333-343](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L333).
- Пресет Russia захардкожен в Kotlin-константу [VpnViewModel.kt:321-366](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt#L321).
- Кастомный режим — две textarea'и для доменов и IP-диапазонов
  [MainActivity.kt:356-362](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L356),
  парсятся хрупкими regex'ами.
- Нет упорядоченного списка правил, нет понятия «переопределить пресет
  одним доменом», нет read-only просмотра, что именно внутри пресета.
- После LLD-01 — пресеты приходят из xr-hub как JSON, но в текущем
  приложении их негде показать.

## 2. Целевое поведение

### 2.1 Вкладка Rules

Четвёртая вкладка в `NavigationBar`: **VPN · Log · Rules · Settings**.
Иконка — «список с галочкой» (Material Symbol `rule`).

### 2.2 Главный экран вкладки

Сверху — карточка **пресета** (read-only), под ней — список **моих
правил** (editable), внизу — кнопка **«+ Добавить правило»**. В
`TopAppBar` — иконка `{ }` «Show TOML», открывающая модальное окно с
собранным конфигом.

### 2.3 Карточка пресета

- Имя пресета (из `InvitePayload.preset` или из prefs `hub.preset`).
- Версия и дата последнего обновления (из кэшированного `Preset`).
- Число правил внутри пресета.
- Кнопка **«Обновить сейчас»** — форсированный fetch из хаба.
- Кнопка **«Подробнее»** — переход на отдельный read-only экран с
  полным содержимым пресета.
- Если хаб ещё не настроен или пресет не скачан — карточка показывает
  «Пресет не подключён», с текстом-подсказкой «Настройте хаб или
  примените приглашение».

Редактировать пресет на клиенте **нельзя**. Это принципиально: пресет —
единый централизованный источник правды, меняется только в xr-hub
Admin UI, клиенты синхронизируются.

### 2.4 Мои правила

Упорядоченный список. Каждое правило — одна строка с:

- Пилюлей действия `[proxy]` (cyan) или `[direct]` (outline), тапабельной
  для быстрого переключения.
- Pattern в моноширинном шрифте (домен или CIDR).
- Иконкой `⋮` — меню: Edit / Delete / Move up / Move down / Move to top /
  Move to bottom.

Порядок важен — срабатывает первое совпадение.

### 2.5 Добавление и редактирование правила

Диалог с двумя полями:

- **Pattern** — текст (домен или CIDR). Тип определяется автоматически
  по формату, никаких radio-buttons.
- **Action** — segmented control `Proxy | Direct`.
- Валидация при Save: понимаем domain (exact/wildcard) или CIDR
  (v4/v6). Если ни то ни другое — ошибка под полем «Некорректный формат».

Кнопки: Отмена / Сохранить.

### 2.6 Live TOML через кнопку

Кнопка `{ }` в `TopAppBar` вкладки. Открывает модальный `Dialog` во весь
экран с read-only моноширинным текстом итогового `[routing]`-блока. Сверху
диалога — кнопка «Скопировать», внизу — «Закрыть».

Содержимое — то же, что Router на самом деле компилирует при запуске:
`user_overrides` + `preset` + `default_action`. TOML в формате, готовом к
вставке в `configs/client.toml` или в OpenWRT-конфиг — это основная
причина наличия функции.

### 2.7 Применение изменений

- Если туннель **не активен** — изменения вступают в силу при следующем
  Connect.
- Если туннель **активен** — правила всё равно не применяются на лету
  (см. LLD-01 §5.9). После Save — Snackbar: «Правила сохранены. Применятся
  при следующем подключении».
- Никакого автоматического рестарта туннеля — пользователь сам решает,
  когда переподключиться.

---

## 3. Дизайн решения

### 3.1 Модель данных

**xr-proto** получает новый модуль `user_rule.rs`:

```rust
pub struct UserRule {
    pub id: String,         // UUID v4, генерируется клиентом
    pub action: Action,     // Proxy | Direct, существующий enum
    pub pattern: String,    // нормализованная строка
    pub kind: RuleKind,     // Domain | Wildcard | Cidr — вычисляется при валидации
}

pub enum RuleKind { Domain, Wildcard, Cidr }

pub struct UserRules {
    pub rules: Vec<UserRule>,  // порядок важен
}

pub fn classify_pattern(s: &str) -> Result<RuleKind, RulePatternError>;
pub fn normalize_pattern(s: &str) -> String;  // lowercase + trim
```

`classify_pattern` — чистая функция, тесты покрывают:
- `github.com` → `Domain`
- `*.github.com` → `Wildcard`
- `10.0.0.0/8` → `Cidr`
- `2001:b28:f23d::/48` → `Cidr`
- `*.*` / `https://...` / пустота → `Err(RulePatternError)`

**Обновление `xr-proto/src/routing.rs`:** в `Router::from_merged` принимаем
`user_overrides: &[UserRule]` отдельным параметром. Внутри строим
`CompiledRule` для каждого override'а и кладём их **перед** правилами
пресета.

**Сериализация в prefs / на диске:** `UserRules` сериализуется в JSON
через `serde_json::to_string`. На Android хранится в файле
`filesDir/user_rules.json` (а не в SharedPreferences — структурированные
данные там неудобны). На OpenWRT — в виде подсекции `[[routing.rules]]`
в `client.toml` как сейчас.

### 3.2 Вкладка Rules — структура UI

Новый composable `ui/rules/RulesSection.kt`:

```
Scaffold
  topBar = TopAppBar(
    title = "Rules",
    actions = { IconButton(onClick = { showTomlDialog = true }) { Icon(ic_code) } }
  )
  content =
    LazyColumn:
      item { PresetCard(preset, onRefresh, onDetails) }
      item { SectionHeader("Мои правила") }
      items(userRules) { rule -> UserRuleRow(rule, ...) }
      item { AddRuleButton(onClick = { showAddDialog = true }) }
```

- `PresetCard` — отдельный composable, §3.3.
- `UserRuleRow` — §3.4.
- `AddRuleButton` — простой `OutlinedButton` с иконкой `ic_add`.
- Диалоги `RuleEditDialog` и `TomlPreviewDialog` — §3.5 и §3.6.

**Расположение вкладки:** в LLD-02 `NavigationBar` имеет три элемента
(VPN / Log / Settings). Здесь добавляем четвёртый:

```kotlin
NavigationBar {
    NavigationBarItem(..., icon = Icons.Default.Lock, label = "VPN")
    NavigationBarItem(..., icon = Icons.Default.List, label = "Log", badge = ...)
    NavigationBarItem(..., icon = Icons.Default.Rule, label = "Rules")  // НОВОЕ
    NavigationBarItem(..., icon = Icons.Default.Settings, label = "Settings")
}
```

`currentTab` становится 0..3.

### 3.3 `PresetCard`

```
┌──────────────────────────────────────────────┐
│ 🛡 Russia · v42                              │
│ 15 правил · обновлён 11 апр 2026              │
│                                               │
│   [ Обновить сейчас ]    [ Подробнее >]      │
└──────────────────────────────────────────────┘
```

- Фон — `surface`, stroke — `outline`, radius 12dp.
- Первая строка: имя (titleMedium) + точка-разделитель + версия
  (`on_surface_variant`).
- Вторая строка: количество правил + дата.
- Кнопки: outlined pill-форма, одинакового веса.

**Обновить сейчас** → `viewModel.refreshPreset()`:
- Вызов `nativeFetchPresetAndKey(hub_url, preset, timeout_ms = 5000)`
  (функция из LLD-04 §3.6, переиспользуем).
- Snackbar «Пресет обновлён до v43» или ошибка.
- Карточка перерисовывается.

**Подробнее** → `PresetDetailsScreen`:

```
TopAppBar: "Russia · v42"  [back]
LazyColumn:
  items(preset.rules) { rule ->
    Card {
      Row: [proxy/direct] pill
      Column:
        if rule.domains.isNotEmpty() { Text("Домены (${rule.domains.size})") }
        if rule.ip_ranges.isNotEmpty() { Text("IP (${rule.ip_ranges.size})") }
        (оба — свёрнуты, тап раскрывает)
    }
  }
```

Этот экран не редактируемый, только просмотр. `onBackPressed` возвращает
на главный экран вкладки Rules.

**Пустое состояние** (пресет не настроен):
```
┌──────────────────────────────────────────────┐
│ 🛡 Пресет не подключён                       │
│ Настройте хаб в Settings или примените       │
│ приглашение.                                  │
└──────────────────────────────────────────────┘
```

### 3.4 `UserRuleRow`

```
┌──────────────────────────────────────────────┐
│ [proxy]  *.github.corp                   ⋮  │
└──────────────────────────────────────────────┘
```

- Высота 56dp, padding 16dp.
- Pill `[proxy]` (`primary` фон, `on_primary` текст) или `[direct]`
  (`outline` stroke, `on_surface_variant` текст). Тап — toggle action,
  обновляет state.
- Pattern — `fontFamily = FontFamily.Monospace`, `bodyMedium`, textColor
  `on_background`. Overflow = `Ellipsis`, чтобы длинные CIDR не ломали
  строку.
- `⋮` справа — `IconButton` с `DropdownMenu`:
  - Edit → открыть `RuleEditDialog` с заполненными полями.
  - Delete → удалить правило.
  - Move up / Move down — перемещение на одну позицию.
  - Move to top / Move to bottom — в начало/конец.
  - Недоступные пункты (Up для первого, Down для последнего) — disabled.

Между строками — тонкий `Divider` цвета `outline`.

**Drag-and-drop reorder.** Не делаем в первой версии — для < 20 правил
меню `⋮` достаточно. Добавление drag-handle требует либо сторонней
библиотеки `sh.calvin.reorderable`, либо ~150 строк кастома через
`detectDragGesturesAfterLongPress`. Не окупается.

### 3.5 `RuleEditDialog`

```kotlin
@Composable
fun RuleEditDialog(
    initial: UserRule?,  // null = новое правило
    onDismiss: () -> Unit,
    onSave: (UserRule) -> Unit,
)
```

Содержимое:

```
┌ Dialog ──────────────────────────────────────┐
│  Добавить правило                             │
│                                               │
│  Pattern                                      │
│  ┌───────────────────────────────────────┐   │
│  │ *.github.corp                          │   │
│  └───────────────────────────────────────┘   │
│  Домен с подстановкой                         │
│                                               │
│  Action                                       │
│  ┌─────────────┬─────────────┐               │
│  │   Proxy ✓   │   Direct    │               │
│  └─────────────┴─────────────┘               │
│                                               │
│              [ Отмена ] [ Сохранить ]        │
└──────────────────────────────────────────────┘
```

- `OutlinedTextField` для pattern, singleLine, autocapitalize=none,
  autocorrect=off, keyboardOptions с `KeyboardType.Uri`.
- Под полем — hint-строка, которая в реальном времени показывает тип
  распознанного pattern'а: «Домен», «Домен с подстановкой», «IP-диапазон
  (IPv4)», «IP-диапазон (IPv6)». При ошибке — «Некорректный формат»
  красным.
- `SingleChoiceSegmentedButtonRow` (Material3) для Proxy/Direct. Дефолт —
  Proxy (типичный случай: «эта штука у меня не работает, надо через
  прокси»).
- Сохранить заблокировано, пока pattern невалиден.

Вызов `classify_pattern` через новый JNI-метод `nativeClassifyPattern(raw: String) -> String`
(возвращает тип или «invalid» + сообщение об ошибке). Это единственный
способ иметь одинаковую логику в xr-core и Android без копирования
валидации в Kotlin. Вызов синхронный, легковесный — просто regex/parse,
никакого tokio.

### 3.6 `TomlPreviewDialog`

Модальный `Dialog(properties = DialogProperties(usePlatformDefaultWidth = false))`
на большую часть экрана:

```
┌──────────────────────────────────────────┐
│  TOML                   [ Скопировать ]  │
├──────────────────────────────────────────┤
│ # Generated by XR Proxy                  │
│ # Merged: user overrides + preset russia │
│                                          │
│ [routing]                                │
│ default_action = "direct"                │
│                                          │
│ # --- User overrides ---                 │
│ [[routing.rules]]                        │
│ action = "proxy"                         │
│ domains = ["*.github.corp"]              │
│                                          │
│ [[routing.rules]]                        │
│ action = "direct"                        │
│ domains = ["youtube.com"]                │
│                                          │
│ # --- Preset russia v42 ---              │
│ [[routing.rules]]                        │
│ action = "proxy"                         │
│ domains = ["youtube.com", "*.youtube.co… │
│ ...                                      │
│                                          │
├──────────────────────────────────────────┤
│                         [ Закрыть ]      │
└──────────────────────────────────────────┘
```

Содержимое генерируется функцией `buildMergedToml(userRules, preset, defaultAction)`:
- чистая Kotlin-функция в `ui/rules/TomlBuilder.kt`,
- принимает три параметра,
- возвращает `String`,
- не зависит от Android SDK.

Тело — обычная конкатенация через `StringBuilder`, лексикографический
порядок полей внутри каждого `[[routing.rules]]` (action → domains →
ip_ranges → geoip), массивы — как многострочные `[\n  "...",\n  "..."\n]`
для читаемости.

Кнопка **Скопировать** → `ClipboardManager.setText(...)` + Snackbar
«Скопировано».

### 3.7 Persistence

**Android.** Файл `context.filesDir / "user_rules.json"`, формат:

```json
{
  "version": 1,
  "rules": [
    { "id": "uuid-1", "action": "proxy", "pattern": "*.github.corp" },
    { "id": "uuid-2", "action": "direct", "pattern": "youtube.com" }
  ]
}
```

Парсинг — через `org.json.JSONObject` / `JSONArray` (часть Android SDK,
новых зависимостей не надо). Чтение — синхронно в `VpnViewModel.init`,
запись — асинхронно в `Dispatchers.IO` через `viewModelScope`.

На записи — атомарность через временный файл + `renameTo`:
```kotlin
val tmp = File(filesDir, "user_rules.json.tmp")
tmp.writeText(json)
tmp.renameTo(File(filesDir, "user_rules.json"))
```

**Передача в Rust при старте движка.** `VpnViewModel.buildConfigJson`
(существующая функция) читает `user_rules.json`, включает в конфиг как
поле `user_rules: [{ action, pattern }, ...]`. Rust-сторона
(`xr-android-jni/src/lib.rs::parse_config`) парсит это поле в
`Vec<UserRule>` и передаёт в `VpnEngine::start` как часть `VpnConfig`.

### 3.8 Слияние с пресетом в `Router`

`xr-proto/src/routing.rs` получает новый конструктор:

```rust
impl Router {
    pub fn from_merged(
        user_overrides: &[UserRule],
        preset_rules: &[RoutingRule],
        default_action: Action,
    ) -> Result<Self, RouterError>;
}
```

Порядок компиляции:
1. Каждый `UserRule` превращается в `CompiledRule` с одним pattern'ом
   (domain или CIDR), action из самого правила.
2. Добавляются `CompiledRule` из `preset_rules` в порядке их определения.
3. `default_action` в конце.

Существующий конструктор `Router::from_config` остаётся для обратной
совместимости и тестов, новый используется из `xr-core::engine` при
старте движка.

### 3.9 Удаление хардкода

**`PRESET_RUSSIA`** в [VpnViewModel.kt:321-366](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt#L321)
— удаляется целиком. Вместе с ним уходят:

- `FilterChip`'ы «russia / proxy_all / custom» из `SettingsSection`
  [MainActivity.kt:333-343](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L333).
- Два textarea'и «customDomains / customIpRanges»
  [MainActivity.kt:356-362](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L356).
- Поля `routingPreset`, `customDomains`, `customIpRanges` из `VpnUiState`
  и `saveSettings` / `loadSettings`.
- Функции `buildRoutingToml`, `buildCustomRoutingToml`, `importToml`
  [VpnViewModel.kt:117-162, 257-288](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt#L117).

**`SettingsSection`** после LLD-05 состоит только из: Server address/port,
Obfuscation key/modifier/salt, блока Hub (url, preset — read-only, с
ссылкой «Изменить через приглашение»), кнопки Save. Никаких FilterChip'ов,
никаких textarea'й.

**Миграция старых настроек.** Если в prefs остался `routing_preset = "russia"`
и пользовательские textarea'и, при первом запуске после обновления:
- Пресет Russia автоматически больше не применяется (его нет в коде).
- `customDomains` и `customIpRanges` конвертируются в `UserRules` один
  раз при миграции, каждая строка → отдельный `UserRule { action: "proxy" }`.
- Если был `proxy_all` — добавляется одно правило `* / proxy` (либо
  `default_action` меняется на `proxy` в prefs).
- Ключ миграции `rules_migrated = true` в prefs, чтобы не повторять.

Это однократная операция в `VpnViewModel.init`, тестируется один раз
вручную на устройстве с сохранённой старой версией.

### 3.10 Применение без рестарта — нет, с Snackbar — да

`VpnViewModel.saveUserRules(rules: List<UserRule>)`:
1. Валидирует каждое правило через `nativeClassifyPattern`.
2. Записывает `user_rules.json` атомарно.
3. Обновляет `_uiState.value.userRules`.
4. Если `phase == Connected` — эмитит в `messages` Snackbar: «Правила
   сохранены. Применятся при следующем подключении».
5. Если `phase == Idle` — тот же эффект, но с текстом «Правила сохранены».

Никакого automatic reconnect. Правила войдут в сборку `Router` при
следующем `VpnEngine::start`, когда `VpnViewModel.buildConfigJson`
прочитает свежий `user_rules.json`.

---

## 4. Изменения в коде

| Файл | Что меняется |
|---|---|
| `xr-proto/src/user_rule.rs` (новый) | `UserRule`, `UserRules`, `RuleKind`, `classify_pattern`, `normalize_pattern`. Unit-тесты на классификацию. |
| `xr-proto/src/lib.rs` | `pub mod user_rule;` |
| [xr-proto/src/routing.rs](../../xr-proto/src/routing.rs) | `Router::from_merged(user_overrides, preset_rules, default_action)`. |
| [xr-core/src/engine.rs](../../xr-core/src/engine.rs) | `VpnConfig` получает поле `user_rules: Vec<UserRule>`. При компиляции `Router` использует `from_merged`. |
| [xr-android-jni/src/lib.rs](../../xr-android-jni/src/lib.rs) | `parse_config` читает массив `user_rules` из JSON. Новая JNI-функция `nativeClassifyPattern(raw: String) -> String`. |
| [NativeBridge.kt](../../xr-android/app/src/main/java/com/xrproxy/app/jni/NativeBridge.kt) | `external fun nativeClassifyPattern(raw: String): String`. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/rules/RulesSection.kt` (новый) | Композит вкладки Rules (PresetCard + UserRules list + AddButton), §3.2. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/rules/PresetCard.kt` (новый) | §3.3. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/rules/PresetDetailsScreen.kt` (новый) | Read-only просмотр правил пресета. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/rules/UserRuleRow.kt` (новый) | §3.4. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/rules/RuleEditDialog.kt` (новый) | §3.5. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/rules/TomlPreviewDialog.kt` (новый) | §3.6, содержит `buildMergedToml`. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/rules/TomlBuilder.kt` (новый) | Чистая функция `buildMergedToml(userRules, preset, defaultAction)`. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/rules/UserRulesStore.kt` (новый) | Чтение/запись `user_rules.json` через `org.json`. Атомарная запись через временный файл. |
| [VpnViewModel.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt) | Удалить `PRESET_RUSSIA`, `buildRoutingToml`, `buildCustomRoutingToml`, `importToml`, поля `routingPreset` / `customDomains` / `customIpRanges`. Добавить `userRules`, `cachedPreset`, `saveUserRules`, `addRule`, `deleteRule`, `moveRule`, `refreshPreset`. Миграция старых prefs в `user_rules.json` при первом запуске после обновления. `buildConfigJson` включает массив `user_rules`. |
| [MainActivity.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt) | `currentTab` 0..3, добавить ветку `2 -> RulesSection(...)` и сдвинуть Settings на индекс 3. В `SettingsSection` убрать FilterChip'ы и textarea'и (остаётся только server/obf/save). |
| `res/drawable/ic_rule.xml` (новый) | Material Symbol `rule` 24dp. |
| `res/drawable/ic_code.xml` (новый) | Material Symbol `code` 20dp для TopAppBar Rules. |
| `res/drawable/ic_add.xml` (новый) | Material Symbol `add` 20dp для AddRuleButton. |
| [ARCHITECTURE.md](../ARCHITECTURE.md) | После имплементации — обновить §6.2 (переход с «планируется» на фактическую модель override + preset, где Android живёт). |

---

## 5. Риски и edge-кейсы

1. **IPv6 CIDR с квадратными скобками.** Пользователь может ввести
   `[2001:db8::]/48`. `classify_pattern` должен либо принять (через
   `InetAddress.getByName` с strip brackets), либо отклонить. Выбираем
   отклонить с подсказкой «введите без скобок» — так проще и единообразно.
2. **Дубликаты id.** Если два UUID случайно совпали (практически
   невозможно) — `renameTo` перезапишет файл без дубликатов, потому что
   id используется как React-key, а не как ключ хранилища. В списке
   правил все id уникальны по построению (генерируются при добавлении).
3. **Огромный пользовательский список.** Ограничение 100 правил в UI.
   При попытке добавить 101-е → Snackbar «Достигнут лимит 100 правил».
   Это защита от случайного вставки тысяч строк из буфера и деградации
   компиляции `Router`. Реальный порог выбран с большим запасом.
4. **Смена пресета в xr-hub Admin UI.** Если админ переименует пресет
   (russia → ru) или удалит, клиент увидит 404 при refresh. Snackbar
   «Пресет russia больше не существует», карточка переходит в «пустое
   состояние», пользовательские правила работают как были.
5. **Конфликт user override с пресетом.** Пользователь добавляет
   `youtube.com → direct`, в пресете есть `youtube.com → proxy`.
   Override выигрывает (срабатывает первым). Это документированное
   поведение, не баг. В LLD-05 явно: пользовательские правила **всегда**
   имеют приоритет.
6. **Preset содержит 200+ правил.** В PresetDetailsScreen `LazyColumn`
   виртуализирует, никаких проблем. В TomlPreviewDialog — один
   монолитный текст, 200 строк по 100 символов = 20 КБ, `Text` рендерит
   без проблем, но скроллится медленнее на старых устройствах.
   Митигация: если текст длиннее 50 КБ — показываем первые 50 и подпись
   «…и ещё N строк (скопируйте для полного просмотра)».
7. **Миграция старых prefs.** Разовая функция в `init { }`, запускается
   один раз по ключу `rules_migrated = true`. Тестируется на устройстве
   с установленной предыдущей версией. Если миграция упадёт — не ломаем
   приложение, просто пишем WARN и оставляем user_rules пустыми.
8. **JNI `nativeClassifyPattern` вызывается часто.** На каждый ввод
   символа в `OutlinedTextField` — это может быть 20 вызовов в секунду.
   Решение: debounce через `kotlinx.coroutines.flow.debounce(150ms)` в
   `rememberUpdatedState`. Вызов сам по себе дешёвый (regex + parse),
   но меньше мигания красной рамки — приятнее UX.
9. **Drag-reorder когда-нибудь всё равно понадобится.** Меню `⋮` с
   Move-опциями работает, но для 15-20 правил неудобно. Если станет
   проблемой — добавим библиотеку `reorderable` одной строкой и drag-handle,
   UserRuleRow готов к этому (высота 56dp и чёткий handle слева от `⋮`).
10. **TOML, сгенерированный Android, отличается от того, что запишет
    xr-client.** Оба клиента используют один и тот же алгоритм сборки
    `Router::from_merged`. TomlPreviewDialog — это визуализация для
    пользователя, это не тот самый TOML, который читает Rust (Rust
    принимает JSON от Kotlin). Поэтому несущественные расхождения в
    форматировании допустимы, главное — семантическая эквивалентность.

---

## 6. План проверки

Ручная (автотесты в Android-слое не заводим).

1. **Вкладка появилась.** После обновления — внизу 4 вкладки, третья
   «Rules».
2. **Пустое состояние без хаба.** Нет настроенного `hub.url` → PresetCard
   показывает «Пресет не подключён», подсказку.
3. **Preset загружен.** После Apply инвайта (LLD-04) карточка показывает
   имя, версию, дату, число правил.
4. **Обновить сейчас.** Тап → Snackbar «Пресет v42» (если не поменялся)
   или «Пресет обновлён до v43» (если поменялся).
5. **Подробнее.** Тап → новый экран со списком правил пресета, tap-back
   возвращает.
6. **Добавить правило.** «+ Добавить правило» → диалог → ввести
   `*.github.corp`, выбрать Proxy → Save → правило появляется в списке.
7. **Автоопределение типа.** В диалоге ввести `10.0.0.0/8` → под полем
   «IP-диапазон (IPv4)». Ввести `github.com` → «Домен». Ввести
   `*.github.com` → «Домен с подстановкой». Ввести `ffff` → «Некорректный
   формат» красным, Save disabled.
8. **Переключение action.** Тап по пилюле [proxy] → становится [direct],
   цвет меняется.
9. **Меню ⋮.** Edit → открывается диалог с заполненными полями. Delete
   → правило удаляется. Move up / down — перемещение. Move to top →
   правило в начале списка.
10. **Порядок важен.** Добавить два правила с одинаковым доменом, но
    разными действиями. Первое в списке — выигрывает, проверить
    реальным подключением.
11. **TOML preview.** Тап `{ }` в TopAppBar → открывается диалог с
    собранным конфигом (user_rules сверху, preset снизу), Скопировать →
    Snackbar «Скопировано», в буфере — полный TOML.
12. **Сохранение в файл.** Добавить правило, закрыть приложение, открыть
    снова → правило на месте. Проверить
    `/data/data/com.xrproxy.app/files/user_rules.json` (через `adb shell
    run-as`).
13. **Применение при Connect.** Добавить `youtube.com → direct`, тап
    Connect → туннель поднимается, youtube через 2ip.ru показывает
    реальный IP, не VPS.
14. **Snackbar при изменении во время connected.** В Connected добавить
    правило → Snackbar «Правила сохранены. Применятся при следующем
    подключении». Отключиться и снова подключиться → правило работает.
15. **Миграция старых настроек.** Установить предыдущую версию,
    настроить routing_preset=russia, custom_domains = "foo.bar". Обновить
    до новой версии. Открыть Rules — `foo.bar → proxy` уже в списке.
    Пресет Russia отсутствует (пользователю нужно подключить хаб).
16. **Удаление хардкода.** В исходниках
    `VpnViewModel.kt` — строки с `PRESET_RUSSIA` отсутствуют.
    В Settings — нет FilterChip'ов, нет textarea'й.
17. **Warnings.** `cargo test --workspace` + `./gradlew :app:assembleDebug`
    — без warnings.

---

## 7. Вне скоупа

- **Drag-and-drop reorder.** Упомянут в §5.9, добавляется отдельной
  правкой, если станет неудобно.
- **Импорт правил из TOML / буфера.** Старый `importToml` удалён. Если
  понадобится — новый импорт должен парсить полный `[routing]`-блок в
  `UserRules`, а не в плоский текст. Отдельный LLD.
- **Экспорт правил в файл.** Через `CreateDocument` SAF (как в LLD-03
  для логов). Не критично, оставляем TOML preview + Copy.
- **Шаблоны правил** («заблокировать youtube одной кнопкой»). Это
  делается добавлением 10 паттернов в один preset на xr-hub, а не в UI.
- **Группировка пользовательских правил.** Все в одном списке, без
  категорий. Если список вырастет до сотен — добавим раскрывающиеся
  группы.
- **Подсчёт статистики «сколько раз это правило сработало»** — требует
  инструментации `Router`, отдельный LLD, если вообще понадобится.
- **Несколько хаб-провайдеров одновременно.** Вне скоупа (см. LLD-04 §7
  «мультипрофиль»).
