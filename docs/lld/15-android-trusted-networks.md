# LLD-15 — Android Trusted Networks (авто-пауза по SSID)

**Статус:** Implemented (post-factum; реализовано в чате C4b как задача `3b-2`)
**Область:** `xr-android` (service + UI), `xr-core` (`trusted.rs`), `xr-android-jni` (JNI-мост)
**Зависимости:**
- [LLD-02](02-android-reliability.md) — база: `XrVpnService` как single source of truth, binder, `ConnectPhase`, polling-стейт.
- Чат **C4** (задача 3b-1, re-bind при смене сети) — `registerDefaultNetworkCallback`, `nativeOnNetworkChanged`, debounce смены аплинка. Авто-пауза комбинируется с этим в одном вотчере.
- Чат **C4c** (задача 3c) — TUN MTU 1280, MSS-кламп (контекст «двойного туннеля», который и устраняет пауза).
- [LLD-08](08-android-multi-server.md) — `ServerRepository`/SharedPreferences-паттерн, по образцу которого сделан `TrustedNetworksRepository`.

**Связанные документы:** `local-docs/problems.md` (задача 3b-2), `local-docs/c4b-start.md` (стартовая разведка), `local-docs/routers.md` (почему дома прокси уже на роутере), [ARCHITECTURE.md §4.6, §7.2](../ARCHITECTURE.md).

**Коммиты:** `820da83` (база) → `0503163` (индикатор/пикер/проба) → `7f0ad15`, `3964b03`, `bac94c4`, `53de7be` (фиксы device-verify, см. §6).

---

## 1. Зачем

Дома телефон заходит в Wi-Fi, **за которым роутер уже проксирует** (xr-client на OpenWRT). Поднимать поверх него туннель приложения = двойной туннель → деградация скорости/MTU, re-bind-churn, стэкинг таймаутов (симптомы лечили C4/C4c). Корень закрывает **авто-пауза**: в доверенном SSID туннель приложения встаёт на паузу — нет двойного туннеля, трафик честно идёт через роутер, защита не теряется (роутер сам проксирует заблокированное).

## 2. Целевое поведение

- Пользователь ведёт список **доверенных Wi-Fi (по SSID)** в приложении (вкладка Servers → секция «Доверенные сети»), с тумблером включения и пикером доступных сетей.
- При подключении/входе в доверенный SSID туннель **автоматически встаёт на паузу** с уведомлением и заметным блоком на главном экране (понятно, что VPN намеренно выключен и трафик идёт мимо приложения).
- При **уходе** из доверенной сети (LTE/другой Wi-Fi) туннель **поднимается сам**, в т.ч. в фоне.
- На паузе фоновая **проба** проверяет, реально ли доступны заблокированные ресурсы; если нет — мягкое предупреждение «в сети ограничения» + кнопка «Включить здесь» (override на сессию).
- Без разрешения на геолокацию фича **мягко деградирует** (не паузит, туннель работает как обычно, не падает).

## 3. Архитектура

### 3.1 Матчинг SSID — в Rust
Чистая строковая логика вынесена в [xr-core/src/trusted.rs](../../xr-core/src/trusted.rs) (`normalize_ssid`, `ssid_matches`) и покрыта 12 unit-тестами — потому что Android в этом проекте без автотестов (правило проекта), а нормализация имеет тонкости: снятие кавычек Android-обёртки (`"ssid"`), trim, регистронезависимое сравнение, отсев сентинелов `<unknown ssid>` / `0x` / пустых. JNI-мост — `nativeSsidMatches(current, String[])` и `nativeNormalizeSsid(raw)` в [xr-android-jni/src/lib.rs](../../xr-android-jni/src/lib.rs); обе функции чистые и работают независимо от того, запущен ли движок (нужно на паузе, когда движок остановлен).

### 3.2 Фаза `Paused`
В `XrVpnService.Phase` добавлена **`Paused`** — отдельно от `Idle`. Это принципиально: пауза ≠ пользовательский стоп, иначе авто-resume сломал бы ручное «выключить». Маппинг в `ConnectPhase.Paused` (VpnViewModel), кнопка на главном = Disconnect (полный стоп), отдельная карточка-индикатор.

**Пауза = `nativeStop` + снятие TUN** (`tearTunnelDown`), но сервис **остаётся foreground** и держит network-callback'и. Выбор в пользу полного teardown (а не «оставить TUN, заглушить relay») закрыт по факту: нет утечки трафика мимо паузы, а латентность resume (~секунды) приемлема. Поскольку TUN снят — **приложения не видят VPN** (это и есть корректное поведение на паузе).

