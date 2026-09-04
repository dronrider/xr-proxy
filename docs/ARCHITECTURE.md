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
| [xr-share/](../xr-share/) | Агент файлообмена (LLD-19, LLD-28): раздаёт директории и файлы (чтение по умолчанию, запись по write-привязке инвайта), подписывает манифест, проверяет токены офлайн; текстовые шары дополнительно несут git-историю с доступом по smart HTTP и вшитую web-страницу с историей и правкой, а подкоманда `sync` держит папку соавтора в синке с шарой (LLD-33). |
| [xr-relay/](../xr-relay/) | Слепой транзит шар за NAT (LLD-23, XR-103): реестр агентов, регистрация, проверка relay-токенов, сплайс без чтения содержимого. |
| [xr-web/](../xr-web/) | Браузерный вход к публикациям агентов (LLD-38 фазы 2 и 3, XR-263, XR-264): адресация по поддомену, вход владельца по cookie-сессии, кеш маршрутов, пул соединений до агента, проксирование HTTP и сплайс апгрейда со штатным закрытием до потолка сплайса. |
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
- [preset.rs](../xr-proto/src/preset.rs) это формат пресета хаба (`Preset`,
  `PresetSummary`) и его подпись (XR-207). `canonical_json` печатает
  детерминированную форму под подпись, поля по алфавиту, без `signature`.
  `verify_preset` сверяет подпись публичной половиной ключа ed25519,
  `decode_verifying_key` разбирает сам ключ из base64. Крипта живёт за фичей
  `share`, поэтому одна реализация служит и хабу, и клиенту, без копии на
  каждой стороне.
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
  соединение в Direct). Такой `Err` перечисляет все серверы по приоритету
  («все 3 сервера недоступны: msk (...), fra (...), aeza (...)»), primary
  первым, а вид ошибки берёт у него же: раньше наружу уезжал отказ последнего
  по порядку обхода, и при блокировке оператором всего пула диагностика
  упиралась в случайный резерв (XR-131). Отказ единственного сервера идёт как
  есть. На primary возвращает фоновый `health_loop` с
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
  loopback/private хосты для https. `http://` тоже принимается, но только
  для loopback/private хоста или `localhost` (XR-259) - так проходит
  онбординг с локального стенда, у которого нет TLS; публичный хост по
  http все равно отклоняется.

### 4.2 xr-core — ядро персонального клиента

Используется Android (через `xr-android-jni`) и, в перспективе, десктопными
клиентами. Полностью платформо-независимо, не содержит Android-API.

- [lib.rs](../xr-core/src/lib.rs) — реэкспорт модулей.
- [client_config.rs](../xr-core/src/client_config.rs) держит конфиг движка с
  обеих сторон (XR-271): `build_config_json` собирает его из профиля активного
  сервера (`ClientProfile`: пул, ключ, правила пользователя, резолверы
  системы, политика отказа), `parse_config` читает обратно в `VpnConfig`.
  Здесь же дефолты (`DEFAULT_ROUTING_ACTION`, порт, модификатор, salt,
  паддинг, период рефреша пресета) и чистка резолверов: адрес самого туннеля,
  loopback и link-local спрашивать бессмысленно. Раньше сборка жила строкой в
  Kotlin со своим экранированием, а разбор в `xr-android-jni`; порт под другую
  платформу писал бы обе половины заново.
- [health.rs](../xr-core/src/health.rs) это здоровье сессии (LLD-06 п. 3.5a,
  XR-271): скользящее окно по накопительным счётчикам движка, пять ступеней
  (`healthy`/`good`/`watching`/`hurt`/`critical`), мгновенное ухудшение и
  придержанное улучшение, заморозка на пропавшей сети (XR-183). Имя ступени
  уходит наружу строкой, картинку по нему выбирает экран.
- [engine.rs](../xr-core/src/engine.rs) — `VpnEngine` (start/stop) и `VpnConfig`.
  Держит smoltcp-стек, `ServerPool` (пул серверов, внутри `MuxPool` на
  каждый), обфускатор, роутер, fake DNS, статистику. `on_network_changed`
  ресайклит весь пул и возвращает активность primary'ю.
- [ip_stack.rs](../xr-core/src/ip_stack.rs) — `PacketQueue` (мост между TUN и
  smoltcp), `IpStack` (userspace TCP/IP).
- [dns.rs](../xr-core/src/dns.rs) — `FakeDns` в диапазоне 198.18.0.0/15 (RFC 2544).
  DNS-ответ подменяется fake-IP, и при TCP-SYN на этот IP ядро восстанавливает
  оригинальный домен для применения правил маршрутизации. Пул это 131070
  адресов с часовым TTL, раздаются они по кругу, протухшие записи снимаются на
  каждом обороте. Круг ограничен размером пула: раньше подбор кандидата при
  полном пуле крутился вечно с захваченным локом и вешал движок насмерть до
  перезапуска VPN (XR-210). Когда весь круг занят живыми записями, адрес
  забирается у той, к которой дольше всех не обращались (последним обращением
  считается и выдача, и запрос домена по адресу на SYN), а в журнал уходит
  WARN. Цена вытеснения такая: адрес, выданный приложению раньше, теперь
  принадлежит другому имени, и соединение, открытое по нему позже, уйдёт не
  туда. Уже поднятые сессии это не задевает, домен снимается один раз на SYN и
  дальше живёт в самой сессии.
- [session.rs](../xr-core/src/session.rs) — `SessionContext`, `relay_session_with_domain()`.
  Решает `Action::Proxy` vs `Direct`, поднимает relay-task. `connect_protected()`
  защищает fd от петли через VPN (вызывает Kotlin-колбэк). Потолок жизни
  сессии час у обоих путей relay. Простой считается только у direct (5 минут)
  и считается по сессии целиком: время последней активности общее, и любая из
  сторон его двигает. Молчание одной стороны штатно (при скачивании наверх не
  идёт ничего), поэтому рвать по нему нельзя. По простою relay сворачивается
  сам, движок видит закрытый канал и снимает запись из `sessions`. У mux-пути
  своего простоя нет: сессию сворачивает сервер, у которого срок стоит на
  стороне от таргета к клиенту (`relay_pump` в
  [mux_handler.rs](../xr-server/src/mux_handler.rs)).
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
- [journal_bridge.rs](../xr-core/src/journal_bridge.rs): мост `tracing` в тот же
  журнал (XR-237). Слой `tracing_subscriber`, который кладёт `WARN` и выше в
  ленту; подробности в разделе 8.
- [presets.rs](../xr-core/src/presets.rs) это кэш пресета хаба (загрузка,
  рефреш, ожидание новой версии) и то, что показывает по нему экран правил:
  `cached_preset_json` отдаёт карточку (имя, версия, дата, группы), а
  `merged_toml` печатает блок `[routing]` из моих правил поверх пресета для
  кнопки `{ }`. Кэш пишет ядро, поэтому оно же его и читает: формат файла
  наружу не выходит (XR-271). Каждый вход пресета проходит проверку подписи
  ключом `trusted_public_key` из конфига или профиля (XR-207). Фетч, ожидание
  изменения, дисковый кэш и карточка с превью проходят её же. Ключ задан, а подписи нет или
  она чужая, пресет отбраковывается целиком, прежний роутер живёт дальше,
  отказ виден в логе и статусе. Ключа нет, поведение прежнее, и об этом уходит
  предупреждение на старте. Неразборная строка ключа тоже считается заданным
  ключом, опечатка в конфиге не выключает проверку молча.
- [onboarding.rs](../xr-core/src/onboarding.rs) — one-shot HTTP-вызовы
  xr-hub для Android onboarding (LLD-04): `fetch_invite_info` (GET,
  без consume) и `apply_invite` (POST `/claim` → `InvitePayload` + TOFU
  `/public-key` + pre-warm preset cache через `PresetCache::write_to_disk`).
  Живёт рядом с `presets.rs`, чтобы переиспользовать тот же reqwest-клиент
  и формат кэша; JNI-обёртки в `xr-android-jni` лишь прокидывают вызовы.
  Claim и запрос сведений несут ключ установки в `X-Claim-Id` (XR-216,
  раздел 5.3), чтобы сорванный разбор ответа не сжигал одноразовый инвайт
  впустую, а экран подтверждения пускал владельца ключа к повтору. Здесь же
  `profile_from_payload`: раскладка принятого приглашения на профиль сервера
  (пул по `priority`, легаси-адрес запасным вариантом, дефолты обфускации,
  адрес хаба с запасным значением). Приложению остаётся имя профиля и
  хранилище (XR-271).

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
- [proxy.rs](../xr-client/src/proxy.rs) это прозрачный TCP-прокси: `accept ->
  SO_ORIGINAL_DST -> SNI extraction -> route -> relay/tunnel`. Извлечение SNI
  и маршрутизация берутся напрямую из `xr_proto` (`sni`, `routing`), своих
  обёрток у клиента нет. До маршрута клиент подсматривает первые байты
  соединения, ожидание зависит от порта (XR-292). На 80/443 маршрут решает
  SNI, peek держит 10 секунд. Вне web-портов байты нужны ровно на первый байт
  0x16, там хватает короткого окна в `300 мс`. Молчащий клиент по таймауту
  уходит в Direct с пустым буфером, соединение живёт. Это случай server-first
  протоколов (VNC, SMTP, MySQL, IRC), раньше такое соединение висело 10 секунд
  и рвалось. TLS-клиент на нестандартном порту успевает в короткое окно,
  обфускация под TLS шлёт ClientHello сразу после connect. Решение о маршруте
  вынесено в `decide_route`, её и гоняют тесты. `handle_connection` требует
  настоящего `SO_ORIGINAL_DST`, которого вне Linux NAT-перехвата нет.
- [redirect.rs](../xr-client/src/redirect.rs) — управление nftables/iptables
  (auto-setup, cleanup). Использует семейство `ip` (не `inet`, см. CLAUDE.md).
  Каждый VPS пула выводится из перехвата не целиком, а двумя правилами: всё,
  кроме 80 и 443 (ssh и служебные порты сервера, туннельный порт при обычном
  его значении сюда же), и отдельно туннельный порт, который нужен, только
  когда туннель сидит на web-порту.
  Web-порты остаются под перехватом: сайты на том же VPS должны уходить через
  прокси, иначе их SNI видит провайдер. Генерация правил вынесена в чистые
  `build_nft_ruleset` и `build_ipt_rules`, они и покрыты тестами.
  Машинные исключения перехвата приходят из конфига полем
  `client.bypass_rules` (XR-248): каждая строка это готовое условие nftables
  без вердикта, клиент дописывает ей `return` и ставит первой в цепочку, до
  общего redirect. Условие с чужим вердиктом, кавычкой, подстановкой или
  разделителем команд отбраковывается с WARN, а если `nft` не принял набор
  целиком, клиент ставит перехват без машинных условий и говорит об этом в
  лог: опечатка в конфиге не должна оставлять LAN без перехвата вовсе.
  Сам критерий отбраковки лежит в `xr_proto::config::bypass_rule_reject_reason`
  и повторяется на shell в `killswitch-setup.sh`: список у обеих половин один,
  иначе условие встаёт с `return` в перехвате и без `accept` в киллсвитче, а
  это блэкхол для устройства. Паритет закреплён тестом стенда `xr-setup`,
  который гоняет один набор злых условий через обе половины.
- [dns.rs](../xr-client/src/dns.rs), локальный DNS-форвардер (XR-285):
  dnsmasq спрашивает его на петле, а он уносит запрос в туннель и говорит с
  публичным резолвером по DoT. Разбор DNS тут минимальный: идентификатор,
  граница вопроса и размер приёмного буфера из EDNS0. Апстрим держится одним
  соединением на все запросы (RFC 7858), идентификаторы на проводе свои, а
  ответ возвращается спрашивающему с его собственным. Молчания нет: не
  дождавшись апстрима, форвардер отвечает SERVFAIL и пишет в журнал причину.
- [udp_relay.rs](../xr-client/src/udp_relay.rs) — UDP TPROXY: `recvmsg` +
  `IP_ORIGDSTADDR`, relay на VPS, spoofed-responses через `IP_TRANSPARENT`.
  Таблица флоу с NAT по туннельному порту вынесена в `FlowTable` без сокетов и
  покрыта юнитами (см. 5.2).

xr-client работает с ядром на сыром уровне сокетов и nftables и **не использует
xr-core** — там другая модель (TUN/smoltcp vs TPROXY).

### 4.4 xr-server — VPS-сервер

- [main.rs](../xr-server/src/main.rs) — TCP listener + опциональный UDP relay.
- [handler.rs](../xr-server/src/handler.rs) — обработчик TCP-соединений:
  `deobfuscate → connect → relay с таймаутами`.
- [udp_relay.rs](../xr-server/src/udp_relay.rs) это flow table по паре (пир,
  `src_port`), `bind(src_port)` для NAT traversal и таск с очередью на каждый
  поток (см. 5.2).
- [mux_handler.rs](../xr-server/src/mux_handler.rs) держит mux-сессию: стрим на
  таргет, свой permit капа стримов (см. ниже).
- [fallback.rs](../xr-server/src/fallback.rs) — фальшивый HTTP-ответ на
  DPI-пробы.

