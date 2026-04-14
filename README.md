# XR Proxy

Лёгкий прокси с обфускацией трафика для обхода региональных блокировок.
Устанавливается на портативный роутер OpenWRT — все устройства в сети
автоматически получают доступ к заблокированным ресурсам без какой-либо
настройки на самих устройствах.

## Как это работает

```text
                           TCP (HTTP/HTTPS)
Телефон/Ноутбук ──┬──→ [OpenWRT роутер] ──→ Интернет (разрешённые сайты)
                   │       xr-client
Nintendo Switch ───┤          │
                   │          ▼ обфусцированный туннель
Любое устройство ──┘    [VPS xr-server]
                              │
                              ▼
                           Интернет (заблокированные сайты)
```

Трафик к заблокированным ресурсам автоматически проксируется через VPS.
Протокол обфусцирован — фаервол не может определить, что это прокси.

### Возможности

- **TCP прокси** — прозрачное проксирование HTTP/HTTPS по доменам, IP-диапазонам (CIDR) и GeoIP
- **UDP relay** — проксирование UDP-трафика для игровых консолей (Nintendo Switch P2P мультиплеер)
- **Обфускация** — уникальный протокол с XOR + позиционные модификаторы + случайный padding, не детектируется DPI
- **Маршрутизация** — гибкие правила: домены, wildcard (`*.google.com`), CIDR (`91.108.56.0/22`), GeoIP
- **Централизованные пресеты** — общие правила маршрутизации хранятся на VPS (`xr-hub`), роутеры подтягивают обновления автоматически без рестарта
- **Защита от сбоев** — автоматический перезапуск через procd + watchdog, crash-логи, fallback на direct при недоступности сервера
- **Лёгкий** — 4-8 МБ RAM, 1-3% CPU на роутере (vs 100+ МБ у v2ray/xray)

## Структура проекта

```text
xr-proxy/
├── xr-proto/          # Общая библиотека: протокол, обфускация, конфиги, роутер
├── xr-core/           # Движок VPN: TUN/smoltcp, сессии, DNS, кэш пресетов
├── xr-client/         # Клиент для OpenWRT роутера (transparent proxy)
├── xr-server/         # Сервер для VPS
├── xr-hub/            # Централизованный контрол-плейн: пресеты + Admin UI
├── xr-android/        # Android-клиент (Kotlin + Compose)
├── xr-android-jni/    # JNI-обвязка xr-core для Android
├── configs/           # Примеры конфигурации
│   ├── client.toml
│   ├── server.toml
│   └── routing-russia.toml
├── deploy/            # Init-скрипты, systemd-юниты, watchdog
├── scripts/           # Утилиты: генерация ключа, диагностика, TPROXY
└── docs/              # Документация: ARCHITECTURE.md, OPENWRT.md, LLD-планы
```

## Требования

**VPS (сервер):** любой Linux с публичным IP в стране без блокировок. Минимум: 1 vCPU, 64 МБ RAM.

**Роутер (клиент):** OpenWRT 21.02+, 32 МБ RAM, 8 МБ flash. Архитектуры: aarch64, arm, mips, mipsel.

