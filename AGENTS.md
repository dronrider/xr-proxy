# CLAUDE.md

Общие правила работы (окружение, git, ревью, стиль кода и текстов, трекинг задач) едины для всех проектов, живут в репозитории devkit и подключаются импортом:


Если импорты не развернулись (соседней директории devkit нет), склонировать `https://github.com/dronrider/devkit` рядом с проектом и прочитать оба файла оттуда явно.

Ниже только специфика xr-proxy.

## Project Overview

xr-proxy - lightweight obfuscated proxy for bypassing regional internet blocks. Deployed on OpenWRT routers (`xr-client`) connected to a VPS (`xr-server`). All LAN devices get transparent access to blocked resources without per-device configuration. There is also an Android client (`xr-android`) that uses the same tunnel via the shared `xr-core` engine (via JNI in `xr-android-jni`).

Language: Rust (core / server / OpenWRT client) + Kotlin (Android). All communication in this project is in Russian.

**Полная архитектура описана в [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).** Читай этот файл, когда нужна карта компонентов, модулей, протоколов, модели конфигурации или жизненного цикла соединения. Не импортируй его сюда автоматически - он большой. Планы крупных доработок лежат в [docs/lld/](docs/lld/); после реализации факты из LLD переносятся в `ARCHITECTURE.md`.

Остальные разделы этого файла это рабочие правила (build, кросс-компиляция, дизайн-решения по nftables/TPROXY, деплой), которые нужны под рукой в каждом сеансе.

## Build & Test

**IMPORTANT**: Before every commit, run `cargo test --workspace` AND verify zero warnings with `cargo test --workspace 2>&1 | grep "warning:" | grep -v "generated"`. Do NOT commit code with warnings.

Kotlin-код приложения автотестами не покрывается: бизнес-логика живёт в Rust и проверяется там, а экранные сценарии гоняются на эмуляторе. Исключение одно, и это чистая Kotlin-логика без Android SDK: она покрывается JVM-юнитами в `xr-android/app/src/test/`, гоняются они `cd xr-android && ./gradlew :app:testDebugUnitTest`. Сейчас там раскладка списка проводника по группам (XR-258).

Свежий чекаут собирается и тестируется без npm: `admin-ui/dist` гитигнорен, и в отладочной сборке `xr-hub/build.rs` вшивает вместо него заглушку (XR-238), а хаб с ней отвечает `503` на весь admin UI. Релизный бинарь по-прежнему требует `cd xr-hub/admin-ui && npm ci && npm run build`.

```bash
# Run all tests
cargo test --workspace

# Build server (on VPS)
cargo build --release -p xr-server

# Cross-compile client for OpenWRT (requires Docker running)
cross build --release --target aarch64-unknown-linux-musl -p xr-client

# Client with GeoIP support
cross build --release --target aarch64-unknown-linux-musl -p xr-client --features geoip
```

## Cross-Compilation Notes (musl libc)

When targeting `*-unknown-linux-musl`, the `libc` crate does NOT export certain constants. These must be defined manually:
- `SOL_IP` = 0
- `IP_TRANSPARENT` = 19
- `IP_RECVORIGDSTADDR` / `IP_ORIGDSTADDR` = 20
- `SO_ORIGINAL_DST` = 80

`libc::msghdr` on musl has private padding fields (`__pad1`, `__pad2`) - cannot use struct literal syntax. Must use `std::mem::zeroed()` + field-by-field assignment.

Integer types differ across targets (`msg_controllen`, `iov_len`). Use `as _` for portable casting.

## Key Design Decisions