Нагрузку сервер держит двумя капами, и считают они разное. `max_connections`
это TCP-коннекты, permit берётся на accept и отбивает лишний коннект до
хендшейка. Стримы внутри mux-сессии он не видит вовсе: коннект один, а стримов
в нём сколько угодно, и каждый стоит fd апстрима с парой тасок. Поэтому есть
второй кап, `max_streams` на весь сервер и `max_streams_per_mux` на одну
сессию (XR-199): своя квота проверяется первой, чтобы жадный клиент не выбрал
бюджет VPS на соседние роутеры. Permit берётся до `ConnectAck`, переезжает в
таску релея и возвращается вместе с ней. Стрим сверх капа получает `Close` с
причиной `CLOSE_REASON_STREAM_LIMIT`; сессия при этом живёт, а на здоровье
сервера (`RelayHealth` в `xr-proto`, 4.1) эта причина не влияет: сервер
исправен, за свою долю вышел клиент.

Хендшейк идёт под одним общим дедлайном (XR-202). Срок ставится в
`handle_client` от самого accept и накрывает весь путь до релея: первый кадр,
DNS и connect к цели. Прежний таймаут стоял на каждый read отдельно. Клиент
капал байты с паузами короче таймаута и держал permit из `max_connections`
сколь угодно долго, а 256 таких коннектов запирали приём новых туннелей.
Дедлайн задаётся `connection_timeout_sec`, по умолчанию 300 секунд. На idle
5 минут и потолок жизни 1 час в релее он не влияет. Обёртка принимающей таски
`serve_connection` держит permit ровно на время соединения в руках
`handle_client`. Дедлайн возвращает таску, и слот приёма освобождается сразу.

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
| `nativeApplyInvite(hub_url, token, preset, cacheDir, timeoutMs)` -> `String (JSON)` | Claim + TOFU public-key + pre-warm preset, пресет сверяется с этим же ключом (XR-207). Одноразовый `tokio::runtime::Runtime` на вызов. |
| `nativeCheckUpdate(hubUrl, currentCode, pinnedKeyB64, timeoutMs)` → `String (JSON)` | LLD-12. Fetch + verify манифеста pinned release-ключом. `{available, manifest?, error?}`. |
| `nativeVerifyApk(path, sha256Hex)` → `Boolean` | LLD-12. Потоковая SHA-256 скачанного APK против манифеста. |
| `nativeBuildConfig(profileJson)` -> `String (JSON)` | XR-271. Конфиг движка из профиля активного сервера, собирает `client_config::build_config_json`. Ошибка (профиль без сервера) приходит как `{error}`. |
| `nativeHealthUpdate(errors, warns)` / `nativeHealthFreeze(...)` / `nativeHealthReset()` | XR-271. Здоровье сессии: один трекер `xr_core::health` на процесс, ответ это имя ступени. |
| `nativeCachedPreset(cacheDir, preset, trustedKey)` -> `String (JSON)` | XR-271. Карточка кэшированного пресета для экрана правил либо `{error:"no_cache"}`. Кэш сверяется с ключом подписи из профиля (XR-207), пустая строка значит «ключа нет». |
| `nativeMergedToml(cacheDir, preset, trustedKey, rulesJson, defaultAction)` -> `String` | XR-271. Превью `[routing]`: мои правила поверх пресета. Пустой `defaultAction` берёт дефолт клиента. `trustedKey` тот же, что у карточки (XR-207). |

**Обратный колбэк:** `NativeBridge.protectSocket(fd): Boolean` — статический
метод Kotlin, вызывается из Rust при создании исходящих сокетов. Реализация
вызывает `VpnService.protect(fd)` — это защищает сокеты от петли через TUN.

Конфиг передаётся одной JSON-строкой, и обе её половины (сборка из профиля,
разбор в `VpnConfig`) живут в `xr_core::client_config`, а не здесь: мост их
только прокидывает. Правила маршрутизации едут массивом `user_rules`; легаси-
конфиг со строкой `routing_toml` внутри тоже читается, его Rust парсит
`toml::from_str` в `RoutingConfig`.

**Граница паник (XR-220).** Все входные точки моста объявляет макрос
`jni_entry!` из [guard.rs](../xr-android-jni/src/guard.rs). Тело функции
выполняется под `catch_unwind`, пойманная паника пишется в трейсинг и журнал
записью `ERROR [jni]`, а Java-сторона получает оговорённый запасной ответ по
сигнатуре. JSON-функции отвечают своим обычным `{"error"}`, `nativeStart`
строкой ошибки запуска, `nativeGetState` строкой `Error:` (по ней Kotlin гасит
сессию), `nativeNormalizeSsid` и `nativePopPacket` отдают null, булевы
отвечают `false`. Голая extern-функция в lib.rs невозможна. Тест покрытия
в guard.rs требует порождения каждой входной точки макросом.
Раскрутку стека либе включает профиль `android-release` в корневом
Cargo.toml (наследует release и ставит `panic = "unwind"`), его же используют
`xr-android/build.sh` и gradle-таска `buildRustRelease`. Без unwind паника
ядра не разворачивается, `catch_unwind` её не ловит, и процесс приложения
умирает вместе с живым VpnService. Unwind заодно снимает аборт с воркер-потоков
tokio. Упавшая задача движка убивает только себя. Локи ENGINE и HEALTH
берутся через `lock_surviving_poison`. Паника под локом не закрывает
движок навсегда. Моста под iOS в репозитории нет (спайк LLD-39); когда
появится `xr-ios-ffi`, каждой входной точке C FFI нужна такая же граница.

### 4.6 xr-android — мобильное приложение

Kotlin + Jetpack Compose, Material3, MVVM без DI-фреймворка.

Тексты приложения живут не в коде, а в таблице строк (XR-092):
`app/src/main/res/values/strings.xml` это русский по умолчанию,
`values-en/strings.xml` перевод, язык выбирает система по своей локали, своего
переключателя в приложении нет. Русский текст канон, английский идёт следом за
ним. Ключи именуются по экрану (`vpn_`, `main_`, `logs_`, `files_`, `share_`,
`servers_`, `invite_`, `trusted_`, `rules_`, `update_`, `notif_`, `data_`),
подстановка позиционными плейсхолдерами (`%1$s`), счётные подписи через
`plurals`. В Compose строка берётся `stringResource`, в сервисе и слое данных
`getString` по `Context`, во вьюмоделях через `getApplication()`.

Ресурсы это механика Android, и на iOS она не переносится, а вот разделение
переносится целиком: код не носит текстов, а ключи строк станут общим словарём
для `Localizable.strings` при порте (цель XR-278). Поэтому чистая логика без
Android SDK текстов тоже не собирает: `humanShareError` стал
`shareErrorOf` и отдаёт `ShareErrorText` (свой вариант ключом `ShareErrorKind`
плюс аргумент, готовый текст движка отдельным вариантом), общий индикатор синка
отдаёт числа вместо подписи «X из N», а заголовки групп проводника едут
`GroupTitle` (свой ключ либо имя источника от агента). Строку по ключу собирает
слой Compose, и он же держит `when` по enum, полноту которого проверяет
компилятор. Журнал диагностики под это не попадает: записи, которые уходят в
`nativeJournalLog`, остаются русскими и совпадают по языку с движком на Rust.

**Что где живёт (XR-271).** Приложение это обвязка платформы поверх ядра, и
граница проведена так: логика, которая обошлась бы без Android SDK, живёт в
`xr-core`, а Kotlin остаётся тонким.

| В ядре | В приложении |
|---|---|
| Туннель целиком: движок, пул серверов, роутер, fake DNS, статистика | TUN и `VpnService`, уведомление, `protect(fd)` колбэком |
| Конфиг движка: сборка из профиля и разбор (`client_config`) | Профиль в `SharedPreferences` и его редактор |
| Здоровье сессии по счётчикам движка (`health`) | Мордочка и подписи по имени ступени |
| Приглашение: разбор ссылки, claim, TOFU-ключ, профиль из payload'а | Экраны онбординга, имя профиля, хранилище |
| Пресет хаба: кэш, рефреш, карточка, превью `[routing]` | Экран правил, диалоги, копирование в буфер |
| Файловые шары: манифест, план зеркала, докачка, перенос хранилища, импорт по URL, алгебра выбора | Проводник, очередь экрана, SAF и выбор папки, `WorkManager` |
| Обновление приложения: манифест, подпись, SHA-256 APK | Скачивание, `PackageInstaller`, разрешение на установку |
| Доверенные сети: сравнение и нормализация SSID | Список сетей в prefs, сканирование Wi-Fi, автопауза сервисом |
| Журнал и мост `tracing` | Вкладка ленты, поиск, экспорт в файл |

Kotlin держит ещё один слой, который в ядро не поедет и поедет в порт как есть:
чистые функции без Android SDK, раскладывающие данные в ключи строк
(`shareErrorOf`, `syncIndicator`, `serverSelectAction`, `GroupTitle`). Это
словарь экрана, а не бизнес-логика, и он покрыт JVM-юнитами в
`app/src/test/`.

Правило для новой правки простое: если единица логики не трогает Android SDK и
понадобится второму клиенту, её место в `xr-core`, а мост дописывается в
`xr-android-jni`. Обратное тоже верно: lifecycle, разрешения, хранилища
платформы и Compose в ядро не тянутся.

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
  `nativeGetStats`, публикует snapshot, обновляет notification). `stopFromUi()`,
  единая команда стопа для VM через binder, и `clearLog()` тоже. `stopFromUi()`
  и `onRevoke()` (системный отзыв VPN: тумблер в настройках Android или
  перехват другим VPN-клиентом) ведут в общий приватный `stopTunnel(reason:
  VpnStopReason)`, но с разной причиной: `VpnStopReason` задаёт текст записи в
  журнале, и `onRevoke()` больше не пишет «остановлен пользователем», когда
  пользователь в приложении ничего не нажимал (XR-221). `onBind`
  разветвляет: `ACTION_BIND_INTERNAL` → `LocalBinder`, иначе `super.onBind()`
  (штатный `BIND_VPN_SERVICE`). `onStartCommand(intent = null, ...)` делает
  `onStartCommand(intent = null, ...)` приходит на START_STICKY-рестарте
  после смерти процесса: при сохранённом желании быть подключённым туннель
  поднимается заново, без него сервис гасится молча и `START_NOT_STICKY`
  воскрешения не просит. Foreground-уведомление: канал `IMPORTANCE_DEFAULT`,
  `CATEGORY_SERVICE`, `VISIBILITY_PUBLIC`, `setOnlyAlertOnce`, моно-иконка
  `ic_notification`, action «Отключить» через `PendingIntent` на `ACTION_STOP`,
  цвет из `R.color.brand_primary`. `foregroundServiceType="specialUse|location"`
  с пермишеном `FOREGROUND_SERVICE_SPECIAL_USE` и property-декларацией
  подтипа на сервисе: это честный тип для стороннего VPN, переживающий
  фоновый старт, а location платформа фоновому старту срезает (XR-279).
  `bringTunnelUp()` перед `establish()` зовёт `Builder.addDisallowedApplication`
  на `com.google.android.projection.gearhead` (XR-270): Android Auto проверяет
  связь с магнитолой по локальной сети и с этим трафиком в TUN отказывается
  стартовать, ругаясь на VPN, а проксировать его незачем, это связь с
  магнитолой и Google, не с заблокированным ресурсом. На устройстве без пакета
  (обычный телефон без Android Auto) метод кидает `NameNotFoundException`,
  исключение из туннеля молча пропускается, а факт уходит строкой в журнал
  (`excludeAndroidAuto`). Кандидат на такое же исключение при нехватке одного
  gearhead это `com.google.android.gms`, пока не заводился.
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

**Проводник шары как файловый менеджер (XR-251).** Уровень папки собирает
`explorerLevel` в [Share.kt](../xr-android/app/src/main/java/com/xrproxy/app/model/Share.kt),
и порядок строк ему задаёт `SortOrder` (поле `FileSort { NAME, DATE }` плюс
направление): папки остаются выше файлов в любом режиме, у папки датой служит
самый свежий файл под ней, совпавшие ключи разводит имя. Переключатель стоит в
шапке проводника, повторный выбор того же поля разворачивает направление, а
выбор один на все шары и лежит в `ShareStore` рядом с ними: от проводника ждут
одной привычки, а не настройки на каждую папку. Там же живут отметки
просмотренных файлов по паре (шара, путь): её ставит `openLocal` при передаче
файла системному вьюеру, и снятие локальной копии её не отменяет. В строке
метится при этом не просмотренное, а наоборот (XR-256): у файла, который ещё не
открывали, слева стоит точка цветом `primary`, у просмотренного место остаётся
пустым. Глазами ищут то, что ещё не смотрели, и серый глазок на просмотренном
владелец на экране не находил. Отобрать одни непросмотренные умеет фильтр
«Только непросмотренные» из меню вида в шапке: он режет только файлы, папки
уровня остаются на месте, иначе в них не зайти, а включённое состояние
показывает плашка со счётчиком под шапкой, потому что короткий список иначе не
отличить от пропавших файлов. Выбор лежит в `ShareStore` тем же паттерном, что
и порядок строк: один на все шары и переживает перезапуск. Оттуда же приезжает
режим группировки `FileGrouping { NONE, DATE, SOURCE }` (XR-258), который в том
же меню вида собирает список группами: `explorerRows` раскладывает готовый
уровень по `ExplorerRow.Header`/`ExplorerRow.Node`, и заголовки едут одним
списком со строками, иначе ленивый список пришлось бы разворачивать целиком.
Группировка задаёт только порядок групп, сортировка работает внутри группы и
живёт своей кнопкой. По дате группы идут «Сегодня», «На этой неделе» и дальше
по месяцам, по источнику берётся `meta.source` (у ролика это канал), и группы
стоят от той, откуда прилетело свежее, к старым. Файлы без источника и без даты
собираются группой в конце, а не прячутся. Папки в группы не ложатся: своей
даты у папки нет, источника тем более, а потерять их нельзя, иначе в них не
зайти, поэтому они идут блоком «Папки» сверху, как и в списке без групп.
Счётчик в заголовке считает показанные строки, поэтому с включённым фильтром
говорит про непросмотренные. Иконка меню вида горит цветом `primary`, когда
включена группировка или фильтр, и называет режим словами в
`contentDescription`: по цвету иконки экранный сценарий и скринридер его не
прочитают. Имя файла занимает
до двух строк целиком, эллипс остаётся только на совсем длинном хвосте; строка
файла показывает дату из `mtime` манифеста (когда файл появился у агента)
коротким системным форматом. Хвост ютуб-импорта (`[<11 символов base64url>]` перед
расширением) в имени не рисуется: настоящее имя остаётся ключом строки, путём
скачивания и тем, что уходит в JNI, обрезка живёт только на отрисовке.

