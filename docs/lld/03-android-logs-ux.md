# LLD-03 — Android Logs UX

**Статус:** Draft
**Область:** `xr-android` (вкладка Log)
**Зависимости:** [LLD-02](02-android-reliability.md) — предполагаем, что `recentErrors: List<String>` уже единственный источник лога, обновляется polling'ом в `XrVpnService`, бадж считается как `recentErrors.warnCount`, поле `errorLog` и `refreshLog()` удалены. [LLD-06](06-android-visual.md) — палитра и компоненты.
**Связанные документы:** [ARCHITECTURE.md §4.6, §8](../ARCHITECTURE.md)

Вкладка Log превращается в рабочий инструмент: sticky toolbar, живое
обновление без ручного refresh, поиск (substring + regex), скачивание
в файл через системный picker.

---

## 1. Текущее состояние (уже после LLD-02)

- Вкладка Log показывает `recentErrors.joinToString("\n")`, обновляется
  автоматически через polling в `XrVpnService`. Refresh-кнопка удалена
  в LLD-02.
- Toolbar (`Row` с заголовком и кнопками Copy/Share/Delete) лежит внутри
  того же `Column` с `verticalScroll`, что и сам лог
  [MainActivity.kt:92-106](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L92).
  При прокрутке toolbar уезжает наверх, кнопки становятся недоступны.
- Поиска нет совсем. На длинном логе WARN-записи приходится искать глазами.
- Скачивания нет — только Share через `FileProvider` в cache
  [MainActivity.kt:229-249](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L229),
  что требует пользоваться сторонним приложением («Сохранить в Файлы»),
  хотя пользователь хочет просто получить файл.

## 2. Целевое поведение

### 2.1 Sticky toolbar

Верхняя часть вкладки Log (строка заголовка + кнопки + поле поиска)
остаётся **зафиксированной** при скролле лога. Пользователь всегда видит
кнопки Copy/Download/Share/Delete и поле поиска, независимо от позиции в
логе.

### 2.2 Живое обновление

Лог обновляется сам, без участия пользователя, в темпе polling'а сервиса
(1 сек). При этом:

- Если пользователь **пролистал вверх** и читает старые записи — новые
  записи НЕ утягивают список обратно вниз. Позиция скролла сохраняется
  относительно той строки, которую пользователь сейчас читает.
- Если пользователь находится **в самом низу** (auto-follow mode) — новые
  записи появляются снизу, список автоматически докручивается.
- Переключение auto-follow определяется по факту: юзер листает в середину
  → auto-follow выключается; юзер вручную докручивает до низа → включается
  обратно.

### 2.3 Поиск

Поле поиска в sticky toolbar:

- По умолчанию — **substring-поиск** (case-insensitive).
- Иконка переключателя `.*` рядом с полем — включает regex-режим. Если
  regex невалиден, иконка подсвечивается красным, список показывает все
  записи без фильтра.
- Фильтрация — на всём актуальном `recentErrors`, результат — список строк,
  которые матчат запрос. При пустом поле — все строки.
- Colorize (WARN красным) работает и на отфильтрованном списке — правило
  подсветки не меняется.
- Если в отфильтрованном списке пусто — показывается placeholder
  «Ничего не найдено».
- Счётчик совпадений рядом с полем: «12 / 143» (12 матчей из 143 всего).

### 2.4 Скачивание

Кнопка «Скачать» в toolbar:

- Открывает системный picker (SAF, `CreateDocument`), пользователь выбирает
  папку и имя. Дефолт: `xr-proxy-log-YYYY-MM-DD-HHMMSS.txt`.
- Сохраняется **весь** лог (не отфильтрованный), UTF-8, без заголовков.
- После записи — Snackbar «Лог сохранён».
- Работает без `WRITE_EXTERNAL_STORAGE` permission — SAF не требует.

Share и Copy остаются, работают по-прежнему (отправляют весь лог, не
отфильтрованный — тот же принцип «действия в toolbar всегда про полный
лог, поле поиска — только визуальный фильтр»).

---

## 3. Дизайн решения

### 3.1 Разметка

Заменяем текущую структуру `LogSection` (одна `Column` с `verticalScroll`)
на две независимые области:

```
Scaffold bottomBar = NavigationBar
  content =
    Column(fillMaxSize):
      ┌──────────────────────────────────────────┐
      │ Sticky header                            │
      │   Row: title "Log (12/143 errors)" + actions  │
      │   Row: SearchField [.*][×]               │
      └──────────────────────────────────────────┘
      LazyColumn(weight = 1f, reverseLayout? = false):
        items(filteredEntries) { ... }
```

- Заголовок и поле поиска — обычные composables вне `LazyColumn`, поэтому
  не скроллятся.
- `LazyColumn` занимает оставшееся место (`Modifier.weight(1f)`), скролл
  только у него. Это решает п. 2.1.

Переход с `verticalScroll(rememberScrollState())` + `Text` на `LazyColumn`
критичен и для поп. 2.2: `LazyColumn` виртуализирует строки и корректно
сохраняет позицию при изменении списка через `LazyListState`.

### 3.2 Auto-follow

Храним в VM `logAutoFollow: Boolean = true`. Логика:

- При создании `LazyListState` — `rememberLazyListState()`, начальная позиция
  `firstVisibleItemIndex = entries.lastIndex` (если не пусто).
- `LaunchedEffect(filteredEntries.size)`: если `autoFollow == true` —
  `listState.animateScrollToItem(filteredEntries.lastIndex)`.
- `LaunchedEffect(listState.isScrollInProgress)`: при старте прокрутки
  пользователя — снимаем флаг, когда `listState.layoutInfo.visibleItemsInfo.last().index ==
  filteredEntries.lastIndex`, флаг снова включается.
- Маленькая кнопка `FloatingActionButton` в углу списка «↓» (иконка
  `ic_scroll_down`), видна только при `autoFollow == false` и
  `filteredEntries.size > 0`. Тап → включить `autoFollow = true` и
  проскроллить вниз.

Эта логика полностью на стороне Compose, стейт живёт в VM.

### 3.3 Поиск

`VpnUiState` получает два новых поля:

```kotlin
val logQuery: String = "",
val logRegexMode: Boolean = false,
```

`VpnViewModel`:

```kotlin
fun updateLogQuery(q: String)
fun toggleLogRegexMode()
```

Фильтрация — **в UI, не в VM**, через `remember(filteredEntries) derivedStateOf`:

```kotlin
val filteredEntries by remember(state.recentErrors, state.logQuery, state.logRegexMode) {
    derivedStateOf { filterLog(state.recentErrors, state.logQuery, state.logRegexMode) }
}
```

Функция `filterLog` — чистая, в отдельном файле `ui/logs/LogFilter.kt`:

```kotlin
data class LogFilterResult(
    val entries: List<String>,
    val invalidRegex: Boolean,
)

fun filterLog(all: List<String>, query: String, regex: Boolean): LogFilterResult {
    if (query.isBlank()) return LogFilterResult(all, false)
    if (!regex) return LogFilterResult(all.filter { it.contains(query, ignoreCase = true) }, false)
    val re = runCatching { Regex(query, RegexOption.IGNORE_CASE) }.getOrNull()
        ?: return LogFilterResult(all, true)
    return LogFilterResult(all.filter { re.containsMatchIn(it) }, false)
}
```

Подсветка поискового совпадения внутри строки — не делаем в первой
версии, оставляем только WARN-colorize. Если потребуется — добавим
вторым проходом через `AnnotatedString`.

### 3.4 Toolbar

Компонент `ui/logs/LogToolbar.kt`:

```
Row(padding = 16.dp, fillMaxWidth):
  Text("Log (${matches}/${total} errors)", titleMedium)
  Spacer(weight 1f)
  IconButton Copy       -> viewModel.copyLog()
  IconButton Download   -> downloadLauncher.launch(defaultName)
  IconButton Share      -> viewModel.shareLog(context)
  IconButton Delete     -> viewModel.clearLog()

Row(padding horizontal = 16.dp, vertical = 8.dp):
  OutlinedTextField(
    value = state.logQuery,
    onValueChange = viewModel::updateLogQuery,
    leadingIcon = { Icon(ic_search) },
    trailingIcon = {
      Row {
        if (state.logQuery.isNotEmpty()) {
          IconButton { viewModel.updateLogQuery("") } // clear
        }
        IconToggleButton(
          checked = state.logRegexMode,
          onCheckedChange = { viewModel.toggleLogRegexMode() },
        ) { Icon(ic_regex) }  // визуально подсвечивается primary при checked
      }
    },
    singleLine = true,
    placeholder = { Text("Поиск…") },
    isError = filterResult.invalidRegex,
    modifier = Modifier.fillMaxWidth(),
  )
```

