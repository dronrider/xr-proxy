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
| [xr-hub/](../xr-hub/) | Control-plane сервис (пресеты, инвайты, шары, Admin UI). |
| [xr-share/](../xr-share/) | Агент файлообмена (LLD-19, LLD-28): раздаёт директории и файлы (чтение по умолчанию, запись по write-привязке инвайта), подписывает манифест, проверяет токены офлайн. |
| [xr-relay/](../xr-relay/) | Слепой транзит шар за NAT (LLD-23, XR-103): реестр агентов, регистрация, проверка relay-токенов, сплайс без чтения содержимого. |
| [xr-setup/](../xr-setup/) | Идемпотентный установщик (LLD-13, XR-015/XR-177): каркас шагов check/apply/verify; server-профиль поднимает xr-server и xr-hub на чистом VPS и заканчивает инвайтом; router-профиль приводит OpenWRT к раздающему обход роутеру (procd+watchdog, dnsmasq на Quad9, отложенный SSID, enroll в реестр LLD-17). |

## 4. Компоненты

### 4.1 xr-proto — общая библиотека

Модули:

- [accept.rs](../xr-proto/src/accept.rs) это общий accept-цикл листенеров
  (xr-server, xr-client, xr-relay). Ошибка `accept()` сначала классифицируется:
  проходящая (EMFILE и родня, ECONNABORTED, EINTR) даёт `warn` и паузу 100 мс,
  после чего цикл живёт дальше, а незнакомый errno считается фатальным, и цикл
  выходит с ошибкой. Тем же выходом кончаются сто проходящих ошибок подряд:
  дескрипторы за десять секунд не освободились, крутиться дальше молча вредно,
  процесс поднимет procd или systemd. До этого нехватка дескрипторов на всплеске
  убивала листенер целиком, хотя проходит она за миллисекунды (XR-209).
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
- [sni.rs](../xr-proto/src/sni.rs) достаёт SNI из TLS ClientHello. Разбор идёт по
  байтам чужого сокета, поэтому всё сомнительное отбрасывается: обрезанный рекорд
  и имя длиннее `MAX_DOMAIN_LEN` (предел длины домена в Connect, 255) дают `None`,
  соединение тогда маршрутизируется по IP, как любое не-TLS, а сам отказ пишется
  в `warn` (при дефолтном `log_level` уровнем ниже его бы не было видно вовсе).
  Тот же предел стоит у второго производителя имени, разбора
  DNS-вопроса в [dns.rs](../xr-core/src/dns.rs): у Connect на длину домена один
  байт, и всё, что в него не влезает, должно отсекаться до кодирования.
  Имя снимается не с одной порции первых байт: постквантовый обмен ключами
  (`X25519MLKEM768`) раздул key_share, и ClientHello перестал влезать в сегмент,
  а на обрезанном начале имени нет и соединение уходило действием по умолчанию.
  `client_hello_record_len` отдаёт по заголовку рекорда, сколько байт ждать
  всего, и оба места съёма дочитывают до этой длины: xr-client повторяет `peek`
  (байты остаются в сокете, релей прочитает их сам), xr-core склеивает
  сообщения из канала в `initial_data`. Ожидание ограничено с двух сторон, по
  времени (полсекунды сверх обычного ожидания первых байт) и по объёму
  (`MAX_CLIENT_HELLO`, предел TLSPlaintext), иначе молчащий клиент или
  соврамшая длина в заголовке подвешивали бы соединение. Не собравшийся рекорд
  оставляет решение по IP, но пишет об этом в `warn`: раньше от него в логе был
  только прочерк в поле SNI.
- [udp_relay.rs](../xr-proto/src/udp_relay.rs) — wire-формат UDP relay:
  `[Nonce:4B][Obfuscated: type + dst + src_port + payload]`.
- mux — поверх TCP создаётся мультиплексированный поток (см. `MuxPool`,
  `MuxStream`). `MuxPool` держит N параллельных TCP-туннелей (`mux_pool_size`,
  default 4); стримы балансируются round-robin, при обрыве слота open_stream
  failover'ит на следующий, мёртвый слот переподнимается лениво. Это убирает
  HoL-blocking одного TCP — потеря пакета на одном туннеле не тормозит остальные.
  Id стримов делятся по чётности: инициатор соединения (клиент) берёт нечётные,
  акцептор (сервер/relay) чётные, поэтому реверс-стримы relay->агент не
  конфликтуют с прямыми (XR-103). `MuxStream::into_io()` даёт
  `AsyncRead + AsyncWrite` поверх стрима (для hyper на агенте и слепого сплайса
  на relay). Стримы под оконным flow control (LLD-27, XR-115): отправитель шлёт
  Data в пределах окна (1 МиБ на стрим) и засыпает на исчерпанном, приёмник
  возвращает кредит кадром `WindowUpdate` по мере вычитывания, поэтому быстрый
  отправитель с медленным потребителем упирается в backpressure, а не
  переполняет per-stream канал (reader такой стрим убивал, тело обрывалось).
  Окно согласуется байтом флагов в `MuxInit`/`MuxInitAck` (`MuxCaps`); пара со
  старым пиром живёт без окна по-старому, лок-степ выката не нужен.
  Пишет в туннель один writer-таск, и планов записи у него два: контрольный
  (`Connect`, `ConnectAck`, `Ping`, `Pong`) и балк (`Data` и `Close`). Контрольный
  сливается первым, поэтому ConnectAck нового стрима не залипает за мегабайтами
  чужой загрузки (XR-086). `Close` стрима сознательно остаётся в балк-плане, в
  общем FIFO со своими `Data`: приоритетным планом он обгонял ещё не записанный
  хвост, пир снимал стрим по нему и молча выбрасывал догоняющие кадры, а ответ
  апстрима, закрывшего соединение сразу за последним блоком, приезжал обрезанным
  (XR-241). Отправитель закрывает стрим его же половиной записи
  (`MuxWriteHalf::close_with`, причина Close уезжает там же в payload);
  `Multiplexer::send_stream_close` остаётся для отказа по входящему `Connect`,
  когда стрима ещё нет.
- [relay_client.rs](../xr-proto/src/relay_client.rs) (фича `share`) вводит
  клиент relay для потребителя (LLD-23): mux к relay, `open_relay_stream` (Connect на
  псевдо-таргет `xr-relay:connect`, hello с relay-токеном первым Data-кадром,
  ждёт байт `OK`), `LoopbackForwarder` (listener на `127.0.0.1:0`, каждое
  принятое соединение становится relay-стримом; HTTP-стек потребителя не
  меняется). Псевдо-таргеты `xr-relay:*` не резолвятся в сеть, SSRF исключён
  конструктивно.
- [share.rs](../xr-proto/src/share.rs) relay-типы (LLD-23): `RelayToken`
  (домен `xr-relay-token`, привязан к share_id+agent_pubkey), `RelayDescriptor`
  / `RelayObf` (адрес + обфускация, `codec()` строит общий `Codec`),
  `RelayGrant` (relay-плечо гранта), `RelayRegister` (challenge-response
  регистрации агента: мандат хаба + подпись nonce identity-ключом), признак
  `via_relay` в `ShareRecord`. Подпись/проверка за фичой `share`.
- [share.rs](../xr-proto/src/share.rs) write-scope (LLD-28): у `ShareToken`
  появилось поле `scope` (OAuth-строка имён через пробел, `SCOPE_READ` /
  `SCOPE_WRITE`), подпись перешла на v2 со строкой скоупа внутри (формат ломаем,
  v1 не проходит), `verify_share_token` принимает требуемое имя и проверяет
  вхождение через `scope_contains`. Признак `writable` в `ShareRecord`
  (мастер-рубильник у записи хаба) и `write_share_ids` в `Invite` (право записи у
  пары шара-инвайт). Хаб минтит `share:write` единственным путём, в грантах
  `invite_shares` при write-привязке и writable-записи; ссылка и `/share/mint`
  дают только `share:read`.