**Общий индикатор очереди синка (XR-056).** Очередь скачивания одна на все шары
(XR-044), и над списком шар с проводником стоит одна строка про неё: имя файла,
который едет сейчас, счётчик «X из N», кнопка «Стоп» и тонкая полоса по батчу.
Байты и скорость остались на строке файла: карточка во всю ширину показывала
один файл и занимала пол-экрана, а сколько всего осталось, читалось мелким
текстом. Расчёт вынесен из экрана в
[SyncProgress.kt](../xr-android/app/src/main/java/com/xrproxy/app/ui/files/SyncProgress.kt)
и покрыт JVM-юнитом. Считается он по файлам, а не по байтам: агрегатных байт на
очередь никто не отдаёт, каждый файл едет своей передачей, и только доля
текущего файла добавляется дробной частью, иначе батч из одного файла держал бы
полосу в нуле до конца. Размер батча это пройденные плюс оставшиеся, счётчик
пройденных живёт в `queueDone` и обнуляется, когда новый батч встаёт в пустую
очередь. Фоновое зеркало считается по своему снимку (`files_done`/`files_total`),
и «Стоп» на нём останавливает саму передачу, а на своей очереди снимает с неё
все файлы той же отменой, что и строка. Показать фоновый синк мешал не расчёт, а
опрос: цикл `ensureTransferPolling` жил, пока открыта шара, занята очередь или
идёт перенос, а на списке шар не выполнено ничего из этого, и снимок никто не
читал. Теперь опрос держится всё время, пока вкладка «Файлы» на экране
(`watchTransfers`/`unwatchTransfers` из `DisposableEffect`). Перенос хранилища
(XR-043) остаётся при своей карточке со своим стопом, общий индикатор при нём
прячется.

**Модель состояния на Android:**

- `ConnectPhase { Idle, NeedsPermission, Starting, Connecting, Connected, Stopping }`
  — единственный источник для рендера «Connect / Cancel / Disconnect» и
  крутилки. Computed `connected`/`connecting` сохранены для совместимости
  UI-кода, но внутри выводятся из `phase`.
- `recentErrors: List<String>` содержит единственный источник журнала и бадджа
  Log. Бадж/заголовок считают WARN-строки по критерию `" WARN "` (тот же, что
  `colorizeLog`). `relayErrors: Long` осталась только как debug-метрика в
  статистике, UI-бадж её не читает. Старое поле `errorLog: String` удалено; его
  заменило `logLines: List<String>`, содержащее хвост единого журнала (XR-042),
  который обновляется методом `refreshLog()` через `nativeJournalTail()`.
- `OnboardingState { ShowingWelcome, Loading(hubUrl), ConfirmInvite(...),
  InviteError(...), Completed }`
  — параллельный StateFlow (LLD-04). Рендер MainActivity до `Completed`
  подменяет главный Scaffold onboarding-экранами; переход в `Completed`
  происходит после успешного `applyInvite` или при ручной настройке.
  `initialOnboardingState()` смотрит на prefs: если пусты `server_address`
  и `hub_url`, показываем Welcome.

Хранилище настроек это SharedPreferences `xr_proxy`. Ключи LLD-04 `hub_url`,
`hub_preset` и `trusted_public_key` пишутся при Apply инвайта, читаются в
`buildConfigJson` и включают в движке PresetCache + периодический
sanity-check раз в 5 минут. Тем же ключом движок проверяет подпись пресета
(XR-207). Ключ приехал из ручки `/public-key` при Apply и проверяет заодно
манифест обновлений. Кэш пресета живёт в `filesDir/presets/<name>.json`.

**Инвайт и смена сервера под живым туннелем (XR-088).** Ручного «сначала
отключите VPN» нет ни при Apply инвайта, ни при выборе другого сервера.
Решение о том, что делать с подключением, лежит отдельным файлом
[ConnectionSwitch.kt](../xr-android/app/src/main/java/com/xrproxy/app/ui/ConnectionSwitch.kt)
чистой логикой без Android SDK и покрыто JVM-юнитом. Инвайт под поднятым
туннелем только пополняет список серверов: активный профиль движок читает на
старте, и подмена его на ходу оставила бы карточку с одним сервером, а трафик
в другом. Первый профиль приложения активен всегда, иначе главному экрану
нечего показывать. Выбор другого сервера при живом туннеле идёт
авто-реконнектом: `reconnectActive` гасит туннель, дожидается `Idle` и
поднимает заново на новом профиле, тем же путём, каким давно применяется
правка активного сервера. Ход виден по фазам подключения на главном экране,
причина мигания приезжает снекбаром.

Сам claim (`POST /api/v1/invite/:token/claim`) идёт обычным reqwest'ом из
`xr-core::onboarding`, без `protect(fd)`, то есть на Android под поднятым
туннелем уходит в TUN и приходит к хабу через прокси. Так и было: сведения об
инвайте (`GET /api/v1/invite/:token`) ходят тем же путём с самого начала, и
экран подтверждения под живым туннелем открывался. Оставлено намеренно: хаб в
стране пользователя бывает доступен как раз только через туннель, а обход TUN
потребовал бы своего коннектора с андроидным `VpnService.protect` внутри
общего крейта. Туннель на паузе (доверенная сеть) TUN закрывает, поэтому
запрос там уходит по обычной сети, а не в чёрную дыру.

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
делает то же на `ureq`. Потребители `delete_file`: тот же `xr-share rm` и
приложение (XR-250), где удаление из шары живёт на экране информации о файле
(XR-257), показывается только при `share:write` в скоупе токена и спрашивает
подтверждение; хеш строки манифеста уходит в `If-Match`, поэтому подменённый на
агенте файл даёт `412`, а не пропадает. Обёртка это `nativeDeleteFile` в
[xr-android-jni](../xr-android-jni/src/lib.rs), по успеху приложение убирает
локальную копию и перезапрашивает манифест.

**Одна передача на процесс, и отмена у неё адресная (XR-217).** Скачивание,
синк шары и перенос хранилища ходят через один контроллер в
[sync.rs](../xr-core/src/sync.rs): `TransferGuard::acquire` занимает
единственный слот (второй желающий получает `busy` и приходит следующим
циклом), обнуляет счётчики и даёт передаче номер, а `transfer_snapshot` отдаёт
UI этот номер вместе с прогрессом и именем шары. Отменяется передача по номеру:
`transfer_cancel(id)` ставит просьбу, только если названная передача идёт
прямо сейчас, и отвечает `false`, когда та уже закончилась. Номера не
повторяются, Drop гварда снимает и номер, и просьбу, поэтому опоздавшая отмена
никуда не попадает. Пока флаг отмены был один на весь процесс, попасть было
куда: приложение сперва читает снимок, решает по нему («идёт та самая шара, тот
самый файл»), и только следующим вызовом отменяет, а между этими двумя шагами
передача успевает закончиться и стартовать заново. Отмена шары A обрывала уже
начатое скачивание шары B, причём молча, обычной ошибкой `cancelled` в отчёте.
На стороне Kotlin номер едет из того же снимка, по которому принято решение
(`cancelNative` в
[FilesViewModel.kt](../xr-android/app/src/main/java/com/xrproxy/app/ui/files/FilesViewModel.kt)),
и `nativeCancelTransfer(id)` возвращает, попала ли просьба.

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
([import.rs](../xr-share/src/import.rs)): одна активная, очередь глубины
`queue_depth` из того же блока (по умолчанию 4, за нею 429; очередь одна на
весь агент, поэтому в доме, где импортируют несколько человек, её расширяют
конфигом), завершённые видны час, рестарт таблицу забывает и подметает
`.xr-import-*` (зарезервирован весь неймспейс `.xr-`). Процесс на джобу в своей группе, argv
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
качества, строка с прогрессом и отменой на каждую свою джобу). Гейта на второй
импорт у приложения нет (XR-175): ссылки кидаются одна за другой, очередь
держит агент, а ждущая своей очереди строка подписана «в очереди». Список джоб
живёт в состоянии экрана и переживает уход в соседнюю шару, но не смерть
процесса; один цикл раз в 2 с обходит все строки сразу, три промаха сети подряд
на джобу терпятся, отказ агента по переполненной очереди (429) виден текстом
«Очередь импорта заполнена, попробуй позже». Чужие джобы приложение не
показывает, листинга джоб у агента для этого нет.
Десктопный харнесс `xr-share import` поллит джобу до конца. Сорвавшаяся джоба
показывает причину не тостом, а диалогом «Импорт не удался» со скроллом и
кнопкой «Скопировать» (XR-161): в тосте с Android 12 помещаются две строки,
а приходит хвост stderr плагина.

**Откуда взялся файл (XR-255).** У строки листинга есть необязательное поле
`meta` ([share.rs](../xr-proto/src/share.rs), тип `FileMeta`): адрес страницы
(`url`), название источника (`source`, у видео это канал), адрес страницы
источника (`source_url`), дата публикации (`published`, всегда `YYYY-MM-DD`) и
заголовок (`title`). Пустое поле на провод не едет, у файла без источника поля
нет вовсе, и залитая руками шара выглядит на проводе ровно как до XR-255.

Едет источник **внутри манифеста**, а не отдельной ручкой: манифест это
единственное, что потребитель проверяет по пиннингованному `agent_pubkey`
(XR-046), и своя ручка потребовала бы второго домена подписи, второго запроса и
второго режима отказа ради того же самого. Заодно это делает канал и ссылку
такими же неподделываемыми, как хеш.

Хранит источник сам агент, по индексу на шару: JSON-файл `.xr-meta.json` в
корне шары ([meta.rs](../xr-share/src/meta.rs)), ключ это относительный путь
файла, тот же, каким его зовёт манифест. Индекс, а не сайдкар у каждого файла,
потому что читается это на каждом обходе листинга, который обязан оставаться
мгновенным (XR-039): одно чтение маленького JSON против open на файл. Ключ путь,
а не sha256, потому что листинг сознательно не хеширует, и у только что
импортированного файла хеша ещё нет; вдобавок один хеш склеил бы два одинаковых
файла и терялся бы при переупаковке. Плата за это переименование мимо агента:
роута переименования у агента нет, свои `PUT`/`DELETE` строку снимают, а
осиротевшие подметает разовый проход при старте. Сам индекс лежит в
зарезервированном неймспейсе `.xr-`, поэтому не виден в листинге и недостижим
роутом.

Пишет источник импорт (LLD-29): джоба знает ссылку с самого начала, поэтому её
получает каждый опубликованный файл, а плагин дополняет остальное строками в
`.xr-meta.tsv` своей джоб-папки, по одной на файл, полями через табуляцию
(`<файл>`, `<url>`, `<источник>`, `<адрес источника>`, `<дата>`, `<заголовок>`).
Референсный yt-dlp пишет их сам через `--print-to-file after_move:...`: печать в
файл, а не на экран, не тянет за собой `--quiet` и не мешает прогрессу. Плагин,
который не сказал ничего, импорт не срывает, у файла просто остаётся одна
ссылка. Строка ложится после rename файла, поэтому недоехавшая джоба не
оставляет источника без файла. По уже скачанным файлам источник дозаполняет
проход при старте агента: из хвоста `[<11 символов base64url>]` в имени
собирается ссылка на ролик, канала в имени нет и он остаётся честно пустым.

