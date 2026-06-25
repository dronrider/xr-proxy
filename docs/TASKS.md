# xr-proxy: задачи (префикс XR)

Канбан-доска открытой работы и единственный источник правды по тому, что в работе.
Подробные разборы лежат в [docs/lld/](lld/) и в гитигнорнутом `local-docs/`, доска их
только индексирует, описания не дублирует. Готовую задачу из доски переносим в
[docs/TASKS-archive.md](TASKS-archive.md). ID сквозные по репозиторию и не
переиспользуются; ID ставим в subject коммита там, где уместно (`feat(core): XR-9 ...`).
Чувствительное (IP, доступы, инфра) в доску не пишем, оно остаётся в `local-docs/`.

Приоритеты: P0 чинить немедленно, P1 мешает ежедневному использованию, P2 заметно
улучшает, P3 терпит. Типы: `bug` / `task` / `LLD`.

## In progress

Нет.

## Check (готово, ждёт проверки пользователем)

| ID | Задача | Тип | P | Ссылка |
|--------|--------|-----|---|--------|
| XR-027 | Файлообмен: агент-шара + хаб-индекс адресов, скачивание и one-way sync на Android | LLD | P3 | [docs/lld/19-file-sharing-agent.md](lld/19-file-sharing-agent.md) |
| XR-028 | Дистрибуция xr-share: install.sh/install.ps1 (one-liner с хаба) + `init`/`service` сабкоманды | task | P2 | — |
| XR-029 | Универсальный мультишаринг xr-share (v2): шара это любой путь (папка или файл), неограниченно шар на агента, agent-credential, CLI `share`/`list`/`unshare`, `install` без привязки к папке | LLD | P2 | [docs/lld/19-file-sharing-agent.md](lld/19-file-sharing-agent.md) (§9) |

XR-027: весь Rust (xr-proto/xr-hub/xr-share/xr-core/JNI) собран, протестирован,
прогнан вживую (hub→agent→sync). Осталась **проверка пользователем**: сборка APK
на маке (cargo-ndk + Gradle; добавлены deps work-runtime + documentfile) и
device-verify экрана «Файлы» (SAF-папка, токен, разовое скачивание, фоновый
mirror). После устройства — перенести факты xr-share в ARCHITECTURE.md §3 и в архив.

XR-028: **Linux И Windows — В ПРОДЕ на обоих хабах** (Timeweb+Aeza). Проверено
end-to-end: `curl -fsSL https://xr-hub.zoobr.top/share/install.sh | sh` ставит
Linux-бинарь (sha256-сверка), Windows-`.exe` раздаётся с совпадающим хешем
(`irm …/share/install.ps1 | iex`). `xr-share init`/`service` рабочие. Бинари —
статический musl + windows-gnu через **cargo-zigbuild** (свежий rustc + zig);
Windows завёлся после перевода rustls на **ring** (TLS-раздача агента — теперь
опциональная фича `tls`, дефолт HTTP-only; aws-lc не кросс-компилится под Windows).
xr-hub тоже собран статическим musl-zigbuild и разложен на оба VPS (бэкап
`xr-hub.bak-pre-xr028`). Остаётся проверка пользователем: запуск `.exe` на реальной
Windows. CI-workflow (`.github/workflows/release-xr-share.yml`) — для будущих
релизов (нужен секрет HUB_SSH_KEY). ⚠️ `cross`/musl в окружении не годится (старый
Rust → icu/zerofrom; вероятно и `xr-client` cross — проверить до деплоя роутера);
рабочий путь — zig+cargo-zigbuild.

XR-029 (v2 мультишаринг): реализован, протестирован (`cargo test --workspace`
зелёный, 0 warnings) и **выкачен на ОБА хаба** (Timeweb+Aeza, паритет проверен).
Хаб: эндпоинты `share/exchange|add|mint|unshare` (422 на пустое тело = роуты
живые), агент-бинари (musl + windows) и v2-`install.sh` опубликованы в
`share-dist` (хеши сходятся), Admin «команда установки» без привязки к папке.
Агент: шара = любой путь (папка или файл), роутинг по `share_id` плюс
legacy-алиасы по токену, горячий релоад конфига, CLI `install`/`share`/`list`/
`unshare`. Бэкапы на обоих VPS: `xr-hub.bak.<ts>` + `share-dist.bak.<ts>`.
Остаётся проверка пользователем: на реальной машине
`curl …/share/install.sh | sudo sh -s -- --token <reg>` (reg-токен из админки),
затем `xr-share share <путь>` и забор файла по ссылке. После устройства перенести
факты в ARCHITECTURE.md §3 и в архив.

## Backlog

| ID | Задача | Тип | P | Ссылка |
|--------|--------|-----|---|--------|
| XR-009 | Мульти-VPS failover на роутере | LLD | P1 | [docs/lld/10-router-multi-vps-failover.md](lld/10-router-multi-vps-failover.md) |
| XR-010 | Мониторинг, уведомления и панель здоровья | LLD | P1 | [docs/lld/11-monitoring-health-panel.md](lld/11-monitoring-health-panel.md) |
| XR-012 | Информативный индикатор соединения вместо смайлика | task/LLD | P2 | `local-docs/problems.md` (10) |
| XR-013 | Гибридный редактор правил xr-hub плюс Android rules editor | LLD | P2 | [docs/lld/14-hub-hybrid-rules-editor.md](lld/14-hub-hybrid-rules-editor.md), [docs/lld/05-android-rules-editor.md](lld/05-android-rules-editor.md) |
| XR-015 | Zero-touch provisioning VPS и роутера | LLD | P2 | [docs/lld/13-zero-touch-provisioning.md](lld/13-zero-touch-provisioning.md) |
| XR-016 | Per-app split tunneling | LLD | P2 | [docs/lld/07-android-per-app-tunnel.md](lld/07-android-per-app-tunnel.md) |
| XR-020 | Живые правила из хаба для серверов, добавленных вручную | LLD | P2 | [docs/lld/16-manual-server-hub-rules.md](lld/16-manual-server-hub-rules.md) |
| XR-025 | Хаб-реестр роутеров и удалённое управление (pull control-plane) | LLD | P2 | [docs/lld/17-hub-router-registry.md](lld/17-hub-router-registry.md) |
| XR-017 | Клиент под macOS | LLD | P3 | `local-docs/problems.md` (12) |
| XR-018 | Автопополнение правил проксирования из community-фидов | LLD | P3 | `local-docs/problems.md` (13) |
| XR-019 | Браузерное расширение | LLD | P3 | `local-docs/problems.md` (5b) |
| XR-026 | Fleet-метрики и Grafana-дашборды | LLD | P3 | [docs/lld/18-fleet-metrics-grafana.md](lld/18-fleet-metrics-grafana.md) |

## Blocked

Нет.