- [server_pool.rs](../xr-proto/src/server_pool.rs) вводит `ServerPool`
  (LLD-10): пул *серверов* поверх нескольких `MuxPool` (по одному на VPS), строгий
  primary/backup по приоритету (не балансировка). `open_stream` идёт в пул
  активного сервера; отказ активного (breaker C1) переключает на следующий
  здоровый, `Err` наружу только когда исчерпан весь пул (тогда клиент уводит
  соединение в Direct). На primary возвращает фоновый `health_loop` с
  hold-down (default 60с непрерывного up) против флаппинга. Health меряется
  не только живостью туннеля: mux считает исходы relay по live-трафику
  (успех = первый Data-кадр стрима; сбой = причина resolve/connect в payload
  Close от сервера), и сервер, у которого туннель жив, а relay массово падает
  (мёртвый DNS/egress на VPS, XR-094), `health_loop` помечает Down и уводит
  трафик на резерв; возврат идёт обычным failback, мигание гасится
  анти-флаппинг-штрафом XR-082. Энергопрофили
  `PoolProfile`: роутер `router()` (тёплые резервы, проба каждые 15с),
  Android `mobile()` (холодный backup, пробер живёт только пока активен
  резерв, поэтому в здоровом простое ни одного лишнего пробуждения радио,
  XR-068). Список серверов роутер берёт из `[[servers]]` в конфиге (legacy
  `[server]` читается как пул из одного), Android держит `endpoints` внутри
  `ServerProfile` и наполняет их руками или полем `servers` подписанного
  инвайт-payload'а.
- [invite_url.rs](../xr-proto/src/invite_url.rs) — парсер invite-ссылок
  для Android onboarding (LLD-04): `InviteLink::{Https, Custom}`,
  `parse_invite_link`, `build_https_url`. Принимает `https://<hub>/invite/<token>`
  (основной формат QR) и `xr://invite/<token>?hub=<host>` (кастомная схема
  для deep link). Валидирует токен (base64url 22 chars), отсекает
  loopback/private хосты.

### 4.2 xr-core — ядро персонального клиента

Используется Android (через `xr-android-jni`) и, в перспективе, десктопными
клиентами. Полностью платформо-независимо, не содержит Android-API.

- [lib.rs](../xr-core/src/lib.rs) — реэкспорт модулей.
- [engine.rs](../xr-core/src/engine.rs) — `VpnEngine` (start/stop) и `VpnConfig`.
  Держит smoltcp-стек, `ServerPool` (пул серверов, внутри `MuxPool` на
  каждый), обфускатор, роутер, fake DNS, статистику. `on_network_changed`
  ресайклит весь пул и возвращает активность primary'ю.
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
  + `add_log`/`add_warn`/`add_error`, они же пишут запись в журнал.
  `snapshot()` -> `StatsSnapshot`.
- [journal.rs](../xr-core/src/journal.rs): единый журнал приложения (XR-042),
  общий буфер на все источники, хвост в памяти для вкладки Log и персист с
  ротацией. Формат строки и общий стиль сообщения описаны в шапке модуля,
  подробности в разделе 8.
- [onboarding.rs](../xr-core/src/onboarding.rs) — one-shot HTTP-вызовы
  xr-hub для Android onboarding (LLD-04): `fetch_invite_info` (GET,
  без consume) и `apply_invite` (POST `/claim` → `InvitePayload` + TOFU
  `/public-key` + pre-warm preset cache через `PresetCache::write_to_disk`).
  Живёт рядом с `presets.rs`, чтобы переиспользовать тот же reqwest-клиент
  и формат кэша; JNI-обёртки в `xr-android-jni` лишь прокидывают вызовы.
  Claim и запрос сведений несут ключ установки в `X-Claim-Id` (XR-216,
  раздел 5.3), чтобы сорванный разбор ответа не сжигал одноразовый инвайт
  впустую, а экран подтверждения пускал владельца ключа к повтору.

**Важно:** `relay_errors` (счётчик) и хвост журнала это два независимых
источника. Бадж вкладки Log в Android UI считается прямо по строкам хвоста и
разбит по уровню (info/warn/err), заголовок фильтра берёт из него WARN (см.
раздел 4.6), а `relay_errors` остался только debug-метрикой в статистике.
Отсюда требование к местам записи: уровень выбирается по смыслу события, иначе
пользователь видит в бадже неверную картину. Отказы relay-задач классифицируются
в `engine.rs` на `add_warn`/`add_error` (счётчик плюс WARN или ERROR в журнале),
а выбранный путь соединения (`через прокси: ...`, `напрямую: ...`) идёт через
`add_log` на INFO.

### 4.3 xr-client — OpenWRT-клиент

- [main.rs](../xr-client/src/main.rs) — точка входа, загрузка конфига, запуск
  TCP-прокси и UDP-relay, обработка сигналов.
- [proxy.rs](../xr-client/src/proxy.rs) — прозрачный TCP-прокси: `accept →
  SO_ORIGINAL_DST → SNI extraction → route → relay/tunnel`.
  Извлечение SNI и маршрутизация берутся напрямую из `xr_proto` (`sni`,
  `routing`), своих обёрток у клиента нет.
- [redirect.rs](../xr-client/src/redirect.rs) — управление nftables/iptables
  (auto-setup, cleanup). Использует семейство `ip` (не `inet`, см. CLAUDE.md).
  Каждый VPS пула выводится из перехвата не целиком, а двумя правилами: всё,
  кроме 80 и 443 (ssh и служебные порты сервера, туннельный порт при обычном
  его значении сюда же), и отдельно туннельный порт, который нужен, только
  когда туннель сидит на web-порту.
  Web-порты остаются под перехватом: сайты на том же VPS должны уходить через
  прокси, иначе их SNI видит провайдер. Генерация правил вынесена в чистые
  `build_nft_ruleset` и `build_ipt_rules`, они и покрыты тестами.
- [udp_relay.rs](../xr-client/src/udp_relay.rs) — UDP TPROXY: `recvmsg` +
  `IP_ORIGDSTADDR`, relay на VPS, spoofed-responses через `IP_TRANSPARENT`.

xr-client работает с ядром на сыром уровне сокетов и nftables и **не использует
xr-core** — там другая модель (TUN/smoltcp vs TPROXY).

### 4.4 xr-server — VPS-сервер

- [main.rs](../xr-server/src/main.rs) — TCP listener + опциональный UDP relay.
- [handler.rs](../xr-server/src/handler.rs) — обработчик TCP-соединений:
  `deobfuscate → connect → relay с таймаутами`.
- [udp_relay.rs](../xr-server/src/udp_relay.rs) это flow table, `bind(src_port)`
  для NAT traversal и таск с очередью на каждый поток (см. 5.2).
- [fallback.rs](../xr-server/src/fallback.rs) — фальшивый HTTP-ответ на
  DPI-пробы.

### 4.5 xr-android-jni — JNI-мост

[lib.rs](../xr-android-jni/src/lib.rs) экспортирует в
`com.xrproxy.app.jni.NativeBridge` два набора функций — engine-control
и onboarding:

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
| `nativeParseInviteLink(raw)` → `String (JSON)` | Парсинг invite-URL (LLD-04). Успех: `{kind,hub_url,token}`, ошибка: `{error}`. |
| `nativeFetchInviteInfo(hub_url, token, cacheDir, timeoutMs)` -> `String (JSON)` | GET `/api/v1/invite/<token>` отдаёт `InviteInfo` (без consume). `cacheDir` тот же, что у Apply: оттуда берётся ключ установки для `X-Claim-Id` (XR-216). |
| `nativeApplyInvite(hub_url, token, preset, cacheDir, timeoutMs)` → `String (JSON)` | Claim + TOFU public-key + pre-warm preset. Одноразовый `tokio::runtime::Runtime` на вызов. |
| `nativeCheckUpdate(hubUrl, currentCode, pinnedKeyB64, timeoutMs)` → `String (JSON)` | LLD-12. Fetch + verify манифеста pinned release-ключом. `{available, manifest?, error?}`. |
| `nativeVerifyApk(path, sha256Hex)` → `Boolean` | LLD-12. Потоковая SHA-256 скачанного APK против манифеста. |

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
- [ui/onboarding/](../xr-android/app/src/main/java/com/xrproxy/app/ui/onboarding/) —
  экраны онбординга (LLD-04): `WelcomeScreen` (три кнопки), `PasteLinkDialog`,
  `InviteConfirmScreen` с live TTL-countdown'ом, `QrScanner` — suspend-обёртка
  над Google Code Scanner (`play-services-code-scanner`, system UI без
  `CAMERA`). Deep link: `AndroidManifest.xml` перехватывает `https://*/invite/*`
  и `xr://invite/*` без `autoVerify` — хаб self-hosted, единого домена нет.