Показывает источник экран информации о файле (XR-257,
[FilesScreen.kt](../xr-android/app/src/main/java/com/xrproxy/app/ui/files/FilesScreen.kt),
`FileInfoScreen`): он открывается долгим нажатием на любой строке и обычным
тапом по нескачанному файлу, скачанный по тапу по-прежнему открывается сразу.
Блок «Файл» несёт путь, размер, дату, признак просмотра и начало SHA-256, блок
«Откуда файл» страницу импорта, канал автора и дату публикации; обе ссылки
уходят в браузер `ACTION_VIEW`. Пустой блок «Откуда файл» не прячется, а
называет причину: спрятанный не отличить от «экран не умеет это показывать».
Действия над файлом (открыть или скачать, снять локальную копию, удалить из
шары) переехали сюда же из строки и диалога. Открытый экран это поле
`detailsPath` в состоянии `FilesViewModel`, а не локальный стейт: строку экран
берёт из живого манифеста, поэтому удалённый из шары файл закрывает его сам, а
«Назад» проходит экран, папку и шару одной кнопкой. На стороне Kotlin поле
`meta` разбирает `ManifestEntry` в
[Share.kt](../xr-android/app/src/main/java/com/xrproxy/app/model/Share.kt) и
кладёт его в кэш листинга, иначе офлайн-заход терял бы ссылку на канал.

**Git-контур шары (LLD-33 фаза 1, XR-188).** Writable-шара может нести историю
изменений: git-репозиторий, который агент ведёт сам. Опт-ин флагом `git = true`
в конфиге (только вместе с `writable`) или командой `xr-share share <dir>
--writable --git`; git обязан быть в PATH на машине агента, и `share` проверяет
это до записи конфига. Репозиторий живёт вне рабочей папки, в
`<state_dir>/git/<share_id>` (bare-style `GIT_DIR` + `core.worktree`). Папка
остаётся без `.git` и служебных файлов, владелец редактирует её в любом
редакторе, не замечая git.

Авто-коммит сводит все источники изменений в один цикл: правку владельца,
`PUT`/`DELETE` по HTTP, публикацию импорта. Watcher файловой системы коммитит
после двухсекундного debounce, пятиминутный страховочный скан ловит то, что
watcher'ы пропускают на сетевых ФС. Авторство коммитов берётся из `git_author`
конфига, иначе hostname, так что соавторы видят, чья машина писала. В историю
не попадают зарезервированные `.xr-*` и файлы больше `git_max_file_mb` (по
умолчанию 10 МиБ): они продолжают ездить манифест-контуром.

Транспорт это smart HTTP спавном системного git
([server.rs](../xr-share/src/server.rs)): `GET /{id}/git/info/refs` плюс
`POST /{id}/git/git-upload-pack` и `git-receive-pack`, все `--stateless-rpc`,
тела стримятся в обе стороны, gzip-запросы принимаются. Процесс на запрос,
дедлайн 5 минут с убийством группы процессов. Лестница гейтов повторяет запись:
`404` нет шары, `403` контур выключен, `403` шара не writable, `401`/`403`
токен без `share:write`. Fetch тоже живёт под `share:write`: репозиторий остаётся частной историей
владельца. Штатный
клиент ходит через `git -c http.extraHeader="Authorization: Bearer <token>"`.

Материализацию пуша предполагалось отдать `receive.denyCurrentBranch =
updateInstead`, но с bare-style топологией тот не работает: `receive-pack`
резолвит worktree своих детей в сам `GIT_DIR`, `diff-files` вечно видит
«изменения» в сервисных файлах и отказывает каждому пушу. Вместо него стоит
`denyCurrentBranch = ignore` с receive-hook'ами
([gitrepo.rs](../xr-share/src/gitrepo.rs)). `pre-receive` режет пуш в грязную
папку именованным отказом. Отдельно режется пуш поверх локального файла вне
истории. Материализация затёрла бы такой файл молча, а отказ сохраняет
черновик владельца, которого авто-коммит ещё не забрал. Шара, пустовавшая при
`share --git`, первого пуша не боится. У unborn main нет базы для сравнения,
грязью он не считается. `post-receive` выполняет `git read-tree -u --reset
HEAD` по `refs/heads/main`. Non-fast-forward пуш и удаление рефов запрещены,
потолок пуша `receive.maxInputSize` равен `8x git_max_file_mb + 64` МиБ
(единица там байты, не мегабайты). Push сериализуется с циклом авто-коммита
общим `op_lock`, поэтому материализация и авто-коммит не пересекаются.
`unshare` репозиторий оставляет на диске и печатает путь: папка может
вернуться, и её история не должна пропадать вместе с флагом. Настройки
`git_author` и `git_max_file_mb` перечитываются hot-reload конфига.
Неизменённые шары сохраняют живой контур и канал HEAD, изменившаяся настройка
перезапускает контур и доезжает до репозитория без рестарта агента.

`GET /{id}/git/head` отдаёт SHA `main` с ed25519-подписью того же
identity-ключа, что подписывает манифест (домен `xr-share-git-head`, байты
домена в [share.rs](../xr-proto/src/share.rs)). Роут умеет long-poll: `since` с
известным head и бюджет `wait` в секундах паркуют запрос до следующего коммита
или пуша (watch-канал шары). Уведомление об изменениях стоит запрос в минуту
вместо шторма опросов. Нерождённый `main` отвечает пустой строкой. Отказ `401`
несёт `WWW-Authenticate: Basic`, а basic-заголовок принимается наравне с
bearer. libgit2 и штатный git шлют креды только после такого вызова. Пароль
basic это тот же токен-блоб.

**Харнесс синка (LLD-33 фаза 2, XR-189).** Сторона соавтора это подкоманда
`xr-share sync --invite <t> --share <id|имя> <папка>`
([sync_cmd.rs](../xr-share/src/sync_cmd.rs)) за cargo-фичей `sync` (по
умолчанию включена; `--no-default-features` даёт агента без libgit2 вовсе).
Внутри libgit2 (`git2`, vendored, без TLS-бэкендов), поэтому git в PATH
соавтору не нужен. Рабочая папка у него это обычный клон с `.git`. Владелец
папки он сам, и прятать тут нечего. Пустая или несуществующая папка
клонируется. Пустовавшая шара клонируется как `init` плюс origin. Веток там
ещё нет, и первый push соавтора создаёт `main`. Непустая посторонняя папка
это отказ с названной причиной.

Цикл симметричен агентскому. Watcher с тем же двухсекундным дебаунсом и
пятиминутным страховочным сканом коммитит локальные правки. Отдельный поток
висит на long-poll HEAD, и смена головы поднимает fetch, merge, push. Цикл
блокирующий нарочно. git2 синхронен, а рантайм заводится только под
relay-мост. Колпак размера и неймспейс `.xr-*` те же, что у агента (константа
`GIT_MAX_FILE_MB` в xr-proto), плюс собственный `.git`. Отвергнутый push это
сигнал повторить fetch-merge-push. Так лечится и гонка двух пишущих, и грязная
папка владельца, чей pre-receive отбил материализацию. Протухший за неделю
токен перезапрашивается по инвайту. Пять неудачных проходов подряд
перевыбирают путь до агента.

Слияние живёт здесь, у соавтора. Fast-forward выставляет рабочую копию.
Настоящий merge идёт трёхсторонним diff по строкам, и непересекающиеся правки
сливаются сами. Пересечение по строкам не сливается. Файл остаётся локальной
версией, а встречная кладётся рядом целым файлом `<имя> (конфликт <автор>
<sha7>)<расширение>`. Копия входит в тот же merge-коммит и доезжает до всех
участников. Маркеров `<<<<<<<` в папке не появляется вовсе. В самосинкающейся
системе авто-коммит разослал бы их как обычный контент, и битый текст стал бы
«разрешением» конфликта. Имя копии несёт автора и короткий SHA встречного
коммита, поэтому повторный разбор той же развилки даёт то же имя.

Транспорт перебирает адреса как `pull` и `push`, LAN раньше публичного,
relay последним. Кандидат принимается только если ручка HEAD ответила
подписью, сходящейся с `agent_pubkey` гранта. Это разом проверка достижимости
и anti-wrong-host. После fetch приехавший `main` сверяется с подписанным HEAD.
Дальше авторитет тянет хеш-связность git. На relay-пути харнесс поднимает
loopback-мост и несёт байты identity-TLS-стримом (`relay_client` плюс
`relay_tls`). libgit2 говорит с `127.0.0.1` обычным plain HTTP и о relay не
знает. Пиннинг остаётся в одном месте, в нашем rustls-коде. Это зеркало
`LoopbackForwarder` с обратными ролями. Там TLS терминирует потребитель, здесь
мост, потому что потребитель это libgit2 без крипто-бэкендов.

**Web-страница шары и экран истории (LLD-33 фаза 3, XR-190).** Каждая шара
отдаёт вшитую web-страницу `GET /{id}/web`: одностраничник без CDN и внешних
запросов, html лежит в [web/share.html](../xr-share/src/web/share.html) и
вшивается `include_str!`. Токен едет в адресе как `?token=<blob>`, тот же
блоб, что у `Bearer`. Поэтому ссылка открывается на любой машине с браузером,
без приложения и git-клиента. Гейт роута только `share:read`. Read-токену
страница показывает дерево из манифеста и рендерит md на клиенте
(экранирование до разметки, внешние ссылки только http(s)). Write-токен
открывает историю файла и правку.

История живёт на двух JSON-роутах за `share:write`:
`GET /{id}/git/log?path=&limit=` (лимит до 500) и
`GET /{id}/git/diff?from=&to=&path=` (кап вывода 1 МиБ). Оба выполняются
спавном git. Sha пропускается только hex 4-40 символов, pathspec-магия
(ведущее `:`) и выход за папку отбракованы. Правка в textarea уезжает тем же
PUT с `If-Match`. Ответ `412` предлагает перечитать, ведь файл на агенте уже
другой, и молча перетирать чужую правку страница не станет.

Ссылку держателю write-гранта печатает `xr-share weblink --invite --share`.
Команда берёт токен из уже выданного гранта, минт не трогается, новых каналов
выпуска write-скеупа не появляется. Вывод несёт предупреждение, что ссылка
равна самому токену и остаётся в истории браузера.

В Android за историей стоят JNI `nativeGitLog` и `nativeUploadFile`
([lib.rs](../xr-android-jni/src/lib.rs)). Экран истории показывает коммиты
одного файла (слово, автор, дата, sha7), дифф остаётся странице шары. Мелкая
правка текстовых файлов (тот же список расширений, что у страницы) открывает
диалог с полем и уезжает PUT-контуром с `If-Match`. Отказ `412` закрывает
диалог и обновляет манифест, текст в поле к этому моменту устарел. Пункт
«Открыть в браузере» в листе действий шары стоит под canWrite и открывает ту
же страницу шары, ссылку собирает
[ShareWeb.kt](../xr-android/app/src/main/java/com/xrproxy/app/ui/files/ShareWeb.kt).

**Осталось за пределами XR-103:** JNI/Kotlin проброс relay-плеча гранта в
`sync_share_grant` на Android; identity-TLS на прямом листенере агента (сейчас
прямой путь plain-HTTP, целостность закрыта подписью манифеста); relay-fallback в
десктопном `xr-share pull`; отметка «через relay» в Admin SPA (данные уже
отдаются, нужен пересбор встроенного SPA).

### 4.8 Публикации: локальный сервис агента наружу (LLD-38 фаза 1, XR-262)

Транзит до машины за NAT уже есть (4.7), и на нём же живёт вторая роль агента:
открыть наружу локальный HTTP-сервис этой машины. Публикация это тройка «имя,
агент, локальный адрес». Имя с агентом помнит хаб (`ExposeRecord`, каталог
`data_dir/expose/`), локальный адрес не уезжает с машины: он берётся строго из
`[[expose]]` в конфиге агента по имени из запроса. Приходящий запрос апстрим не
выбирает никогда, поэтому вход не становится плечом SSRF внутрь домашней сети.
Имя обязано быть DNS-меткой, потому что в фазе 2 оно же станет поддоменом
браузерного входа, и уникально на весь хаб: чужой агент занятое имя не займёт
(`409`).

Гейт стоит у агента, и одного relay-токена ему мало. Держатель валидного токена
на шару того же агента иначе дотянулся бы до локального сервиса, подставив
заголовок публикации: транзит к машине и выбор того, что на ней открыто, это
разные права. Поэтому у публикации свой мандат `ExposeToken` (домен подписи
`xr-expose-token`, поля `publication`, `agent_pubkey`, `exp`), который выписывает
только хаб и проверяет офлайн только агент, тем же пришпиленным ключом, которым
он проверяет токены шар. Отдельный тип, а не скоуп внутри `ShareToken`: смешение
неймспейсов имён публикаций и идентификаторов шар рождает ровно один класс
ошибок, токен на шару, открывший публикацию.

- [expose.rs](../xr-share/src/expose.rs) несёт гейт (`ExposeGate`, офлайн-проверка
  мандата), проксирование на апстрим (клиент hyper, тело потоком в обе стороны,
  снятие служебных заголовков, возврат `X-Xr-Forwarded-Authorization` на место
  `Authorization` приложения) и команды `expose add/rm/ls/open`.
- `Host` переписывается на адрес апстрима, а внешнее имя уезжает в
  `X-Forwarded-Host`, если посредник не назвал его сам. Без этого публикация
  упиралась бы ровно в те сервисы, ради которых заведена: webpack-dev-server,
  Vite и CRA сверяют `Host` со своим списком разрешённых и на внешнее имя
  отвечают `Invalid Host header` при верно пройденном гейте. Цена в том, что
  приложение, которое строит абсолютные ссылки из `Host`, увидит локальный
  адрес; внешнее имя ему полагается брать из `X-Forwarded-Host`, как у любого
  сервиса за прокси.
