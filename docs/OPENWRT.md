# Развёртывание XR Proxy Client на OpenWRT

Пошаговая инструкция: от чистого роутера до работающего прокси.

---

## 1. Подготовка: определите свой роутер

Подключитесь к роутеру по SSH:

```bash
ssh root@192.168.1.1
# (пароль — тот, что задали при настройке OpenWRT)
```

Выясните архитектуру:

```bash
uname -m
```

Таблица соответствия:

| `uname -m` | Rust target                          | Типичные устройства                    |
| ---------- | ------------------------------------ | -------------------------------------- |
| `mipsel`   | `mipsel-unknown-linux-musl`          | GL-MT300N-V2, MT7621-based             |
| `mips`     | `mips-unknown-linux-musl`            | GL-AR750S, Atheros/QCA-based           |
| `aarch64`  | `aarch64-unknown-linux-musl`         | GL-MT3000 (Beryl AX), Raspberry Pi 3/4 |
| `armv7l`   | `armv7-unknown-linux-musleabihf`     | GL-A1300, некоторые Linksys            |

Проверьте ресурсы:

```bash
# Свободная память
free -m

# Свободное место на flash
df -h /
```

Минимум: 32 МБ RAM, ~2 МБ свободного flash (без GeoIP).

---

## 2. Сборка клиента (на вашем компьютере)

### Установка инструментов

```bash
# Rust (если ещё нет)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# cross — для кросс-компиляции (требует Docker)
cargo install cross --git https://github.com/cross-rs/cross
```