- [ui/components/](../xr-android/app/src/main/java/com/xrproxy/app/ui/components/) это
  переиспользуемые куски UI (снекбар, иконки, карточки статистики). Здесь же
  `XrPullToRefresh`, обёртка над `PullToRefreshBox`.

**Обновление списков.** Любой экран, где список приходит из сети и может быть
перезапрошен, заворачивает свой скролл-контейнер в `XrPullToRefresh` (XR-181):
свайп-вниз обновляет, это жест по умолчанию, а не кнопка-исключение. Флаг
загрузки экрана служит и индикатору жеста, поэтому отдельный инлайн-спиннер не
нужен, а кнопка «Обновить» в шапке (если есть) остаётся дублёром жеста.
Исключение это первая загрузка пустого списка (кэша ещё нет): маленького
индикатора жеста мало, поэтому там держим явный центральный спиннер, пока не
приедут первые данные. Сейчас так сделаны список шар по инвайту, проводник шары
и карточка пресета правил;
новый сетевой список подключает жест сразу, а не ждёт отдельной задачи. Экраны
без сетевого источника (главная с кнопкой Connect, живой журнал) жест не несут.

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
- `OnboardingState { ShowingWelcome, Loading(hubUrl), ConfirmInvite(...),
  InviteError(...), Completed }`
  — параллельный StateFlow (LLD-04). Рендер MainActivity до `Completed`
  подменяет главный Scaffold onboarding-экранами; переход в `Completed`
  происходит после успешного `applyInvite` или при ручной настройке.
  `initialOnboardingState()` смотрит на prefs: если пусты `server_address`
  и `hub_url`, показываем Welcome.

Хранилище настроек — SharedPreferences `xr_proxy`. Новые ключи от LLD-04:
`hub_url`, `hub_preset`, `trusted_public_key` — пишутся при Apply инвайта,
читаются в `buildConfigJson` и включают в движке PresetCache +
периодический sanity-check раз в 5 минут. Кэш пресета живёт в
`filesDir/presets/<name>.json`.

### 4.7 xr-relay: слепой транзит шар за NAT (LLD-23, XR-103)

Отдельный сервис на тех же VPS, что и прокси (не хаб, не xr-server: юр-чистота
хаба и другая модель угроз прокси-выхода). Собран из тех же кирпичей `xr-proto`
(Codec, Multiplexer, паттерны accept/semaphore). Байты не читает и не хранит.

- [config.rs](../xr-relay/src/config.rs) описывает блок `[relay]`: адрес/порт,
  обфускация (общая с деплоем), `hub_pubkey` (проверка мандатов и токенов
  офлайн, приватного ключа хаба у relay нет), лимиты.
- [registry.rs](../xr-relay/src/registry.rs) вводит `AgentRegistry`
  (`agent_pubkey -> mux`, вытеснение дубля с глушением старого mux,
  generation-guard на снятии), `Counters` (байты per share, §2.6), `IpCaps`
  (кап регистраций с одного IP).
- В [lib.rs](../xr-relay/src/lib.rs) `handle_connection` различает роль
  соединения по первому стриму: `xr-relay:register` (агент, challenge-response,
  реестр, стрим-liveness) против `xr-relay:connect` (потребитель, hello с
  relay-токеном, поиск агента, реверс-стрим `xr-relay:reverse`, слепой сплайс
  через `copy_bidirectional`). Агент офлайн -> Close с `CLOSE_REASON_AGENT_OFFLINE`,
  исчерпанные транзитные слоты -> `CLOSE_REASON_RELAY_BUSY`.

Сигналинг на хабе: блок `[relay]` в конфиге, признак `via_relay` у шары,
дескриптор relay агенту (ответы `exchange`/`add`) и потребителю (relay-плечо в
гранте с минтом `RelayToken`). Прямых адресов у шары теперь несколько (XR-050):
`ShareRecord.addr` это публичный/DDNS вход, `addrs` это LAN-адреса агента (агент
определяет свой адрес UDP-connect-трюком без отправки пакета и шлёт в `share/add`,
хаб чистит и дедупит). `candidate_addrs` гранта отдаёт их LAN-адресами вперёд.
Потребитель перебирает прямые адреса по очереди (LAN раньше публичного, чтобы в
своей сети идти по LAN-IP без hairpin), relay последним.

Оконечный E2E-TLS (LLD-23 §2.3) поверх сплайса: агент серверит реверс-стримы
через тот же axum-роутер по identity-TLS (self-signed сертификат из ed25519
identity-ключа, rcgen на ring), потребитель проверяет не CA-цепочку, а
`SPKI == agent_pubkey` из гранта (кастомный rustls-verifier, имя хоста
игнорируется). Relay видит только шифртекст, подмена сертификата ломает пиннинг.

- [relay_tls.rs](../xr-proto/src/relay_tls.rs) (фича `relay-tls`) даёт verifier
  и билдеры rustls-конфигов на ring; rustls уже в дереве через reqwest,
  кросс-сборка не страдает. Генерация сертификата (rcgen) сюда не тащится.
- В [relay.rs](../xr-share/src/relay.rs) (фича `relay`, default off) агент держит
  исходящий mux к relay с экспоненциальным бэкофом, регистрируется
  (challenge-response), обслуживает реверс-стримы поверх identity-TLS через hyper.
  Фича off по умолчанию: rcgen/tokio-rustls/hyper утяжеляют кросс-сборку агента
  под Windows/musl (XR-105), обычная сборка без них.
- В [sync.rs](../xr-core/src/sync.rs) `direct_then_relay` перебирает прямые
  адреса-кандидаты по очереди (LAN раньше публичного, XR-050): каждый пробуется
  коротким liveness-probe при наличии запасного пути, первый, где отозвался наш
  агент, обслуживает операцию. Мёртвый LAN-IP снаружи отсекается за секунды, а не
  за полный таймаут. Прощуп это неавторизованный `GET /healthz` у агента, и
  живостью считается только его же ответ (`2xx` с телом `ok`): по приватному
  адресу в чужой сети сплошь и рядом сидит роутер, принтер или captive-портал, и
  раньше их `404` или страница входа забирали операцию себе (XR-219). Дальше по
  адресам уводит и ответ, которого наш агент не дал бы: несошедшаяся подпись
  манифеста, тело не манифест (`parse:`, `read:`), хеш файла мимо листинга,
  статусы `404` и `5xx`. Авторитетный ответ самого агента (`401`, `403`, `412`,
  `413`, `422`, `429`, протухший токен) возвращается сразу, его другой путь не
  переиграет. Relay (pinned-TLS поверх loopback-forwarder) поднимается, только
  когда исчерпаны все прямые.
  Android шлёт список адресов через тот же строковый параметр `agent_url`
  (кандидаты через перевод строки), grant-путь строит список сам. Прямой путь
  plain-HTTP с целостностью по подписи манифеста (XR-046), relay-путь с E2E-TLS.
- Отказ relay «агент офлайн» доезжает до пользователя, а не тонет в обрыве
  loopback-сокета (XR-134): mux хранит причину Close пира (`close_reason()`),
  relay-клиент называет ошибку (`relay: agent offline`), loopback-forwarder
  запоминает её, и sync подменяет сырую сетевую ошибку категорией
  `agent_offline: агент шары не на связи`; приложение показывает «Агент шары
  не на связи» и помечает шару офлайн.

