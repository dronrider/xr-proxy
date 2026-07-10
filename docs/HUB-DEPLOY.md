# Деплой xr-hub на VPS

Все действия выполняются на VPS. Исходники забираются через git, сборка —
там же.

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

```bash
cat > /etc/systemd/system/xr-hub.service <<'EOF'
[Unit]
Description=xr-hub control plane
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/xr-hub --config /etc/xr-hub/config.toml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

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