Рефактор жизненного цикла: `bringTunnelUp()` / `tearTunnelDown()` (без unregister callback'ов) / `doPause()` / `requestPause()` / `requestResume()`. Все переходы туннеля сериализованы через `transitionMutex` (overlapping network-колбэки не должны поднимать и рвать туннель одновременно).

### 3.3 Вотчер сети
SSID-детект навешен на тот же `registerDefaultNetworkCallback`, что и C4-re-bind, чтобы **пауза и re-bind не срабатывали разом**. Вся логика в одном `maybeEvaluate(network, caps)`:
- **trusted + Connected** → `requestPause`;
- **не trusted + был реальный switch (`pendingSwitch`)** → `nativeOnNetworkChanged` (C4 re-bind);
- **Paused** → резюм только при **смене сети** на недоверенную (см. 3.4).

На connect — короткое ожидание первого SSID-вердикта (`firstCapsSignal`, ≤1.5с, armed ДО регистрации колбэка) + backstop после `Connected`, чтобы в доверенной сети не моргать полным коннектом.

### 3.4 Авто-resume — по СМЕНЕ сети, не по SSID
Резюм-решение завязано на **идентичность `Network`** (`pausedNetwork`), а не на разовое чтение SSID. Пока тот же `Network` — туннель остаётся на паузе, что бы Wi-Fi ни прислал в обновлениях capabilities. Резюмим при переходе на другую недоверенную сеть; на другую доверенную — остаёмся на паузе (retarget + перепроба); новый Wi-Fi без ещё прочитанного SSID — ждём. (См. §6.3.)

### 3.5 Проба ограничений
[RestrictionProbe.kt](../../xr-android/app/src/main/java/com/xrproxy/app/service/RestrictionProbe.kt): после паузы (с задержкой ~1.2с на устаканивание маршрутов) фоном TLS-проба 3 заведомо РКН-блокируемых хостов (`youtube/instagram/telegram/x/facebook`, с ротацией) по физической сети: DNS (ловит DNS-MITM `127.0.0.1`) + TCP:443 + TLS-handshake с SNI, таймаут 4с, кворум **≥2/3 недоступны = ограничения**. **Форсит IPv4** (роутерный TPROXY IPv4-only — см. §6.2). Это эвристика-предупреждение: надёжно ловит DPI/DNS-блок, geo-блок (app-level 403) — нет, и не должна.

### 3.6 Persistence
[TrustedNetworksRepository.kt](../../xr-android/app/src/main/java/com/xrproxy/app/data/TrustedNetworksRepository.kt) — SharedPreferences `"xr_proxy"`, ключи `trusted_networks` (JSON-массив SSID) + `trusted_networks_enabled`, по образцу `ServerRepository`. Сервис и ViewModel держат **свои инстансы** над одним process-wide backing map (сервис в том же процессе — без `android:process`), поэтому `apply()` из UI виден сервису мгновенно; сервис читает `activeTrustedSsids()` свежим на каждой смене сети.

### 3.7 Permission
`ACCESS_FINE_LOCATION` (кросс-версийный путь чтения SSID, API 29–34) + `NEARBY_WIFI_DEVICES`/`neverForLocation` (API 33+), runtime-запрос из UI (Compose `rememberLauncherForActivityResult`, `permissionEpoch` для перечитки статуса). Без разрешения/геолокации — `<unknown ssid>` → нет матча → мягкая деградация.

### 3.8 UI
- Секция [TrustedNetworksSection.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/trusted/TrustedNetworksSection.kt): список SSID, тумблер, удалить, **пикер** доступных сетей (`WifiManager.scanResults` + текущая, уже добавленные отфильтрованы) с ручным вводом-фолбэком для скрытых.
- Главный экран ([MainActivity.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt) `ConnectionSection`): при паузе — карточка «🛡 Доверенная сеть «SSID» · VPN на паузе» + (если проба сработала) оранжевый баннер ограничений + кнопка «Включить здесь».

## 4. Карта кода

| Файл | Роль |
|---|---|
| [xr-core/src/trusted.rs](../../xr-core/src/trusted.rs) | `normalize_ssid` + `ssid_matches` + 12 тестов |
| [xr-android-jni/src/lib.rs](../../xr-android-jni/src/lib.rs) | `nativeSsidMatches`, `nativeNormalizeSsid` |
| [jni/NativeBridge.kt](../../xr-android/app/src/main/java/com/xrproxy/app/jni/NativeBridge.kt) | external-объявления |
| [data/TrustedNetworksRepository.kt](../../xr-android/app/src/main/java/com/xrproxy/app/data/TrustedNetworksRepository.kt) | persistence + `activeTrustedSsids()` |
| [service/XrVpnService.kt](../../xr-android/app/src/main/java/com/xrproxy/app/service/XrVpnService.kt) | фаза Paused, `maybeEvaluate`, `doPause`/`requestPause`/`requestResume`, `bringTunnelUp`/`tearTunnelDown`, FLAG_INCLUDE_LOCATION_INFO, WifiManager-фолбэк, `pausedNetwork`, запуск пробы |
| [service/RestrictionProbe.kt](../../xr-android/app/src/main/java/com/xrproxy/app/service/RestrictionProbe.kt) | TLS-проба ограничений |
| [ui/trusted/TrustedNetworksSection.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/trusted/TrustedNetworksSection.kt) | UI секции + пикер |
| [ui/VpnViewModel.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt) | `trustedRepo`, `availableSsids()`, `resumeOnTrustedNetwork()`, `restrictedNetwork` |
| [ui/MainActivity.kt](../../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt) | карточка паузы, permission-лаунчер, проводка пикера |
| [AndroidManifest.xml](../../xr-android/app/src/main/AndroidManifest.xml) | `ACCESS_FINE_LOCATION`, `NEARBY_WIFI_DEVICES`, `ACCESS_WIFI_STATE` |

## 5. Открытые вопросы (как решены)
- **Пауза = stop+teardown TUN** (не «оставить TUN») — нет утечки, resume ~секунды приемлемо.
- **Авто-resume** — foreground-сервис + default-callback, работает в фоне.
- **Матчинг по SSID** (не BSSID) — для «дома» достаточно; ⚠️ известная слабость (см. §7).
- **Permission** — FINE_LOCATION как основной путь (надёжнее на всех версиях), NEARBY — компаньон на 33+.

## 6. Грабли device-verify (ВАЖНО для доработок)

Эти баги всплыли только на устройстве (`R5CY60312LV`, Samsung SM-S938B, Android 14+) и неочевидны — закладывай их в любые доработки SSID-детекта:

1. **SSID редактируется без `FLAG_INCLUDE_LOCATION_INFO`** (`7f0ad15`). На API 31+ SSID в `NetworkCapabilities.transportInfo` приходит `<unknown ssid>` даже при `ACCESS_FINE_LOCATION` + включённой геолокации, **если** `NetworkCallback` зарегистрирован без `FLAG_INCLUDE_LOCATION_INFO`. Конструктор `NetworkCallback(int)` есть только с API 31 → ветка по SDK. Без этого авто-пауза молча не работает (пикер при этом работает — он читает SSID через `WifiManager.scanResults`, другой путь).
2. **Проба ложно срабатывала на IPv4-only сети** (`3964b03`). `getAllByName` часто возвращает первым IPv6 (AAAA). Домашняя сеть фактически IPv4-only, роутерный TPROXY тоже IPv4 → IPv6-коннект таймаутил → ложное «ограничения». Фикс — **форсить IPv4** (как делает движок-резолвер). Проверено с MacBook в той же сети: все хосты доступны по IPv4, TLS <0.3с.
3. **Спонтанный авто-резюм на той же сети** (`bac94c4`). Резюм по любому `!trusted` ловил разовые `<unknown ssid>` в потоке `onCapabilitiesChanged` → туннель сам поднимался, хотя сеть не менялась. Фикс — резюм только по **смене `Network`** (`pausedNetwork`), глюки SSID на той же сети игнорируются.
4. **Не паузило при возврате домой с поднятым туннелем** (`53de7be`). Пока активен НАШ VPN, SSID в default-callback caps приходит пустым → `trusted=false` → вместо паузы re-bind, туннель оставался включённым. Фикс — фолбэк на `WifiManager.getConnectionInfo().ssid` (виден независимо от VPN), когда uplink — Wi-Fi, но caps-SSID пуст.

**Вывод для доработок:** чтение SSID имеет два независимых источника с разными свойствами — `NetworkCapabilities.transportInfo` (нужен FLAG, ломается при VPN-up) и `WifiManager` (VPN-независим, но сообщает ассоциированный Wi-Fi, не обязательно активный аплинк). Любая работа с SSID должна держать оба в голове.

## 7. Известные ограничения
- **SSID не уникален и подделываем (evil twin).** Совпадение имени в кафе/у соседа или вражеская точка с именем домашней сети → туннель встанет на паузу там, где не надо (downgrade защиты). Принятая слабость ради простоты «для дома». **Направление усиления:** матч по BSSID или паре SSID+BSSID (логика в `trusted.rs` → легко расширить тестами). При добавлении «текущей сети» можно заодно сохранять BSSID.
- **Проба — эвристика**: ловит DPI/DNS-блок, не geo-блок (app-level 403); набор хостов зашит в `RestrictionProbe.CANDIDATES`.
- **Зависит от геолокации**: выключенный системный GPS-тумблер ⇒ SSID не читается ⇒ фича не работает (молча).

## 8. План проверки (ручной; автотесты только для Rust-матчинга)
1. Добавить домашний SSID через пикер → подключиться в этой сети → **пауза** + карточка, без ложного баннера ограничений.
2. Уйти на LTE → туннель **сам поднимается**.
3. Вернуться домой (туннель поднят) → снова **пауза** (грабля §6.4).
4. Посидеть на паузе → **не должно** само переключаться (грабля §6.3).
5. Сеть без проксирующего роутера (реальные ограничения) → баннер «в сети ограничения» появляется законно.
6. Отозвать разрешение на геолокацию → фича не паузит, туннель работает, приложение не падает.

## 9. Вне скоупа
- BSSID-матчинг (см. §7) — отдельная доработка.
- Видимый live-статус «текущая сеть распознана как доверенная» в секции и предупреждение-диалог при Connect в доверенной сети — обсуждалось, не реализовано (после device-verify базовая авто-пауза + карточка закрыли потребность; вернуться при необходимости).
- Расписание/гео-условия помимо SSID.