**Запись в шару (LLD-28).** Карта эндпоинтов агента
([server.rs](../xr-share/src/server.rs)): `GET /{id}/manifest`, `GET /{id}/file/{*rel}`
(scope `share:read`), `PUT /{id}/file/{*rel}`, `DELETE /{id}/file/{*rel}` (scope
`share:write`, только v2). Порядок гейтов у записи: шара существует (`404`),
`writable` в конфиге агента (`403`), токен с `share:write` (`401`/`403`),
safepath (`403`). Заливка стримится во временный `.xr-part-<rand>` рядом с целью
(зарезервированный префикс: обход манифеста его пропускает, роуты отвергают),
хеш на лету, `fsync` + атомарный rename поверх цели, посев `HashCache`; `201`
на новый файл, `204` на перезапись. Оптимистический контроль против lost update:
`If-Match: <sha>` (и у `PUT` `If-None-Match: *`) сверяется с текущим содержимым,
нарушение это `412`; заголовок `X-Xr-Sha256` даёт `422` на расхождении, колпак
`max_file_mb` это `413`, `ENOSPC` это `507`, временный файл убирается в любом
исходе. Тот же relay/прямой путь несёт запись: в
[sync.rs](../xr-core/src/sync.rs) `upload_file`/`delete_file` идут поверх
`direct_then_relay`, до сети проверяют `share:write` в скоупе гранта и
транслируют ожидаемый хеш в `If-Match`. Десктопный харнесс `xr-share push`/`rm`
делает то же на `ureq`.

**Импорт по URL (LLD-29, XR-141).** Поверх записи агент принимает джобы
импорта: держатель write-инвайта шлёт ссылку, и контент страницы скачивает не
устройство, а машина агента внешним плагином-фетчером (референс это обёртка
yt-dlp + ffmpeg). Ядро остаётся тонким файлсервером: плагины не вендорятся, их
ставит владелец и вписывает в блок `[import]` конфига (лимиты `timeout_min` /
`max_total_mb`, песочница, реестр `[[import.plugin]]` с роутингом по суффиксам
хоста и планкой качества `max_height`); шара включает импорт флагом
`import = true` только вместе с `writable`, а `share --import` бутстрапит
референс-блок сам после проверки бинарей в `PATH`. Роуты
([server.rs](../xr-share/src/server.rs), scope `share:import`, минтится вместе
с `share:write`): `POST /{id}/import` (`202 {job_id}`), `GET` и
`DELETE /{id}/import/{job_id}` (опрос и отмена). Джобы живут в памяти
([import.rs](../xr-share/src/import.rs)): одна активная, очередь глубины 4,
завершённые видны час, рестарт таблицу забывает и подметает `.xr-import-*`
(зарезервирован весь неймспейс `.xr-`). Процесс на джобу в своей группе, argv
с `{url}`-литералом без shell и `{height}` числом; `xr-progress N` со stdout
кормит прогресс, хвост stderr становится текстом ошибки; публикация результата
идёт тем же контуром хеш + fsync + rename с посевом `HashCache`. SSRF режется
слоями: до старта гейт (только http/https, все адреса хоста вне приватных и
специальных диапазонов), на Linux с systemd плагин дополнительно заперт в
`systemd-run`-scope с `IPAddressDeny` тех же диапазонов (редирект и DNS
rebinding после проверки бьются об ядро); на Windows остаток риска принят.
Потребительская сторона: `import_url`/`import_status`/`import_cancel` в
[sync.rs](../xr-core/src/sync.rs) поверх `direct_then_relay` (до сети
проверяется `share:import` в скоупе гранта), JNI-обёртки `nativeImport*`,
в приложении действие «Импорт по URL» в папке шары (диалог ссылки с чипами
качества, строка прогресса с отменой, опрос раз в 2 с, пока экран открыт);
десктопный харнесс `xr-share import` поллит джобу до конца.

**Осталось за пределами XR-103:** JNI/Kotlin проброс relay-плеча гранта в
`sync_share_grant` на Android; identity-TLS на прямом листенере агента (сейчас
прямой путь plain-HTTP, целостность закрыта подписью манифеста); relay-fallback в
десктопном `xr-share pull`; отметка «через relay» в Admin SPA (данные уже
отдаются, нужен пересбор встроенного SPA).

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
канал со множеством логических стримов (`MuxStream`) внутри. Хендшейк
`MuxInit`/`MuxInitAck` несёт версию и байт флагов возможностей; согласованный
флаг включает оконный flow control стримов (окно 1 МиБ, возврат кредита кадром
`WindowUpdate`, LLD-27). Клиент (xr-core или
xr-client) держит `MuxPool`, который переиспользует туннель между сессиями и
умеет переподключаться. Над пулами стоит `ServerPool` (LLD-10): по `MuxPool`
на каждый VPS из списка, primary/backup по приоритету, failover при падении
активного и failback с hold-down после восстановления primary.

### 5.2 UDP relay

```
[Nonce:4B][Obfuscated: type + dst + src_port + payload]
```

Клиент пересылает UDP-пакеты LAN → VPS → Интернет. Ответы возвращаются от VPS
клиенту и спуфятся с IP оригинального сервера (через `IP_TRANSPARENT`) — это
нужно игровым приставкам, которые проверяют адрес источника ответа.

На VPS у каждого `src_port` роутера свой таск с очередью на 64 пакета. Приём из
туннеля идёт одним циклом и ничего не ждёт: пакет кладётся под локом таблицы в
очередь своего потока, цикл сразу возвращается за следующим, а сокет на этот
src_port поднимает уже таск, пока приём разбирает пакеты остальных потоков.
Порядок пакетов внутри потока держит очередь (переполнение отбрасывает пакет,
как отбросил бы его любой промежуточный узел), а сокетом владеет ровно один
таск, поэтому двух сокетов на один src_port не бывает. Тот же таск заворачивает
ответы из интернета обратно роутеру и сам снимает свой слот из таблицы: по
неудачному bind сразу, по простою дольше `flow_timeout` с перепроверкой очереди
под тем же локом, чтобы пакет, попавший в неё в последний момент, не пропал
вместе с потоком. Отдельного сборщика протухших потоков нет, сокет умирает
вместе со своим таском.

### 5.3 Control Plane HTTP

Реализовано в крейте `xr-hub`. API под префиксом `/api/v1`:

**Публичные эндпоинты:**
- `GET /api/v1/presets` — список `PresetSummary` (имя, версия, дата, кол-во правил). Поддержка `ETag`.
- `GET /api/v1/presets/:name` — полный `Preset` с правилами. `304 Not Modified` по `If-None-Match`.
- `GET /api/v1/invite/:token` это `InviteInfo` (метаданные без секретов), инвайт не потребляет. Читает `X-Claim-Id` и ставит `reclaimable`, когда потреблённый инвайт принадлежит спрашивающему (XR-216, см. ниже).
- `POST /api/v1/invite/:token/claim` это `InvitePayload` (полный конфиг подключения). Одноразовый инвайт потребляется здесь же, повтор получает `410 Gone`; исключение это повтор того же клиента по ключу `X-Claim-Id` (XR-216, см. ниже).
- `GET /api/v1/invite/:token/view` - HTML-страница приглашения для получателя (QR, deep link на Android, кнопка APK). Голые пути `/invite/:token` и `/invite/:token/view` редиректят сюда.
- `GET /api/v1/public-key` — публичный ключ ed25519 для проверки подписей пресетов.
- `GET /api/v1/app/latest` — подписанный манифест последнего APK: `{manifest, signature}` с диска (LLD-12). `404` если релиз не выложен.
- `GET /api/v1/app/download/:ver` — APK стримом (`application/vnd.android.package-archive`) из `releases/<ver>.apk`.

**Admin (Bearer-token):**
- `POST/PUT/DELETE /api/v1/admin/presets` — CRUD пресетов, автоподпись при наличии ключа.
- `GET/POST/DELETE /api/v1/admin/invites` — управление инвайтами.

Admin SPA встроена в бинарь через `rust-embed`. Подробности — [lld/01-control-plane.md](lld/01-control-plane.md).

**Страница приглашения (XR-192).** `/invite/:token/view` это единственный HTML,
который хаб собирает из данных, и открывает его посторонний человек по ссылке
из мессенджера, а в самой ссылке лежит одноразовый токен. Поэтому страница
замкнута на себя: QR рисуется инлайновым SVG прямо в ответе (крейт `qrcode`),
внешних картинок, шрифтов и скриптов на ней нет, и утекать токену наружу
нечем. Всё, что подставляется в разметку (комментарий инвайта, deep link из
`hub_url`, срок), проходит через экранирование, а ответ несёт
`Content-Security-Policy` с `default-src 'none'`; инлайновые стили пускает
одноразовый nonce, а не `unsafe-inline`.

