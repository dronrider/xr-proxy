# Деплой xr-hub на VPS

Все действия выполняются на VPS. Исходники забираются через git, сборка —
там же.

## Автоустановка одной командой (xr-setup)

Разделы ниже описывают ручной путь. Вместо них чистый VPS поднимается
идемпотентным установщиком (LLD-13, XR-015):

```bash
curl -fsSL https://<хаб-с-раздачей>/api/v1/setup/install.sh \
  | sh -s -- server --with-hub --hub-domain xr-hub.example.com
```

Обёртка качает `xr-setup` под арку машины, сверяет SHA-256 и запускает его.
База раздачи по умолчанию зашита на основной хаб проекта; при установке со
своего хаба задать её явно, окружением для обёртки и флагом для установщика:
`XR_SETUP_BASE=https://<хаб>/api/v1/setup` (обёртка передаст её дальше как
`--dist-url`). Установщик доводит систему до целевого состояния по шагам:
бинари `xr-server` и `xr-hub` (с раздачи `setup-dist` этой базы), конфиги с
генерацией ключа обфускации и пароля админа, ключ подписи, sysctl
(bbr/fq, буферы), systemd-юниты из `deploy/`. По завершении печатает
одноразовый инвайт для приложения и пароль админки (он же остаётся в
`/etc/xr-hub/admin.pass`).

Свойства и обслуживание:

- **Повторный запуск безопасен**: каждый шаг сначала проверяет состояние;
  существующие конфиги, ключ и salt не перетираются (перегенерация только
  с `--force`, ключ подписи не перегенерируется никогда). Упавшая на
  середине установка доводится повторным запуском той же команды.
- Без сети на цели: накидать бинари scp и указать `--from-dir <dir>`.
- Откат: `systemctl disable --now xr-proxy-server xr-hub`, удалить
  `/usr/local/bin/{xr-server,xr-hub,xr-setup}`, `/etc/xr-proxy`,
  `/etc/xr-hub`, `/var/lib/xr-hub` и `/etc/sysctl.d/99-xr-proxy.conf`.
- TLS установщик не настраивает: хаб слушает `127.0.0.1:8080`, наружу его
  выводит nginx с сертификатом (раздел 3); до этого ссылка инвайта у
  получателя не откроется.
- Раздача `setup-dist` наполняется с машины сборки скриптом
  `scripts/release-xr-setup.sh` (musl-бинари обеих арок, включая `xr-client`
  для роутерной цели, + `install.sh` + `SHA256SUMS` в
  `/var/lib/xr-hub/setup-dist`).
- Той же командой ставится и OpenWRT-роутер (`sh -s -- router ...`),
  подробности в `docs/OPENWRT.md`.

## Требования