> ⚠️ `cross` использует **Docker** для кросс-компиляции. Docker должен быть
> установлен и запущен. Проверка: `docker info` — если ошибка, запустите Docker.
>
> Установка Docker: [docs.docker.com/get-docker](https://docs.docker.com/get-docker/)

### Сборка

Замените `<TARGET>` на значение из таблицы выше:

```bash
cd xr-proxy

# Убедиться, что Docker запущен
docker info >/dev/null 2>&1 || echo "Docker не запущен! Запустите Docker и повторите."

# Стандартная сборка (без GeoIP, рекомендуется)
cross build --release --target <TARGET> -p xr-client

# Пример для aarch64:
cross build --release --target aarch64-unknown-linux-musl -p xr-client
```

> **Внимание для mips (big-endian):** если `cross` не работает,
> добавьте musl target вручную:
>
> ```bash
> rustup target add mips-unknown-linux-musl
> cargo build --release --target mips-unknown-linux-musl -p xr-client
> ```
>
> Может потребоваться установить линкер: `apt install gcc-mips-linux-gnu`

### Проверка размера

```bash
ls -lh target/<TARGET>/release/xr-client
# Ожидаемо: 1.5-2.0 МБ
```

---

## 3. Загрузка на роутер

```bash
# Замените <TARGET> и IP роутера на свои значения

# Создать директории на роутере
ssh root@192.168.1.1 "mkdir -p /etc/xr-proxy"

# Загрузить бинарь
scp -O target/<TARGET>/release/xr-client root@192.168.1.1:/usr/bin/xr-client

# Загрузить конфиг
scp -O configs/client.toml root@192.168.1.1:/etc/xr-proxy/config.toml

# Загрузить init-скрипт и watchdog
scp -O deploy/xr-proxy.init root@192.168.1.1:/etc/init.d/xr-proxy
scp -O deploy/xr-watchdog.sh root@192.168.1.1:/usr/bin/xr-watchdog.sh

# Сделать исполняемыми
ssh root@192.168.1.1 "chmod +x /usr/bin/xr-client /etc/init.d/xr-proxy /usr/bin/xr-watchdog.sh"
```

---

## 4. Настройка конфига

```bash
ssh root@192.168.1.1
vi /etc/xr-proxy/config.toml
```

Минимально нужно заменить 3 поля:

```toml
[server]
address = "203.0.113.10"              # ← IP вашего VPS
port = 8443

[obfuscation]
key = "ваш-ключ-из-generate-key.sh"  # ← ваш ключ (одинаковый на клиенте и сервере!)
modifier = "positional_xor_rotate"
salt = 0xDEADBEEF                     # ← должен совпадать с сервером

[routing]
default_action = "direct"

[[routing.rules]]
action = "proxy"
domains = [                           # ← перечислите нужные вам домены
  "youtube.com", "*.youtube.com", "*.googlevideo.com",
  "*.google.com", "*.gmail.com",
  "telegram.org", "*.telegram.org", "*.t.me",
]

[client]
listen_port = 1080
auto_redirect = true
on_server_down = "direct"
log_level = "info"
```

---

## 5. Запуск

```bash
# Тестовый запуск (смотрим, нет ли ошибок)
/usr/bin/xr-client -c /etc/xr-proxy/config.toml -l debug

# Если всё OK (ctrl+C для остановки), включить как сервис:
/etc/init.d/xr-proxy enable    # автозапуск при включении роутера
/etc/init.d/xr-proxy start     # запустить сейчас
```

---

## 6. Проверка

### На роутере

```bash
# Убедиться, что процесс запущен
ps | grep xr-client

# Проверить, что nftables правила установлены
nft list ruleset | grep xr_proxy
# Или для iptables:
iptables -t nat -L | grep XR_PROXY

# Смотреть логи
logread -f | grep xr
```

### С устройства (телефон/ноутбук)

Подключитесь к WiFi роутера и откройте один из проксируемых доменов.

---

## 7. Управление

```bash
# Запуск / остановка / перезапуск
/etc/init.d/xr-proxy start
/etc/init.d/xr-proxy stop
/etc/init.d/xr-proxy restart

# Отключить автозапуск
/etc/init.d/xr-proxy disable

# Полное удаление
/etc/init.d/xr-proxy stop
/etc/init.d/xr-proxy disable
rm /usr/bin/xr-client
rm /usr/bin/xr-watchdog.sh
rm -rf /etc/xr-proxy
rm /etc/init.d/xr-proxy
```

---

## 8. Обновление

При выходе новой версии:

```bash
# На своём компьютере (Docker должен быть запущен):
cd xr-proxy
git pull
cross build --release --target <TARGET> -p xr-client

# Загрузить на роутер:
scp -O target/<TARGET>/release/xr-client root@192.168.1.1:/usr/bin/xr-client

# Перезапустить:
ssh root@192.168.1.1 "/etc/init.d/xr-proxy restart"
```

---

## Устранение неполадок

### «Нет доступа к серверу»

```bash
# С роутера — проверить доступность сервера:
# (установите пакет netcat если нет)
opkg update && opkg install netcat
nc -zv YOUR_SERVER_IP 8443

# Если timeout — порт закрыт на VPS (файрвол, security group)
# Если connection refused — xr-server не запущен
```

### «nftables: command not found»

```bash
# На старых OpenWRT (до 21.02) используется iptables.
# xr-client автоматически определяет бэкенд.
# Проверьте, что хотя бы один установлен:
which nft iptables
```

### «Всё через прокси медленно»

Проблема: `default_action = "proxy"` гонит весь трафик через VPS.
Решение: переключиться на `"direct"` и добавить только нужные домены в правила.

### «Клиент падает при старте с OOM»

Это не должно случаться (~2 МБ RAM), но если случилось:

```bash
# Проверить свободную память
free -m
# Если <10 МБ свободно — роутер перегружен другими сервисами
# Попробуйте отключить ненужные пакеты
```

### Ручная очистка nftables/iptables

Если клиент завершился аварийно и правила не убрались:

```bash
# nftables
nft delete table ip xr_proxy

# iptables
iptables -t nat -D PREROUTING -j XR_PROXY
iptables -t nat -F XR_PROXY
iptables -t nat -X XR_PROXY
```

### cross: ошибка «Docker not running»

```bash
# Проверить Docker
docker info

# Если не установлен — установите: https://docs.docker.com/get-docker/
# Если установлен, но не запущен:
sudo systemctl start docker        # Linux
# Или откройте Docker Desktop      # macOS / Windows

# Если ваш пользователь не в группе docker:
sudo usermod -aG docker $USER
# Перелогиньтесь после этого
```

---

## Структура файлов на роутере

```text
/usr/bin/xr-client              # бинарь (~1.5 МБ)
/usr/bin/xr-watchdog.sh         # watchdog (автоочистка правил если клиент упал)
/etc/xr-proxy/config.toml       # конфигурация
/etc/init.d/xr-proxy            # init-скрипт (procd)
```

При сборке с GeoIP (`--features geoip`) добавляется:

```text
/etc/xr-proxy/GeoLite2-Country.mmdb  # GeoIP-база (~5 МБ)
```

---

## Безопасность развёртывания

Три уровня защиты от потери доступа к роутеру:

| Уровень                    | Механизм                                                                      | Что делает                                                                   |
| -------------------------- | --------------------------------------------------------------------------    | ---------------------------------------------------------------------------- |
| **SSH всегда работает**    | Redirect перехватывает только порты 80/443, приватные подсети исключены       | Вы всегда можете зайти на роутер по SSH, даже если всё сломалось             |
| **Cleanup при остановке**  | init-скрипт принудительно удаляет правила nftables/iptables при `stop`        | Достаточно выполнить `/etc/init.d/xr-proxy stop`                             |
| **Watchdog (cron)**        | Каждую минуту проверяет, жив ли процесс xr-client. Если нет — удаляет правила | Интернет восстанавливается автоматически в течение 1 минуты                  |

**Если всё совсем плохо** — SSH на роутер и выполните:

```bash
# Удалить правила вручную
nft delete table ip xr_proxy 2>/dev/null
iptables -t nat -D PREROUTING -j XR_PROXY 2>/dev/null
iptables -t nat -F XR_PROXY 2>/dev/null
iptables -t nat -X XR_PROXY 2>/dev/null

# Полностью отключить xr-proxy
/etc/init.d/xr-proxy stop
/etc/init.d/xr-proxy disable
```