**Повторный claim по ключу клиента (XR-216).** Одноразовый инвайт хаб потребляет
в тот момент, когда отдаёт `InvitePayload`, и дальше всё зависит от того, доехал
ли ответ целым. Если тело не легло в `InvitePayload` (разошёлся формат, оборвалась
сеть на чтении), получатель оставался без подключения, а инвайт уже сгорел, и
разблокировать его мог только админ новым инвайтом. Поэтому claim идёт с ключом
установки в заголовке `X-Claim-Id`: хаб запоминает его в поле `claim_id` инвайта
рядом с `consumed_at`, и повтор с тем же ключом получает тот же payload, пока
инвайт не истёк. Чужой повтор, повтор без ключа и отозванный инвайт (`revoke`
ключ снимает) упираются в `410 Gone` как раньше. Клиентскую половину держит
`xr-core/onboarding.rs`: ключ это 16 случайных байт, он лежит файлом `claim-id`
рядом с кэшем пресетов и переживает перезапуск приложения, поэтому повторить
Apply можно и после обновления, которое чинит разбор.

Одного идемпотентного claim мало: до него надо дойти. Приложение сначала
спрашивает сведения об инвайте (`GET /api/v1/invite/:token`) и по статусу решает,
показывать ли кнопку применения, поэтому `consumed` гасил её раньше, чем повтор
успевал случиться. Ручка сведений тоже читает `X-Claim-Id` и ставит
`InviteInfo.reclaimable`, когда потреблённый инвайт принадлежит спрашивающему;
статус при этом остаётся честным (`consumed`), выдавать инвайт за активный нельзя,
его видят и посторонние. Экран подтверждения включает кнопку по
`status == "active" || reclaimable` и пишет владельцу, что инвайт уже применяли на
этом устройстве и можно применить снова. На странице `/invite/:token/view` браузер
ключа не держит и решить за приложение не может, поэтому кнопка «Открыть в
приложении» осталась живой у инвайта, который потреблён и помнит забравший его
ключ; под ней объяснение, кому она поможет, а бейдж «Уже использовано» не
меняется. Там, где повтор невозможен ни для кого (инвайт истёк или отозван,
`revoke` ключ стирает), кнопка гаснет и обещаний не даётся: страница звала бы в
приложение, а оно ответило бы отказом.

**Состояние и его резерв (XR-224).** Хаб держит состояние файлами на диске:
`config.toml` (хеш пароля админки, ключ обфускации и salt пула), ключ подписи
из `[signing]` и содержимое `data_dir` (`presets/`, `invites/`, `shares/`).
Дороже всего ключ подписи: это корень доверия флота, перевыпуск означает новый
инвайт каждому устройству. Резерв делают подкоманды `xr-hub backup` и
`xr-hub restore`: архив с `MANIFEST.json` (отпечаток публичной половины ключа,
счётчики записей) без раздачи дистрибутивов, ежедневная отправка на второй VPS
скриптом `deploy/xr-hub-backup.sh` (приёмник зажат forced command, алерт в
Telegram при провале), восстановление отказывается подменять ключ подписи с
другим отпечатком без `--force`. Порядок разворачивания расписан в
[HUB-DEPLOY.md](HUB-DEPLOY.md).

**APK self-update (LLD-12).** Манифест версии подписывается **отдельным
release-ключом** ed25519, приватная половина которого живёт **офлайн у
владельца** (не на VPS) — это отдельный ключ от серверного (подпись пресетов,
TOFU) и от APK-signing keystore. Публичная половина зашита в приложение
(`BuildConfig.RELEASE_PUBLIC_KEY`). Хаб раздаёт пресобранные `manifest.json` +
`manifest.sig` + `<ver>.apk` из `releases/` (default `<data_dir>/releases`,
переопределяется `[server].releases_dir`) и **сам не подписывает** — релиз
готовит владелец командой `xr-hub sign-release` (ключ `xr-hub gen-release-key`).
Проверка подписи + SHA-256 — в `xr-core/update.rs` (unit-тесты), скачивание и
установка через `PackageInstaller` — в Kotlin. Компрометация VPS позволяет
подменить файлы, но не подделать подпись → клиент отвергает обновление.

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

Домены компилируются через ту же проверку, что и пользовательские правила
(`user_rule::normalize_pattern` плюс `classify_pattern`), независимо от того,
приехало правило из пресета хаба, из TOML или с экрана «Правила». Имя
приводится к нижнему регистру, а неразобранное отбраковывается с WARN, как и
битый CIDR. Границы у проверки такие: домен из ASCII-меток (буквы, цифры,
дефис) минимум в две метки, подстановка только ведущая (`*.example.com`),
одиночная `*` матчит любой SNI, кириллические имена пишутся в punycode.
Fail-soft здесь принципиален, потому что пресет общий на весь парк и одна
опечатка не должна ни ронять клиента, ни лишать его маршрутизации (XR-206:
`*яндекс.рф` резался по байтам внутри многобайтового символа и валил процесс
на первом же SNI).

На роутере конфиг лежит в `/etc/xr-proxy/config.toml`. На Android локальных
захардкоженных пресетов нет (XR-047): пресет приходит с хаба, а пользовательские
правила редактируются на экране «Правила» (вкладка «Серверы») и хранятся
глобальным упорядоченным списком в `filesDir/user_rules.json`. При Connect
`VpnViewModel.buildConfigJson` кладёт их в конфиг движка массивом `user_rules`
(`[{action, pattern}]` плюс `default_action`); на каждом последующем поднятии
туннеля из кэша сессии (resume из паузы, «включить здесь», рестарт после
ошибки) `XrVpnService.withFreshUserRules` подменяет массив свежим содержимым
`user_rules.json`, поэтому правка правил доезжает и без полного реконнекта
(XR-118). JNI-слой конвертирует массив в
`RoutingConfig` через `xr_proto::user_rule::to_routing_config`. Валидация
паттернов (домен / `*.wildcard` / CIDR) одна на всех:
`xr_proto::user_rule::classify_pattern`, из Kotlin она дёргается через
`nativeClassifyPattern`; кнопка «Обновить сейчас» на карточке пресета идёт в
`nativeRefreshPreset` и пишет в тот же дисковый кэш, что и движок.

### 6.2 Пресеты и override'ы

- Пресеты хранятся централизованно в `xr-hub` (файлы JSON на диске),
  версионируются, опционально подписываются ed25519.
- Клиент указывает пресет хаба и локальные override'ы с более высоким
  приоритетом: OpenWRT — `[hub] preset = "russia"` плюс `[[routing.rules]]`
  в TOML, Android — пресет из инвайта (`hubPreset` профиля) плюс глобальный
  `user_rules.json` (правила пользователя срабатывают первыми, первое
  совпадение выигрывает).
- При старте клиент делает `GET /api/v1/presets/:name` (forced fetch, timeout 2 с),
  кэширует результат локально. Фоновая задача раз в `refresh_interval_secs`
  сверяет версию по `ETag`. Обновлённые правила применяются **hot-swap'ом**
  без рестарта и в `xr-client` (OpenWRT), и в `VpnEngine` (Android):
  активный `Router` хранится как `RwLock<Arc<Router>>` (в `ProxyState.router` /
  `SessionContext.router` соответственно), задача при `fetch_if_stale == true`
  перестраивает `Router::from_merged(...)` и подменяет `Arc` целиком.
  Живые сессии продолжают со старым выбором, новые сразу видят новые правила.
- `Router::from_merged(overrides, preset, geoip)` — overrides-правила проверяются
  первыми, пресет — как fallback, `default_action` берётся из overrides.
- Если хаб недоступен — клиент работает с кэшом или только с локальными правилами.

## 7. Жизненный цикл соединения

### 7.1 xr-client (OpenWRT)

1. Старт: читает TOML, поднимает TCP listener + UDP TPROXY socket, ставит
   nftables-правила перенаправления.
2. Входящее TCP-соединение (TPROXY): `SO_ORIGINAL_DST` → SNI extraction →
   `Router::resolve(host, ip)` → либо `ServerPool` (mux до активного VPS,
   failover на резервный внутри пула), либо прямое соединение.