Счётчик `(12/143 errors)` — слово «errors» оставляем по смыслу из LLD-02
(считаются только WARN-записи, см. LLD-02 §3.4). Полная формула:
```
val totalWarn = state.recentErrors.warnCount  // из LLD-02
val matchedWarn = filteredEntries.warnCount
"Log ($matchedWarn/$totalWarn errors)"
```

### 3.5 Скачивание через SAF

В `MainActivity`:

```kotlin
private val downloadLogLauncher = registerForActivityResult(
    ActivityResultContracts.CreateDocument("text/plain")
) { uri: Uri? ->
    if (uri != null) viewModel.writeLogTo(uri, contentResolver)
}
```

В `VpnViewModel`:

```kotlin
fun writeLogTo(uri: Uri, resolver: ContentResolver) {
    viewModelScope.launch(Dispatchers.IO) {
        try {
            resolver.openOutputStream(uri)?.use { out ->
                out.writer(Charsets.UTF_8).use { w ->
                    _uiState.value.recentErrors.forEach { line ->
                        w.write(line); w.write("\n")
                    }
                }
            }
            _messages.emit("Лог сохранён")
        } catch (e: Exception) {
            _messages.emit("Не удалось сохранить: ${e.message}")
        }
    }
}
```

Дефолтное имя файла — `"xr-proxy-log-${timestamp()}.txt"`, где
`timestamp()` — `YYYY-MM-DD-HHMMSS` по локальному времени. Генерируется
на стороне UI в момент тапа по кнопке и передаётся в `launcher.launch(name)`.

**Почему SAF:**
- не требует `WRITE_EXTERNAL_STORAGE`, работает на API 21+;
- пользователь сам выбирает куда (Downloads, SD-карта, любое SAF-подключённое облако);
- стандартный Android way для API 29+ (scoped storage), не нужны исключения.

### 3.6 Copy и Share

Логика переезжает в VM для единообразия, вместо прямого вызова
`clipboardManager.setText` в composable:

```kotlin
fun copyLog() {
    clipboardManager.setText(AnnotatedString(buildFullLog()))
    _messages.emit("Скопировано")
}

fun shareLog(context: Context) {
    val file = File(context.cacheDir, "xr-proxy.log")
    file.writeText(buildFullLog())
    val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", file)
    val intent = Intent(Intent.ACTION_SEND).apply { ... }
    context.startActivity(Intent.createChooser(intent, "Share log"))
}

private fun buildFullLog(): String = _uiState.value.recentErrors.joinToString("\n")
```

`ClipboardManager` — передаётся в VM через `AndroidViewModel(application)`
и `application.getSystemService(ClipboardManager::class.java)`. `Context` в
`shareLog` — явным параметром из composable, чтобы VM не держала ссылок на
Activity.

### 3.7 Colorize на `LazyColumn`

