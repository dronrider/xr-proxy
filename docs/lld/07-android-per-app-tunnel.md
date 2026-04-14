# LLD-07 — Android per-app tunneling (split VPN)

**Статус:** Draft
**Область:** `xr-android` (UI настроек, VpnService.Builder), `xr-android-jni` (проброс списков), `xr-core` (не затрагивается)
**Зависимости:** [LLD-02](02-android-reliability.md) — жизненный цикл `XrVpnService`, источник конфигурации. [LLD-06](06-android-visual.md) — `XrTheme`, палитра.
**Связанные документы:** [ARCHITECTURE.md §4](../ARCHITECTURE.md)

Даём пользователю управлять тем, **видит ли конкретное приложение VPN**
вообще. Сейчас `VpnService` захватывает трафик **всех** приложений в TUN,
и даже direct-маршрут физически проходит через TUN на этапе TCP SYN. Из-за
этого приложения, чей трафик фактически идёт мимо прокси (direct), всё
равно получают от системы флаг `NetworkCapabilities.TRANSPORT_VPN`, видят
`tun0`-интерфейс и `VpnService.prepare() != null` — а многие банковские,
стриминговые и игровые приложения на это реагируют (блокируют запуск,
режут функциональность, шлют ошибки о «небезопасном подключении»).

---

## 1. Текущее состояние