3. Входящий UDP: `recvmsg` + `IP_ORIGDSTADDR` → UDP-relay до VPS → spoofed-ответ.
4. Стоп: cleanup nftables, закрытие mux-пулов. Всё управляется procd + watchdog
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
   мирорит snapshot в `VpnUiState`. Это единственный источник статистики для UI;
   ленту журнала VM тянет своим `refreshLog()` через `nativeJournalTail()`.
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
- **Logs.** Единый журнал ([journal.rs](../xr-core/src/journal.rs), XR-042): все
  источники (движок, пробы доверенной сети, смены сети и режима, файловые
  операции) пишут в один буфер. Последние 400 строк держатся в памяти, UI берёт
  их отдельным вызовом `nativeJournalTail()` (лента видна и при остановленном
  движке, статистика её не возит); на диске лежит `journal.log` с ротацией по
  размеру и числу файлов, настройка в приложении.
  - Стиль записи общий для всех источников и описан в шапке `journal.rs`: одно
    событие в строку, текст русский со строчной буквы, тема и детали через
    двоеточие и запятые, своей разметки сообщение не несёт. Уровень и `[source]`
    ставит журнал, по ним UI красит строки и считает бадж, поэтому рамки и
    значки внутри сообщения только ломают поиск по ленте.
  - Бадж вкладки Log это тройка info/warn/err, посчитанная по уровню строк
    хвоста (свёрнутые дубликаты учитываются по множителю), поэтому уровень
    записи виден пользователю сразу. Заголовок фильтра считает только WARN.
  - `relay_errors: AtomicU64` это счётчик ошибок relay-задач. На Android
    остался только как debug-метрика в статистике, бадж его не читает.
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
| 4 | [04-onboarding-qr-uri.md](lld/04-onboarding-qr-uri.md) | Welcome-экран, Google Code Scanner, HTTPS deep link, экран подтверждения инвайта, TOFU public key. | Шаги 1-3 | Implemented |
| 5 | [03-android-logs-ux.md](lld/03-android-logs-ux.md) | Sticky toolbar, substring + regex поиск, auto-follow, скачивание через SAF. | Шаг 1 | Implemented |
| 6 | [05-android-rules-editor.md](lld/05-android-rules-editor.md) | Четвёртая вкладка Rules, read-only пресет + упорядоченные user overrides, TOML-preview модал, удаление хардкода `PRESET_RUSSIA`. Закрывает всю пачку. **Единая модель правил с LLD-14** (`RuleFragment` в `xr-proto`). | Шаги 1, 2, 4 | Draft |
| 7 | [07-android-per-app-tunnel.md](lld/07-android-per-app-tunnel.md) | Per-app split tunneling: `VpnService.Builder.addAllowed/DisallowedApplication`. Три режима (all/exclude/include), picker приложений, QUERY_ALL_PACKAGES. Фикс жалоб приложений на «вы используете VPN», когда их трафик идёт direct. | Шаг 1 | Draft |
| 8 | [08-android-multi-server.md](lld/08-android-multi-server.md) | Мультисерверная модель: `ServerProfile` + `ServerRepository`, переключатель серверов (chip + BottomSheet) на главном экране, вкладка Servers (CRUD), Edit с реконнектом, Delete с disconnect, миграция из flat-prefs, интеграция с LLD-04 (Apply = добавить профиль). | Шаги 1, 4 | Implemented |
| 9 | [09-multi-mux-pool.md](lld/09-multi-mux-pool.md) | Multi-mux pool: `MuxPool` ведёт N (default 4) параллельных TCP-туннелей к VPS, стримы балансируются round-robin, failover при разрыве слота. Устраняет HoL-blocking одного TCP — главный bottleneck по медленному старту стримов (5-7с/Mac, 20с/Android) после фиксов 104c268/dde442b/3a56e89. | — | Implemented |
| 15 | [15-android-trusted-networks.md](lld/15-android-trusted-networks.md) | Авто-пауза туннеля в доверенных Wi-Fi (по SSID): фаза `Paused`, вотчер сети, матчинг SSID в `xr-core/trusted.rs`, проба ограничений, пикер сетей, permission FINE_LOCATION/NEARBY_WIFI. Реализовано в чате C4b (задача 3b-2), LLD оформлен post-factum. Грабли SSID-детекта — в §6 документа. | Шаг 1, C4, C4c | Implemented |