- В [relay.rs](../xr-share/src/relay.rs) обслуживание реверс-стрима выбирает
  обработчик по заголовку `X-Xr-Expose`: назвали публикацию - гейт и мандат, не
  назвали - прежний роутер шары, как и до LLD-38. Клиентский путь и код relay не
  изменились ни на строчку.
- Апгрейд едет насквозь (XR-264): `Upgrade` и `Connection` объявляются апстриму
  заново, и после его `101` агент перестаёт быть HTTP-посредником, гоняя байты в
  обе стороны. Объявляет он их только тогда, когда есть чем сплайсить: обещать
  `101` и не суметь переключиться хуже, чем не обещать. Своего срока жизни
  такому соединению агент не назначает, потолок сплайса знает фронт (4.10).
- В [web.rs](../xr-hub/src/api/web.rs) реестр публикаций и минт мандатов под
  мандатом агента: `POST /api/v1/expose/add`, `GET /api/v1/expose`,
  `DELETE /api/v1/expose/{name}`, `POST /api/v1/expose/{name}/mandate`.

Отказы названы поимённо и не сваливаются в одну пятисотку: мандата нет или он
чужой это `403` (до апстрима запрос не доходит), имя есть в хабе, но записи в
конфиге агента нет это `404` с этой самой причиной, локальный сервис не поднят
это `502` с адресом и текстом отказа. Публикации видно и без браузера: в логе
агента на старте строкой на каждую, командой `expose ls` с пометкой,
обслуживается ли она (запись в хабе без записи в конфиге не обслуживается, и
это тоже видно). Список `[[expose]]` перечитывается горячо тем же
наблюдателем конфига, что и шары, поэтому `expose add` не требует перезапуска.

Проверяется всё без VPS: `xr-share expose open --name dash` поднимает локальный
форвардер, который говорит в тот же обработчик, что и реверс-стрим, подставляя
мандат вместо будущего фронта, и печатает адрес для curl. Тот же форвардер с
`--without-mandate` показывает работу гейта: `403` вместо тела сервиса.

### 4.9 Фундамент браузерного входа: коннектор и служебные ручки (LLD-38 фаза 2, XR-267)

Между реестром публикаций и самим фронтом лежит слой, которым фронт пользуется
и который живёт без него: коннектор до агента в `xr-proto` и три служебные
ручки хаба под общим секретом.

`relay_tls_connect(endpoint)` в
[relay_client.rs](../xr-proto/src/relay_client.rs) открывает стрим к relay по
токену эндпоинта и поднимает поверх него pinned-TLS до агента, возвращая
готовый `AsyncRead + AsyncWrite` для hyper. Пин берётся из relay-токена
(`agent_pubkey`), поэтому право на транзит и пин ключа не могут разъехаться, а
rustls-конфиг строится один раз на эндпоинт. Рядом лежит старый
loopback-форвардер: он остаётся у Android и `share pull`, где HTTP-клиент умеет
только сокет, а посреднику сокет на каждое соединение обошёлся бы лишним файлом
и лишним копированием. Оттуда же `probe_agent_online`: открыть стрим и сразу
закрыть, не начиная TLS, чтобы получить вердикт relay о живости агента.

Служебные ручки хаба (`POST /api/v1/web/route`,
`POST /api/v1/web/verify-password`, `GET /api/v1/web/status`) закрыты общим
секретом `[web] shared_secret`, а не админской сессией: у транзитного сервиса
не должно быть прав админки, и приватного ключа хаба он не видит вовсе. Маршрут
приходит собранным целиком (`WebRoute`: агент, дескриптор relay, relay-токен,
мандат публикации, `exp` и потолок жизни сплайса), поэтому взломанный фронт не
выпишет себе мандат на агента, которого хаб ему не отдавал. Relay-токен для
публикации минтится с `share_id` вида `web:<имя>`, и браузерный расход виден в
агрегатах relay отдельной строкой, не смешиваясь с шарами.

Пароль владельца проверяет хаб той же учётной базой, что и админка
(`password_matches`), и отвечает только вердиктом: второго пароля владельцу не
заводим, второго места хранения хэшей тоже. Перебор гасится и на хабе: первые
три промаха на имя проходят свободно, дальше задержка удваивается до пяти
минут, верный пароль снимает счётчик, отказы уходят в лог. Статус отвечает
списком публикаций с полем `online`, где `true` это агент в реестре relay,
`false` его там нет, `null` спросить не вышло (причина рядом): «не знаю» и
«выключен» это разные ответы. Живость хаб спрашивает у relay тем же
`probe_agent_online`, своего состояния о ней не держит и кода relay не трогает.

Web-домен нигде не зашит: он приходит конфигом `[web] domain` и попадает в
статус полем `host` (`<имя>.<домен>`). Блока `[web]` нет значит браузерный вход
выключен, и служебные ручки отвечают `503` с этой причиной, а реестр публикаций
работает как работал. Владельцу публикации видно разделом «Публикации» в
админке (`GET/DELETE /api/v1/admin/exposes`): снять поддомен можно и с
выключенной машины, реестр общий на хаб.

**Чего здесь ещё нет.** Страница шары наружу это фаза 4 (XR-265).

### 4.10 xr-web: браузерный вход владельца (LLD-38 фазы 2 и 3, XR-263, XR-264)

Отдельный сервис на VPS, а не режим relay, и это главное решение дизайна.
Браузер не умеет ни обфусцированный mux, ни пиннинг ключа агента, сертификат ему
нужен на публичное имя, поэтому TLS терминирует VPS и на браузерном пути
посредник видит содержимое. Обещание «relay ничего не видит» от этого не
размывается: relay остаётся слепым и не правится ни на строчку, клиентский путь
(Android, `pull`, `sync`) идёт как шёл, а плейнтекст живёт в памяти другого
процесса, со своим юнитом и своими правами. Владельцу это сказано прямо на
странице входа строкой «трафик расшифровывается на сервере входа».

Адресация поддоменом (`https://<имя>.<web-домен>`), потому что проксируемое
приложение видит себя в корне: относительные и абсолютные пути, адреса
WebSocket, его собственные cookie и редиректы работают без переписывания. Цена
это wildcard-сертификат на фронте. Web-домен нигде не зашит, он приходит
конфигом `[web] domain` (на боевом узле `web.zoobr.top`). Метка `s`
зарезервирована под страницы шар (фаза 4) и публикацией не считается.

Вход и сессия ([session.rs](../xr-web/src/session.rs),
[app.rs](../xr-web/src/app.rs)):

- Пароль владельца проверяет хаб (`POST /api/v1/web/verify-password`), своей
  учётной базы у фронта нет: второй пароль владельцу помнить незачем, а второе
  место хранения хэшей это второе место утечки.
- Сессия своя, потому что браузеру нужна cookie, а хаб cookie никому не ставит.
  `xrweb=<токен>` с `HttpOnly; Secure; SameSite=Lax; Path=/` и **без атрибута
  `Domain`**: host-only cookie не утекает между публикациями, а `SameSite=Lax`
  режет cross-site POST ещё до приложения. Тот же рубеж держит сервер: сессия
  помнит свою публикацию и на чужом поддомене не считается, потому что токен
  могут принести и мимо браузера. TTL неделя с продлением при активности,
  сессии в памяти, рестарт разлогинивает всех.
- Нет сессии: обычный запрос получает форму входа (`200`), запрос с
  `Accept: application/json` или `X-Requested-With` получает `401` и
  `{"error":"unauthenticated"}`, чтобы XHR дашборда видел код, а не HTML.
- Служебные пути живут под префиксом `/.xr-web/` (`healthz`, `login`, `logout`)
  и до агента не доезжают: у приложения не отбирается ни один его путь.
- Перебор пароля гасится на обеих сторонах: фронт считает промахи на пару
  «адрес, публикация» с растущей задержкой, у хаба свой лимит на ручку.

Путь запроса ([hub.rs](../xr-web/src/hub.rs), [pool.rs](../xr-web/src/pool.rs)):
маршрут публикации берётся у хаба (`POST /api/v1/web/route`) и живёт в кеше до
`exp` минус минута, поэтому страница с десятком запросов не стоит десяти походов
в хаб. Соединение до агента (mux к relay плюс pinned-TLS) лежит в пуле на
публикацию и переиспользуется: каждое новое стоит лишних RTT через VPS. Аренда
соединения кончается не с ответом, а с последним кадром его тела, поэтому
следующий запрос не получит середину чужого ответа, а большой файл не собирается
в памяти VPS. Апгрейд забирает аренду насовсем (`detach`): после `101` HTTP на
соединении больше не живёт. До агента запрос едет как пришёл (метод, путь, query, тело потоком)
плюс `X-Xr-Expose`, мандат публикации в `Authorization` (свой заголовок браузера
переезжает в `X-Xr-Forwarded-Authorization`), `X-Forwarded-Proto: https` и
`X-Forwarded-Host`. Адрес браузера дальше VPS не едет: `X-Forwarded-For` и
родня снимаются, адрес остаётся в логе фронта. Своя cookie сессии тоже
снимается, cookie приложения едут целыми.

Долгие соединения ([upgrade.rs](../xr-web/src/upgrade.rs)): апгрейд идёт
насквозь, кадры фронт не разбирает, ping/pong это дело сторон. Живёт такое
соединение не дольше, чем ему позволяет relay: `splice_lifetime_secs` рубит
сплайс жёстко и молча, а для живой ленты дашборда молчаливый обрыв неотличим от
зависания. Поэтому фронт закрывает апгрейд сам и заранее, штатным закрытием
(`1001 going away` у WebSocket, обычный FIN у прочих апгрейдов): приложение
получает событие закрытия и переподключается, как оно это делает при потере
Wi-Fi. Потолок приходит в маршруте от хаба, запас это минута, но не больше
пятой части потолка (на коротком лимите проверки минута съела бы его целиком), а
отсчитывается срок от рождения соединения, потому что relay отмеряет свой
потолок от открытия стрима, а соединение могло полежать в пуле. Единственное,
что фронт всё же считает в кадрах, это их границы: служебный `close`, всунутый в
середину чужого кадра, приехал бы пиру мусором вместо закрытия. Выключенная
машина на пути апгрейда отвечает тем же `502` с причиной за один RTT, а не
висящим рукопожатием.

Отказы названы поимённо: хаб не знает публикации это `404`, хаб не ответил
`502` с причиной, агента нет в реестре relay `502` со страницей «машина не на
связи» (ретраев на этом пути нет, вердикт relay приходит за один RTT), агент
отверг мандат это его `403`, после которого маршрут забывается, чтобы следующий
заход взял свежий. Лог пишет вход (успех и отказ с адресом), выбор маршрута,
отказ мандата, обрыв туннеля, начало апгрейда со сроком штатного закрытия, само
закрытие и строку `метод-статус-длительность`; тела, query-строки и cookie в лог
не попадают. Живость публикации смотрится не в логе, а
`GET /api/v1/web/status` на хабе (4.9): выключенная машина это `online: false`.

Проверяется всё скриптом
[check-browser-entry.py](../scripts/check-browser-entry.py): он поднимает
синтетический сервис на машине агента (страница по GET, эхо по WebSocket),
держит через вход живой WebSocket, гоняет кадры и дожидается штатного закрытия
(`browser entry ws: ok`), а подкомандой `offline` судит выключенную машину:
`502` с названной причиной за миллисекунды и публикация не на связи в статусе
хаба.

Раскладка: `xr-setup server --with-web --web-domain <домен>` ставит бинарь,
конфиг с общим секретом (он же дописывается блоком `[web]` в конфиг хаба, если
хаб на этой машине), юнит `deploy/xr-web.service` и печатает находки про то,
чего установщик сделать не может: DNS-запись `*.<домен>`, wildcard-сертификат и
правило фронта. Сам фронт слушает `127.0.0.1:8090` за тем же nginx/Cloudflare,
что выводит наружу хаб; блок `[tls]` в конфиге это установка без фронта.

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

Флоу на клиенте это устройство целиком, пара `(адрес, порт)` в LAN. Игровые
приставки сидят на фиксированных портах (Xbox 3074, PS 3478), поэтому две
консоли одной модели в одной LAN дают один и тот же `src_port`. Обратно из
туннеля приходят только `src_port` и `dst`, адреса устройства `RelayPacket` не
несёт, так что развести такие флоу можно лишь тем, что доезжает назад: клиент
ведёт свой NAT и назначает устройству туннельный порт, уникальный на всю
таблицу. Настоящий номер достаётся первому владельцу, конфликтующим идёт первый
свободный из пула 40000-65000, выдача детерминированная. Ответ из туннеля
разбирается по туннельному порту и находит ровно одно устройство, спуфинг
источника прежний. Протухший флоу возвращает свой номер в пул, а исчерпание
пула отбрасывает пакет со строкой в журнале, relay продолжает работать.

Потолок таблицы задаёт `max_flows` (по умолчанию 1024). Флоу сверх потолка
вытесняет наименее свежий, и его туннельный порт возвращается в пул тем же
путём, что и по таймауту. Живые флоу двигают время активности каждым пакетом
в обе стороны, очередь вытеснения до них не доходит. Отказ новому не годится:
устройство, пишущее по многим портам, держало бы потолок до конца flow_timeout
и оставило бы LAN без relay.

