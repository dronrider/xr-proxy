# XR Proxy — архитектура

Единый источник правды о том, как устроено приложение. Обновляется при любых
изменениях, влияющих на публичные интерфейсы, топологию, протоколы, состав
компонентов или модель данных.

- Оперативные правила разработки — в [CLAUDE.md](../CLAUDE.md).
- Деплой на OpenWRT — в [OPENWRT.md](OPENWRT.md).
- Низкоуровневые планы доработок — в [lld/](lld/).

---

## 1. Назначение

XR Proxy — лёгкий обфусцированный прокси для обхода региональных блокировок.
Одна система, два класса клиентов:

1. **Сетевой (OpenWRT-роутер)** — прозрачно проксирует весь выбранный трафик
   LAN без настройки на конечных устройствах.
2. **Персональный (Android, в перспективе — iOS/desktop)** — работает на самом
   устройстве через системный VPN API, использует то же ядро.

Оба класса общаются с одним сервером на VPS по одному обфусцированному
протоколу.

## 2. Топология

```text
                               ┌────────────────────────────────┐
                               │  Control Plane (planned)       │
                               │  xr-hub (HTTPS)                │
                               │  • пресеты правил              │
                               │  • одноразовые инвайты         │
                               │  • Admin UI                    │
                               └──────────────┬─────────────────┘
                                              │ GET /presets/...
                                              │ GET /invite/<token>
                   ┌──────────────────────────┴──────────────────────────┐
                   │                                                     │
       ┌───────────▼───────────┐                         ┌───────────────▼──────────────┐
       │  OpenWRT router       │                         │  Персональные устройства      │
       │  xr-client            │                         │  Android: xr-android          │
       │  • TCP TPROXY         │                         │  (VpnService + xr-core)       │
       │  • UDP TPROXY         │                         │                              │
       └───────────┬───────────┘                         └───────────────┬──────────────┘
                   │                                                     │
                   │        obf. TCP tunnel + UDP relay                  │
                   └──────────────────────┬──────────────────────────────┘
                                          │
                             ┌────────────▼────────────┐
                             │  VPS                    │
                             │  xr-server              │
                             │  • :8443  TCP tunnel    │
                             │  • :9999  UDP relay     │
                             │  • DPI fallback HTTP    │
                             └────────────┬────────────┘
                                          │
                                          ▼
                                 Internet / blocked resources
```

**Control Plane (`xr-hub`)** — отдельный сервис на VPS (крейт `xr-hub`).
Раздаёт пресеты правил маршрутизации и обслуживает одноразовые инвайты для
первой настройки клиентов. HTTP API (`axum`) + встроенная Admin SPA (Vue 3).
TLS через `rustls`. Подробности — [lld/01-control-plane.md](lld/01-control-plane.md),
деплой — [HUB-DEPLOY.md](HUB-DEPLOY.md).

## 3. Состав репозитория

Cargo-workspace + Android-модуль:

| Путь | Роль |
|---|---|
| [xr-proto/](../xr-proto/) | Общая библиотека: wire-протокол, обфускация, UDP relay, routing, mux, конфиг. |
| [xr-core/](../xr-core/) | Платформо-независимое VPN-ядро для персональных клиентов (smoltcp, fake DNS, сессии, state, stats). |
| [xr-client/](../xr-client/) | Бинарь для OpenWRT. TCP TPROXY, UDP TPROXY, управление nftables. |
| [xr-server/](../xr-server/) | Бинарь для VPS. Туннельный сервер, UDP relay, DPI fallback. |
| [xr-android-jni/](../xr-android-jni/) | JNI-мост Kotlin ↔ xr-core. |
| [xr-android/](../xr-android/) | Android-приложение (Compose + MVVM), использует `xr-core` через JNI. |
| *planned* `xr-hub/` | Control-plane сервис (пресеты, инвайты, Admin UI). |

## 4. Компоненты

### 4.1 xr-proto — общая библиотека

Модули:

- [config.rs](../xr-proto/src/config.rs) — TOML-конфиги клиента/сервера. Ключевые
  структуры: `ClientConfig`, `ServerAddress`, `ObfuscationConfig`, `RoutingConfig`,
  `RoutingRule`, `ClientSettings`, `UdpRelayClientConfig`.
- [obfuscation.rs](../xr-proto/src/obfuscation.rs) — XOR с позиционными
  модификаторами и таблицами подстановки. Ключ задаётся base64; `modifier` и
  `salt` должны совпадать у клиента и сервера.