> Номера LLD-10..14 зарезервированы под второй дизайн-пакет (чат C5: мульти-VPS
> failover, мониторинг/панель здоровья, самообновление APK, provisioning,
> гибридный редактор правил — см. `local-docs/c5-start.md`), поэтому
> trusted-networks занял свободный id **15**.
| 10 | [10-client-multi-vps-failover.md](lld/10-client-multi-vps-failover.md) | Multi-VPS failover клиента (роутер + Android): `ServerPool` поверх нескольких `MuxPool` (по одному на сервер), primary/backup по приоритету, пассивный (breaker C1) + активный health-check, sticky-to-primary с failback hold-down. На Android пул живёт внутри профиля (LLD-08), список серверов раздаётся подписанным инвайтом/пресетом хаба, на мобильном экономная политика проб без тёплого backup (XR-068). Обобщает LLD-09 от пула TCP до пула серверов. | Шаги 9 (LLD-09), 8 (LLD-08), 4 (LLD-04), 2 (LLD-01) | Implemented |
| 11 | [11-monitoring-health-panel.md](lld/11-monitoring-health-panel.md) | Мониторинг + уведомления + панель здоровья: классификация сбоя (`ServerUnreachable`/`HandshakeReset`/`AuthFailed`) в breaker, слои индикатора вместо смайлика, локальные уведомления падение/восстановление, напоминание об оплате (`paidUntil` в профиле). Объединяет задачи 6 и 10. | Шаги 3, 8 (LLD-08), 10 | Draft |
| 12 | [12-android-apk-self-update.md](lld/12-android-apk-self-update.md) | Самообновление APK: xr-hub раздаёт APK + подписанный манифест версии (`/api/v1/app/latest`, `/api/v1/app/download/:ver`, файлы в `releases/`), приложение проверяет подпись **отдельным release-ключом** (pinned в сборке `BuildConfig.RELEASE_PUBLIC_KEY`, ≠ серверный) + SHA-256, ставит через `PackageInstaller`. VPS-compromise ≠ RCE. Verify — в `xr-core/update.rs`; CLI `xr-hub sign-release` / `gen-release-key` (офлайн-ключ). | Шаг 2 (LLD-01) | Implemented |
| 13 | [13-zero-touch-provisioning.md](lld/13-zero-touch-provisioning.md) | Автоустановка (zero-touch provisioning): идемпотентный `xr-setup` (VPS: xr-server+xr-hub; роутер: xr-client) + Android SSH-обёртка. Один движок, два профиля. Заканчивается выдачей инвайта (LLD-04). Этап 1 (установщик) реализован (XR-015/XR-177), этап 2 (SSH из приложения) идёт поверх. | Шаги 2, 4, 8 (LLD-08), 10 | Этап 1 Implemented |
| 14 | [14-hub-hybrid-rules-editor.md](lld/14-hub-hybrid-rules-editor.md) | Гибридный редактор правил в xr-hub: TOML — источник правды (комментарии-категории), JSON — derived; фрагмент-мастер + сырой TOML, line-surgical правки. **Единая модель `RuleFragment` с LLD-05.** | Шаг 2 + LLD-05 | Draft |
| 16 | [16-manual-server-hub-rules.md](lld/16-manual-server-hub-rules.md) | Живые правила из хаба для серверов, добавленных **вручную** (не только инвайт): выбор «источник правил» (локальный/хаб) + список пресетов с хаба, TOFU ключа; движок рефреша переиспользуется. Опц. усиление — реальная верификация подписи пресета (сейчас не проверяется ни у кого). | Шаги 2 (LLD-01), 4 (LLD-04), 8 (LLD-08), 12 | Draft |
| 17 | [17-hub-router-registry.md](lld/17-hub-router-registry.md) | Хаб-реестр роутеров: идентичность/enrollment роутера, **исходящий** poll-канал роутер -> хаб (отчёт статуса), раздел «Роутеры» в админке. Несёт «последний снимок» статуса (история/Grafana в LLD-18). Шов с LLD-13: установщик регистрирует роутер. Удалённое управление командами вынесено в LLD-20. | Шаги 2 (LLD-01), 10 (LLD-10), 11 (LLD-11), 13 (LLD-13) | Draft |
| 18 | [18-fleet-metrics-grafana.md](lld/18-fleet-metrics-grafana.md) | Fleet-метрики + Grafana: хаб накапливает кольцо `RouterReport` и экспонирует Prometheus-формат, VictoriaMetrics + Grafana поверх; дашборды скорость/аптайм/инциденты, опц. алерты. Транспорт данных уже в LLD-17; приватность (только операционные метрики) — явный раздел. Follow-up, включается при росте флота. | Шаг 17 (LLD-17), 11 (LLD-11) | Draft |
| 19 | [19-file-sharing-agent.md](lld/19-file-sharing-agent.md) | Файлообмен: агент `xr-share` (server-режим, Win/Linux) раздаёт директорию **read-only**; владелец вручную регистрирует `адрес:порт` в хабе; хаб — **индекс адресов без байтов** (юр-чистота); доступ по подписанному хабом токену, верифицируемому агентом офлайн; идентичность агента — TOFU через хаб; манифест агент подписывает своим identity-ключом, потребитель проверяет по pinned `agent_pubkey` из гранта, fail-closed (XR-046, закрывает MITM «файл+хеш разом» на plain-HTTP data-path). MVP-потребитель = **Android**: **разовое скачивание + однонаправленный sync** (mirror server→устройство), движок дифа в `xr-core`. Прямой доступ, один хаб (релей для CGNAT / заливка / E2E / десктопный sync — отдельно). | Шаг 2 (LLD-01), 17 (LLD-17), 4 (LLD-04), 12 (LLD-12) | Draft |
| 20 | [20-router-remote-management.md](lld/20-router-remote-management.md) | Удалённое управление роутерами поверх реестра LLD-17: подписанные команды из закрытого enum (`apply_preset`/`update_config` по белому списку полей/`reload`/`restart`/`deregister`) через тот же исходящий poll, верификация закреплённым ключом, least-privilege (не shell), аудит-лог. Компрометация VPS не равна RCE без офлайн-ключа подписи. | Шаги 17 (LLD-17), 2 (LLD-01), 16 (LLD-16) | Draft |
| 21 | [21-messenger.md](lld/21-messenger.md) | Мессенджер как сервис экосистемы (болванка на будущее): чат поверх федерации хабов (не глобальный сервер, класс Matrix), E2E-группы (ориентир MLS), ориентир по фичам Signal. Отличия: быстрый перенос истории, продвинутый поиск и срезы, кворум групп, глубокая кастомизация, маскировка иконки, эфемерность по политике, эффективные треды. Спорные фичи (кворум, свой/готовый федеративный протокол, камера-детекция, ключ бэкапа) в открытых вопросах LLD, обсуждаются. Далёкий сервис. | XR-058, XR-030/074, XR-061 | Draft |
| 22 | [22-router-load-balancing.md](lld/22-router-load-balancing.md) | Балансировка устройств по VPS на роутере (XR-080): ключ это LAN source IP, правила «IP/CIDR -> сервер» плюс weighted rendezvous для устройств без правила, стабильный exit-IP на устройство. Слой выбора дома над механикой отказа LLD-10 (дом, если стабильно жив -> глобальный порядок), без per-device состояния. Роутер-only; Android получит тот же ключевой API после per-app туннеля XR-016 (ключ UID). | Шаг 10 (LLD-10) | Draft |
| 23 | [23-share-relay-nat.md](lld/23-share-relay-nat.md) | Доступ к шаре без белого IP (XR-035): агент за NAT держит исходящий обфусцированный mux-туннель к отдельному сервису `xr-relay`, потребитель приходит туда с relay-токеном хаба, relay слепо сплайсит стримы; E2E это pinned TLS до агента (SPKI == agent_pubkey), хаб остаётся чистым сигналингом. Hole-punching отдельной фазой после XR-064, relay остаётся fallback'ом. | LLD-19, шаг 2 (LLD-01); стык с XR-046/XR-050 | XR-103: транзит (`xr-relay`), протокол (`xr-proto`) и сигналинг (`xr-hub`) готовы; оконечный identity-TLS у агента и pinned-verifier у потребителя осталось |
| 24 | [24-share-hash-index.md](lld/24-share-hash-index.md) | Локальный индекс хэшей для синка шары (XR-098): персистентный `(отн. путь, size, mtime) -> sha256` в `xr-core/sync.rs` по образцу агентского `HashCache`, тёплый скан это stat-обход без пересчёта SHA-256; файл индекса в `filesDir/share-index/<shareId>.json`, битый/чужой файл даёт полный пересчёт; хэш скачанного кладётся в индекс сразу (верифицирован при скачивании). | LLD-19; стык с XR-043/XR-097 | Implemented |
| 28 | [28-share-write-scope.md](lld/28-share-write-scope.md) | Доступ к шаре на запись (XR-051): OAuth-вида scope внутри `ShareToken` (строка имён через пробел, `share:read share:write`; подпись v2, формат ломаем: парк тестовый, токены эфемерны; при переезде на JWT XR-030 scope-клейм переносится дословно), право записи у привязки шара-инвайт (при LLD-25/XR-030 переезжает в scope мандата, капабилити-слой не меняется), двойной опт-ин владельца (writable в записи хаба и в конфиге агента), приём `PUT`/`DELETE` агентом строго в пределах шары (safepath, атомарная заливка temp + rename, хеш на лету, оптимистический `If-Match` против lost update), харнесс `push`/`rm`. Фундамент XR-052 (импорт по URL) и любых правок шары с устройства. | LLD-19, LLD-23; стык с LLD-27 и LLD-25 | Implemented (XR-139) |
| 29 | [29-share-url-import.md](lld/29-share-url-import.md) | Импорт контента по URL как плагин агента (XR-052): скоуп `share:import` (минтится вместе с write, формат токена не меняется), реестр плагинов-фетчеров в конфиге агента (внешний exec, argv-литерал без shell, референс yt-dlp+ffmpeg, роутинг по суффиксам хоста), асинхронные джобы с поллингом прогресса (`POST /{id}/import` + опрос), качество параметром джобы в пределах планки `max_height` владельца, бутстрап референс-конфига самим `share --import`, публикация через тот же safepath/rename-контур записи, SSRF-гейт (схема, резолв, приватные диапазоны) + сетевая песочница systemd-run на Linux, резерв неймспейса `.xr-`. Ядро xr-share остаётся тонким файлсервером, плагины опциональны. | LLD-28 (XR-139), LLD-19, LLD-23 | Implemented (XR-141) |
| 27 | [27-mux-flow-control.md](lld/27-mux-flow-control.md) | Оконный flow control в mux (XR-115): окно отправки на стрим (1 МиБ), возврат кредита кадром `WindowUpdate`, согласование capability-байтом в `MuxInit`/`MuxInitAck` без бампа версии (смешанные пары живут по-старому). Чинит обрыв скачивания через relay (быстрый агент + медленный потребитель переполнял per-stream канал, reader убивал стрим) и тот же механизм на основном прокси (XR-071). | LLD-23 (relay-путь приёмки) | Implemented |
| 32 | [32-share-structure.md](lld/32-share-structure.md) | Структурирование шары с устройства (XR-168): директории как сущности манифеста (`ShareManifest.dirs`, все, включая пустые; подпись кроет байты тела и не меняется), эндпоинты агента `POST /{id}/mkdir`, `/move`, `/copy` под тем же `share:write` (гейты как у PUT, занятая цель 409 без перезаписи, move и copy работают для файла и директории, рекурсивная копия атомарно через staging `.xr-copy-` + rename), пересев хеш-кеша при move, зеркало пустых директорий в синке (`SyncPlan.mkdir`/`rmdir`, прунинг становится desired-aware), клиент mkdir/move/copy в xr-core + JNI + приложение (создание папки, перенос/копирование/переименование с пикером целевой папки, гейт `canWrite`), харнесс `mkdir`/`mv`/`cp`. Хаб и минтинг не трогаются. Оптимизация синка под серверный move это XR-169. | LLD-28 (XR-139), LLD-19, LLD-23, LLD-29 | Draft |
| 33 | [33-share-git-sync.md](lld/33-share-git-sync.md) | Совместное редактирование текстовых шар (XR-178): git как движок синка. У агента репозиторий вне рабочей папки (`GIT_DIR` + `core.worktree`, `updateInstead`), авто-коммит по watcher'у, smart HTTP спавном системного git (`upload-pack`/`receive-pack --stateless-rpc`, опт-ин `share --git` только с `--writable`), подписанный HEAD identity-ключом с long-poll вместо частого polling; весь git-контур под `share:write`, включая fetch. Харнесс `xr-share sync` на libgit2 (gitoxide без push): авто-коммит, fetch/merge/push, пересечения строк оседают конфликт-копиями (модель SparkleShare), relay через rustls-мост с pinned identity-TLS. Только текст: колпак `git_max_file_mb`, крупное живёт манифест-контуром вне истории. Третья фаза: web-страница шары у агента (просмотр md, история/дифф/правка по write-токену из гранта, `weblink`) и экран истории в приложении. Хаб и минтинг не тронуты. | LLD-19, LLD-28, LLD-23, LLD-29; стык с LLD-32 и XR-129 | Draft |
| 37 | [37-preset-longpoll.md](lld/37-preset-longpoll.md) | Мгновенный подхват правил хаба (XR-174): long-poll `GET /presets/{name}/wait?version=N` (ответ сразу полным пресетом при несовпадении версии, 304 по таймауту 55 с; поколение пресетов на `watch`-канале в `AppState`), клиентский `PresetCache::wait_for_update` и общий цикл `watch_loop` в xr-core вместо дублей в xr-client и движке; при ошибке откат в существующий опрос с backoff, `refresh_interval_secs` становится интервалом деградированного режима. Новых конфиг-ручек нет, Kotlin не меняется. | Шаг 2 (LLD-01); стык с LLD-16, XR-180 | Draft |
| 30 | [30-max-carrier.md](lld/30-max-carrier.md) | Max как транзитный носитель (болванка, crate `xr-max`, ядро плюс CLI): чужой мессенджер Max как недоверенная труба для шифрованных датаграмм на случай шатдауна, когда свои IP недоступны, а Max в белом списке. Трейт `Carrier` общий с XR-061/XR-064, framing поверх, крипта на Noise (XR-061). Честная рамка: канал не анонимный (SIM/юрлицо) и палевный по паттерну, годен как редкий bootstrap под шатдаун, не как повседневный прокси. Не путать с LLD-21 (там свой мессенджер, тут чужой носитель). Реверс клиента гейтнут результатами bot-стадии. Далёкая research-ставка. | XR-061, XR-064, XR-058 | Draft |
| 26 | [26-share-access-mode.md](lld/26-share-access-mode.md) | Режим доступа к шаре (XR-129): поле `AccessMode { auto, direct, relay }` у `ShareRecord` и `ShareGrant`, где `auto` это дефолт-прощуп XR-128, а `direct`/`relay` ручные оверрайды (достижимость это свойство четвёрки потребитель-агент-путь-момент, фиксация типа не единственный механизм). Кеш auto-вердикта per-agent с коротким TTL в `xr-core` (серия операций не перепрощупывает, инвалидация по TTL и по факту неудачи). Неавторизованная `/health` у агента (версия плюс статус relay-аплинка), прощуп уходит с `/manifest` на неё. Умный дефолт режима хабом при регистрации как подсказка, дефолт владельца (`--mode`, `setmode`) плюс локальный оверрайд консьюмера в приложении, эффективный режим схлопывается у границы `xr-core`. | LLD-23, XR-128; стык с LLD-33 | Draft |
| 34 | [34-android-reconnect.md](lld/34-android-reconnect.md) | Живучее переподключение Android при провале авто-резюма (XR-132): девятое значение `Phase`/`ConnectPhase` `Reconnecting` вместо тихой смерти сервиса и снятия уведомления. Держит foreground-сервис и постоянное уведомление; вечный авто-ретрай по приходу/смене сети и screen-on с backoff 5с -> 5мин (потолок), в мёртвой зоне (`noNetwork`) таймер не крутится. Флаг намерения `stayUp` (нативный Connected либо авто-подъём) разводит авто-резюм и ручной коннект: ручной провал остаётся Error -> Idle плюс снекбар. Ретрай только поверх мёртвого движка, пуловый failover LLD-10 внутри живой сессии не трогается; вход в доверенную сеть уводит в Paused. Чистое расписание backoff в `xr-core/reconnect.rs` плюс мост `nativeReconnectDelayMs` под юнит-тесты, оркестрация в Kotlin device-verify. | LLD-02, LLD-15, LLD-10; стык XR-095/XR-183/XR-049 | Draft |
| 35 | [35-protocol-v2.md](lld/35-protocol-v2.md) | Протокол v2, зонтик кластера (XR-060): замена статического XOR Noise-транспортом. Паттерн IK с профильным PSK (`Noise_IKpsk_25519_ChaChaPoly_BLAKE2s`, крейт `snow`), ChaCha20-Poly1305 как оптимум на ARM без AES-NI, forward secrecy и anti-replay. Прежние кадр/команда/mux едут плейнтекстом внутри AEAD-записей (магия 0xA0 убрана), версия транспорта раздаётся out-of-band подписанным инвайтом/пресетом, фичи через существующий `MuxCaps`. Статический ключ на VPS, клиентский ключ пока профильный (задел под per-client XR-030/073). Миграция двойным приёмом на отдельном v2-порту (не trial-decode), сервер первым, роутеры последними, откат конфигом; компат-слой короткий (парк тестовый). UDP relay на датаграммный AEAD с явным nonce и окном повтора. Задаёт очередь листьев и швы под XR-062/057/064/066/067. | XR-060; гейтит XR-061, далее XR-063/062/057/064/066/067; стык LLD-27, LLD-10, LLD-23, LLD-30 | Draft |
| 36 | [36-release-federation.md](lld/36-release-federation.md) | Федеративная дистрибуция софта (XR-173), первая фаза федерации хабов: хабы делят софт и цепочку доверия, данные пока локальные. Два корня доверия: релиз-ключ (офлайн у владельца, тот же, что у APK LLD-12) подписывает код всех компонентов, ключ хаба подписывает данные. Единый стор `soft-dist` с подписанным индексом (`SHA256SUMS.sig`), публичная половина релиз-ключа зашита в сборку компайл-тайм (хаб не подменит). Релиз-CI по компонентам (musl `xr-server`/`xr-relay`/`xr-hub`/`xr-client`/`xr-setup` по образцу `release-xr-share.yml`, у клиента матрица арок роутеров), приватный ключ офлайн, вне CI. Pull-апдейтер `xr-setup update` (сверка подписи плюс SHA-256, atomic swap, рестарт), хаб зеркалит набор от родителя с проверкой подписи пиннутым ключом, пиринг попарный без транзитивного доверия. XR-111/112 переформулируются в обёртки оператора над `xr-setup update`, из закрытой XR-110 переезжают обновление всех роутеров одной командой и сверка exit-IP. | LLD-12, LLD-13, LLD-19, LLD-23; XR-109 смежно; стык с XR-074/XR-061 | Draft |

**Предварительный порядок реализации второго пакета (C6+):** LLD-03 ✓ →
**LLD-10** (failover клиента: движок + роутер + Android) → **LLD-08** (Android мультисервер) → **LLD-11**
(панель здоровья поверх 10+08) → **LLD-05 + LLD-14** связкой (общий `RuleFragment`)
→ **LLD-12** (self-update) → **LLD-13** (provisioning) → **LLD-17** (реестр +
удалённое управление, поверх 13) → **LLD-18** (fleet-метрики/Grafana, поверх 17,
follow-up) → **LLD-19** (файлообмен, поверх 17) → **LLD-07** (per-app, по ситуации).
Номера шагов в таблице — историческая нумерация (порядок появления LLD);
фактическую очередь задаёт этот список и колонка «Зависит от».

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
   карты и ссылок на файл с якорем строки вида `#L42`.
4. Любое изменение, затрагивающее: состав крейтов, wire-протокол, формат
   конфига, топологию деплоя, модель состояния клиента, — обязано
   отражаться здесь **в том же коммите**.
5. Если документ начал расходиться с кодом — чинить этот документ, а не
   код.