- VPS с публичным IP (Ubuntu/Debian)
- Git
- Rust toolchain: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- Node.js 18+: `curl -fsSL https://deb.nodesource.com/setup_18.x | bash - && apt install -y nodejs`
- TLS-сертификат (Let's Encrypt или самоподписанный)

## 1. Клонирование и сборка

```bash
cd /opt
git clone <repo-url> xr-proxy
cd xr-proxy

# Собрать Admin UI
cd xr-hub/admin-ui
npm ci
npm run build
cd ../..

# Собрать бинарь
cargo build --release -p xr-hub

# Установить
cp target/release/xr-hub /usr/local/bin/
```

## 2. Каталоги и конфигурация

```bash
mkdir -p /etc/xr-hub/tls /var/lib/xr-hub/presets /var/lib/xr-hub/invites

# Захешировать пароль админки (argon2id)
ADMIN_HASH=$(xr-hub hash-password 'ВАШ_ПАРОЛЬ')

cat > /etc/xr-hub/config.toml <<EOF
[server]
bind = "0.0.0.0:8080"
data_dir = "/var/lib/xr-hub"

[tls]
cert = "/etc/xr-hub/tls/fullchain.pem"
key  = "/etc/xr-hub/tls/privkey.pem"

[[admin.users]]
username = "admin"
password_hash = "$ADMIN_HASH"

[invites]
dev_mode = false
default_ttl_seconds = 86400
max_ttl_seconds = 604800
EOF
chmod 600 /etc/xr-hub/config.toml
```

### Забыли пароль админки

На сервере по SSH, одной командой (спросит новый пароль дважды, ввод скрыт):

```bash
xr-hub reset-password                # сбрасывает пользователя "admin"
xr-hub reset-password --user NAME    # другой пользователь
systemctl restart xr-hub             # применить
```

Команда правит только строку `password_hash` в `/etc/xr-hub/config.toml`
(комментарии и форматирование не трогает); путь к конфигу можно переопределить
через `--config`. Альтернатива вручную: `xr-hub hash-password 'НОВЫЙ'` →
вписать хеш в `password_hash` нужного `[[admin.users]]` → рестарт сервиса.

## 3. TLS-сертификат

### Cloudflare Origin Certificate (рекомендуется если домен на Cloudflare)

Cloudflare терминирует публичный TLS. Между Cloudflare и VPS — Origin
Certificate. Клиенты (браузер, xr-client, Android) видят валидный
сертификат Cloudflare, а не origin cert.

1. Cloudflare Dashboard → SSL/TLS → Origin Server → Create Certificate.
2. Скопировать сертификат и ключ на VPS:

```bash
# Вставить содержимое из Cloudflare:
nano /etc/xr-hub/tls/fullchain.pem   # Origin Certificate (PEM)
nano /etc/xr-hub/tls/privkey.pem     # Private Key (PEM)
chmod 600 /etc/xr-hub/tls/privkey.pem
```

3. В Cloudflare: SSL/TLS → Overview → режим **Full (strict)**.
4. DNS-запись домена — Proxied (оранжевое облако).
5. В `config.toml` порт `bind` может быть любым (например 8080) — Cloudflare
   пойдёт на него через origin rules, либо можно повесить на 443 напрямую.

> **Важно:** Origin Certificate подписан Cloudflare CA, который не в
> публичных trust store'ах. Если клиент ходит мимо Cloudflare (например
> по IP напрямую), reqwest отклонит сертификат. Это нормально — весь
> трафик должен идти через Cloudflare.

### Let's Encrypt

```bash
apt install -y certbot

# Порт 80 должен быть свободен
certbot certonly --standalone -d xr-hub.example.com

ln -sf /etc/letsencrypt/live/xr-hub.example.com/fullchain.pem /etc/xr-hub/tls/fullchain.pem
ln -sf /etc/letsencrypt/live/xr-hub.example.com/privkey.pem /etc/xr-hub/tls/privkey.pem
```

### Самоподписанный (только для тестов)

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
  -keyout /etc/xr-hub/tls/privkey.pem \
  -out /etc/xr-hub/tls/fullchain.pem \
  -days 365 -nodes -subj "/CN=xr-hub"
```

### Wildcard на web-домен публикаций (LLD-38)

Браузерный вход адресует машину поддоменом: публикация `dash` открывается на
`https://dash.<web-домен>`. Поэтому на web-домен нужны своя DNS-запись и свой
сертификат, и заводятся они один раз на все публикации.

1. DNS: запись `*.<web-домен>` на адрес VPS с фронтом. На Cloudflare это тот же
   proxied-режим, что у хаба, домен может быть и поддоменом хабового
   (`*.web.example.com`).
2. Сертификат на `*.<web-домен>`. Cloudflare Origin CA даёт wildcard в форме
   `*.web.example.com` сразу; у Let's Encrypt wildcard выпускается только
   проверкой DNS-01:

```bash
certbot certonly --manual --preferred-challenges dns \
  -d '*.web.example.com' -d 'web.example.com'
```

Своей автоматики продления не заводим: продлевает штатный certbot или
Cloudflare, как и сертификат самого хаба.

3. Фронт. Сертификатами `xr-web` не занимается: он слушает HTTP на локальном
   порту за тем же фронтом (Cloudflare либо nginx на VPS), что выводит наружу
   хаб, и собственный блок `[tls]` в его конфиге это вариант для установки без
   фронта. Хаб и публикации живут на разных именах, поэтому фронт разводит их
   по `Host`: хабовое имя на порт хаба, `*.<web-домен>` на порт `xr-web`.