- [protocol.rs](../xr-proto/src/protocol.rs) — TCP-wire: `[Nonce:4B][Header:4B obfuscated][Padding][Payload obfuscated]`.
  `Codec` — верхнеуровневая оболочка поверх обфускатора.
- [routing.rs](../xr-proto/src/routing.rs) — `Router`, `Action::{Proxy,Direct}`,
  скомпилированные правила (exact / wildcard / CIDR / GeoIP).
- [udp_relay.rs](../xr-proto/src/udp_relay.rs) — wire-формат UDP relay:
  `[Nonce:4B][Obfuscated: type + dst + src_port + payload]`.
- mux — поверх TCP создаётся мультиплексированный поток (см. `MuxPool`,
  `MuxStream`). Это позволяет держать один живой обфусцированный туннель и
  гонять по нему множество логических соединений.

### 4.2 xr-core — ядро персонального клиента

Используется Android (через `xr-android-jni`) и, в перспективе, десктопными
клиентами. Полностью платформо-независимо, не содержит Android-API.

- [lib.rs](../xr-core/src/lib.rs) — реэкспорт модулей.
- [engine.rs](../xr-core/src/engine.rs) — `VpnEngine` (start/stop) и `VpnConfig`.
  Держит smoltcp-стек, `MuxPool`, обфускатор, роутер, fake DNS, статистику.
- [ip_stack.rs](../xr-core/src/ip_stack.rs) — `PacketQueue` (мост между TUN и
  smoltcp), `IpStack` (userspace TCP/IP).
- [dns.rs](../xr-core/src/dns.rs) — `FakeDns` в диапазоне 198.18.0.0/15 (RFC 2544).
  DNS-ответ подменяется fake-IP, и при TCP-SYN на этот IP ядро восстанавливает
  оригинальный домен для применения правил маршрутизации.
- [session.rs](../xr-core/src/session.rs) — `SessionContext`, `relay_session_with_domain()`.
  Решает `Action::Proxy` vs `Direct`, поднимает relay-task. `connect_protected()`
  защищает fd от петли через VPN (вызывает Kotlin-колбэк).
- [state.rs](../xr-core/src/state.rs) — `VpnState { Disconnected, Connecting,
  Connected, Disconnecting, Error(String) }` + `StateHandle` на базе
  `tokio::sync::watch`. Реактивная доставка смены состояния.
- [stats.rs](../xr-core/src/stats.rs) — `Stats` (atomic-счётчики без блокировок)
  + `recent_errors` (Mutex<Vec>). `snapshot()` → `StatsSnapshot`.

**Важно:** `relay_errors` (счётчик) и `recent_errors` (журнал строк) — два
независимых источника. В Android UI бадж и заголовок вкладки Log считают
WARN-строки прямо из `recent_errors` (см. §4.6), так что срез `entries.drain`
в Rust автоматически уменьшает и бадж; `relay_errors` остался только как
debug-метрика. Чтобы инвариант «бадж = число WARN в журнале» выполнялся,
в `session.rs` строго разделены уровни: отказы mux-стрима идут через
`add_relay_error` (WARN + счётчик), а шумный `mux relay for` — только через
`tracing::debug!`, не засоряя `recent_errors` и не вытесняя WARN'ы через
`drain(0..50)`.

### 4.3 xr-client — OpenWRT-клиент

- [main.rs](../xr-client/src/main.rs) — точка входа, загрузка конфига, запуск
  TCP-прокси и UDP-relay, обработка сигналов.
- [proxy.rs](../xr-client/src/proxy.rs) — прозрачный TCP-прокси: `accept →
  SO_ORIGINAL_DST → SNI extraction → route → relay/tunnel`.
- [routing.rs](../xr-client/src/routing.rs) — тонкая обёртка над `xr_proto::routing`.
- [redirect.rs](../xr-client/src/redirect.rs) — управление nftables/iptables
  (auto-setup, cleanup). Использует семейство `ip` (не `inet`, см. CLAUDE.md).
- [sni.rs](../xr-client/src/sni.rs) — извлечение SNI из TLS ClientHello.
- [udp_relay.rs](../xr-client/src/udp_relay.rs) — UDP TPROXY: `recvmsg` +
  `IP_ORIGDSTADDR`, relay на VPS, spoofed-responses через `IP_TRANSPARENT`.

xr-client работает с ядром на сыром уровне сокетов и nftables и **не использует
xr-core** — там другая модель (TUN/smoltcp vs TPROXY).

### 4.4 xr-server — VPS-сервер

