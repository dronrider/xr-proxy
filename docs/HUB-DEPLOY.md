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

# Сгенерировать admin-токен
ADMIN_TOKEN=$(openssl rand -base64 32)
echo "Сохрани admin-токен: $ADMIN_TOKEN"

cat > /etc/xr-hub/config.toml <<EOF
[server]
bind = "0.0.0.0:8080"
data_dir = "/var/lib/xr-hub"

[tls]
cert = "/etc/xr-hub/tls/fullchain.pem"
key  = "/etc/xr-hub/tls/privkey.pem"

[admin]
token = "$ADMIN_TOKEN"

[invites]
dev_mode = false
default_ttl_seconds = 86400
max_ttl_seconds = 604800
EOF
```

## 3. TLS-сертификат

### Let's Encrypt (рекомендуется)

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
# Ввести admin-токен
```

## 7. Firewall

```bash
ufw allow 8080/tcp
```

## Обновление

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