- **nftables `ip` family, not `inet`** - `inet` family conflicts with TPROXY + `ip saddr` in the same rule. Always use `ip` family for TPROXY rules.
- **Older nftables (OpenWRT)** - require explicit `add table`/`add chain`/`add rule` syntax; block syntax (`table { chain { ... } }`) only works for updating existing tables.
- **`meta l4proto udp`** must appear on the same rule as the `tproxy` statement, not on a separate line above.
- **TPROXY source filtering in nftables, not application code** - if the proxy is down, intercepted traffic is blackholed. Filter by source IP in firewall rules so only specific devices (e.g., game consoles) are affected.
- **Response spoofing (UDP relay)** - Switch expects UDP responses from the original server IP, not the router. The client creates per-destination sockets with `IP_TRANSPARENT` + `bind(server_ip:port)` to send spoofed-source responses.
- **UDP relay клиента ведёт свой NAT по туннельному порту** (XR-201) - номер выдаётся на устройство целиком, на пару `(адрес, порт)` в LAN, и уходит в туннель вместо настоящего. Обратно из туннеля приходят только `src_port` и `dst`, адреса устройства `RelayPacket` не несёт, поэтому две приставки одной модели на фиксированном порту (Xbox 3074) развести больше нечем: с общим ключом ответы обеим уходили последнему написавшему. Настоящий номер достаётся первому владельцу, конфликтующим идёт первый свободный из пула 40000-65000, протухший флоу возвращает номер в пул. **Адресата в ключ не брать**: маппинг обязан оставаться endpoint-independent (один порт устройства на всех пиров), иначе NAT на VPS становится symmetric, а от типа NAT у Switch и Xbox зависит мультиплеер, ради которого relay и заведён. Спуфящие сокеты по той же причине живут не по таблице флоу, а по своему времени последнего использования.
- **UDP relay сервера: таск с очередью на каждый поток, приём ничего не ждёт** (XR-200) - пакет из туннеля кладётся под локом таблицы в очередь своего потока, и цикл приёма сразу идёт за следующим, а bind нового сокета выполняет таск потока. Иначе первый пакет нового src_port останавливает relay целиком на время bind. Сокетом владеет ровно один таск, он же снимает слот из таблицы (по неудачному bind сразу, по простою с перепроверкой очереди под тем же локом), поэтому второго сокета на тот же поток не заводится и осиротеть ему не с чего.
- **Поток UDP relay это пара (пир, `src_port`), ответ уходит владельцу потока** (XR-208) - ключ обфускации на VPS общий, на relay-порт пишет любой, кто его знает. Пока адрес роутера жил одним полем на весь сервер, его перетирал каждый расшифровавшийся пакет, и входящий UDP уходил написавшему последним: два роутера на одном VPS получали чужой трафик, а устройство со знанием ключа одним пакетом переводило на себя ответы чужих потоков. Адрес пира теперь живёт в самом потоке, а пир входит в ключ таблицы, поэтому приставки двух роутеров на одном порту 3074 не делят поток. Keepalive по-прежнему отвечает написавшему.
- **Потолок UDP-флоу-таблиц это вытеснение наименее свежего** (XR-204) - между 30-секундными чистками таблицы клиента и сервера росли без границы, устройство, пишущее по многим адресам, нагоняло карты и порты. Теперь у обеих сторон жёсткий лимит `max_flows` (по умолчанию 1024), и флоу сверх лимита вытесняет запись с самым старым временем последней активности. Отказ новому оставил бы потолок в руках одного пишущего до конца flow_timeout и закрыл бы relay остальным устройствам LAN, поэтому выбрано вытеснение. Свежесть записи на клиенте двигает каждый пакет в обе стороны, очередь вытеснения до живых флоу не доходит. Свежесть потока на сервере двигает только запись в туннель, входящие из интернета её не трогают. Таск потока не берёт лок таблицы на каждый ответ, иначе сломался бы беслоковый цикл приёма XR-200. Поток, давно не писавший в туннель, но активно принимающий, вытесняется раньше своего простою. На клиенте туннельный порт жертвы возвращается в пул. Спуфящие сокеты не трогаются, они живут своим временем последнего использования. На сервере снятие слота роняет канал таска потока, таск выходит и забирает сокет с собой. Ключи NAT прежние, у клиента endpoint-independent, у сервера пара (пир, `src_port`).
- **Скрипт настройки TPROXY судят по состоянию ядра, а не по его выводу** (XR-247) - `udp-tproxy-setup.sh` печатал прогресс голым `echo`, и стоило вызвавшему закрыть свой конец трубы (init под procd, `ssh роутер '... start' | grep -q`), как сигнал PIPE убивал его на первой же строке, до установки правил: перехват UDP приставок молча выключался до ручного прогона. Теперь PIPE игнорируется, печать на ход установки не влияет, а код возврата считается по тому, что реально стоит в ядре (таблица `xr_udp_relay`, правило `fwmark 0x200`, `local default` в таблице маршрутов 201). Init забирает вывод скрипта подстановкой, поэтому трубу держит до конца, и кладёт причину отказа в logread: раньше предупреждение было и лживым, и безымянным. Обвязка роутера гоняется тестами `xr-setup` на стенде из заглушек nft и ip.
- **QUIC блокируется самим клиентом (`block_quic`, default true)** - TPROXY перехватывает только TCP; без drop UDP/443 из LAN любой сайт с `alpn="h3"` в DNS уходит по QUIC напрямую мимо прокси (так geo-blocked сайты «не работали» при честно проксируемом TCP). Chain `quic_block` ставится в таблицу `xr_proxy` вместе с redirect-правилами.
- **Из перехвата выводится не весь адрес VPS, а два правила на него** - рабочее это `ip daddr <VPS> tcp dport != { 80, 443 } return` (ssh и служебные порты сервера в прокси не нужны, там первым говорит сервер и соединение висит до таймаута), оно же выпускает и туннельный порт при обычном его значении. Второе, `ip daddr <VPS> tcp dport <порт туннеля> return`, стреляет только когда туннель сидит на 80 или 443: там без него телефон с персональным клиентом в той же LAN получил бы туннель в туннеле. Под перехват остаются 80 и 443: сайты, которые живут на том же VPS, уходили из LAN голыми и провайдерский фильтр видел их SNI. Туннель на web-порту делает правку холостой для этого VPS, клиент ругается о таком в лог на старте.
- **Машинные исключения перехвата живут в конфиге, а не в init-скрипте** (XR-248) - на живых роутерах исключения под конкретную LAN (per-port bypass для NAS, hairpin на шару по своему же WAN-адресу) годами стояли дописанными в `/etc/init.d/xr-proxy`, и любая раскладка обвязки стёрла бы их молча: init переписывается целиком, а `config.toml` раскладка не трогает по умолчанию. Теперь такие исключения задаёт `client.bypass_rules`, строка это готовое условие nftables без вердикта. Вердикт дописывает потребитель, и потребителей двое: перехват ставит `return` первым правилом цепочки, kill-switch тем же условием ставит `accept`. Обе половины обязаны идти от одного списка, иначе выпущенное из прокси устройство режется на forward и остаётся вообще без выхода. Условие с чужим вердиктом, подстановкой или разделителем команд отбраковывается, а непринятый `nft` набор ставится второй попыткой без машинных условий: опечатка в конфиге не должна лишать LAN перехвата целиком. Критерий отбраковки живёт в `xr_proto::config::bypass_rule_reject_reason` и повторяется функцией `bypass_rule_reject_reason` в `killswitch-setup.sh`: разъедься эти два списка, и условие встанет с `return` в перехвате без `accept` в kill-switch, то есть даст ровно тот блэкхол, ради которого всё и затевалось. Паритет двух половин закреплён тестом стенда, который гоняет один набор злых условий через обе. `xr-setup` переносит `bypass_ips` и `bypass_rules` в перегенерированный под `--force` конфиг, как переносит выданную хабом секцию `[control]`.
- **Конфиг проверяется сухим прогоном до старта (`validate`, XR-227)** - `xr-server validate -c ...` и `xr-client validate -c ...` прогоняют парсинг, ключи, salt и адреса без листенеров и файрвола. Годный конфиг отвечает `ok` и нулём, битый уходит в stderr с названной причиной и кодом 1. Обычный старт зовёт ту же проверку, поэтому validate и старт не расходятся в том, что считать годным. Обвязка ходит субкомандой. Init роутера проверяет конфиг до киллсвитча, и отказ с причиной попадает в logread вместо бесконечного respawn. Systemd-юнит сервера держит её в `ExecStartPre`, и причина видна в `systemctl status`. `diagnose.sh` печатает шаг `xr-client validate`. До киллсвитча проверка идёт нарочно. Сервис, который не поднялся, не должен оставлять LAN с правилами, которые некому снять. Salt вне u32 ловится проверкой, хотя рантайм молча обрезает его до u32. Клиент и сервер обрезают одинаково, связи это не рвёт, но заданное в конфиге число тогда врёт.
- **Паника ядра на Android ловится границей JNI (XR-220)** - все входные точки `xr-android-jni` объявляет макрос `jni_entry!` из `guard.rs`: `catch_unwind`, запись в журнал (`ERROR [jni]`) и запасной ответ по конвенции сигнатуры (`{"error"}` у JSON-функций, `Error:` у `nativeGetState`, null, `false`). Профиль `android-release` в корневом Cargo.toml наследует release и ставит `panic = "unwind"`; его используют `xr-android/build.sh` и gradle-таска `buildRustRelease`. С общим release-профилем (`panic = "abort"`) паника ядра убивала весь процесс с живым VpnService, а без unwind `catch_unwind` не ловит панику. Локи ENGINE и HEALTH берутся через `lock_surviving_poison` (паника под локом не закрывает движок навсегда). Новая входная точка в lib.rs возможна только через макрос: голую extern-функцию ловит тест покрытия в guard.rs.
- **Tokio AsyncFd for TPROXY socket** - DO NOT use `UdpSocket::from_std()` + `AsyncFd::new()` on the same fd. It causes `EEXIST` (double reactor registration). Use `AsyncFd` exclusively with raw `recvmsg`/`sendto`.
- **procd respawn** - `respawn 3600 15 0` (threshold=3600s, interval=15s, retry=0=unlimited)
- **Timeouts everywhere** - idle 5min, max lifetime 1h, TCP keepalive 60s. Prevents zombie connection memory leaks.
- **SO_REUSEADDR** on TCP listener - prevents "address already in use" on rapid restart.