- [main.rs](../xr-server/src/main.rs) — TCP listener + опциональный UDP relay.
- [handler.rs](../xr-server/src/handler.rs) — обработчик TCP-соединений:
  `deobfuscate → connect → relay с таймаутами`.
- [udp_relay.rs](../xr-server/src/udp_relay.rs) — flow table, `bind(src_port)`
  для NAT traversal, per-port receiver tasks.
- [fallback.rs](../xr-server/src/fallback.rs) — фальшивый HTTP-ответ на
  DPI-пробы.

### 4.5 xr-android-jni — JNI-мост

[lib.rs](../xr-android-jni/src/lib.rs) экспортирует 8 функций в
`com.xrproxy.app.jni.NativeBridge`:

| JNI-функция | Назначение |
|---|---|
| `nativeStart(tunFd, configJson)` | Инициализация engine, кэширование `protectSocket`, старт tokio runtime. Коды ошибок: -1 JSON, -2 config, -3 runtime, -4 engine. |
| `nativeStop()` | Graceful shutdown. |
| `nativeGetState()` → `String` | Одно из `Disconnected/Connecting/Connected/Disconnecting/Error`. |
| `nativeGetStats()` → `String (JSON)` | Снимок `StatsSnapshot` в JSON. |
| `nativeGetErrorLog()` → `String` | `recent_errors`, разделённый \n. |
| `nativeClearErrorLog()` | Очистка журнала и счётчика `relay_errors`. |
| `nativePushPacket(packet)` | Пакет TUN → `PacketQueue.inbound`. |
| `nativePopPacket()` → `byte[]?` | Пакет `PacketQueue.outbound` → TUN. |

**Обратный колбэк:** `NativeBridge.protectSocket(fd): Boolean` — статический
метод Kotlin, вызывается из Rust при создании исходящих сокетов. Реализация
вызывает `VpnService.protect(fd)` — это защищает сокеты от петли через TUN.

Конфиг передаётся одной JSON-строкой. Правила маршрутизации лежат внутри как
подстрока `routing_toml`, которую Rust парсит `toml::from_str` в
`RoutingConfig`.

### 4.6 xr-android — мобильное приложение

Kotlin + Jetpack Compose, Material3, MVVM без DI-фреймворка.

Ключевые файлы:

- [MainActivity.kt](../xr-android/app/src/main/java/com/xrproxy/app/ui/MainActivity.kt) —
  единственная Activity. Три вкладки: VPN / Log / Settings. Держит два
  `ActivityResultLauncher`: для системного диалога `VpnService.prepare()` и для
  runtime-запроса `POST_NOTIFICATIONS` (обязателен на API 33+ — иначе
  foreground-уведомление молча не показывается). Подписывается на
  `VpnViewModel.permissionRequest` и `VpnViewModel.messages` через
  `LaunchedEffect`, сообщения уходят в `SnackbarHost`.
- [VpnViewModel.kt](../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt) —
  настройки и фасад над сервисом. В `init` делает `bindService` к
  `XrVpnService` с экшеном `ACTION_BIND_INTERNAL` (без `BIND_AUTO_CREATE`) и
  подписывается на `service.stateFlow`. Входная точка Connect — `onConnectClicked()`,
  которая мгновенно переводит UI в `ConnectPhase.Starting`, затем либо вызывает
  `VpnService.prepare` и эмитит intent в `_permissionRequest`, либо стартует
  туннель через `actuallyStart()` + второй `tryBind(autoCreate=true)`, чтобы
  подхватить binder после `startForegroundService`. Результат диалога разрешения
  возвращается в `onPermissionResult(granted)`. Никакого native polling'а в VM
  больше нет — статистика приходит через `applyServiceState`.
- [XrVpnService.kt](../xr-android/app/src/main/java/com/xrproxy/app/service/XrVpnService.kt) —
  `android.net.VpnService` + единственный источник правды. Держит
  `LocalBinder`, `StateFlow<ServiceState>` (`Phase` + `StatsSnapshot?`),
  `CoroutineScope` с `SupervisorJob`. `startVpn` живёт в `scope` как suspend,
  после успешного `nativeStart` запускает `pollLoop()` (раз в секунду читает
  `nativeGetStats`, публикует snapshot, обновляет notification). `stopFromUi()`
  — единая команда стопа для VM через binder; `clearLog()` — тоже. `onBind`
  разветвляет: `ACTION_BIND_INTERNAL` → `LocalBinder`, иначе `super.onBind()`
  (штатный `BIND_VPN_SERVICE`). `onStartCommand(intent = null, ...)` делает
  `stopSelf()` → `START_NOT_STICKY`, чтобы не воскрешать зомби-сервис после
  process death. Foreground-уведомление: канал `IMPORTANCE_DEFAULT`,
  `CATEGORY_SERVICE`, `VISIBILITY_PUBLIC`, `setOnlyAlertOnce`, моно-иконка
  `ic_notification`, action «Отключить» через `PendingIntent` на `ACTION_STOP`,
  цвет из `R.color.brand_primary`. `foregroundServiceType="systemExempted"`.