- `XrVpnService.startVpn()` вызывает `Builder()` без `addAllowedApplication`
  / `addDisallowedApplication` — весь трафик (все UID) уходит в TUN
  [XrVpnService.kt:173-180](../../xr-android/app/src/main/java/com/xrproxy/app/service/XrVpnService.kt#L173).
- В Settings нет ни одного поля про приложения.
- Маршрут `direct` в движке физически уходит через protected socket, но
  для приложения это невидимо: перед этим моментом оно уже отправило пакет
  в TUN и Android системно считает его «VPN-трафиком».

## 2. Целевое поведение

### 2.1 Настройка списка исключений

На вкладке **Settings** — новый блок **«Приложения и VPN»** с тремя
режимами:

- **Проксировать все приложения** (default) — как сейчас, весь трафик в TUN.
- **Исключить выбранные приложения** — все в TUN, кроме указанных. Для них
  Android вообще не поднимает TUN; они работают с обычным маршрутом сети.
- **Проксировать только выбранные приложения** — в TUN попадают только
  указанные, остальные идут в обход. Удобно для сценария «включаю VPN
  только для браузера и Telegram».

Под радиокнопкой режима — экран выбора приложений: список всех
установленных (packageManager.getInstalledApplications), мультивыбор,
поиск по имени/package, иконки приложений, сортировка «сначала выбранные».
Системные приложения по умолчанию скрыты (чекбокс «Показать системные»).

### 2.2 Хранение

Сохраняется в существующих `SharedPreferences` xr_proxy:

- `tunnel_scope`: `"all"` | `"exclude"` | `"include"`, default `"all"`.
- `tunnel_apps`: `Set<String>` package names.

При `tunnel_scope == "all"` список игнорируется.

### 2.3 Применение при старте VPN

`XrVpnService.startVpn()` перед `establish()`:

```kotlin
val builder = Builder().setSession("XR Proxy").addAddress(...) ...
when (scope) {
    "all" -> { /* no-op */ }
    "exclude" -> apps.forEach { builder.addDisallowedApplication(it) }
    "include" -> apps.forEach { builder.addAllowedApplication(it) }
}
```

Исключения: **нельзя добавить собственное приложение** — Android
выбросит `PackageManager.NameNotFoundException`, если пакет не существует,
и `IllegalArgumentException` если добавлять self. Фильтруем на этапе
сохранения списка.

### 2.4 Перестройка при изменении

Изменение списка/режима **не применяется на лету** — нужно дисконнект и
коннект. Показываем Snackbar «Перезапустите VPN, чтобы применить
изменения» через `UiMessage(severity=Info)`, если пользователь правит
список при активном туннеле.

### 2.5 UI — экран выбора приложений

Отдельный composable `AppPickerScreen(scope, selected, onDone)`:

- `TopAppBar` с Back-arrow, заголовком «Исключить приложения» / «Проксировать
  только эти» (по режиму), кнопкой «Готово».
- `SearchBar` (Material3) — фильтр по labels / package names.
- `LazyColumn` с `AppRow` — checkbox, icon 40dp, label, package name
  мелким текстом.
- В нижнем right-corner FAB `«показать системные»` toggle (переключает
  видимость приложений с флагом `ApplicationInfo.FLAG_SYSTEM` без
  FLAG_UPDATED_SYSTEM_APP).

Загрузка списка — `packageManager.getInstalledApplications(0)` в
`Dispatchers.IO`, кэш в ViewModel. При >500 приложений — индикатор
загрузки на первые 300ms, чтобы UI не фризился на старых устройствах.

## 3. Дизайн-решения

### 3.1 Почему `scope` + список вместо двух независимых списков

Три режима (all / exclude / include) — честное отражение ограничений
`VpnService.Builder`: **нельзя смешивать** `addAllowedApplication` и
`addDisallowedApplication` в одном сеансе (вторая выбросит исключение).
Моделируем это в UI явно, чтобы не создавать невозможных состояний.

### 3.2 Почему не per-app action (proxy/direct/bypass)

Можно было бы дать выбор на уровне «это приложение проксировать, это
пускать direct» — но это **не про приложения, а про трафик**, и у нас для
этого уже есть Routing rules (LLD-05). Per-app VPN — про **видимость VPN
для приложения**, это другая ось.

### 3.3 Self-exclusion запрещаем

Если пользователь добавит собственное приложение в exclude, он сам не
сможет общаться с xr-hub или выполнять self-ping. Фильтруем.

### 3.4 Системные приложения по умолчанию скрыты

Экран выбора иначе превращается в 200+ пунктов, из которых 90% —
бесполезные системные сервисы. Toggle «показать системные» для
продвинутых кейсов (напр. исключить Google Play Services).

## 4. Изменения в коде

| Файл | Что меняется |
|---|---|
| `xr-android/app/src/main/java/com/xrproxy/app/service/XrVpnService.kt` | В `startVpn()` применить `addAllowedApplication` / `addDisallowedApplication` к `Builder` перед `establish()`. Список и scope читаются из конфига, который VM передаёт через intent extras. |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt` | Поля `tunnelScope: String`, `tunnelApps: Set<String>`. Persist в SharedPreferences. В `actuallyStart()` положить их в intent как extras (не в JSON-конфиг — это чисто Android-слой, движку не нужно). |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/screens/AppPickerScreen.kt` (новый) | Composable экрана выбора приложений (§2.5). |
| `xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt` | В `SettingsSection` добавить блок «Приложения и VPN» с радиокнопками режима и кнопкой «Выбрать приложения…». Навигация на `AppPickerScreen` через `rememberLauncherForActivityResult` или простой state-switch. |
| `xr-android/app/src/main/res/drawable/ic_apps.xml` (новый) | Иконка 20dp для блока (Material Symbols `apps`). |

Rust-слой (`xr-core`, `xr-android-jni`) **не затрагивается** — это чисто
Android-фича VpnService Builder.

## 5. Риски и edge-кейсы

1. **Приложение удалено между сохранением списка и стартом VPN.**
   `addDisallowedApplication(nonexistent)` бросает NameNotFoundException.
   Оборачиваем в try/catch, пропускаем неизвестные пакеты, в лог пишем
   WARN «приложение `X` удалено, пропущено».

2. **Пользователь выбрал «include» и пустой список.**
   По документации Android это эквивалентно «никакое приложение не
   видит VPN» — т.е. VPN запущен, но трафик через него ни от кого не
   идёт. Блокируем на этапе Save: если `scope==include` и `apps.isEmpty()`,
   показываем Snackbar-warn и не сохраняем.

3. **Разрешение `QUERY_ALL_PACKAGES`.**
   На Android 11+ для получения полного списка приложений требуется
   `QUERY_ALL_PACKAGES` или `<queries>` в манифесте. Добавляем
   `QUERY_ALL_PACKAGES` — мы VPN-приложение, это легитимный use-case
   (Google Play считает это допустимым для VPN-категории).

4. **VPN-приложение не может добавить себя в disallowed.**
   Android сам бросает исключение. Фильтруем на этапе сохранения (см. §3.3).

5. **Совместимость с `on_server_down=direct`.**
   Не влияет. Исключённые приложения не видят TUN вообще — они идут
   мимо нашего движка, значит ни `direct`, ни `proxy` к ним не применяются.

## 6. План проверки

Ручная (автотестов в Android-слое нет, по правилу LLD-02 §6):

1. **Установить APK**, в Settings выбрать «Исключить выбранные приложения»,
   добавить банковское приложение.
2. Запустить VPN, открыть банковское приложение — оно **не** должно
   показывать warning «вы используете VPN». Проверить, что оно ходит в
   интернет (перевод, баланс).
3. Проверить через `adb shell dumpsys connectivity` — у процесса
   банковского приложения нет VPN network в `Networks requested by UID`.
4. **Переключить на «Проксировать только выбранные приложения»**, оставить
   только браузер. Открыть YouTube-приложение — оно **не** в списке, идёт
   прямо; YouTube.com в браузере — через прокси.
5. **Удалить выбранное приложение** из системы, запустить VPN — VPN
   поднимается без исключения, в логе WARN «приложение `...` удалено».
6. **Пустой include-список** — сохранение блокируется Snackbar'ом.
7. **Изменить список при активном VPN** — Snackbar «Перезапустите VPN»,
   при следующем connect изменения применены.

## 7. Вне скоупа

- **Управление приложениями через правила роутинга** (per-app action
  proxy/direct). Это другая ось, см. §3.2 и LLD-05.
- **Автоматическое исключение на основе эвристик** (банки, стриминг) —
  нужна ручная кураторская работа, не вписывается в «минимум ручной
  настройки».
- **Экспорт/импорт списка приложений** через xr-hub-пресеты — пакетные
  имена привязаны к конкретному устройству (вендорские приложения,
  локализованные сборки), в централизованный пресет не ложится.
- **iOS-эквивалент** — этот LLD только для Android; iOS Network Extension
  имеет другую модель (per-flow matching, не per-app allowlist).