4. В конфиге хаба включается блок `[web]` (см. `configs/hub.toml`):

```toml
[web]
domain = "web.example.com"
shared_secret = "тот же секрет, что в конфиге xr-web"   # openssl rand -base64 32
```

Секрет заменяет фронту права админки: сессии админки ему не дают, приватного
ключа подписи он не видит, а без блока `[web]` служебные ручки отвечают `503` и
браузерный вход просто выключен. Публикации при этом заводятся с машины
владельца (`xr-share expose add`), и видно их разделом «Публикации» в админке.

Проверка, что хаб готов принять фронт (секрет из своего конфига):

```bash
curl -s -H "Authorization: Bearer $XR_WEB_SECRET" \
  https://xr-hub.example.com/api/v1/web/status
# список публикаций с полем online; без заголовка тут 401
```

### Сервис xr-web (браузерный вход)

Ставится профилем установщика вместе с хабом или к уже стоящему:

```bash
xr-setup server --with-hub --hub-domain xr-hub.example.com \
                --with-web --web-domain web.example.com
# хаб на другой машине: --with-web --web-domain ... --hub-url https://xr-hub.example.com
```

Установщик кладёт бинарь `/usr/local/bin/xr-web`, конфиг
`/etc/xr-web/config.toml` (образец в `configs/web.toml`), юнит
`deploy/xr-web.service` и генерирует общий секрет, дописывая его блоком `[web]`
в конфиг хаба на этой же машине. Повторный запуск ничего не перетирает: уже
стоящий секрет берётся из того конфига, где он есть. По завершении печатаются
находки про то, чего установщик сделать не может: DNS-запись, сертификат и
правило фронта.

Фронт разводит хаб и публикации по `Host` (nginx на VPS):

```nginx
server {
    listen 443 ssl;
    server_name *.web.example.com;
    ssl_certificate     /etc/nginx/ssl/web-example-com.pem;      # wildcard
    ssl_certificate_key /etc/nginx/ssl/web-example-com.key;
    location / {
        proxy_pass http://127.0.0.1:8090;
        proxy_set_header Host $host;                 # имя публикации едет в Host
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;      # WebSocket живой ленты
        proxy_set_header Connection $connection_upgrade;
        proxy_read_timeout 3600s;
    }
}
```

`Host` обязан доехать до `xr-web` неизменным: по нему выбирается публикация.
`X-Forwarded-For` нужен логу входа и лимиту попыток, дальше VPS он не едет.

Проверка после раскладки (публикация `dash` заведена на машине владельца
командой `xr-share expose add --name dash`):

```bash
systemctl status xr-web
curl -s https://dash.web.example.com/.xr-web/healthz            # ждём ok
curl -s -H 'Accept: application/json' https://dash.web.example.com/
# {"error":"unauthenticated"}, код 401
curl -sI https://dash.web.example.com/ | grep -i set-cookie      # пусто до входа
```

В логе сервиса (`journalctl -u xr-web`) видно вход, выбор маршрута, начало
апгрейда со сроком штатного закрытия, обрыв туннеля и строку
`метод-статус-длительность` на каждый запрос; тела, query-строки и cookie туда
не попадают.

Живая лента (WebSocket) идёт через вход насквозь, но не дольше, чем ей позволяет
relay: `[relay] splice_lifetime_secs` (по умолчанию час) рубит сплайс жёстко.
Чтобы обрыв не выглядел зависанием, `xr-web` закрывает такое соединение сам,
штатным `1001 going away`, за минуту до потолка, а приложение обязано
переподключиться само. Потолок фронт берёт из маршрута, поэтому значение в
конфигах relay и хаба должно совпадать: разъедутся, и фронт будет считать срок
не от того числа. `proxy_read_timeout` на фронте держим не меньше потолка, иначе
соединение оборвёт nginx раньше всех.

Проверить весь путь целиком (WebSocket, штатное закрытие, поведение при
выключенной машине) можно скриптом `scripts/check-browser-entry.py`. Он ставит
на машине владельца синтетический сервис и судит вход снаружи:

```bash
# на машине владельца: сервис и публикация на него
python3 scripts/check-browser-entry.py echo-service --port 8765 &
xr-share expose add --name dash --upstream 127.0.0.1:8765

# откуда угодно: вход, кадры и штатное закрытие по лимиту
python3 scripts/check-browser-entry.py ws --entry https://dash.web.example.com \
    --host dash.web.example.com --username owner --password ... --wait 3700
# browser entry ws: ok

# машина выключена: 502 с причиной за секунды и статус публикации на хабе
python3 scripts/check-browser-entry.py offline --entry https://dash.web.example.com \
    --host dash.web.example.com --username owner --password ... \
    --hub https://xr-hub.example.com --secret "$XR_WEB_SECRET" --publication dash
# browser entry offline: ok
```

Ждать штатного закрытия час не обязательно: на время проверки
`splice_lifetime_secs` уменьшают в конфигах relay и хаба (например до 60) и
перезапускают оба сервиса.

## 4. Подпись пресетов (опционально)

```bash
openssl genpkey -algorithm ed25519 -out /tmp/ed25519.pem
openssl pkey -in /tmp/ed25519.pem -outform DER | tail -c 32 | base64 > /var/lib/xr-hub/signing_key
rm /tmp/ed25519.pem
chmod 600 /var/lib/xr-hub/signing_key
```

Добавить в `/etc/xr-hub/config.toml`:

```toml
[signing]
private_key = "/var/lib/xr-hub/signing_key"
```

## 5. Systemd unit

Юнит лежит в репозитории, тот же файл вшит в xr-setup: ручной и
автоматический пути ставят одно и то же.

```bash
cp deploy/xr-hub.service /etc/systemd/system/xr-hub.service
systemctl daemon-reload
systemctl enable --now xr-hub
```

## 6. Проверка

```bash
# API
curl -k https://localhost:8080/api/v1/presets
# Ожидаемый ответ: []

# Admin UI — открыть в браузере:
# https://xr-hub.example.com/
# Войти логином/паролем из [[admin.users]]
```

## 7. Firewall

```bash
ufw allow 8080/tcp
```

## 8. Бэкап и восстановление (XR-224)

Состояние хаба живёт единственной копией: потеря диска уносит ключ подписи, а
вместе с ним доверие всего флота (приложение пиннит публичную половину при
apply инвайта, relay проверяет ею же). Поэтому ежедневный архив уезжает на
второй VPS, а разворачивает его человек.

### Что попадает в архив

`xr-hub backup` собирает `tar.gz` с правами 600 и кладёт его в `--out`
(по умолчанию `/var/backups/xr-hub`), имя несёт UTC-метку:

- `MANIFEST.json` (время, хост, версия хаба, отпечаток **публичной** половины
  ключа подписи, счётчики пресетов, инвайтов и шар);
- `config.toml` из `--config` и ключ подписи по пути из `[signing]`;
- содержимое `data_dir`: `presets/`, `invites/`, `shares/`.

Не попадают раздачи дистрибутивов: `releases/`, `setup-dist/`, `share-dist/` и
их копии от прошлых выкатов (`share-dist.bak.<метка>` и подобные). Раздачи
воспроизводятся с машины сборки (`scripts/release-xr-setup.sh`, `sign-release`,
релиз агента), а весят сотни мегабайт, и гонять их каждую ночь незачем.
`--keep N` (по умолчанию 14) подчищает старые архивы, `--keep 0` отключает
ротацию.

```bash
xr-hub backup --out /var/backups/xr-hub --keep 14
```

Команда печатает путь, размер, отпечаток ключа, счётчики и список отсечённых
раздач: по выводу видно, что уехало непустое и что осталось за бортом. Список
раздач ведётся руками, поэтому новая в него сама не попадёт, и на этот случай
всякий каталог тяжелее 10 МБ, всё же поехавший в архив, называется в stderr
именем и размером:

```
внимание: в архив поехал тяжёлый каталог plugin-dist (281.5 МБ). Раздачам
дистрибутивов в бэкапе не место, их место в списке исключений.
```