**Сборка:** Linux или macOS с [Rust](https://rustup.rs/) 1.70+ и [cross](https://github.com/cross-rs/cross) + Docker.

## Быстрый старт

### 1. Сгенерируйте ключ

```bash
git clone https://github.com/dronrider/xr-proxy.git
cd xr-proxy
./scripts/generate-key.sh
```

### 2. Сервер (на VPS)

```bash
# Установить Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Собрать и установить
cargo build --release -p xr-server
sudo mkdir -p /etc/xr-proxy
sudo cp target/release/xr-server /usr/local/bin/
sudo cp configs/server.toml /etc/xr-proxy/server.toml
sudo cp deploy/xr-proxy-server.service /etc/systemd/system/
```

Отредактируйте `/etc/xr-proxy/server.toml` — вставьте ключ из шага 1. Запустите:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now xr-proxy-server
```

Откройте порт 8443/tcp (и 9999/udp если нужен UDP relay) в файрволе VPS.

### 3. Клиент (на роутере)

```bash
# На вашем компьютере — кросс-компиляция (Docker должен быть запущен!)
cross build --release --target aarch64-unknown-linux-musl -p xr-client

# Загрузить на роутер
scp -O target/aarch64-unknown-linux-musl/release/xr-client root@192.168.1.1:/usr/bin/
scp -O configs/client.toml root@192.168.1.1:/etc/xr-proxy/config.toml
scp -O deploy/xr-proxy.init root@192.168.1.1:/etc/init.d/xr-proxy
scp -O deploy/xr-watchdog.sh root@192.168.1.1:/usr/bin/xr-watchdog.sh
```

На роутере: отредактируйте `/etc/xr-proxy/config.toml` (IP сервера, ключ, домены). Запустите:

```bash
chmod +x /usr/bin/xr-client /etc/init.d/xr-proxy /usr/bin/xr-watchdog.sh
/etc/init.d/xr-proxy enable
/etc/init.d/xr-proxy start
```

### 4. Проверьте

Подключите устройство к WiFi роутера, откройте заблокированный сайт.

```bash
# Логи на роутере
logread -f | grep xr

# Диагностика
scp -O scripts/diagnose.sh root@192.168.1.1:/tmp/ && ssh root@192.168.1.1 sh /tmp/diagnose.sh
```

## Маршрутизация

Правила проверяются по порядку. Первое совпавшее — применяется. Если ничего не совпало — `default_action`.

### По доменам (SNI)

```toml
[routing]
default_action = "direct"

[[routing.rules]]
action = "proxy"
domains = [
  "youtube.com", "*.youtube.com", "*.googlevideo.com",
  "telegram.org", "*.telegram.org", "*.t.me",
]
```

### По IP-диапазонам (CIDR)

Для сервисов, которые подключаются напрямую по IP без SNI (например Telegram):

```toml
[[routing.rules]]
action = "proxy"
domains = ["telegram.org", "*.telegram.org"]
ip_ranges = [
  "91.108.56.0/22",
  "91.108.4.0/22",
  "149.154.160.0/20",
  "2001:b28:f23d::/48",
]
```

Домены и IP-диапазоны в одном правиле работают через ИЛИ.

### По GeoIP

Требует сборки с `--features geoip` и базы [MaxMind GeoLite2-Country](https://dev.maxmind.com/geoip/geolite2-free-geolocation-data):

```toml
[[routing.rules]]
action = "proxy"
geoip = ["US", "NL", "DE"]

[geoip]
database = "/etc/xr-proxy/GeoLite2-Country.mmdb"
```

### Готовый конфиг для России

В `configs/routing-russia.toml` — правила для YouTube, Meta, Twitter/X, Telegram (включая IP-диапазоны), LinkedIn, Discord, AI-сервисов, GitHub и других ресурсов.

## Централизованные пресеты (xr-hub)

Когда роутеров больше одного, держать списки доменов в синхроне через SSH
становится больно. `xr-hub` — отдельный HTTPS-сервис на VPS, хранит
именованные пресеты (`russia`, `home-lab`, …) и отдаёт их клиентам при
старте и раз в несколько минут в фоне.

Кейс: «обновил YouTube-список в хабе → через 5 минут все 10 роутеров
работают по новым правилам, никто никуда не ходил».

### Архитектура

```text
       [Admin SPA / TOML-редактор]
                 │ HTTPS
                 ▼
           [xr-hub] ──── /api/v1/presets/:name
                 ▲             │
  GET            │             │ JSON + ed25519 signature
  раз в 5 мин    │             ▼
   ┌─────────────┴──────────────┐
   │           │                │
 [роутер 1] [роутер 2]  … [роутер N]
```

- **Источник правды** — файлы JSON на диске VPS в `xr-hub`, версионируются.
- **Подпись ed25519** (опционально): хаб подписывает пресеты, клиент
  проверяет — поддельный ответ от MITM отклоняется.
- **Локальные override'ы** на роутере (`[[routing.rules]]` в `config.toml`)
  имеют приоритет над пресетом. Пресет — база, overrides — тонкая
  настройка под конкретный роутер.

### Как это выглядит для оператора

**На VPS — один раз** установить `xr-hub` и настроить HTTPS. Готовый
скрипт раскатки — `scripts/deploy-hub.sh` (собирает Admin UI, бинарь,
systemd-юнит).

Зайти в Admin UI (`https://hub.example.com/admin`), создать пресет
`russia`, вставить TOML с правилами, сохранить. Хаб выдаёт номер версии.

**На каждом роутере — один раз** дописать в `/etc/xr-proxy/config.toml`:

```toml
[hub]
url = "https://hub.example.com"
preset = "russia"
trusted_public_key = "BASE64_ED25519_PUBKEY"  # опционально
refresh_interval_secs = 300                    # проверка раз в 5 мин
```

Перезапустить: `/etc/init.d/xr-proxy restart`. В логах появится:

```
INFO fetched preset 'russia' v3 from hub
INFO preset 'russia' loaded, merging with local overrides
```

### Как происходит обновление

1. Оператор редактирует пресет в Admin UI → хаб увеличивает версию.
2. Каждый роутер раз в `refresh_interval_secs` (5 мин по умолчанию)
   делает `GET /api/v1/presets` — сравнивает номер версии. Если
   не изменился — ничего не происходит (HTTP 304 по `If-None-Match`).
3. Если версия выросла — роутер скачивает полный пресет, проверяет
   подпись, сохраняет в `/var/lib/xr-proxy/presets/<name>.json`.
4. **Hot-swap**: клиент пересобирает активный `Router` и заменяет его
   в памяти без рестарта. Живые TCP-соединения продолжают работать со
   старым выбором (direct/proxy), новые подключения уже видят
   обновлённые правила. Рестарт службы не требуется.

То есть с момента сохранения в Admin UI до момента, когда последний
роутер начинает работать по новым правилам, проходит максимум
`refresh_interval_secs` (по умолчанию 5 минут).

### Когда хаб недоступен

Хаб — не single-point-of-failure. Клиент работает с последним
закэшированным пресетом (`/var/lib/xr-proxy/presets/<name>.json`) плюс
локальными override'ами. В логах WARN вида «preset 'russia' unavailable»,
трафик идёт по старым правилам без деградации.

Подробности: [LLD-01](docs/lld/01-control-plane.md),
[ARCHITECTURE.md §5.3 и §6.2](docs/ARCHITECTURE.md).

## Исключение устройств из прокси

Если прокси мешает работе какого-то устройства, его можно исключить на уровне файрвола — трафик с этого IP вообще не будет заворачиваться в прокси:

```toml
[client]
bypass_ips = ["192.168.1.50"]
```

Рекомендуется закрепить статический IP за устройством через DHCP.

## UDP Relay (игровые консоли)

Nintendo Switch и другие консоли используют P2P UDP для онлайн-мультиплеера. Если провайдер блокирует входящие UDP-пакеты, онлайн-игры не работают. UDP relay решает эту проблему — весь UDP-трафик проходит через VPS.

### Настройка

**Сервер** (`server.toml`):

```toml
[udp_relay]
enabled = true
listen_port = 9999
```

**Клиент** (`config.toml`):

```toml
[udp_relay]
enabled = true
listen_port = 1081
vps_port = 9999
source_ips = ["192.168.1.188"]   # IP устройств для relay (обязательно!)
exclude_dst_ports = [53, 67, 68]
```

**Роутер** — настроить nftables TPROXY:

```bash
scp -O scripts/udp-tproxy-setup.sh root@192.168.1.1:/usr/bin/
ssh root@192.168.1.1 "chmod +x /usr/bin/udp-tproxy-setup.sh && udp-tproxy-setup.sh"
```

Скрипт автоматически читает `source_ips` из конфига и перехватывает UDP только от этих устройств. Остальной трафик (игры, VoIP, видеозвонки) не затрагивается.

Откройте порт 9999/udp на файрволе VPS.

### Как это работает

```text
Switch ──UDP──→ [роутер: TPROXY → xr-client] ══обфусцированный UDP══→ [VPS: xr-server]
                                                                            │
                                                                     bind(src_port)
                                                                            │
                                                                     UDP → Другой игрок
```

VPS сохраняет source port Switch при отправке в интернет — критично для NAT traversal.

## Обфускация

Каждый параметр делает трафик уникальным и неузнаваемым для DPI:

| Параметр | Описание |
|---|---|
| `key` | Общий секретный ключ (base64). Без него данные нерасшифруемы |
| `modifier` | Алгоритм: `positional_xor_rotate`, `rotating_salt`, `substitution_table` |
| `salt` | 32-bit число, меняет выходные данные при том же ключе |
| `padding_min/max` | Случайный padding в каждом пакете, маскирует размеры |

Все параметры должны совпадать на клиенте и сервере.

## Надёжность

**procd** — init-скрипт с `respawn 3600 15 0` (бесконечные перезапуски с паузой 15 сек).

**Watchdog** — cron каждую минуту проверяет процесс. Если мёртв: записывает диагностику в `/etc/xr-proxy/crash.log`, чистит firewall-правила, перезапускает. Также ставит OOM-защиту (`oom_score_adj=-900`).

**Crash log** — персистентный (`/etc/xr-proxy/crash.log`), переживает ребут. Содержит: dmesg (OOM-killer), последние логи, состояние памяти.

**Защита от петель** — nftables input chain блокирует доступ к порту прокси с WAN + детекция петель в коде.

**Таймауты** — idle 5 мин, max lifetime 1 час, TCP keepalive 60 сек на всех сокетах. Зомби-соединения невозможны.

**`SO_REUSEADDR`** — быстрый рестарт без "address already in use".

## Устранение неполадок

```bash
# Диагностика (на роутере)
sh /tmp/diagnose.sh

# Логи
logread -f | grep xr

# Причины крашей
cat /etc/xr-proxy/crash.log

# Ручной запуск с debug-логами
/usr/bin/xr-client -c /etc/xr-proxy/config.toml -l debug

# Проверить firewall-правила
nft list table ip xr_proxy

# Ручная очистка правил
nft delete table ip xr_proxy
```

## Сборка

```bash
# Сервер (на VPS)
cargo build --release -p xr-server

# Клиент — кросс-компиляция (требует Docker!)
cross build --release --target aarch64-unknown-linux-musl -p xr-client
cross build --release --target mipsel-unknown-linux-musl -p xr-client
cross build --release --target armv7-unknown-linux-musleabihf -p xr-client

# Клиент с GeoIP
cross build --release --target aarch64-unknown-linux-musl -p xr-client --features geoip

# Тесты
cargo test --workspace
```

## Файлы на роутере

```text
/usr/bin/xr-client              # Бинарь (~1.5 МБ)
/usr/bin/xr-watchdog.sh         # Watchdog (перезапуск + crash-лог)
/etc/xr-proxy/config.toml       # Конфигурация
/etc/xr-proxy/crash.log         # Лог падений (создаётся автоматически)
/etc/init.d/xr-proxy            # Init-скрипт (procd)
```

## Безопасность

- **Ключ** — храните в секрете. Любой, кто знает ключ, может расшифровать трафик.
- **Это НЕ полноценное шифрование** — протокол обфусцирует трафик, чтобы он не распознавался DPI. Для конфиденциальности используйте HTTPS.
- **Сервер доступен из интернета** — используйте файрвол, ограничьте SSH, обновляйте систему.

## Документация

- [Развёртывание на OpenWRT](docs/OPENWRT.md) — подробная пошаговая инструкция
- [Правила маршрутизации для России](configs/routing-russia.toml) — готовый конфиг

## Лицензия

MIT