- [NativeBridge.kt](../xr-android/app/src/main/java/com/xrproxy/app/jni/NativeBridge.kt) —
  объект-синглтон с `external fun`. Ссылка `current: XrVpnService?`
  обновляется в `XrVpnService.onCreate/onDestroy` (не из `startVpn`), что
  гарантирует актуальность колбэка `protectSocket` при пересоздании сервиса.

**Модель состояния на Android:**

- `ConnectPhase { Idle, NeedsPermission, Starting, Connecting, Connected, Stopping }`
  — единственный источник для рендера «Connect / Cancel / Disconnect» и
  крутилки. Computed `connected`/`connecting` сохранены для совместимости
  UI-кода, но внутри выводятся из `phase`.
- `recentErrors: List<String>` — единственный источник журнала и бадджа Log.
  Бадж/заголовок считают WARN-строки по критерию `" WARN "` (тот же, что
  `colorizeLog`). `relayErrors: Long` осталась только как debug-метрика в
  статистике, UI-бадж её не читает. Старое поле `errorLog: String` и метод
  `refreshLog()` удалены.

Хранилище настроек — SharedPreferences `xr_proxy`.

## 5. Протоколы

### 5.1 TCP туннель

```
[Nonce:4B][Header:4B (obf)][Padding:N][Payload (obf)]
```

- `Nonce` — случайный, используется обфускатором как дополнительный материал.
- `Header` — длина payload + метаданные, обфусцирован.
- `Padding` — случайный заполнитель для размазывания паттернов по размеру.
- `Payload` — полезная нагрузка, обфусцирована.

Поверх одного TCP-соединения работает **mux**: один живой обфусцированный
канал, внутри — множество логических стримов (`MuxStream`). Клиент (xr-core или
xr-client) держит `MuxPool`, который переиспользует туннель между сессиями и
умеет переподключаться.

### 5.2 UDP relay

```
[Nonce:4B][Obfuscated: type + dst + src_port + payload]
```

Клиент пересылает UDP-пакеты LAN → VPS → Интернет. Ответы возвращаются от VPS
клиенту и спуфятся с IP оригинального сервера (через `IP_TRANSPARENT`) — это
нужно игровым приставкам, которые проверяют адрес источника ответа.

### 5.3 Control Plane HTTP

Реализовано в крейте `xr-hub`. API под префиксом `/api/v1`:

**Публичные эндпоинты:**
- `GET /api/v1/presets` — список `PresetSummary` (имя, версия, дата, кол-во правил). Поддержка `ETag`.
- `GET /api/v1/presets/:name` — полный `Preset` с правилами. `304 Not Modified` по `If-None-Match`.
- `GET /api/v1/invite/:token` — `InvitePayload` (полный конфиг подключения). Одноразовый → `410 Gone` при повторе.
- `GET /api/v1/public-key` — публичный ключ ed25519 для проверки подписей.

**Admin (Bearer-token):**
- `POST/PUT/DELETE /api/v1/admin/presets` — CRUD пресетов, автоподпись при наличии ключа.
- `GET/POST/DELETE /api/v1/admin/invites` — управление инвайтами.

Admin SPA встроена в бинарь через `rust-embed`. Подробности — [lld/01-control-plane.md](lld/01-control-plane.md).

## 6. Конфигурация и правила маршрутизации

### 6.1 Состояние сейчас

Модель — **плоский список правил** (`RoutingConfig { default_action, rules }`):

```toml
[routing]
default_action = "direct"

[[routing.rules]]
action = "proxy"
domains = ["youtube.com", "*.youtube.com", "*.googlevideo.com"]
ip_ranges = ["91.108.56.0/22", "2001:b28:f23d::/48"]
```

Правила компилируются один раз в `CompiledRule` и применяются по порядку в
`Router::resolve()`. Поддержка: exact, wildcard (`*.domain`), CIDR (IPv4/IPv6),
GeoIP (за feature-flag).