Так новая раздача видна с первой же ночи (cron принесёт письмо), а новый
каталог состояния при этом не теряется молча: он уезжает в архив, просто с
предупреждением. Само исключение добавляется в `DIST_DIRS` (`xr-hub/src/backup.rs`).

### Отправка на второй VPS

Отправляет `deploy/xr-hub-backup.sh` (его же ставит `xr-setup --with-hub`
шагом `hub:backup` в `/usr/local/bin` вместе с `/etc/cron.d/xr-hub-backup`).
Адрес приёмника и ключ он берёт из `/etc/xr-hub/backup.env` (chmod 600, в git
не попадает), болванку кладёт установщик:

```ini
BACKUP_HOST=<адрес приёмника>
BACKUP_KEY=/root/.ssh/xr-hub-backup
```

На приёмнике ключ зажат forced command на `deploy/xr-hub-backup-receive.sh`,
как это сделано у cert-sync (раздел про TLS): скрипт принимает поток на stdin,
проверяет gzip и наличие `MANIFEST.json`, кладёт архив под именем с меткой
времени, оставляет `KEEP` поколений и обновляет `.last-received`. Больше этим
ключом сделать нечего.

```bash
# на приёмнике, один раз
install -m 755 deploy/xr-hub-backup-receive.sh /usr/local/bin/
# в /root/.ssh/authorized_keys:
# command="/usr/local/bin/xr-hub-backup-receive.sh",no-port-forwarding,\
# no-agent-forwarding,no-pty ssh-ed25519 AAAA... xr-hub-backup
```

Сторож встроен в тот же скрипт: провал сборки или отправки, незаполненный
`backup.env` и устаревшая метка последнего успеха уходят алертом в Telegram
через `/etc/xr-proxy/alert.env` (общий с `cert-alert.sh`). Когда и `alert.env`
нет, скрипт ругается в stderr, и это письмо приносит cron.

### Разворачивание на чистом VPS

1. Поднять машину установщиком (раздел «Автоустановка одной командой»), но
   сервис остановить: `systemctl stop xr-hub`.
2. Забрать свежий архив с приёмника и разложить:

   ```bash
   xr-hub restore --from /var/backups/xr-hub/xr-hub-backup-<метка>.tar.gz
   ```

   Пути берутся из конфига внутри архива, ключ и конфиг ложатся с правами 600,
   затираемые файлы остаются рядом копией `*.bak.<метка>`. Каталоги секретов
   (`/var/lib/xr-hub`, каталог конфига, каталог архивов) restore и backup
   сужают до 700, даже если те уже существовали с 755 от установщика: имена
   ключей, инвайтов и бэкапов посторонним ни к чему. Если на месте лежит
   **другой** ключ подписи, restore отказывается работать: молча подменённый
   корень доверия хуже отсутствия бэкапа. Убедившись, что архив от этого хаба,
   повторить с `--force`.
3. `systemctl start xr-hub` и сверить, что пресеты и шары на месте.

Проверить бэкап, не трогая живой хаб, можно тем же restore во временные
каталоги: `--data-dir /tmp/xr-hub-check` перенацелит и пути в восстановленном
конфиге, так что хаб поднимется на нём с ключом рядом с данными.

```bash
xr-hub --config /tmp/xr-hub-check.toml restore \
  --from <архив> --data-dir /tmp/xr-hub-check
sed -i 's/^bind = .*/bind = "127.0.0.1:18080"/' /tmp/xr-hub-check.toml
xr-hub --config /tmp/xr-hub-check.toml &
curl -s http://127.0.0.1:18080/api/v1/presets
```

### Что бэкап не закрывает

Отдельного ключа шифрования у архива нет: приёмник это root-only VPS того же
оператора, а потерянный ключ шифрования сам стал бы второй точкой потери.
Автоматика на машину владельца не ходит, поэтому разовую офлайн-копию ключа
подписи (менеджер паролей, как у release-ключа ниже) снимает владелец руками.

## Выпуск релиза Android-APK (самообновление, LLD-12)