## File Locations on Router

```
/usr/bin/xr-client              - binary
/usr/bin/xr-watchdog.sh         - cron watchdog (restart + crash log)
/usr/bin/udp-tproxy-setup.sh    - nftables TPROXY setup (reads config)
/etc/xr-proxy/config.toml       - configuration
/etc/xr-proxy/crash.log         - persistent crash diagnostics
/etc/init.d/xr-proxy            - procd init script
```

## Config Files

- `configs/client.toml` - reference client config with all options documented
- `configs/server.toml` - reference server config
- `configs/routing-russia.toml` - comprehensive routing rules for Russia (domains + CIDR for Telegram)

## Scripts

- `deploy/xr-proxy.init` - procd init: start (TCP + UDP TPROXY setup), stop (cleanup both), respawn
- `deploy/xr-watchdog.sh` - cron every minute: check process, log crash, cleanup rules, restart, set OOM protection
- `deploy/xr-service-alert.sh` - сторож crash-loop сервисов VPS (XR-226): каждую минуту по cron смотрит факты systemd (`NRestarts` + `ActiveState`) у xr-proxy-server, xr-relay и xr-hub, цикл рестартов (порог 10) шлёт алертом в Telegram, восстановление отмечает одним сообщением, повтор не чаще раза в час. Судит по systemd, а не по самочувствию сервисов. Токен и чат в `/etc/xr-proxy/alert.env`, общем с cert-alert (нет файла: молча нулём, лишь строчка в stderr cron). Ставит `xr-setup` шагом `server:service-alert` в `/usr/local/bin` с `/etc/cron.d/xr-service-alert`, тесты стенда в `xr-setup/src/render.rs` (`service_alert_tests`)
- `scripts/udp-tproxy-setup.sh` - reads source_ips from config, creates nftables TPROXY rules (ip family). Refuses to run with empty source_ips (safety).
- `scripts/udp-tproxy-cleanup.sh` - removes TPROXY rules and policy routes
- `scripts/diagnose.sh` - comprehensive diagnostics (binary, config, process, ports, firewall, connectivity)
- `scripts/generate-key.sh` - generate base64 obfuscation key
- `scripts/release-apk.sh` - релиз Android APK одной командой (XR-109): сборка с нужными `ORG_GRADLE_PROJECT_*`, подпись манифеста офлайн-ключом, заливка на оба хаба через staging с бэкапом прежнего манифеста, проверка `latest` на обоих (первичный по HTTPS, второй изнутри машины с Host-хедером). Адреса и путь к ключу в гитигнорнутом `local-docs/release.env`, запуск без него печатает шаблон. Ручную цепочку из `docs/HUB-DEPLOY.md` повторять не надо: `--bump` берёт следующий versionCode у живого релиза, `--only-upload` дозаливает после обрыва, `--dry-run` печатает шаги. Подпись судится командой `xr-hub verify-release` (она же гоняется в тестах стенда `xr-hub/tests/release_apk_script.rs`)
- `scripts/check-browser-entry.py` - проверка браузерного входа `xr-web` (XR-264): подкоманда `echo-service` поднимает на машине владельца синтетический сервис (страница по GET, эхо по WebSocket), `ws` держит через вход живой WebSocket, гоняет кадры и дожидается штатного закрытия по лимиту жизни сплайса (`browser entry ws: ok`), `offline` судит выключенную машину (502 с названной причиной за миллисекунды плюс `online: false` в `GET /api/v1/web/status` на хабе). Только стандартная библиотека python3, гоняется и с VPS, и с машины владельца
- `scripts/fleet-status.py` - сводка по флоту одной командой (XR-113): по одному ssh на машину, все разом, и в таблицу ложатся md5 с временем файла у бинарей, состояние юнитов, процесс и таблицы nftables роутера, его exit-IP и релиз приложения у обоих хабов. Судится кодом возврата: расхождение сборки по машинам, разный `version_code` у хабов, exit-IP не из ожидаемых, мёртвый юнит и недоступная машина уходят в список проблем и дают код 1, недоступная при этом видна строкой с причиной от ssh. Адреса флота в гитигнорнутом `local-docs/fleet.ini` (путь переопределяется `FLEET_CONF`), запуск без него печатает шаблон. Есть `--only` и `--json`. Только стандартная библиотека python3, тесты стенда гоняются `python3 scripts/fleet_status_test.py`
- `scripts/check-git-share.sh` - проверка git-контура шары живьём (XR-188, LLD-33 п. 6 фаза 1). Поднимает локальный хаб и агента из свежей сборки в temp-каталоге, клон, пуш с материализацией и fetch авто-коммитов идут штатным git. Первый пуш в пустовавшую при `--git` шару тоже входит в прогон. Сверяет лестницу гейтов, подпись HEAD с `agent_pubkey` гранта, long-poll и колпак `git_max_file_mb`. Судится кодом возврата, последняя строка успеха `git-контур шары: ок`. Нужен git в PATH и OpenSSL 3 (на macOS берёт homebrew, системный LibreSSL не умеет ed25519)
- `taskctl` - утилита канбан-доски `docs/TASKS.md` (`add`/`move`/`close`/`sort`/`lint`/`id`), живёт в общем репозитории devkit рядом с проектом, бинарь ставится `cd ../devkit/taskctl && go build -o ~/go/bin/taskctl .`. Операции с доской (завести строку, статус, закрытие в архив с переносом файла задачи) делать ей, а не ручной правкой markdown; подробности в `../devkit/taskctl/README.md`

## Known Issues / Watch Out For

- `Connection reset by peer` in tunnel logs can mean VPS overloaded or semaphore full (max_connections=256)
- BusyBox crond logs all cron executions as `cron.err` - this is normal, not an actual error
- UDP relay `source_ips` MUST be specified - empty list intercepts ALL LAN UDP and breaks games/VoIP
- `bypass_ips` in client config excludes devices from TCP proxy only (nftables prerouting return)
- init script `stop_service` must clean both `ip xr_proxy` (TCP) and `ip xr_udp_relay` (UDP) tables + policy route