На роутере конфиг лежит в `/etc/xr-proxy/config.toml`, на Android —
генерируется в `VpnViewModel.buildRoutingToml()` из захардкоженного пресета +
ручной ввод в режиме `custom`.

### 6.2 Пресеты и override'ы

- Пресеты хранятся централизованно в `xr-hub` (файлы JSON на диске),
  версионируются, опционально подписываются ed25519.
- Клиент (OpenWRT и Android) указывает `[hub] preset = "russia"` и локальные
  `[[routing.rules]]` как override'ы с более высоким приоритетом.
- При старте клиент делает `GET /api/v1/presets/:name` (forced fetch, timeout 2 с),
  кэширует результат локально. Фоновая задача раз в `refresh_interval_secs`
  сверяет версию по `ETag`. Обновлённые правила применяются при следующем старте.
- `Router::from_merged(overrides, preset, geoip)` — overrides-правила проверяются
  первыми, пресет — как fallback, `default_action` берётся из overrides.
- Если хаб недоступен — клиент работает с кэшом или только с локальными правилами.

## 7. Жизненный цикл соединения

### 7.1 xr-client (OpenWRT)

1. Старт: читает TOML, поднимает TCP listener + UDP TPROXY socket, ставит
   nftables-правила перенаправления.
2. Входящее TCP-соединение (TPROXY): `SO_ORIGINAL_DST` → SNI extraction →
   `Router::resolve(host, ip)` → либо `MuxPool` до VPS, либо прямое соединение.
3. Входящий UDP: `recvmsg` + `IP_ORIGDSTADDR` → UDP-relay до VPS → spoofed-ответ.
4. Стоп: cleanup nftables, закрытие MuxPool. Всё управляется procd + watchdog
   (см. [deploy/](../deploy/)).

### 7.2 xr-android

1. `VpnViewModel.init` делает `bindService` к `XrVpnService` с
   `ACTION_BIND_INTERNAL` (без `BIND_AUTO_CREATE`). Если сервис уже жив —
   `onServiceConnected` сразу мапит `service.stateFlow` в `VpnUiState`, и UI
   догоняет реальное состояние без действий пользователя. Если нет — VM
   остаётся в `ConnectPhase.Idle`.
2. Пользователь нажимает **Connect** → `onConnectClicked()`:
   - Мгновенно `phase = Starting`, кнопка показывает крутилку.
   - Если не заполнены `serverAddress`/`obfuscationKey` — Snackbar, возврат в
     `Idle`.
   - `VpnService.prepare(app)`: `null` → `actuallyStart()`; non-null → `phase =
     NeedsPermission`, intent эмитится в `permissionRequest`, `MainActivity`
     запускает системный диалог.
3. `MainActivity` всегда прокидывает результат диалога в
   `viewModel.onPermissionResult(granted)` — `RESULT_OK` → `actuallyStart()`,
   иначе Snackbar «VPN-разрешение не получено» и возврат в `Idle`.
4. `actuallyStart()` → `startForegroundService(ACTION_START, configJson)` +
   повторный `tryBind(autoCreate = true)` для ride-out гонки между стартом
   сервиса и подключением binder'а.
5. `XrVpnService` в suspend-`startVpn`: `Phase.Preparing` →
   `Builder().establish()` → `Phase.Connecting` → `NativeBridge.nativeStart(fd,
   cfg)` → поднимает TUN read/write-потоки → `Phase.Connected`. Каждый переход
   публикуется в `stateFlow`, и `updateNotification()` переотрисовывает
   foreground-уведомление.
6. `pollLoop()` внутри `scope` раз в секунду читает `nativeGetState()` +
   `nativeGetStats()` → строит `StatsSnapshot` → публикует в `stateFlow`. VM
   мирорит snapshot в `VpnUiState`. Это единственный источник статистики и
   журнала `recentErrors` для UI.
7. **Stop** (`viewModel.disconnect()` → `boundService.stopFromUi()` или
   pending-intent action «Отключить» из уведомления → `ACTION_STOP` →
   `stopFromUi()`): `Phase.Stopping` → `nativeStop()` → закрытие TUN →
   `Phase.Idle` → `stopForeground(STOP_FOREGROUND_REMOVE)` → `stopSelf()`.
8. `onStartCommand(intent = null)` после `START_STICKY`-рестарта делает
   `stopSelf()` + `START_NOT_STICKY`: восстановление живого туннеля после
   process death — вне скоупа, зато без зомби-сервиса в foreground.

## 8. Наблюдаемость