xr-hub раздаёт APK + подписанный манифест версии; приложение проверяет подпись
**отдельным release-ключом** и SHA-256, и ставит обновление через системный
установщик. Главное свойство безопасности — **компрометация VPS ≠ RCE**: на
сервере подписи нет, подделанный манифест приложение отвергает.

### Три разных ключа — не путать

| Ключ | Где приватная половина | Что подписывает |
|---|---|---|
| Серверный (LLD-01) | на VPS | пресеты |
| **Release (этот раздел)** | **офлайн у владельца, НИКОГДА не на VPS** | манифест APK |
| APK-signing (Android keystore) | офлайн | сам `.apk`-пакет |

### 0. Одноразово: release-ключ и сборка приложения

> ⚠️ **Release-ключ генерируется ОДИН раз и переиспользуется для всех релизов.**
> Если устройства уже работают на сборке с зашитым публичным ключом, **каждый
> следующий релиз подписывай той же приватной половиной** — иначе приложение
> отвергнет обновление (подпись не сойдётся с pinned-ключом). Генерация нового
> ключа = ротация = нужна **новая сборка** для всех устройств (новый
> `xrReleasePublicKey`) и разовая ручная переустановка. Не генерируй новый ключ
> на каждый релиз. (Для разработки достаточно одной тестовой пары; её приватная
> половина живёт офлайн у владельца, как и боевая.)

Release-ключ генерируется **на машине владельца** (не на VPS) и больше туда не
попадает:

```bash
# на ноутбуке владельца, не на VPS:
xr-hub gen-release-key
# печатает приватный (хранить офлайн, например в менеджере паролей) и
# публичный base64. Приватный — в файл с chmod 600, публичный — в сборку.
echo '<приватный_base64>' > ~/.xr/release.key && chmod 600 ~/.xr/release.key
```

Публичную половину **впаять в приложение** через gradle-проперти
`xrReleasePublicKey` (это НЕ секрет — гейтит обновление приватный ключ):

```properties
# xr-android/gradle.properties  (или ~/.gradle/gradle.properties)
xrReleasePublicKey=<публичный_base64>
```

либо `./gradlew … -PxrReleasePublicKey=<публичный_base64>`. Пустое значение ⇒
самообновление в этой сборке выключено (проверка вернёт `no_release_key`).

> ⚠️ **APK-signing keystore должен быть ОДИН для всех релизов.** Иначе Android
> откажет ставить новый APK поверх старого (разные подписи пакета). Текущие
> `build.sh --release` подписаны **debug-keystore** — значит и
> self-update-APK подписывайте тем же debug-keystore (он стабилен на машине
> сборки). Переход на отдельный production-keystore — разовая ручная
> переустановка на каждом устройстве (новый pinned release-ключ роли не играет,
> речь про подпись самого пакета).

> ⚠️ **Бампайте `versionCode` на каждый релиз.** Версия в файлы репозитория не
> зашита, а передаётся сборке гредл-пропертями `xrVersionCode` и
> `xrVersionName` (см. команду ниже); то же число уходит в `--version-code` при
> подписи. Приложение предлагает обновление только когда `version_code`
> манифеста **строго больше** установленного; манифест с меньшим/равным кодом
> (в т.ч. replay старого) игнорируется. Без `xrVersionName` сборка останется с
> dev-именем версии (`0.1.0-<commit>-NNNN`).

### Всё это одной командой

Шаги 1 и 2 ниже расписаны как ручная цепочка, но каждый релиз они одинаковые,
поэтому упакованы в `scripts/release-apk.sh` (XR-109):

```bash
./scripts/release-apk.sh --version 1.0.0 --version-code 100
./scripts/release-apk.sh --bump --notes "Проводник по группам"
```

Скрипт собирает APK с нужными гредл-пропертями, подписывает манифест,
раскладывает файлы на оба хаба через staging-каталог с бэкапом прежней пары и
проверяет, что оба отдают новый `latest`. Адреса, порты и путь к ключу лежат в
гитигнорнутом `local-docs/release.env`, запуск без него печатает шаблон. Ручная
цепочка ниже остаётся описанием того, что скрипт делает, и путём на случай
нештатного релиза.