Текущий `colorizeLog` работает с одной склеенной строкой
([MainActivity.kt:279-291](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt#L279)).
С переходом на `LazyColumn` каждая строка рендерится отдельным item'ом,
`colorizeLog` переписываем как `colorizeLine(line: String): AnnotatedString`
— возвращает одну строку, красную целиком для WARN, дефолтную иначе.

```kotlin
@Composable
fun colorizeLine(line: String): AnnotatedString {
    val warn = MaterialTheme.colorScheme.error
    return buildAnnotatedString {
        if (line.contains(" WARN ")) {
            withStyle(SpanStyle(color = warn)) { append(line) }
        } else {
            append(line)
        }
    }
}
```

Использование в `LazyColumn`:

```kotlin
items(
    count = filteredEntries.size,
    key = { index -> filteredEntries[index] },  // строка уникальна по timestamp
) { index ->
    Text(
        colorizeLine(filteredEntries[index]),
        style = MaterialTheme.typography.bodySmall,
        fontSize = 11.sp,
        lineHeight = 16.sp,
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 12.dp, vertical = 1.dp),
    )
}
```

`key` нужен для корректного сохранения позиции скролла при добавлении
новых элементов — иначе `LazyColumn` считает, что все item'ы сдвинулись.
Используем саму строку как ключ: формат из [stats.rs:143](../../xr-core/src/stats.rs#L143)
включает timestamp, так что строки уникальны (если два события в одну
секунду — коллизия возможна, но `LazyColumn` с дубликатами key'ев падает
в рантайме, поэтому делаем `key = "${index}_${line.hashCode()}"` как
более надёжный вариант).

---

## 4. Изменения в коде

| Файл | Что меняется |
|---|---|
| [MainActivity.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt) | Переписать `LogSection`: sticky toolbar (не скроллится), `LazyColumn` с состоянием, auto-follow, поле поиска. Регистрация `downloadLogLauncher` (SAF `CreateDocument`). Удалить inline `colorizeLog` (multi-line), заменить на вызовы `colorizeLine` из нового файла. Badge и заголовок вкладки Log — обновить через `warnCount` (совместно с LLD-02). |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/logs/LogFilter.kt` (новый) | Чистая функция `filterLog` + `LogFilterResult` (§3.3). |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/logs/LogToolbar.kt` (новый) | Composable sticky-toolbar (§3.4). |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/logs/LogList.kt` (новый) | `LazyColumn`-компонент с auto-follow логикой и `colorizeLine` (§3.7). |
| [VpnViewModel.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt) | Новые поля `logQuery`, `logRegexMode`, `logAutoFollow`. Методы `updateLogQuery`, `toggleLogRegexMode`, `setLogAutoFollow`, `copyLog`, `shareLog`, `writeLogTo`. Удалить прямую работу с `ClipboardManager` / `FileProvider` из composable — перенести в VM. |
| `res/drawable/ic_search.xml` (новый) | Material Symbol «search» 20dp. |
| `res/drawable/ic_regex.xml` (новый) | Material Symbol «regular_expression» или текстовый `.*` в vector-форме 20dp. |
| `res/drawable/ic_download.xml` (новый) | Material Symbol «download» 20dp. |
| `res/drawable/ic_scroll_down.xml` (новый) | Material Symbol «arrow_downward» 20dp, для FAB auto-follow. |

Все Material Symbols — Apache-2.0, как договорились в LLD-06 §5.6.

---

## 5. Риски и edge-кейсы

1. **Дубликаты ключей в `LazyColumn`.** Две WARN-записи с одинаковым
   timestamp и текстом теоретически возможны (две идентичные ошибки в ту
   же секунду). Ключ через `hashCode()` — всё ещё не уникален. Решение:
   `key = "${index}_${line}"` — индекс в списке плюс содержимое. Это
   убивает оптимизацию кеширования между обновлениями (сдвиг индекса → новый
   ключ), но для нашего объёма (≤ 200 строк) перерисовка всего списка
   раз в секунду пренебрежима. Альтернатива сложнее и не окупается.
2. **Regex crash на эскейп-последовательности.** `Regex(query)` кидает
   `PatternSyntaxException` — ловим через `runCatching`, `invalidRegex = true`,
   поле подсвечивается, список показывает всё. Пользователь видит красную
   рамку и понимает, что надо исправить.
3. **Auto-follow vs прокрутка вверх.** Если пользователь чуть-чуть
   провёл вверх (100 пикселей) и остановился, `isScrollInProgress` станет
   `false`, логика должна корректно оставить `autoFollow = false` до
   момента, когда юзер сам докрутит до низа. Проверяем в тесте ручно.
4. **Filter на 200 строках × раз в секунду.** Linear scan, каждое
   обновление `recentErrors`. O(N·M) где N — 200, M — длина строки ~80
   символов. 16K операций раз в сек — пренебрежимо.
5. **SAF: пользователь отменил picker.** `uri == null`, просто ничего не
   делаем, никаких сообщений (в отличие от ошибок записи, где важно
   сказать причину).
6. **SAF: место закончилось / permissions denied (на SD-карте).**
   `openOutputStream` бросает `IOException` — ловим, Snackbar с message.
7. **Buffer/clipboard недоступен на некоторых устройствах.** На самом
   деле доступен всегда в API 21+, но если вдруг `null` — catch в `copyLog`,
   Snackbar «не удалось скопировать».
8. **Сохранение позиции скролла при изменении поискового запроса.**
   Если юзер ввёл `WARN`, увидел 10 строк, скролл в середину, изменил на
   `WARNX` (что-то невалидное в regex mode) — список показывает всё.
   Сохранять позицию не пытаемся: при смене фильтра всегда сбрасываем
   auto-follow в `false` и оставляем `LazyListState` с текущим индексом.
   Это даёт неидеальное UX (индекс между старым и новым списком), но
   проще, чем «вспомнить и проскроллить туда же». Первый раз пользователя
   немного дезориентирует; дальше привыкает. Если станет проблемой —
   добавим «помнить первую видимую строку по содержимому».
9. **Длинные строки.** Некоторые сообщения (полные пути, base64) длиннее
   экрана. `LazyColumn` с `Text` обеспечивает перенос автоматически;
   горизонтальный скролл не добавляем.

---

## 6. План проверки

Ручная (автотесты не заводим, см. правило из LLD-02 §6).

1. **Sticky toolbar.** На вкладке Log длинный список (>30 записей).
   Скролл вверх-вниз → заголовок, кнопки и поле поиска не двигаются,
   остаются видимыми.
2. **Auto-follow.** Подключиться, отключить сервер → накопить 5-10 WARN.
   В режиме auto-follow новые записи появляются снизу, список докручивается.
3. **Отмена auto-follow.** Во время активного лога провести вверх →
   появляется FAB «↓» в углу. Новые записи появляются, но не утягивают
   скролл. Тап на FAB → автоматически вниз, auto-follow включён снова.
4. **Substring-поиск.** Ввести `mux` → фильтр показывает только строки
   с «mux». Счётчик «3/27 errors».
5. **Чистка поля поиска.** Кликнуть ×-иконку → поле пустое, фильтр
   сброшен, список снова полный.
6. **Regex mode.** Переключить `.*` → иконка primary-цвета. Ввести
   `mux.*fail` → фильтр применяется.
7. **Невалидный regex.** Ввести `(unclosed` → поле подсвечено красным,
   список показывает всё, placeholder «Ничего не найдено» НЕ появляется.
8. **Пустой результат.** Ввести `нетакогостанного` → placeholder «Ничего
   не найдено».
9. **Download.** Кнопка Download → системный picker → выбрать Downloads
   / имя по умолчанию → Snackbar «Лог сохранён». Открыть файл в текстовом
   редакторе → содержит все записи с `\n` между строками, UTF-8.
10. **Отмена Download.** Открыть picker, нажать back → никаких
    сообщений, лог не меняется.
11. **Copy.** Тап Copy → Snackbar «Скопировано». Вставить в заметку →
    полный лог (не отфильтрованный).
12. **Share после фильтра.** Включить фильтр `mux`, нажать Share → в
    share-sheet выбрать Gmail → в теле письма **полный** лог, не
    отфильтрованный.
13. **Clear.** Тап Delete → список опустел, бадж на вкладке Log исчез,
    поле поиска сохранило текст (не сбрасываем).
14. **Dark theme.** WARN-строки красные на тёмном фоне, читаемы. Цвет
    совпадает с `error` из палитры LLD-06.
15. **Warnings.** `./gradlew :app:assembleDebug` — без warnings.

---

## 7. Вне скоупа

- **Подсветка поискового совпадения** внутри строки (span-highlight).
  Добавится, если станет нужно — через тот же `AnnotatedString`, но в
  первой версии достаточно фильтра.
- **Фильтр по уровню** (только WARN, только INFO, оба). Пока цель — это
  поиск, а не классификатор. Два чекбокса «WARN / INFO» добавим, если
  окажется, что WARN теряется в массе INFO-строк.
- **Экспорт в JSON / structured logs.** Файл — plain text, как в
  `recent_errors`. Если появится структурированный лог в `xr-core` —
  отдельный LLD.
- **Бесконечная история логов.** Лимит 200 строк живёт в `xr-core/stats.rs`
  (`entries.drain(0..50)` при переполнении). Не трогаем — это сознательное
  ограничение для экономии памяти на мобиле.
- **Поиск по регулярным группам с заменой.** Для этого уровня нужен
  полноценный viewer, которым бы пользоваться в грен-терминале. Здесь
  только read-only фильтр.
- **Live tail в уведомлении.** Бесполезно в шторке, там и так есть
  статистика.
