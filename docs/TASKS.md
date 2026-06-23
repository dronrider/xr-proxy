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
| XR-007 | Доверенные Wi-Fi: авто-пауза по SSID | task | P1 | [docs/lld/15-android-trusted-networks.md](lld/15-android-trusted-networks.md), `local-docs/problems.md` (3b-2) |
| XR-023 | Возврат в доверенную Wi-Fi в фоне не ставит паузу (location FGS для чтения SSID в фоне) | bug | P1 | [docs/lld/15-android-trusted-networks.md](lld/15-android-trusted-networks.md), `local-docs/xr-021-verify.md`, `81f1c8d` |
| XR-021 | Авто-пауза не встаёт в Doze, пока телефон спит | bug | P1 | [docs/lld/15-android-trusted-networks.md](lld/15-android-trusted-networks.md), `local-docs/xr-021-verify.md`, `15c6d53` |
| XR-022 | Баннер «в сети есть ограничения» проскакивает при авто-паузе, хотя ограничений нет | bug | P2 | [docs/lld/15-android-trusted-networks.md](lld/15-android-trusted-networks.md), `local-docs/xr-021-verify.md`, `fc8b6e8` |

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
| XR-017 | Клиент под macOS | LLD | P3 | `local-docs/problems.md` (12) |
| XR-018 | Автопополнение правил проксирования из community-фидов | LLD | P3 | `local-docs/problems.md` (13) |
| XR-019 | Браузерное расширение | LLD | P3 | `local-docs/problems.md` (5b) |

## Blocked

Нет.