Проверка подписи отдельной командой (ей же скрипт судит и живой манифест, и
свежезалитый):

```bash
curl -s https://xr-hub.example.com/api/v1/app/latest \
  | xr-hub verify-release --signed - --pubkey <публичный_base64> --expect-version-code 100
```

`--key <приватный>` вместо `--pubkey` выводит публичную половину из ключа: так
проверяется, что подписывать собрались тем же ключом, каким подписан живой
манифест.

### 1. Собрать и подписать релиз (на машине владельца, офлайн-ключ)

```bash
cd xr-android
ORG_GRADLE_PROJECT_xrVersionCode=<N> \
ORG_GRADLE_PROJECT_xrVersionName=<X.Y.Z> \
ORG_GRADLE_PROJECT_xrReleasePublicKey=<публичный_base64> \
./build.sh --release
```

Затем подпись манифеста:

```bash
xr-hub sign-release \
  --apk xr-android/app/build/outputs/apk/release/app-release.apk \
  --version <X.Y.Z> \
  --version-code <N> \
  --key ~/.xr/release.key \
  --base-url https://xr-hub.example.com \
  --notes "Multi-VPS failover, панель здоровья" \
  --out ./release-staging
```

Команда считает SHA-256 и размер APK, формирует `manifest.json`, подписывает
его **локально** приватным ключом и пишет рядом `manifest.sig`, а также копию
APK как `<version>.apk`. Хаб ничего не подписывает — у него release-ключа нет.

`--out` с отдельной директорией не косметика: заливать надо из неё, а не из
`apk/release/`. Следующая сборка молча перепишет `app-release.apk`, и фоновая
заливка прямо из выходной директории однажды утаскивает недописанный файл
(ловили на живом релизе).

### 2. Выложить файлы на хаб

Скопировать **три файла** в каталог релизов хаба (по умолчанию
`<data_dir>/releases`, т.е. `/var/lib/xr-hub/releases`):

```bash
ssh -p 8822 root@<vps> 'mkdir -p /var/lib/xr-hub/releases'
scp -P 8822 release-staging/manifest.json release-staging/manifest.sig \
            release-staging/0.2.0.apk \
            root@<vps>:/var/lib/xr-hub/releases/
```

Если хабов больше одного (основной плюс failover-standby с тем же
`server_name`, как у нас), релиз выкладывается на **каждый**: паритет держится
руками, и забытый резерв после переключения DNS продолжит раздавать старую
версию. Перед перезаписью сохранить старые `manifest.json`/`manifest.sig` в
`*.bak.<ts>` рядом, это и есть весь откат (старые `<version>.apk` с диска не
удаляются). Многомегабайтный APK на медленный канал удобнее лить
`rsync --partial --inplace`: докачает после обрыва вместо рестарта с нуля.

Каталог можно переопределить в конфиге:

```toml
[server]
releases_dir = "/var/lib/xr-hub/releases"   # необязательно; дефолт = <data_dir>/releases
```

Перезапуск хаба не нужен — эндпоинты читают файлы с диска при каждом запросе:

```bash
# манифест + подпись
curl -k https://xr-hub.example.com/api/v1/app/latest
# APK (стрим)
curl -k -o test.apk https://xr-hub.example.com/api/v1/app/download/0.2.0
```

### 3. Что увидит пользователь

Приложение раз в сутки (и по кнопке «Проверить обновления» во вкладке Servers)
дёргает `/app/latest`, проверяет подпись pinned-ключом, и при более новой версии
показывает баннер «Доступно обновление». «Обновить» → скачивание → проверка
SHA-256 → системный установщик. Если разрешение «устанавливать из этого
источника» не выдано — приложение ведёт в системный экран, не падает.

## Обновление xr-hub

```bash
cd /opt/xr-proxy
git pull

# Пересобрать UI (если менялся)
cd xr-hub/admin-ui && npm ci && npm run build && cd ../..

# Пересобрать бинарь
cargo build --release -p xr-hub
cp target/release/xr-hub /usr/local/bin/
systemctl restart xr-hub
```

Данные (`/var/lib/xr-hub/`) не затрагиваются при обновлении.