Маппинг endpoint-independent: номер выдаётся на устройство и держится на всех
его адресатов сразу. Ключ с адресатом сделал бы NAT на VPS symmetric, а у
Switch и Xbox от типа NAT зависит мультиплеер, ради которого relay и заведён.
По той же причине спуфится отправитель пакета, а не адресат исходящего: у
входящего P2P пакет приходит от пира, которому мы не писали. Спуфящие сокеты
живут своим временем последнего использования и уходят по простою дольше
`flow_timeout`: адресатов в таблице флоу нет, а через живого пира шлют чаще.

На VPS поток принадлежит паре (пир, `src_port`): ключ обфускации общий на весь
сервер, поэтому на relay-порт пишет не один роутер, а любой, кто знает ключ, и
два роутера с приставкой на одном порту потоки не делят. У каждого потока свой
таск с очередью на 64 пакета. Приём из туннеля идёт одним циклом и ничего не
ждёт: пакет кладётся под локом таблицы в очередь своего потока, цикл сразу
возвращается за следующим, а сокет на этот src_port поднимает уже таск, пока
приём разбирает пакеты остальных потоков. Порядок пакетов внутри потока держит
очередь (переполнение отбрасывает пакет, как отбросил бы его любой
промежуточный узел), а сокетом владеет ровно один таск, поэтому двух сокетов
на один поток не бывает. Тот же таск заворачивает ответы из интернета тому
пиру, который поток завёл. Адрес пира живёт в самом потоке, а не одним полем
на весь сервер, иначе входящий трафик уходил бы написавшему последним. Свой
слот из таблицы таск снимает сам: по неудачному bind сразу, по простою дольше
`flow_timeout` с перепроверкой очереди под тем же локом, чтобы пакет, попавший
в неё в последний момент, не пропал вместе с потоком. Размер таблицы держит
`max_flows` (по умолчанию 1024). Новый поток сверх потолка вытесняет наименее
свежий, слот уходит из таблицы вместе с отправным концом очереди, канал таска
закрывается, и таск выходит тем же путём, что и по простою, забирая сокет
с собой. Отдельного сборщика
протухших потоков нет, сокет умирает вместе со своим таском.

### 5.3 Control Plane HTTP

Реализовано в крейте `xr-hub`. API под префиксом `/api/v1`:

**Публичные эндпоинты:**
- `GET /api/v1/presets` — список `PresetSummary` (имя, версия, дата, кол-во правил). Поддержка `ETag`.
- `GET /api/v1/presets/:name` — полный `Preset` с правилами. `304 Not Modified` по `If-None-Match`.
- `GET /api/v1/presets/:name/wait?version=N&timeout_secs=S` это ожидание новой версии (LLD-37). Версия в хабе отличается от `N` (именно отличается: откат админом доезжает как изменение) это сразу `200` с полным пресетом и ETag, совпадает это висящий запрос до публикации либо `304` по истечении удержания (default 55 с, потолок 60). Без кэша клиент присылает `version=0` и получает пресет первым же запросом. Ждущих будит поколение пресетов (`watch`-канал `preset_gen` в `AppState`), которое двигают `POST/PUT/DELETE` админки; канал один на все пресеты, чужой подписчик сверяет свою версию и перевзводится.
- `GET /api/v1/invite/:token` это `InviteInfo` (метаданные без секретов), инвайт не потребляет. Читает `X-Claim-Id` и ставит `reclaimable`, когда потреблённый инвайт принадлежит спрашивающему (XR-216, см. ниже).
- `POST /api/v1/invite/:token/claim` это `InvitePayload` (полный конфиг подключения). Одноразовый инвайт потребляется здесь же, повтор получает `410 Gone`; исключение это повтор того же клиента по ключу `X-Claim-Id` (XR-216, см. ниже). Потребление, не легшее на диск, отвечает `500` и инвайт не тратит (XR-211, см. ниже).
- `GET /api/v1/invite/:token/view` - HTML-страница приглашения для получателя (QR, deep link на Android, кнопка APK). Голые пути `/invite/:token` и `/invite/:token/view` редиректят сюда.
- `GET /api/v1/public-key` отдаёт публичный ключ ed25519 для проверки подписей пресетов. Клиент, у которого ключ уже назван в конфиге или профиле, отбраковывает пресет без подписи и с чужой подписью (XR-207), поэтому хаб без секции `[signing]` такому клиенту пресет не доставит.
- `GET /api/v1/app/latest` — подписанный манифест последнего APK: `{manifest, signature}` с диска (LLD-12). `404` если релиз не выложен.
- `GET /api/v1/shares` это индекс всех шар флота: имя, `addr:port`, LAN-адреса
  и ключ агента. Индекс закрывает карту инфраструктуры от анонима (XR-193):
  запрос без аутентификации отвечает `401`, читает индекс предъявитель живого
  инвайта или гранта в `Authorization: Bearer` (грант это base64url-блоб
  `ShareToken`, проверяется подписью хаба со скоупом `share:read`). Инвайт
  живёт по правилу ручки `/api/v1/invite/:token/shares`: просроченный и
  отозванный отвечают `410`, мёртвый грант `403`. Любой живой грант открывает
  индекс целиком: это общий каталог, а не одна шара, и проверка идёт по
  подписи, не по таблице шар. Путь потребителя без гранта это
  `/api/v1/invite/:token/shares`.
**Живость и готовность (XR-230):**
- `GET /healthz` отвечает `ok` у живого процесса. Ручка живёт на верхнем
уровне, без аутентификации, и ничего не смотрит в состояние: аптайм-чек и
логика переключения на standby судят по ней, не дёргая содержательные ручки
(пресеты, инвайты).
- `GET /readyz` отражает hydrate: пока не загружены инвайты, шары и ключ
подписи, ответ `503 not ready`, после конца hydrate `200 ready`. Флаг
`ready` в `AppState` поднимает сам `state::hydrate`, слушатель хаб
поднимает после него, поэтому на практике неготовность наружу видна
закрытым портом, а ручка отличает «процесс жив» от «состояние поднято»
тем же ответом, что и мониторинг.

**Публикации локальных сервисов (мандат агента, LLD-38):**
- `POST /api/v1/expose/add` это заведение публикации под ключом предъявленного мандата агента, повтор своей же идемпотентен, занятое чужим агентом имя это `409`.
- `GET /api/v1/expose` и `DELETE /api/v1/expose/:name` это список и снятие своих публикаций, мандат агента едет в `Authorization: Bearer`.
- `POST /api/v1/expose/:name/mandate` это мандат публикации (`ExposeToken`) на свою публикацию: им проверяют путь харнессом `expose open`, а фронт берёт маршрут служебной ручкой под общим секретом.

**Служебные ручки браузерного входа (общий секрет `[web] shared_secret`, LLD-38):**
- `POST /api/v1/web/route` это маршрут публикации (`WebRoute`: агент, дескриптор relay, relay-токен с `share_id` вида `web:<имя>`, мандат публикации, `exp`, потолок жизни сплайса). Секрет едет в `Authorization: Bearer`, сравнивается постоянным временем; без блока `[web]` ручка отвечает `503`, на неизвестное имя `404`.
- `POST /api/v1/web/verify-password` это только вердикт `{"ok": true|false}` по учётке админки. Серия неверных упирается в растущую задержку на имя (`429` с временем ожидания), верный пароль счётчик снимает.
- `GET /api/v1/web/status` это публикации с полем `online` (`true` агент в реестре relay, `false` его там нет, `null` спросить не вышло, причина в `probe_error`) и полным именем `host` из `[web] domain`.

**Admin (Bearer-token):**
- Вход это `POST /api/v1/auth/login` по учёткам из `[[admin.users]]`. Сессия
  живёт `session_ttl_secs` (по умолчанию 12 часов), у оператора не больше
  `max_sessions_per_user` живых сессий (по умолчанию 5, новая вытесняет
  старейшую), а `POST /api/v1/auth/logout` гасит предъявленный Bearer
  (XR-194). Утёкший токен перестаёт работать с истечением TTL, logout'ом или
  вытеснением, а не с рестартом процесса, и карта сессий память не копит:
  протухшая сессия снимается при первой же проверке.
- Попытки входа ограничены на источник (XR-195): `login_max_attempts`
  попыток за `login_window_secs` (по умолчанию 10 в минуту). Исчерпавший
  лимит источник получает `429` без проверки пароля до конца окна, верный
  пароль счётчик снимает. Argon2-проверка пароля (m=19MiB) уезжает в
  `spawn_blocking`, поэтому шторм логинов не занимает воркеры рантайма, и
  содержательные ручки хаба продолжают отвечать. Одновременность проверок
  ограничена семафором на число ядер: распределённый шторм из свежих
  источников не раздувает блокирующий пул и пик памяти. Счётчики живут
  в памяти процесса, рестарт их обнуляет, протухшие окна выметаются из
  карты при её доросте до потолка.
- `POST/PUT/DELETE /api/v1/admin/presets` — CRUD пресетов, автоподпись при наличии ключа.
- `GET/POST/DELETE /api/v1/admin/invites` это управление инвайтами.
- `GET /api/v1/admin/exposes` и `DELETE /api/v1/admin/exposes/:name` это раздел «Публикации»: список всех публикаций хаба и снятие любой из них, в том числе когда машина агента не на связи.

Admin SPA встроена в бинарь через `rust-embed`, подробности в
[lld/01-control-plane.md](lld/01-control-plane.md).

**Каталог для вшивания выбирает build.rs (XR-238).** `admin-ui/dist` гитигнорен
и появляется только после `npm run build`, поэтому в свежем чекауте вшивать
было нечего и крейт не собирался вовсе, а с ним спотыкался весь
`cargo test --workspace`. Теперь `xr-hub/build.rs` кладёт путь в `XR_HUB_UI_DIR`,
откуда его берёт `#[folder]`: собран UI, вшивается он, а в отладочной сборке без
него в `OUT_DIR` пишется заглушка на одну страницу. Хаб с заглушкой не
притворяется рабочим: на старте он ругается в лог, а SPA на любой путь отвечает
`503` с командой сборки UI. Релизная сборка заглушки не допускает и по-прежнему
падает с той же командой в тексте.

**Страница приглашения (XR-192).** `/invite/:token/view` это единственный HTML,
который хаб собирает из данных, и открывает его посторонний человек по ссылке
из мессенджера, а в самой ссылке лежит одноразовый токен. Поэтому страница
замкнута на себя: QR рисуется инлайновым SVG прямо в ответе (крейт `qrcode`),
внешних картинок, шрифтов и скриптов на ней нет, и утекать токену наружу
нечем. Всё, что подставляется в разметку (комментарий инвайта, deep link из
`hub_url`, срок), проходит через экранирование, а ответ несёт
`Content-Security-Policy` с `default-src 'none'`; инлайновые стили пускает
одноразовый nonce, а не `unsafe-inline`.

**Защитные заголовки (XR-239).** Все ответы хаба несут три защитных заголовка:
`Content-Security-Policy`, `X-Content-Type-Options: nosniff` и
`Referrer-Policy: no-referrer`. Статика админки из fallback и ответы API
получают их наравне со страницами. Ставит заголовки один слой поверх всего
роутера (`SetResponseHeaderLayer` в `api::router`). Подключён он после
SPA-сервиса: слой, вставленный раньше, статику админки не накрывал бы.

Общий CSP пускает к админке только её собственный origin. Скрипты и стили
загружаются с него же, `data:` нужен QR-кодам, инлайновые атрибуты стилей
остаются у разметки. Встраивание в чужой iframe запрещено
(`frame-ancestors 'none'`). Заголовок вписывается только в пустое место.
Страница приглашения от этого сохраняет свой строгий CSP с nonce из XR-192,
второго общего рядом не появляется.

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

**Потребление переживает сбой записи (XR-211).** Файл инвайта в `data_dir` это
вся память хаба о потреблении: карта в оперативке поднимается с диска на старте.
Ошибка `save_invite` в claim глоталась, клиент получал payload и `200`, а
ближайший рестарт поднимал тот же инвайт активным, и одноразовая ссылка
срабатывала второй раз, уже у другого человека. Теперь пометка (`consumed_at`,
`claimed_by_ip`, `claim_id`) сначала ложится на диск и только потом в карту, как
это давно делает создание инвайта, а несохранённое потребление уходит в `500`:
получатель по той же ссылке придёт за payload'ом ещё раз.

Путь, на котором споткнулась запись, кладёт в ошибку сам `storage`: каждая
io-ошибка сохранения обёрнута `context` с именем каталога или файла, иначе
разбирать отказ на живой машине пришлось бы по голому `Permission denied`.
Наружу этот путь не уходит. Публичные ручки, которые пишут состояние (`claim`,
`share/register`, `share/add`, `share/unshare`, `share/attach`, `share/detach`,
`expose/add`, `expose/:name`),
переводят отказ записи в ответ общим помощником `api::persist_failed`: в тело
он кладёт неизменное `failed to persist state`, а полную ошибку с путём пишет в
лог оператора строкой `не сохранилось <что>`. Устройство каталогов хаба
предъявителю инвайта или мандата агента знать незачем. Админские ручки
(пресеты, шары, управление инвайтами) отдают ошибку как есть: там за ответом
сидит оператор, и путь ему по делу.