- **Stats.** Все счётчики — atomics без блокировок, читаются по snapshot.
  Снимок сериализуется в JSON для Kotlin. В UI отображаются bytes up/down,
  connections, uptime, а также debug-метрики (DNS, SYNs, smol, relay_errors).
- **Logs.** Два источника:
  - `recent_errors: Mutex<Vec<String>>` — последние ~200 записей, старые
    обрезаются пачками по 50. Читаются через `StatsSnapshot` (поле `errors`
    в JSON), т.е. теми же вызовами `nativeGetStats()`, что и метрики.
    Android UI показывает журнал и бадж Log из этого списка.
    `nativeGetErrorLog()` оставлен как JNI-экспорт для совместимости, но
    актуальная Android-реализация его не использует.
  - `relay_errors: AtomicU64` — счётчик ошибок relay-задач. На Android
    остался только как debug-метрика в статистике; UI-бадж вкладки Log его
    не читает.
- **Серверные логи.** Нет централизованного сбора; пишется в stdout/stderr,
  procd/systemd забирает.
- **Crash log на OpenWRT.** Watchdog сохраняет `/etc/xr-proxy/crash.log`
  (последние 50 КБ, включает dmesg OOM, фрагмент logread, свободную память).

Поиск, auto-follow и скачивание журнала на Android — в LLD-03.

## 9. Запланированные доработки

Каждая крупная доработка оформлена в виде LLD в [docs/lld/](lld/) со
статусом `Draft / In Progress / Implemented`.

**Порядок имплементации зафиксирован** — каждый LLD берётся в работу в
отдельном чате; номера шагов соответствуют порядку реализации, а не
нумерации LLD:

| Шаг | LLD | Область | Зависит от | Статус |
|---|---|---|---|---|
| 1 | [02-android-reliability.md](lld/02-android-reliability.md) | Connect / state hydration / бадж / foreground notification. Задаёт базу для всех остальных Android-LLD (binder, `ConnectPhase`, `recentErrors` как единый источник). | — | Implemented |
| 2 | [01-control-plane.md](lld/01-control-plane.md) | `xr-hub`: пресеты, одноразовые инвайты, Admin SPA (Vue + PrimeVue), подпись ed25519, HTTPS через axum-server. Независим от Android, катается параллельно. | — | Implemented |
| 3 | [06-android-visual.md](lld/06-android-visual.md) | Иконка «щит со стрелой-молнией», тёмная палитра navy + cyan, анимация `ShieldArrowIcon` по фазам, перекомпоновка статистики с live-скоростью, Debug за аккордеоном. Параллелится с шагом 2. | Шаг 1 | Implemented |
| 4 | [04-onboarding-qr-uri.md](lld/04-onboarding-qr-uri.md) | Welcome-экран, Google Code Scanner, HTTPS deep link, экран подтверждения инвайта, TOFU public key. | Шаги 1-3 | Draft |
| 5 | [03-android-logs-ux.md](lld/03-android-logs-ux.md) | Sticky toolbar, substring + regex поиск, auto-follow, скачивание через SAF. | Шаг 1 | Draft |
| 6 | [05-android-rules-editor.md](lld/05-android-rules-editor.md) | Четвёртая вкладка Rules, read-only пресет + упорядоченные user overrides, TOML-preview модал, удаление хардкода `PRESET_RUSSIA`. Закрывает всю пачку. | Шаги 1, 2, 4 | Draft |

## 10. Как поддерживать этот документ

1. **Работа ведётся в отдельных чатах.** Один чат — один LLD. В начале
   нового чата в первую очередь прочитать: `CLAUDE.md`, релевантный
   `lld/XX-....md` и разделы `ARCHITECTURE.md`, на которые он ссылается.
2. После реализации LLD помечай его `Implemented` в таблице §9 и
   **переноси релевантные факты** из LLD в соответствующие разделы
   `ARCHITECTURE.md` (состав крейтов, новые протоколы/эндпоинты,
   изменение модели конфигурации).
3. Не дублируй в `ARCHITECTURE.md` детали, которые легко извлечь из кода:
   имена приватных функций, сигнатуры, конкретные строки. Достаточно
   карты и ссылок вида [file.rs:line](../path#L42).
4. Любое изменение, затрагивающее: состав крейтов, wire-протокол, формат
   конфига, топологию деплоя, модель состояния клиента, — обязано
   отражаться здесь **в том же коммите**.
5. Если документ начал расходиться с кодом — чинить этот документ, а не
   код.
