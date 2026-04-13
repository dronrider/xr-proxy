# LLD-01 — Control-plane сервис `xr-hub`

**Статус:** Implemented
**Область:** новый крейт `xr-hub`, обновления клиентской конфигурации в `xr-client` / `xr-android`, расширения `xr-proto`
**Связанные документы:** [ARCHITECTURE.md §2, §5.3, §6.2](../ARCHITECTURE.md), [LLD-04](04-onboarding-qr-uri.md), [LLD-05](05-android-rules-editor.md)

Создаём отдельный сервис на VPS, который раздаёт пресеты маршрутизации и
одноразовые инвайты для подключения новых клиентов. Клиенты (OpenWRT,
Android) получают пресеты по сети, кэшируют их локально и расширяют
собственными override-правилами. Управление — через Vue SPA, встроенный в
тот же бинарь.

---

## 1. Текущее состояние

- В `xr-proto/config.rs` [RoutingConfig](../../xr-proto/src/config.rs#L40) —
  плоский список правил без понятия «пресет». Никакого версионирования и
  источника обновлений.
- В Android «пресет Russia» захардкожен как Kotlin-константа
  [VpnViewModel.kt:321-366](../../xr-android/app/src/main/java/com/xrproxy/app/ui/VpnViewModel.kt#L321).
  Изменение правил требует пересборки и переустановки APK.
- На OpenWRT правила редактируются вручную в `/etc/xr-proxy/config.toml`
  через SSH. Нет способа централизованно обновить все роутеры сразу.
- Ни на сервере, ни на клиенте нет HTTP-стека, нет никакого control-plane.
- Подключение нового клиента требует ручной передачи IP сервера, порта,
  obfuscation-key, salt. Ошибка в любом поле → непонятная ошибка
  подключения.

## 2. Целевое поведение

### 2.1 Пресеты

- Пресеты живут централизованно на VPS, версионируются, редактируются через
  Admin UI.
- Клиент (`xr-client` или `xr-android`) в конфигурации указывает имя пресета
  (например, `russia`) и локальные override-правила.
- При старте и раз в N часов клиент сверяет локальную версию пресета с той,
  что раздаёт хаб. Если серверная новее — скачивает, опционально проверяет
  подпись, атомарно заменяет локальный файл.
- При компиляции `Router` клиент объединяет пресет + override'ы: правила
  override применяются первыми, пресет — как fallback.
- Если хаб недоступен — клиент работает с закэшированной версией. Если и её
  нет — работает с `default_action` из локального конфига (обычно `direct`).

### 2.2 Инвайты

- Админ генерирует одноразовый инвайт через Admin UI. Инвайт — это короткая
  случайная строка, по которой клиент может получить полный конфиг
  подключения (server_address, port, obfuscation_key, salt, имя пресета).
- Инвайт живёт в файле на диске хаба, содержит TTL и флаг «одноразовый».
- После первого успешного GET одноразовый инвайт помечается как
  `consumed_at` и при повторном обращении возвращает 410 Gone.
- В dev-режиме (флаг в конфиге) одноразовость отключается — инвайт
  используется многократно до истечения TTL.
- Использование самого инвайта — вне этого LLD (QR, URI, UX onboarding'а
  описаны в LLD-04). Здесь только серверная часть: создание, выдача,
  ревокация, TTL.

### 2.3 Admin UI

- Одна страница списка пресетов (таблица с именем, версией, датой
  обновления, числом правил).
- Страница редактирования пресета: редактор правил (добавить/удалить
  правило, правило = `action` + списки `domains`/`ip_ranges`/`geoip`), live
  preview итогового TOML, кнопка Save. Без drag-n-drop в первой версии —
  простой порядок по индексу, добавляется в LLD-05.
- Страница инвайтов: таблица действующих (token, created_at, expires_at,
  ссылка, кнопки «ревокация», «копировать ссылку»), форма генерации нового
  (TTL, пресет, комментарий).
- Аутентификация — Bearer-token, введённый при первом заходе и сохранённый
  в `localStorage`. Всё управление пользователями отложено до отдельной LLD.

### 2.4 Один бинарь, один порт

- `xr-hub` — отдельный Cargo-крейт, отдельный бинарь, отдельный порт
  (по умолчанию `:8080`). Не пересекается с `xr-server:8443` (туннель) и
  `xr-server:9999` (UDP relay).
- TLS терминируется внутри сервиса через `rustls` + `axum-server`. Nginx не
  используется.
- Статика SPA встроена в бинарь через `rust-embed`, dev-режим читает с
  диска (feature `dev-ui`). Деплой = `scp xr-hub vps:/usr/local/bin/`.

---

## 3. Дизайн решения

### 3.1 Модель данных

Расширяем `xr-proto/src/config.rs` (новый модуль `preset.rs`):

```rust
pub struct Preset {
    pub name: String,         // slug: [a-z0-9_-]+
    pub version: u64,         // монотонно растёт при каждом Save
    pub updated_at: String,   // RFC3339 UTC
    pub description: String,  // свободный текст, до 512 символов
    pub rules: RoutingConfig, // существующая структура
    pub signature: Option<String>, // base64(ed25519), может быть None
}
```

Подпись считается над канонизированным JSON тела
(`name || version || updated_at || rules`) без поля `signature`. Канонизация —
сортировка ключей + отсутствие пробелов, через `serde_json::to_value` +
ручной обход Value (либо крейт `canonical_json`, если окажется в дереве
зависимостей бесплатно). Детали канонизации зафиксируем на первой сборке
— важно, что алгоритм детерминированный и одинаков на сервере и клиенте.

Инвайт:

```rust
pub struct Invite {
    pub token: String,        // 16 байт случайных, base64url без padding → 22 символа
    pub created_at: String,
    pub expires_at: String,
    pub consumed_at: Option<String>,
    pub one_time: bool,
    pub comment: String,
    pub payload: InvitePayload,
}

pub struct InvitePayload {
    pub server_address: String,
    pub server_port: u16,
    pub obfuscation_key: String, // base64
    pub modifier: String,
    pub salt: u64,
    pub preset: String,          // имя пресета, обязательно
    pub hub_url: String,         // публичный URL этого же xr-hub
}
```

`InvitePayload` — то, что клиент получает по `GET /invite/<token>` и
кладёт к себе в конфиг. Структура стабильна, добавление полей делается
только через новое опциональное поле с `Default`.

Обе структуры идут в `xr-proto` с `#[derive(Serialize, Deserialize)]` и
`#[cfg_attr(feature = "ts-rs", derive(TS))]`, чтобы `xr-hub` мог генерировать
TS-типы через `ts-rs` в один проход с `cargo test`.

### 3.2 Раскладка файлов на диске

Корень данных — `/var/lib/xr-hub/` (задаётся в конфиге, дефолт):

```
/var/lib/xr-hub/
  presets/
    russia.json
    streaming.json
  invites/
    <token>.json
  signing_key          (ed25519 private, опционально, 0600)
  signing_key.pub      (ed25519 public, 0644)
```

Атомарная запись файлов — через `tempfile::NamedTempFile::persist` в ту же
директорию (на том же filesystem) с последующим `rename`. Чтения — без
блокировки (ОС гарантирует атомарность rename на POSIX).

Параллельные изменения одного пресета через Admin UI защищены `RwLock` на
`HashMap<String, Preset>` в памяти (см. §3.4). Диск — источник правды при
старте, дальше in-memory — источник правды, диск обновляется при каждом
изменении.

Нет базы данных. Нет SQLite. Файлы-и-только-файлы.

### 3.3 HTTP API

Префикс `/api/v1` для всех JSON-эндпоинтов. Корневой `/` и `/admin/*` — SPA.

**Публичные (без аутентификации):**

| Метод | Путь | Что делает |
|---|---|---|
| `GET` | `/api/v1/presets` | Массив `PresetSummary { name, version, updated_at, rules_count }`. Ключевой эндпоинт для «сверки версий» клиентами. Лёгкий, кэшируемый по `ETag`. |
| `GET` | `/api/v1/presets/:name` | Полный `Preset`. Если существует и `If-None-Match` совпадает — `304 Not Modified`. |
| `GET` | `/api/v1/invite/:token` | `InvitePayload`. Если не найден → 404. Если истёк → 410 Gone. Если одноразовый и уже использован → 410. При успехе одноразового — пишет `consumed_at` и отдаёт payload. |
| `GET` | `/api/v1/public-key` | Публичный ключ для проверки подписи (base64). Если подписи не используются — возвращает 404. |

**Admin (Bearer-token):**

| Метод | Путь | Что делает |
|---|---|---|
| `POST` | `/api/v1/admin/presets` | Создать пресет. Тело — `{ name, description, rules }`. `version = 1`, подпись (если ключ есть) проставляется на сервере. |
| `PUT` | `/api/v1/admin/presets/:name` | Обновить. Тело — то же, сервер инкрементит `version` и подписывает. |
| `DELETE` | `/api/v1/admin/presets/:name` | Удалить. |
| `GET` | `/api/v1/admin/invites` | Массив `Invite` (включая истёкшие и consumed, до 1000 последних). |
| `POST` | `/api/v1/admin/invites` | Тело — `{ ttl_seconds, one_time, comment, payload }`. Возвращает созданный `Invite` + полный URL вида `https://<host>/api/v1/invite/<token>`. |
| `DELETE` | `/api/v1/admin/invites/:token` | Ревокация (пишет `consumed_at = now`, оставляя запись). |

**Аутентификация Admin.** Заголовок `Authorization: Bearer <token>`. Токен
сравнивается константным временем с `admin_token` из конфига. На 1 неверный
токен — 401; тротлинг (rate limit) не вводим на первой версии, он опишется
отдельно, когда появится публичный хаб.

**Rate limits и CORS.** Публичные эндпоинты — без лимитов (нагрузка
минимальна: один GET раз в несколько часов от клиента). CORS разрешает
`Origin` только для собственного хоста (список в конфиге `allowed_origins`),
чтобы Admin SPA мог ходить в API из браузера.

### 3.4 Структура крейта `xr-hub`

```
xr-hub/
  Cargo.toml
  src/
    main.rs           # точка входа, парсинг конфига, запуск axum-server
    config.rs         # HubConfig: bind, tls, data_dir, admin_token, signing, dev_mode
    storage.rs        # файловое хранилище: load_all_presets, save_preset, ...
    state.rs          # AppState { presets: RwLock<HashMap<String, Preset>>, invites: ... }
    signing.rs        # ed25519 подписание и верификация Preset
    api/
      mod.rs          # router()
      presets.rs      # публичные и admin handlers пресетов
      invites.rs      # handlers инвайтов
      auth.rs         # middleware Bearer-token
    embed.rs          # rust-embed + ServeDir через cfg(feature = "dev-ui")
  admin-ui/
    package.json
    vite.config.ts
    tsconfig.json
    src/
      main.ts
      router.ts
      stores/
        presets.ts    # Pinia store
        invites.ts
        auth.ts       # хранит Bearer token в localStorage
      pages/
        PresetsList.vue
        PresetEdit.vue
        InvitesList.vue
        Login.vue
      components/
        RulesEditor.vue
        RuleRow.vue
      api.ts          # тонкий fetch-клиент
      types.generated.ts  # генерируется через ts-rs из Rust
    dist/             (.gitignore)
```

**AppState** — общий `Arc<AppState>`, инжектится в handler'ы через
`State<Arc<AppState>>`:

```rust
pub struct AppState {
    pub presets: RwLock<HashMap<String, Preset>>,
    pub invites: RwLock<HashMap<String, Invite>>,
    pub config: HubConfig,
    pub signing: Option<SigningContext>,
}
```

### 3.5 Подпись пресетов

Алгоритм: **ed25519 через `ed25519-dalek`**. Причины: маленький ключ и
подпись, зрелая реализация, детерминированная подпись (тот же ввод → та же
подпись — важно для тестов).

Канонизация для подписи:
1. Собираем `CanonicalPreset { name, version, updated_at, description, rules }`
   (без `signature`).
2. `serde_json::to_vec` с сортировкой ключей через свою обёртку либо через
   promise-free детерминированный сериализатор (`serde_json` на объектах с
   `BTreeMap` даёт сортировку автоматически). Решение реализации — в
   `signing.rs`, но тесты покрывают инвариант «одна и та же Preset → одни
   и те же байты».

Поведение:
- Если `HubConfig.signing.private_key_path` задан — сервер читает ключ при
  старте, подписывает каждый `save_preset` и хранит подпись в `Preset.signature`.
- Если не задан — `signature = None`, клиент принимает без проверки.
- Публичный ключ доступен через `GET /api/v1/public-key`, чтобы клиент мог
  единожды скачать и сохранить как trust anchor.
- Клиент в своём конфиге хранит base64 публичного ключа; если ключ задан и
  ответ хаба содержит подпись, клиент верифицирует перед принятием. Если
  клиент требует подпись, а её нет — отказ с явным логом.

### 3.6 TLS

- Крейт `axum-server` с фичей `tls-rustls`.
- Сертификат и ключ — PEM-файлы, путь задаётся в `HubConfig.tls`:
  ```toml
  [tls]
  cert = "/etc/xr-hub/tls/fullchain.pem"
  key  = "/etc/xr-hub/tls/privkey.pem"
  ```
- Получение сертификатов — вне скоупа сервиса. Пользователь сам решает:
  Let's Encrypt через `certbot certonly --standalone` (можно на отдельном
  порту 80), либо самоподписанный, либо уже имеющийся wildcard. Документация
  этого — в `docs/HUB-DEPLOY.md` (создаётся в рамках реализации).
- Если в конфиге TLS не задан — сервис стартует в plain HTTP и пишет WARN
  в лог. Plain HTTP допустим только для локальной отладки.
- ACME-автоматизация (`acme-lib`, `rustls-acme`) — отложена до отдельного
  LLD. Слишком много failure modes, не хочется тащить в первую версию.

### 3.7 Dev-режим и сборка

**Feature `dev-ui`:**

```toml
[features]
default = []
dev-ui = ["tower-http/fs"]
```

В `embed.rs`:

```rust
#[cfg(not(feature = "dev-ui"))]
pub fn spa_service() -> Router {
    // rust-embed: index.html + assets/
}

#[cfg(feature = "dev-ui")]
pub fn spa_service() -> Router {
    // tower_http::services::ServeDir::new("xr-hub/admin-ui/dist")
}
```

Dev workflow (два терминала):
- `cargo run -p xr-hub --features dev-ui -- --config xr-hub/config.dev.toml`
- `cd xr-hub/admin-ui && npm run dev` — Vite dev-server на `:5173`,
  проксирует `/api/v1/*` на `http://localhost:8080`.

Релиз:
- `cd xr-hub/admin-ui && npm ci && npm run build`
- `cargo build --release -p xr-hub`
- Один бинарь `target/release/xr-hub` со всей статикой внутри.

В `xr-hub/build.rs` добавляем проверку: если включен релизный профиль и
`admin-ui/dist/index.html` отсутствует — ошибка с понятным сообщением
«запусти `npm run build` в admin-ui перед `cargo build --release`». Никакого
авто-вызова npm из build.rs (магия, которую пользователь не ожидает).

### 3.8 Admin UI — Vue 3 + PrimeVue

**Стек:**
- Vue 3 + `<script setup>` + TypeScript
- Vite
- Vue Router (history mode, fallback на `index.html` внутри xr-hub)
- Pinia для state (stores: `auth`, `presets`, `invites`)
- PrimeVue (DataTable — ключевой компонент для списков пресетов, инвайтов и
  правил)
- `@primevue/themes` с базовой темой (Aura) — выбирается один раз, без
  дизайн-системы.

**Типы.** `types.generated.ts` — результат `ts-rs` экспорта Rust-структур
(`Preset`, `Rule`, `Invite`, `InvitePayload`, etc.). Генерируется при
`cargo test -p xr-proto --features ts-rs`. Файл коммитится в git, чтобы Vite
не требовал Rust-сборки для своей работы.

**Экраны:**
- `/` — редирект на `/presets`.
- `/login` — форма Bearer-token, тест через `GET /api/v1/admin/presets` с
  этим токеном, при 200 → сохранение в `localStorage` и редирект.
- `/presets` — `DataTable` со списком. Колонки: name, version, updated_at,
  rules_count, actions. Кнопка «New».
- `/presets/:name` — редактор:
  - заголовок с version и updated_at;
  - `<RulesEditor>` — список `<RuleRow>`. Каждая строка — action-select,
    textarea для domains (по одному на строку), textarea для ip_ranges,
    textarea для geoip. Кнопка «удалить правило», «добавить правило».
  - справа — read-only preview итогового TOML (для копирования в ручной
    конфиг).
  - Save → `PUT /api/v1/admin/presets/:name`, успех → тост-уведомление.
- `/invites` — `DataTable`: token (сокращённый), created, expires, status
  (active/expired/consumed), actions (copy link, revoke). Кнопка «New
  invite» открывает `Dialog`:
  - поля: preset (select из списка пресетов), server_address, server_port,
    obfuscation_key, modifier, salt, TTL (select: 1h / 24h / 7d / custom),
    one_time checkbox (default true), comment.
  - на Create — POST, результат показывается как QR + ссылка + копипаст
    кнопка. Сам QR-рендерится на фронте через `qrcode` npm-пакет.

**Валидация** — поверх `zod` или ручными проверками в Vue. Лучше `zod` —
он ~10 КБ и даёт единообразие. Взять.

### 3.9 Конфигурация `xr-hub`

```toml
# /etc/xr-hub/config.toml

[server]
bind = "0.0.0.0:8080"
data_dir = "/var/lib/xr-hub"

[tls]
cert = "/etc/xr-hub/tls/fullchain.pem"
key  = "/etc/xr-hub/tls/privkey.pem"

[admin]
token = "<длинная случайная строка>"
allowed_origins = ["https://xr-hub.example.com"]

[signing]
# опционально; если не задано — подписи отключены
private_key = "/var/lib/xr-hub/signing_key"

[invites]
# в dev-режиме инвайты не одноразовые независимо от флага
dev_mode = false
default_ttl_seconds = 86400      # 24h
max_ttl_seconds = 604800         # 7d
```

Парсится через `serde` + `toml`. При отсутствии файла — сервис пишет в stderr
путь и минимальный пример и выходит с кодом 2.

### 3.10 Интеграция с клиентами

Меняем `xr-proto::ClientConfig` (минимальное дополнение, не ломая старые
конфиги — все поля `Option`):

```toml
[hub]
url = "https://xr-hub.example.com"
trusted_public_key = "<base64 ed25519>"  # optional
preset = "russia"
# Фоновый sanity-check раз в 5 минут: GET /presets с If-None-Match,
# обычно возвращает 304. Реальная свежесть достигается forced-fetch
# при старте движка (см. ниже), а этот интервал страхует долго-живущие
# клиенты, которые не переподключаются сутками.
refresh_interval_secs = 300

[routing]
# default_action и rules остаются, но теперь это OVERRIDES поверх пресета
default_action = "direct"

[[routing.rules]]
action = "direct"
domains = ["github.corp.internal"]
```

**Семантика слияния** в Router:
1. Загружаем пресет (кэш → хаб → fallback).
2. Создаём `CompiledRouter` из `overrides.rules` + `preset.rules`
   **именно в этом порядке**. Override-правила имеют приоритет и
   срабатывают первыми.
3. `default_action` берётся из локального конфига (он описывает поведение
   «что делать с тем, что ни одно правило не покрыло»), пресетовский
   `default_action` игнорируется — это не политика, а hint.

**Кэш пресета:**
- OpenWRT: `/var/lib/xr-proxy/presets/<name>.json` (та же схема файлов, что
  у хаба).
- Android: файл в `context.filesDir / "presets" / "<name>.json"`. Простые
  байты, никакой SharedPreferences (они не для JSON).

**Обновление.** Две параллельные стратегии, обе — простые HTTP GET без
long-lived соединений:

1. **Forced fetch при `VpnEngine::start()` / старте `xr-client`.** Перед
   компиляцией `Router` клиент синхронно (с коротким timeout ~2 сек) делает
   `GET /api/v1/presets` + при необходимости `GET /api/v1/presets/:name`,
   сравнивает версию, обновляет кэш. Это обеспечивает свежесть ровно в тот
   момент, когда пользователь активно подключается — типичный сценарий.
   Если сеть не отвечает в отведённый timeout — используется локальный кэш,
   WARN в лог, старт не блокируется.
2. **Фоновая sanity-check-задача раз в `refresh_interval_secs` (дефолт 5
   мин).** На долгоживущих сессиях (роутер не перезагружается сутками)
   раз в 5 минут делает `GET /api/v1/presets` с `If-None-Match`; при 304
   никаких действий, при свежей версии — скачивание и атомарная подмена
   кэша. Применение новых правил — только при следующем рестарте движка
   (см. §5.9), сама задача никак не трогает живой `Router`.

Подпись. Если `trusted_public_key` задан и верификация не прошла — новая
версия отклоняется, пишется WARN, используется старый кэш. Это правило
одинаково для обеих стратегий.

Первое скачивание. При старте, если кэша нет вообще — ждём forced fetch
(timeout до 5 сек). Если и он провалился — движок стартует с пустым
пресетом и только локальными override'ами; пользователь увидит в логе
WARN «preset russia unavailable, running overrides-only».

Для Android эта логика живёт в `xr-core` (новый модуль `presets.rs`,
использует `reqwest` с фичей `rustls-tls`) и дергается из `VpnEngine` при
старте. Через `xr-android-jni` ничего нового не надо — конфиг клиента
парсится там же, а реальные HTTP-запросы делает Rust.

### 3.11 Версионирование API

- Единственная версия `/api/v1` на старте. Ломающие изменения → `/api/v2`,
  `/api/v1` продолжает работать до следующего мажорного релиза клиентов.
- В summary-эндпоинте возвращается заголовок `X-Hub-Version: <cargo_pkg_version>`
  — клиент пишет это в свои логи для диагностики «какая версия хаба
  раздаёт нам пресет».

---

## 4. Изменения в коде

| Файл | Что меняется |
|---|---|
| `Cargo.toml` (workspace) | Добавить член `xr-hub`. |
| `xr-hub/Cargo.toml` | Новый крейт. Deps: axum, axum-server (`tls-rustls`), tower, tower-http (`cors`, `trace`), tokio, serde, serde_json, toml, rust-embed, tracing, tracing-subscriber, ed25519-dalek, rand, base64, anyhow, thiserror, reqwest (dev). Feature `dev-ui`. |
| `xr-hub/src/main.rs` | Парсинг CLI `--config`, загрузка `HubConfig`, чтение `data_dir`, hydrate `AppState`, запуск `axum_server::bind_rustls(...).serve(router.into_make_service())`. |
| `xr-hub/src/config.rs` | `HubConfig` + `TlsConfig` + `AdminConfig` + `SigningConfig` + `InvitesConfig`. Все `Deserialize`. |
| `xr-hub/src/storage.rs` | `load_all_presets`, `save_preset`, `delete_preset`, `load_all_invites`, `save_invite`, `delete_invite`. Атомарная запись через `tempfile::NamedTempFile::persist`. |
| `xr-hub/src/state.rs` | `AppState { presets: RwLock<HashMap>, invites: RwLock<HashMap>, config, signing }` + `hydrate_from_disk`. |
| `xr-hub/src/signing.rs` | `SigningContext { keypair }`, `sign_preset(preset) -> String`, `verify_preset(preset, pubkey) -> bool`. Канонизация в отдельной функции с тестами. |
| `xr-hub/src/api/mod.rs` | `router() -> Router<Arc<AppState>>` — собирает `/api/v1/*`, SPA через `embed::spa_service()`, CORS, middleware логирования. |
| `xr-hub/src/api/presets.rs` | Публичные `list`, `get`; admin `create`, `update`, `delete`. |
| `xr-hub/src/api/invites.rs` | Публичный `get_by_token` (включая consume); admin `list`, `create`, `revoke`. |
| `xr-hub/src/api/auth.rs` | Middleware `require_admin_token`, константное сравнение через `subtle::ConstantTimeEq`. |
| `xr-hub/src/embed.rs` | `rust-embed` для релиза, `ServeDir` для dev. |
| `xr-hub/build.rs` | Проверка наличия `admin-ui/dist/index.html` в релизной сборке без фичи `dev-ui`. |
| `xr-hub/admin-ui/` | Полный Vue-проект (см. §3.4). `npm ci && npm run build` → `dist/`. |
| [xr-proto/src/config.rs](../../xr-proto/src/config.rs) | Добавить `HubClientConfig` (url, trusted_public_key, preset, refresh_interval_secs); сделать `routing` полями override-а. Никакого breaking change — поля опциональные, старые конфиги работают. |
| `xr-proto/src/preset.rs` (новый) | `Preset`, `PresetSummary`, `Invite`, `InvitePayload`. `#[cfg_attr(feature = "ts-rs", derive(TS))]`. |
| `xr-proto/src/routing.rs` | Добавить `Router::from_merged(overrides, preset)` — создание компилированного роутера поверх двух источников. Существующий конструктор оставить. |
| `xr-proto/Cargo.toml` | Новая optional feature `ts-rs`. |
| `xr-core/src/presets.rs` (новый) | `PresetCache` с методами `load_from_disk`, `fetch_if_stale`, `merged_router_config`. Использует `reqwest` с `rustls-tls` (уже в дереве). |
| [xr-core/src/engine.rs](../../xr-core/src/engine.rs) | При `VpnEngine::start` — если `hub` задан, инициализировать `PresetCache` и слить с `routing` override'ами перед построением роутера. |
| [xr-client/src/main.rs](../../xr-client/src/main.rs) | Аналогично: при старте подгрузить пресет, запустить фоновую tokio-задачу обновления раз в `refresh_interval_secs`. |
| [configs/client.toml](../../configs/client.toml) | Обновить пример с новой секцией `[hub]` + пояснение семантики override'ов. |
| `configs/hub.toml` (новый) | Пример конфига `xr-hub`. |
| `docs/HUB-DEPLOY.md` (новый) | Пошаговая установка на VPS: systemd unit, каталоги, права, сертификаты, первая генерация admin_token, первый заход в UI. |
| [docs/ARCHITECTURE.md](../ARCHITECTURE.md) | После имплементации — обновить §2, §3, §4, §5.3, §6.2: убрать «planned», вписать фактические интерфейсы. |

---

## 5. Риски и edge-кейсы

1. **Канонизация JSON для подписи.** Если сериализатор изменит порядок
   ключей между версиями зависимостей, подписи «потухнут» задним числом.
   Митигация: единая функция `canonical_json(&Preset) -> Vec<u8>`, тест
   с golden-файлом `tests/golden/preset_russia.json` + известной подписью,
   регрессия ловится на `cargo test`.
2. **Гонка одноразового инвайта.** Два клиента одновременно читают
   `/api/v1/invite/<token>`. Решение: write-lock на `invites`, проверка
   `consumed_at.is_none()`, установка consumed, fsync, release. Второй
   получает 410.
3. **Рассинхрон клиента с хабом при смене подписи.** Если админ сменил
   `signing_key`, все клиенты с прошитым старым pubkey начнут отклонять
   обновления. Решение: в первой версии не ротируем ключ, ротация — в
   отдельной LLD. Документация явно это фиксирует.
4. **TLS неправильно настроен → клиенты не могут подключиться.** Решение:
   `xr-hub` при старте делает self-check — пытается bind и HTTP-пинг
   самому себе по TLS, в случае ошибки даёт понятное сообщение. Отдельный
   флаг `--check-config` для dry-run.
5. **Слишком большой пресет.** Лимиты: до 10 000 правил, до 100 КБ
   JSON-файла. Превышение → 413 в API, ошибка валидации в UI.
6. **Локальное время на клиенте отстаёт.** Клиент сверяет `expires_at`
   инвайта с серверным временем, не своим. В `InvitePayload` кладём
   `server_time` (текущее время сервера на момент выдачи), клиент сравнивает
   относительно этого, а не своего `SystemTime::now`.
7. **Admin UI утечка токена через reflected XSS.** PrimeVue-компоненты
   экранируют текст, собственных `v-html` не используем. CSP-заголовок
   в ответе SPA: `default-src 'self'; connect-src 'self'; img-src 'self' data:`.
8. **`allowed_origins` пустой.** Тогда CORS блокирует всё, включая Admin
   UI на том же хосте. Решение: если SPA отдаётся с того же origin, CORS
   не нужен вообще — `fetch` идёт на тот же host. Проверка в первой
   реализации.
9. **Обновление пресета во время активного соединения.** Клиент просто
   не применяет новые правила на лету — они вступают в силу при следующем
   старте/рестарте `VpnEngine`. Это сознательное упрощение.
10. **`build.rs` без `dist/`.** Если разработчик делает `cargo build -p xr-hub`
    без `npm run build`, мы даём ошибку — но в dev-режиме (`--features dev-ui`)
    проверку пропускаем. Ошибка содержит точную команду для починки.

---

## 6. План проверки

Ручная:

1. **Сборка релиза.** `cd xr-hub/admin-ui && npm ci && npm run build && cd ../.. && cargo build --release -p xr-hub`. Должен получиться один бинарь, без warnings.
2. **Пустой запуск.** `xr-hub --config configs/hub.toml` на свежем каталоге
   `/var/lib/xr-hub/`. Сервис стартует, `GET /api/v1/presets` → `[]`.
3. **Admin login.** Открыть `https://<host>/`, ввести токен, попасть на
   `/presets`.
4. **Создание пресета.** New → name=russia, добавить 2 правила (proxy
   youtube, direct github.corp), Save. На диске появился
   `/var/lib/xr-hub/presets/russia.json` c version=1, updated_at.
5. **Обновление пресета.** Редактировать, добавить правило, Save. Version=2,
   updated_at свежее.
6. **Подпись.** Подать `[signing] private_key = ...` в конфиге, перезапустить.
   Re-save пресет, файл содержит `signature`. `GET /api/v1/public-key`
   возвращает тот же ключ, что на диске.
7. **Инвайт (одноразовый).** Создать через Admin UI. `curl https://<host>/api/v1/invite/<token>`
   → 200 + JSON. Второй `curl` → 410.
8. **Инвайт (не одноразовый).** `one_time=false`, два curl'а → оба 200. TTL
   истёк → 410.
9. **Клиент подхватывает пресет.** Конфиг OpenWRT-клиента дополнен `[hub]`
   + `preset = "russia"`. Запустить `xr-client`, в логах видно «preset
   russia fetched, version 2». Traffic на youtube идёт через VPS, на
   github.corp — напрямую (проверяется через override в client.toml).
10. **Клиент без хаба.** `hub.url` не задан — поведение как сейчас, ничего
    не ломается.
11. **Клиент с хабом, но хаб выключен.** Запустить клиент, подождать, убить
    `xr-hub`. Клиент продолжает работать с кэшом, пишет INFO про недоступность.
12. **Неверная подпись.** Вручную испортить `signature` в
    `/var/lib/xr-hub/presets/russia.json`, перезапустить клиент → WARN
    «signature mismatch», используется старый кэш.
13. **Warnings.** `cargo test --workspace` — без warnings. `npm run build`
    — без warnings TypeScript.

---

## 7. Вне скоупа

- **Автоматический выпуск TLS** (ACME/Let's Encrypt) — отдельный LLD.
- **Ротация `signing_key`** — отдельный LLD, требует multi-key поддержки
  на клиенте.
- **Управление пользователями и роли** — отдельный LLD, в первой версии
  один админ с Bearer-token.
- **Публичный реестр хабов / федерация** — вне проекта.
- **Rate limits и DoS-защита** — добавится, когда хаб станет публичным.
- **Drag-n-drop правил, продвинутый редактор с валидацией CIDR/доменов** —
  в `RulesEditor` делается минимум, полный редактор в LLD-05.
- **QR/URI-обёртка для инвайта, UX onboarding'а на клиенте** — LLD-04.
- **Админ-страница со списком подключённых клиентов / телеметрия** — вне
  скоупа, нет механизма собирать.
- **Миграция старых хардкод-пресетов из `VpnViewModel`** — разовая задача
  на этапе реализации: скопировать Kotlin-константу в `russia.json` и
  зафиксировать как первую версию пресета. Не требует отдельного LLD.
- **Push-обновления пресета (SSE).** Сознательно отложены. Текущая
  гибридная модель «forced fetch на Connect + фоновый poll с ETag раз в
  5 минут» даёт задержку применения 0 при активном подключении и < 5 минут
  на долгоживущих сессиях, без long-lived соединений и broadcast-каналов
  на сервере. Возвращаемся к вопросу, если появится требование «секундной»
  доставки или клиентов станет настолько много, что суммарный poll-трафик
  окажется заметным.