**Состояние и его резерв (XR-224).** Хаб держит состояние файлами на диске:
`config.toml` (хеш пароля админки, ключ обфускации и salt пула), ключ подписи
из `[signing]` и содержимое `data_dir` (`presets/`, `invites/`, `shares/`,
`expose/`).
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

Тем же ключом релиз судится и снаружи приложения: `xr-hub verify-release` берёт
ответ `/api/v1/app/latest` (или локальную пару файлов) и проверяет подпись
публичной половиной, а с `--key` выводит её из приватного ключа. На этом стоит
релизный скрипт `scripts/release-apk.sh` (XR-109), который гоняет весь путь от
сборки до проверенного `latest` на обоих хабах.

### 5.4 DNS через туннель (XR-285)

Провайдер домашней сети отвечает на UDP-запрос поддельным NXDOMAIN с флагом
`aa`, причём даже «от имени» публичных резолверов: тот же вопрос по TCP отдаёт
настоящие адреса. Роутерная часть прокси такой сайт не спасала. Порты 53 и 853
выведены из перехвата, решение о проксировании принимается по SNI уже открытого
TCP, а без адреса LAN-клиент TCP не откроет, так что перехватывать нечего:
заблокированный по DNS сайт лежал на всех устройствах сети при живом туннеле.
На Android этой болезни нет, там `FakeDns` отвечает приложению служебным
адресом и настоящего резолва в сети не происходит вовсе.

Резолв роутера теперь уходит внутрь туннеля:

```
dnsmasq -> 127.0.0.1:5353 (xr-client) -> mux-стрим до VPS -> TLS -> 9.9.9.9:853
```

Форвардер слушает петлю по UDP и по TCP (по TCP приходит переспрос за
усечённым ответом), а до апстрима идёт DNS-over-TLS поверх обычного
туннельного стрима: `MuxStream::into_io` даёт `AsyncRead + AsyncWrite`, поверх
которого встаёт rustls с корнями webpki. Провайдер видит одно соединение с
адресом нашего же VPS, и подменить в нём нечего; блокировать по адресу тоже
нечего, это тот самый VPS, ради которого всё и стоит.

Почему не два других пути. `https-dns-proxy` перед dnsmasq тянет пакет из
opkg, ставить который надо как раз в сети со сломанным резолвом, и ходит DoH
напрямую с роутера мимо туннеля: провайдеру остаётся заблокировать резолвер по
адресу. Перенос Fake-IP из `xr-core` не годится по устройству клиента: движок
Android живёт на TUN, где через него проходит весь трафик, а `xr-client`
работает по TPROXY, и рядом с ним есть `bypass_ips`, `bypass_rules` и UDP
relay, куда служебный адрес отдавать некуда.

Отказ виден снаружи и не откатывается на провайдерский резолвер. Апстрим
недоступен (лежит туннель, не поднялся TLS), и спрашивающий получает SERVFAIL
сразу, а в журнал ложится причина: первая жалоба немедленно, дальше не чаще
раза в минуту, иначе переспросы dnsmasq залили бы logread. Восстановление
отмечается отдельной строкой. Не поднявшийся листенер тоже уходит в журнал и в
`crash.log`, но прокси не роняет: TCP-перехват без DNS остаётся полезен.

Раскладку делает `xr-setup` шагом `dnsmasq:tunnel`: `noresolv=1` и ровно один
апстрим `127.0.0.1#5353`. Ровно один тут важен, соседний открытый адрес
dnsmasq спрашивает наравне, и часть запросов уходила бы из LAN голыми, а какая
именно, решал бы случай. Порт живёт одной константой `render::DNS_FORWARDER_PORT`
и попадает и в конфиг клиента, и в раскладку dnsmasq; тест профиля роутера
держит их в согласии, потому что разъехавшись они оставят LAN вообще без
резолва. Стенд с подложным резолвером живёт в юнитах `xr-client/src/dns.rs`:
рядом с форвардером поднимается резолвер, отвечающий NXDOMAIN с `aa`, и
проверяется сперва, что он и правда подделывает, а потом что форвардер всё
равно отдаёт настоящий адрес.

На перезагрузке роутера dnsmasq поднимается раньше `xr-proxy`, и всё окно до
подъёма форвардера LAN сидит без резолва вовсе: dnsmasq спрашивает петлю, где
пока никто не слушает. Окно короткое и кончается само, а порядок procd
нарочно не переставлен: клиенту нужна поднятая сеть, и `xr-client` ставит
nftables-перехват, который до сети не встанет. Отката на резолвер провайдера
на это окно нет и не будет, ради этого всё и затевалось.

Что осталось за границей задачи: LAN-устройство с прописанным вручную внешним
резолвером (или с DoH в браузере) спрашивает мимо dnsmasq и получает ту же
подделку. Перехват LAN-запросов на 53 в сторону dnsmasq это отдельная задача,
и заводить её надо с оглядкой: DoT такого устройства придётся дропать так же,
как дропается QUIC, иначе оно молча уйдёт мимо.

## 6. Конфигурация и правила маршрутизации

### 6.1 Состояние сейчас

Модель — **плоский список правил** (`RoutingConfig { default_action, rules }`):

```toml
[routing]
default_action = "direct"

[[routing.rules]]
name = "YouTube"
action = "proxy"
domains = ["youtube.com", "*.youtube.com", "*.googlevideo.com"]
ip_ranges = ["91.108.56.0/22", "2001:b28:f23d::/48"]
```

`name` это название тематической группы (XR-117). На маршрутизацию оно не
влияет, `Router` его не смотрит; смысл в том, что список из двадцати доменов
показывается человеку словом («Мессенджеры»), а не счётчиком. Поле
опциональное и без значения не сериализуется, поэтому пресеты, заведённые до
него, читаются по-прежнему и их подписи остаются действительными.

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
(XR-118). На живом туннеле поднятия не ждут вовсе (XR-180): сохранение списка
на экране правил зовёт `nativeApplyUserRules`, а тот идёт в
`VpnEngine::apply_user_rules`, который пересобирает merged-роутер из нового
списка и дискового кэша пресета и подменяет `Arc` в `SessionContext.router`.
Путь общий с публикацией пресета (`engine::swap_router`), локальная половина
у обоих одна на движок, поэтому пришедшая следом версия с хаба применённое
правило не откатывает. Сама пересборка сериализована: `swap_router` держит
лок локальной половины на весь цикл «записать новый список, собрать роутер,
подменить `Arc`», и новый список пишет тот же вызов. Разбейся это на
отдельные захваты, финальную запись в `SessionContext.router` решал бы не
порядок событий, а возраст снимка, который каждая из пересборок успела
прочитать, и публикация пресета молча перекрывала бы только что применённое
правило. Обещания «применятся при следующем подключении» на экране правил
больше нет. JNI-слой конвертирует массив в
`RoutingConfig` через `xr_proto::user_rule::to_routing_config`. Валидация
паттернов (домен / `*.wildcard` / CIDR) одна на всех:
`xr_proto::user_rule::classify_pattern`, из Kotlin она дёргается через
`nativeClassifyPattern`; кнопка «Обновить сейчас» на карточке пресета идёт в
`nativeRefreshPreset` и пишет в тот же дисковый кэш, что и движок.

### 6.2 Пресеты и override'ы

- Пресеты хранятся централизованно в `xr-hub` (файлы JSON на диске),
  версионируются, опционально подписываются ed25519. Подпись проверяет
  клиент. С заданным `trusted_public_key` пресет без подписи или с чужой
  подписью не применяется, прежний роутер остаётся жить (XR-207).
- Название группы правится в Admin UI полем рядом с действием и уезжает
  клиентам в том же JSON. На Android оно стоит заголовком карточки правила в
  просмотре пресета (счётчик доменов уходит второй строкой) и печатается
  строкой `name = "..."` в TOML-превью. Эталон сгруппированного пресета лежит
  в `configs/routing-russia.toml`.
- Клиент указывает пресет хаба и локальные override'ы с более высоким
  приоритетом: OpenWRT — `[hub] preset = "russia"` плюс `[[routing.rules]]`
  в TOML, Android — пресет из инвайта (`hubPreset` профиля) плюс глобальный
  `user_rules.json` (правила пользователя срабатывают первыми, первое
  совпадение выигрывает).
- При старте клиент делает `GET /api/v1/presets/:name` (forced fetch, timeout 2 с),
  кэширует результат локально. Дальше правила доставляет long-poll (LLD-37):
  фоновая задача висит на `GET /api/v1/presets/:name/wait` со своей версией, и
  публикация из админки будит её за секунды. Цикл общий у xr-client и движка
  Android (`xr_core::presets::watch_loop`), у обоих он свапает свой `Router`
  колбэком. Отказ хаба уводит в деградированный режим: пауза с backoff от 5
  секунд до `refresh_interval_secs`, на каждом пробуждении прежний опрос через
  `fetch_if_stale` и новая попытка встать на ожидание. Старый хаб без ручки
  ожидания отдаёт SPA-заглушку, разбор падает, и клиент оказывается в том же
  деградированном опросе: отдельной детекции версии хаба нет.
  Обновлённые правила применяются **hot-swap'ом**
  без рестарта и в `xr-client` (OpenWRT), и в `VpnEngine` (Android):
  активный `Router` хранится как `RwLock<Arc<Router>>` (в `ProxyState.router` /
  `SessionContext.router` соответственно), и на каждой новой версии колбэк
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
8. Соединение держится до явного отключения пользователем (XR-279).
   `XrVpnService.onStartCommand` на `ACTION_START` записывает в
   `WantedSessionRepository` (SharedPreferences `xr_proxy`) желание быть
   подключённым: флаг, конфиг сессии целиком и override «включить здесь».
   Гасят флаг два случая: явное отключение `stopFromUi` (кнопка и
   уведомление) и отвергнутый движком конфиг в окне восстановления
   (`ENGINE_START`), чтобы каждая загрузка не крутила пустые попытки.
   Системный отзыв `onRevoke` флаг оставляет (различение XR-221). Три
   события, раньше убивавшие
   туннель молча, проходят одним путём `restoreSession`: sticky-рестарт с
   `intent = null` (система убила процесс, свайп из недавних на OEM-ядрах),
   `BOOT_COMPLETED` и `MY_PACKAGE_REPLACED` через `BootCompletedReceiver`.
   Решение «восстанавливать или нет» считает `RestorePolicy`, чистая логика
   под JVM-юнитами. Без сохранённого желания сервис гасится, как раньше.
9. Восстановление поднимает туннель тем же `startVpn`, пауза на
   доверенной сети и override работают как при живом коннекте. Конфиг
   сохраняется целиком, пресет и правила движок всё равно перечитывает на
   старте (кеш на диске), восстановленная сессия выходит свежей. Пока сети
   ещё нет (перезагрузка) или сервер недоступен, движок умирает на warmup, и
   восстановление повторяет попытку с backoff 5с -> 60с, оставаясь
   foreground с фазой Connecting. Остановкой пользователя гасится и повтор.
   Стык с LLD-34: ретрай сегодня живёт только в окне восстановления, до
   первого подтверждённого Connected или паузы, смерть уже живого туннеля
   остаётся территорией LLD-34. Фоновые старты (восстановление, повтор
   ретрая) идут без типа location. Тип location из while-in-use, его
   получает только старт из UI с выданным разрешением, решение считает
   `RestorePolicy.foregroundLocationTypeAllowed`. Базовый тип у старта
   specialUse: это честный тип для стороннего VPN, документация разрешает
   его без ограничений отправителя, systemExempted же описан для системных
   UID, Device Owner и always-on VPN. Первые два круга XR-279 стояли на
   systemExempted и читали строки «does not have any types» из лога как
   срезание типов. По исходникам AOSP это шум наблюдательного модуля
   ForegroundServiceTypeLoggerModule, он есть и при успешном старте чистого
   specialUse. Старт сервиса после загрузки система разрешала всегда.
   Туннель пропадал по другой причине, establish() возвращал null. Слот
   подготовленного VPN-пакета живёт в системе до перезагрузки. Согласие
   пользователя это персистентный appop, ребут он переживает. Слот
   возвращается приложению лениво, очередным prepare(), и bringTunnelUp
   зовёт его перед establish(). Ненулевой intent означает отзыв согласия и
   идёт путём TUN_ESTABLISH без повтора. Фоновая сессия идёт одним
   specialUse, её SSID закрыт до первого открытия приложения. Авто-пауза
   доверенной сети в восстановленной сессии ждёт этого открытия, сам
   туннель от него не зависит.

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
  - **Мост `tracing` в ленту** ([journal_bridge.rs](../xr-core/src/journal_bridge.rs),
    XR-237). Движок и протокол пишут диагностику через `tracing`, а он уходит в
    stdout процесса, который Android выбрасывает: `warn!` об отказе разбора или
    деградации не видел ни пользователь, ни разбор инцидента. `JournalLayer` это
    слой подписчика, кладущий события в тот же журнал, и живёт он в `xr-core`, а
    не в JNI: повод android-специфичен, нужда общая, iOS-порт берёт слой готовым.
    В ленту идут `WARN` и `ERROR` от любого источника плюс `INFO` с явно
    перечисленных target (`with_info_targets`, по умолчанию список пуст: `info!`
    стоит на каждом выбранном пути соединения и утопит ленту). `DEBUG` и `TRACE`
    не проходят никогда. Строка выглядит как `xr_core::session: текст, поле=...`,
    тег `[source]` берётся по крейту (`core`, `proto`, остальное `trace`).
    Сверх фильтра стоит потолок 20 записей в секунду: цикл повторов с разным
    текстом журнальной свёрткой дубликатов не ужимается и вытеснил бы из хвоста
    всё остальное. Придержанное не пропадает молча, в следующем окне мост пишет
    их число. Что мост жив, видно по строке `мост диагностики движка включён` при
    установке и по счётчикам `journal_bridge::counters()`. Подписчика ставит
    `nativeJournalInit` (то есть до старта движка, вместе с журналом), слоем
    рядом с `fmt`; `journal_bridge::install` собирает подписчика целиком для
    платформы, которой `fmt` не нужен.
  - `relay_errors: AtomicU64` это счётчик ошибок relay-задач. На Android
    остался только как debug-метрика в статистике, бадж его не читает.
