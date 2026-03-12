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
Игровая приставка ─┤          │
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
- **UDP relay** — проксирование UDP-трафика для игровых консолей (например Nintendo Switch P2P мультиплеер)
- **Обфускация** — уникальный протокол с XOR + позиционные модификаторы + случайный padding, не детектируется DPI
- **Маршрутизация** — гибкие правила: домены, wildcard (`*.google.com`), CIDR (`91.108.56.0/22`), GeoIP
- **Защита от сбоев** — автоматический перезапуск через procd + watchdog, crash-логи, fallback на direct при недоступности сервера
- **Лёгкий** — 4-8 МБ RAM, 1-3% CPU на роутере (vs 100+ МБ у v2ray/xray)

## Структура проекта

```text
xr-proxy/
├── xr-proto/          # Общая библиотека: протокол, обфускация, конфиги
├── xr-client/         # Клиент для OpenWRT роутера
├── xr-server/         # Сервер для VPS
├── configs/           # Примеры конфигурации
│   ├── client.toml
│   ├── server.toml
│   └── routing-russia.toml
├── deploy/            # Init-скрипты, systemd-юниты, watchdog
├── scripts/           # Утилиты: генерация ключа, диагностика, TPROXY
└── docs/              # Документация по развёртыванию
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
source_ips = []                  # Все LAN-устройства
exclude_dst_ports = [53, 67, 68]
```

**Роутер** — настроить nftables TPROXY:

```bash
scp -O scripts/udp-tproxy-setup.sh root@192.168.1.1:/usr/bin/
ssh root@192.168.1.1 "chmod +x /usr/bin/udp-tproxy-setup.sh && udp-tproxy-setup.sh"
```

Скрипт без аргументов настраивает relay для всех LAN-устройств. Для конкретного IP: `udp-tproxy-setup.sh 192.168.1.187`.

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
