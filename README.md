# XR Proxy

Лёгкий прокси с обфускацией трафика для обхода региональных блокировок.
Устанавливается на портативный роутер OpenWRT — все устройства в сети
автоматически получают доступ к заблокированным ресурсам без какой-либо
настройки на самих устройствах.

## Как это работает

txt```
Телефон/Ноутбук → [OpenWRT роутер с xr-client] → Интернет
                         │                          (напрямую для разрешённых)
                         ▼
                   [VPS с xr-server]
                         │
                         ▼
                      Интернет
                   (для заблокированных)

```

Трафик к заблокированным ресурсам автоматически проксируется через ваш VPS.
Протокол обфусцирован — фаервол не может определить, что это прокси.

## Требования

**VPS (сервер):**

- Любой Linux VPS с публичным IP (Ubuntu, Debian, CentOS и т.д.)
- В стране без блокировок (Нидерланды, Германия, США и т.д.)
- Минимум: 1 vCPU, 64 МБ RAM

**Роутер (клиент):**

- OpenWRT 21.02 или новее
- Минимум: 32 МБ RAM, 8 МБ flash
- Архитектура: mipsel, mips, arm, aarch64

**Компьютер для сборки:**

- Linux или macOS
- Установленный [Rust](https://rustup.rs/) (1.70+)
- Для кросс-компиляции: [cross](https://github.com/cross-rs/cross) + Docker

---

## Быстрый старт

### Шаг 1. Клонируйте репозиторий

```bash
git clone https://github.com/dronrider/xr-proxy.git
cd xr-proxy
```

### Шаг 2. Сгенерируйте ключ

```bash
chmod +x scripts/generate-key.sh
./scripts/generate-key.sh
```

Запишите сгенерированный ключ — он понадобится и для сервера, и для клиента.

### Шаг 3. Настройте и запустите сервер (на VPS)

#### Сборка на VPS

```bash
# Установить Rust (если ещё нет)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Собрать сервер
cd xr-proxy
cargo build --release -p xr-server
```

#### Настройка

```bash
# Создать директорию конфига
sudo mkdir -p /etc/xr-proxy

# Скопировать и отредактировать конфиг
sudo cp configs/server.toml /etc/xr-proxy/server.toml
sudo nano /etc/xr-proxy/server.toml
```

В файле `server.toml` замените:

- `key = "..."` → ваш ключ из шага 2
- `port = 8443` → при необходимости измените порт
- `salt = 0xDEADBEEF` → при желании замените на своё значение

#### Установка как сервис

```bash
# Скопировать бинарь и сервис
sudo cp target/release/xr-server /usr/local/bin/
sudo cp deploy/xr-proxy-server.service /etc/systemd/system/

# Запустить
sudo systemctl daemon-reload
sudo systemctl enable xr-proxy-server
sudo systemctl start xr-proxy-server

# Проверить статус
sudo systemctl status xr-proxy-server
```

#### Открыть порт в файрволе VPS

```bash
# UFW (Ubuntu)
sudo ufw allow 8443/tcp

# Или firewalld (CentOS)
sudo firewall-cmd --permanent --add-port=8443/tcp
sudo firewall-cmd --reload
```

> ⚠️ Также откройте порт в панели управления VPS (AWS Security Group, DigitalOcean Firewall и т.д.)

### Шаг 4. Настройте и запустите клиент (на роутере)

#### Определите архитектуру роутера

```bash
# На роутере через SSH:
uname -m
# или
opkg print-architecture
```

Типичные значения:

| Модель роутера | Архитектура | Target в Rust |
| ---------------- | ------------- | --------------- |
| GL.iNet GL-MT300N-V2 | mipsel | `mipsel-unknown-linux-musl` |
| GL.iNet GL-AR750S | mips | `mips-unknown-linux-musl` |
| GL.iNet Beryl (MT1300) | aarch64 | `aarch64-unknown-linux-musl` |
| Raspberry Pi | arm/aarch64 | `armv7-unknown-linux-musleabihf` / `aarch64-unknown-linux-musl` |

#### Сборка (на вашем компьютере)

```bash
# Установить cross для кросс-компиляции
cargo install cross --git https://github.com/cross-rs/cross

# Собрать для вашей архитектуры (пример: aarch64)
cross build --release --target aarch64-unknown-linux-musl -p xr-client

# Бинарь будет здесь:
ls -lh target/aarch64-unknown-linux-musl/release/xr-client
```

Для минимального размера (опционально):

```bash
# Установить strip для целевой архитектуры и сжать
strip target/aarch64-unknown-linux-musl/release/xr-client
# UPX может дополнительно уменьшить размер (необязательно):
# upx --best target/aarch64-unknown-linux-musl/release/xr-client
```

#### Загрузите на роутер

```bash
# С вашего компьютера
scp target/aarch64-unknown-linux-musl/release/xr-client root@192.168.1.1:/usr/bin/
scp configs/client.toml root@192.168.1.1:/etc/xr-proxy/config.toml
scp deploy/xr-proxy.init root@192.168.1.1:/etc/init.d/xr-proxy
```

#### Настройте конфиг на роутере

```bash
ssh root@192.168.1.1

# Создать директорию (если не существует)
mkdir -p /etc/xr-proxy

# Отредактировать конфиг
vi /etc/xr-proxy/config.toml
```

В файле `config.toml` замените:

- `address = "YOUR_SERVER_IP"` → IP-адрес вашего VPS
- `key = "..."` → тот же ключ из шага 2
- `port`, `salt`, `modifier` → должны совпадать с сервером
- `domains = [...]` → список доменов, которые нужно проксировать

#### Запустите

```bash
# Сделать файлы исполняемыми
chmod +x /usr/bin/xr-client
chmod +x /etc/init.d/xr-proxy

# Запустить и включить автозапуск
/etc/init.d/xr-proxy enable
/etc/init.d/xr-proxy start
```

### Шаг 5. Проверьте работу

Подключите телефон или ноутбук к WiFi роутера и откройте один из заблокированных сайтов.

Для диагностики:

```bash
# На роутере — смотреть логи в реальном времени
logread -f | grep xr

# На сервере — смотреть подключения
sudo journalctl -u xr-proxy-server -f
```

---

## Примеры конфигурации маршрутизации

### Минимальный — проксировать всё

```toml
[routing]
default_action = "proxy"
```

### Стандартный — проксировать только нужное

```toml
[routing]
default_action = "direct"

[[routing.rules]]
action = "proxy"
domains = [
  "youtube.com", "*.youtube.com", "*.googlevideo.com",
  "*.google.com", "*.gmail.com",
  "telegram.org", "*.telegram.org", "*.t.me",
]
```

### С GeoIP — проксировать по стране

Требует сборки с `--features geoip` и скачивания базы GeoLite2:

```bash
# Сборка с GeoIP
cross build --release --target aarch64-unknown-linux-musl -p xr-client --features geoip

# Скачать базу (нужна бесплатная регистрация на maxmind.com)
# Скопировать GeoLite2-Country.mmdb на роутер в /etc/xr-proxy/
```

```toml
[routing]
default_action = "direct"

[[routing.rules]]
action = "proxy"
geoip = ["US", "NL", "DE"]

[geoip]
database = "/etc/xr-proxy/GeoLite2-Country.mmdb"
```

---

## Настройка обфускации

Каждый параметр обфускации делает ваш трафик уникальным:

| Параметр | Описание | Влияние |
| ---------- | ---------- | --------- |
| `key` | Общий секретный ключ | Без правильного ключа данные невозможно расшифровать |
| `modifier` | Алгоритм модификации | Меняет паттерн обфускации. Варианты: `positional_xor_rotate`, `rotating_salt`, `substitution_table` |
| `salt` | Дополнительный параметр | Любое 32-bit число, меняет выходные данные при том же ключе |
| `padding_min/max` | Случайный мусор в каждом пакете | Маскирует реальные размеры пакетов от статистического анализа |

**Все параметры должны совпадать на клиенте и сервере!**

---

## Устранение неполадок

### Клиент не запускается

```bash
# Проверить, что бинарь запускается
/usr/bin/xr-client -c /etc/xr-proxy/config.toml -l debug
```

### Сайты не открываются через прокси

1. Проверьте, что сервер запущен: `curl http://YOUR_SERVER_IP:8443` (должен ответить HTML-заглушкой)
2. Проверьте, что порт открыт: `nc -zv YOUR_SERVER_IP 8443`
3. Проверьте, что ключи совпадают в обоих конфигах
4. Посмотрите логи: `logread -f` на роутере, `journalctl -u xr-proxy-server -f` на сервере

### nftables/iptables ошибки

```bash
# Проверить, что redirect правила созданы:
nft list ruleset   # для nftables
iptables -t nat -L  # для iptables

# Ручная очистка (если клиент завершился аварийно):
nft delete table ip xr_proxy        # nftables
iptables -t nat -F XR_PROXY         # iptables
iptables -t nat -D PREROUTING -j XR_PROXY
iptables -t nat -X XR_PROXY
```

### Высокое потребление памяти

- Убедитесь, что используете release-сборку (`--release`)
- Без GeoIP потребление: ~2 МБ, с GeoIP: ~5 МБ
- Если всё равно много — уменьшите `max_connections` в серверном конфиге

---

## Безопасность

- **Ключ** — храните в секрете. Любой, кто знает ключ, может расшифровать трафик.
- **Это НЕ полноценное шифрование** — протокол обфусцирует трафик, чтобы он не распознавался DPI. Для конфиденциальности используйте HTTPS (который и так используется большинством сайтов).
- **Сервер доступен из интернета** — используйте файрвол, ограничьте доступ к SSH, обновляйте систему.

---

## Сборка из исходников

```bash
# Только сервер (для VPS)
cargo build --release -p xr-server

# Только клиент (нативная сборка, для тестов)
cargo build --release -p xr-client

# Клиент с GeoIP
cargo build --release -p xr-client --features geoip

# Кросс-компиляция клиента
cross build --release --target mipsel-unknown-linux-musl -p xr-client
cross build --release --target aarch64-unknown-linux-musl -p xr-client
cross build --release --target armv7-unknown-linux-musleabihf -p xr-client

# Запуск тестов
cargo test --workspace
```

## Лицензия

MIT