- **Серверные логи.** Нет централизованного сбора; пишется в stdout/stderr,
  procd/systemd забирает.
- **Crash log на OpenWRT.** Watchdog сохраняет `/etc/xr-proxy/crash.log`
  (последние 50 КБ, включает dmesg OOM, фрагмент logread, свободную память).
- **Сводка по флоту.** [fleet-status.py](../scripts/fleet-status.py) (XR-113)
  опрашивает все машины разом, по одному ssh на машину, и печатает, что за
  сборки на них стоят (md5 бинаря, размер, время файла), живы ли юниты и процесс
  роутера, стоят ли таблицы nftables, куда выходит роутер и какой релиз
  приложения отдают оба хаба. Версии из бинарей не берутся: `--version` есть
  только у `xr-setup`, поэтому личность сборки это md5, и работает он
  сравнением. Расхождение по одному имени бинаря между машинами, разный
  `version_code` у хабов, exit-IP не из ожидаемых и недоступная машина попадают
  в список проблем и роняют код возврата: сводка судится кодом, а не глазами.
  Роли (хаб, сервер, роутер) решают, что спрашивать; адреса живут в
  гитигнорнутом `local-docs/fleet.ini`, в git едут только роли.

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
| 12 | [12-android-apk-self-update.md](lld/12-android-apk-self-update.md) | Самообновление APK: xr-hub раздаёт APK + подписанный манифест версии (`/api/v1/app/latest`, `/api/v1/app/download/:ver`, файлы в `releases/`), приложение проверяет подпись **отдельным release-ключом** (pinned в сборке `BuildConfig.RELEASE_PUBLIC_KEY`, не серверный) + SHA-256, ставит через `PackageInstaller`. Компрометация VPS не даёт RCE. Проверка подписи живёт в `xr-core/update.rs`; CLI `xr-hub sign-release` / `gen-release-key` (офлайн-ключ) / `verify-release` (проверка выложенного манифеста, XR-109). | Шаг 2 (LLD-01) | Implemented |
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
| 33 | [33-share-git-sync.md](lld/33-share-git-sync.md) | Совместное редактирование текстовых шар (XR-178): git как движок синка. У агента репозиторий вне рабочей папки (`GIT_DIR` + `core.worktree`, материализация пуша receive-hook'ами: `updateInstead` с такой топологией не работает), авто-коммит по watcher'у, smart HTTP спавном системного git (`upload-pack`/`receive-pack --stateless-rpc`, опт-ин `share --git` только с `--writable`), подписанный HEAD identity-ключом с long-poll вместо частого polling; весь git-контур под `share:write`, включая fetch. Харнесс `xr-share sync` на libgit2 (gitoxide без push): авто-коммит, fetch/merge/push, пересечения строк оседают конфликт-копиями (модель SparkleShare), relay через rustls-мост с pinned identity-TLS. Только текст: колпак `git_max_file_mb`, крупное живёт манифест-контуром вне истории. Третья фаза: web-страница шары у агента (просмотр md, история/дифф/правка по write-токену из гранта, `weblink`) и экран истории в приложении. Хаб и минтинг не тронуты. | LLD-19, LLD-28, LLD-23, LLD-29; стык с LLD-32 и XR-129 | Фазы 1, 2, 3 Implemented (XR-188, XR-189, XR-190) |
| 37 | [37-preset-longpoll.md](lld/37-preset-longpoll.md) | Мгновенный подхват правил хаба (XR-174): long-poll `GET /presets/{name}/wait?version=N` (ответ сразу полным пресетом при несовпадении версии, 304 по таймауту 55 с; поколение пресетов на `watch`-канале в `AppState`), клиентский `PresetCache::wait_for_update` и общий цикл `watch_loop` в xr-core вместо дублей в xr-client и движке; при ошибке откат в существующий опрос с backoff, `refresh_interval_secs` становится интервалом деградированного режима. Новых конфиг-ручек нет, Kotlin не меняется. | Шаг 2 (LLD-01); стык с LLD-16, XR-180 | Implemented (XR-191) |
| 30 | [30-max-carrier.md](lld/30-max-carrier.md) | Max как транзитный носитель (болванка, crate `xr-max`, ядро плюс CLI): чужой мессенджер Max как недоверенная труба для шифрованных датаграмм на случай шатдауна, когда свои IP недоступны, а Max в белом списке. Трейт `Carrier` общий с XR-061/XR-064, framing поверх, крипта на Noise (XR-061). Честная рамка: канал не анонимный (SIM/юрлицо) и палевный по паттерну, годен как редкий bootstrap под шатдаун, не как повседневный прокси. Не путать с LLD-21 (там свой мессенджер, тут чужой носитель). Реверс клиента гейтнут результатами bot-стадии. Далёкая research-ставка. | XR-061, XR-064, XR-058 | Draft |
| 26 | [26-share-access-mode.md](lld/26-share-access-mode.md) | Режим доступа к шаре (XR-129): поле `AccessMode { auto, direct, relay }` у `ShareRecord` и `ShareGrant`, где `auto` это дефолт-прощуп XR-128, а `direct`/`relay` ручные оверрайды (достижимость это свойство четвёрки потребитель-агент-путь-момент, фиксация типа не единственный механизм). Кеш auto-вердикта per-agent с коротким TTL в `xr-core` (серия операций не перепрощупывает, инвалидация по TTL и по факту неудачи). Неавторизованная `/health` у агента (версия плюс статус relay-аплинка), прощуп уходит с `/manifest` на неё. Умный дефолт режима хабом при регистрации как подсказка, дефолт владельца (`--mode`, `setmode`) плюс локальный оверрайд консьюмера в приложении, эффективный режим схлопывается у границы `xr-core`. | LLD-23, XR-128; стык с LLD-33 | Draft |
| 34 | [34-android-reconnect.md](lld/34-android-reconnect.md) | Живучее переподключение Android при провале авто-резюма (XR-132): девятое значение `Phase`/`ConnectPhase` `Reconnecting` вместо тихой смерти сервиса и снятия уведомления. Держит foreground-сервис и постоянное уведомление; вечный авто-ретрай по приходу/смене сети и screen-on с backoff 5с -> 5мин (потолок), в мёртвой зоне (`noNetwork`) таймер не крутится. Флаг намерения `stayUp` (нативный Connected либо авто-подъём) разводит авто-резюм и ручной коннект: ручной провал остаётся Error -> Idle плюс снекбар. Ретрай только поверх мёртвого движка, пуловый failover LLD-10 внутри живой сессии не трогается; вход в доверенную сеть уводит в Paused. Чистое расписание backoff в `xr-core/reconnect.rs` плюс мост `nativeReconnectDelayMs` под юнит-тесты, оркестрация в Kotlin device-verify. Часть закрыта XR-279 (сохранённое желание подключения через `WantedSessionRepository`, возврат туннеля после смерти процесса, перезагрузки и самообновления APK, ретрай восстановления с backoff). Осталось про сами живые сессии: смерть уже поднятого туннеля, провал авто-резюма и ручного коннекта, отдельная фаза вместо Connecting. | LLD-02, LLD-15, LLD-10; стык XR-095/XR-183/XR-049 | Draft |
| 35 | [35-protocol-v2.md](lld/35-protocol-v2.md) | Протокол v2, зонтик кластера (XR-060): замена статического XOR Noise-транспортом. Паттерн IK с профильным PSK (`Noise_IKpsk_25519_ChaChaPoly_BLAKE2s`, крейт `snow`), ChaCha20-Poly1305 как оптимум на ARM без AES-NI, forward secrecy и anti-replay. Прежние кадр/команда/mux едут плейнтекстом внутри AEAD-записей (магия 0xA0 убрана), версия транспорта раздаётся out-of-band подписанным инвайтом/пресетом, фичи через существующий `MuxCaps`. Статический ключ на VPS, клиентский ключ пока профильный (задел под per-client XR-030/073). Миграция двойным приёмом на отдельном v2-порту (не trial-decode), сервер первым, роутеры последними, откат конфигом; компат-слой короткий (парк тестовый). UDP relay на датаграммный AEAD с явным nonce и окном повтора. Задаёт очередь листьев и швы под XR-062/057/064/066/067. | XR-060; гейтит XR-061, далее XR-063/062/057/064/066/067; стык LLD-27, LLD-10, LLD-23, LLD-30 | Draft |
| 36 | [36-release-federation.md](lld/36-release-federation.md) | Федеративная дистрибуция софта (XR-173), первая фаза федерации хабов: хабы делят софт и цепочку доверия, данные пока локальные. Два корня доверия: релиз-ключ (офлайн у владельца, тот же, что у APK LLD-12) подписывает код всех компонентов, ключ хаба подписывает данные. Единый стор `soft-dist` с подписанным индексом (`SHA256SUMS.sig`), публичная половина релиз-ключа зашита в сборку компайл-тайм (хаб не подменит). Релиз-CI по компонентам (musl `xr-server`/`xr-relay`/`xr-hub`/`xr-client`/`xr-setup` по образцу `release-xr-share.yml`, у клиента матрица арок роутеров), приватный ключ офлайн, вне CI. Pull-апдейтер `xr-setup update` (сверка подписи плюс SHA-256, atomic swap, рестарт), хаб зеркалит набор от родителя с проверкой подписи пиннутым ключом, пиринг попарный без транзитивного доверия. XR-111/112 переформулируются в обёртки оператора над `xr-setup update`, из закрытой XR-110 переезжают обновление всех роутеров одной командой и сверка exit-IP. | LLD-12, LLD-13, LLD-19, LLD-23; XR-109 смежно; стык с XR-074/XR-061 | Draft |
| 38 | [38-browser-entry.md](lld/38-browser-entry.md) | Браузерный вход к живому HTTP-сервису агента (XR-252): новый сервис `xr-web` на VPS терминирует TLS браузера, пускает владельца по паролю хаба с host-only cookie на публикацию, адресует машину поддоменом и ходит к агенту обычным потребителем relay (обфусц. mux плюс pinned-TLS), поэтому код `xr-relay` не меняется и его слепота остаётся проверяемой. У агента блок `[[expose]]` и прокси на локальный upstream под мандатом публикации `ExposeToken` (минтит хаб, проверяет агент офлайн): держатель relay-токена на шару в локальный сервис не попадает. Хаб источник правды по публикациям, служебные ручки под общим секретом без прав админки. Честная рамка модели доверия: на браузерном пути плейнтекст живёт в памяти `xr-web`, клиентский путь остаётся E2E. Четыре фазы: экспорт у агента (XR-262), фронт и вход (XR-263), WebSocket и живучесть (XR-264), страница шары наружу после XR-190 (XR-265). Потребитель первой очереди это дашборд агентской разработки devkit (цель DK-112). | LLD-23, LLD-19; стык с LLD-33 (XR-190) и LLD-13 | Draft |
| 39 | [39-ios-core-spike.md](lld/39-ios-core-spike.md) | Спайк iOS (XR-272, заключительная задача цели XR-278): `xr-core` собирается под `aarch64-apple-ios` без Rust-несовместимостей (крипта на `ring`, не `aws-lc-rs`; `jni` не зависимость ядра), единственный барьер это iOS SDK для asm `ring` и линковки, то есть машина с Xcode. Мост вместо JNI это C FFI через `cbindgen` в отдельном крейте `xr-ios-ffi` (staticlib, параллельно `xr-android-jni`), UniFFI отброшен (граница уже строковая, горячий пакетный путь без копий, чужая async-модель). `protect()` на iOS это no-op через существующую DI `ProtectSocketFn`. Ключевой вывод по памяти Network Extension (лимит 15 МБ до iOS 15, 50 МБ с iOS 15, jetsam): дефолтная раскладка движка (128 КБ `smoltcp`-буферов на соединение, окно mux 1 MiB, без клиентского капа сессий) под нагрузкой не влезает, влезание это iOS-профиль тюнинга существующих рычагов без форка движка. Спайк-документ, реализация порта отдельными задачами. | XR-278, XR-271, XR-092, XR-237; смежно XR-085 | Draft (спайк) |

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
