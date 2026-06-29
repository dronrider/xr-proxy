# xr-proxy: сделано

Append-only журнал закрытых задач, растёт свободно. Фундаментальные LLD первого пакета
(шаги 1-9 в [ARCHITECTURE.md §9](ARCHITECTURE.md): 01-02, 04, 06, 09) велись до этой
доски и зафиксированы в §9 и git-истории, сюда не дублируются. Дата закрытия и коммит
взяты из git log.

| ID | Задача | Тип | P | Закрыто | Ссылка |
|--------|--------|-----|---|---------|--------|
| XR-001 | Fail-open на роутере при мёртвом VPS (circuit-breaker в MuxPool) | bug | P0 | 2026-06-12 | `6888d79`, `local-docs/problems.md` (1a) |
| XR-002 | Обрыв скачивания на Direct-пути (потеря хвоста чанка на partial send) | bug | P1 | 2026-06-12 | `72975ca`, `local-docs/problems.md` (2) |
| XR-003 | Geo-blocked сайты через роутер: block_quic плюс пресет geo-blocked | bug | P1 | 2026-06-13 | `ff06399`, `local-docs/problems.md` (5a) |
| XR-004 | xr-hub reset-password одной командой по SSH | task | P1 | 2026-06-13 | `35cbc39`, `local-docs/problems.md` (9) |
| XR-005 | Re-bind туннеля при смене сети LTE и Wi-Fi | bug | P1 | 2026-06-13 | `3840f6c`, `local-docs/problems.md` (3b-1) |
| XR-006 | Direct виснет на IPv6/NAT64 LTE: MSS-кламп плюс TUN MTU 1280 | bug | P1 | 2026-06-13 | `aa90a12`, `local-docs/problems.md` (3c) |
| XR-008 | Logs UX: sticky toolbar, поиск, auto-follow, скачивание через SAF | LLD | P1 | 2026-06-14 | [docs/lld/03-android-logs-ux.md](lld/03-android-logs-ux.md), `828ec86` |
| XR-011 | Android: мультисерверная модель, CRUD, переключатель серверов | LLD | P2 | 2026-04-17 | [docs/lld/08-android-multi-server.md](lld/08-android-multi-server.md), `f02fa36` |
| XR-014 | Самообновление APK (подписанный манифест плюс PackageInstaller) | LLD | P2 | 2026-06-19 | [docs/lld/12-android-apk-self-update.md](lld/12-android-apk-self-update.md), `b9fa3b7` |
| XR-024 | Авто-проверка обновлений: баннер всплывает сам на холодном старте (ретрай с бэкоффом, не штамповать дедуп на провале) | bug | P2 | 2026-06-23 | [docs/lld/12-android-apk-self-update.md](lld/12-android-apk-self-update.md), `d57ea6c` |
| XR-007 | Доверенные Wi-Fi: авто-пауза по SSID (пауза на доверенной, резюм при уходе) | task | P1 | 2026-06-23 | [docs/lld/15-android-trusted-networks.md](lld/15-android-trusted-networks.md), `820da83` |
| XR-023 | Возврат в доверенную Wi-Fi в фоне ставит паузу (location FGS для чтения SSID в фоне) | bug | P1 | 2026-06-23 | [docs/lld/15-android-trusted-networks.md](lld/15-android-trusted-networks.md), `81f1c8d` |
| XR-021 | Авто-пауза встаёт в Doze при пробуждении экрана (SSID через location-info колбэк) | bug | P1 | 2026-06-23 | [docs/lld/15-android-trusted-networks.md](lld/15-android-trusted-networks.md), `15c6d53` |
| XR-022 | Баннер «в сети есть ограничения» больше не мелькает при авто-паузе (дебаунс пробы) | bug | P2 | 2026-06-23 | [docs/lld/15-android-trusted-networks.md](lld/15-android-trusted-networks.md), `fc8b6e8` |
| XR-027 | Файлообмен: агент-шара + хаб-индекс адресов, скачивание и one-way sync на Android (v1, перекрыт v2) | LLD | P3 | 2026-06-28 | [docs/lld/19-file-sharing-agent.md](lld/19-file-sharing-agent.md), `68ff2cf` |
| XR-028 | Дистрибуция xr-share: install.sh/install.ps1 (one-liner с хаба) + init/service сабкоманды | task | P2 | 2026-06-28 | `1bdf46d` |
| XR-029 | Универсальный мультишаринг xr-share (v2): шара это любой путь, неогр. шар, agent-credential, CLI share/list/unshare, install без папки | LLD | P2 | 2026-06-28 | [docs/lld/19-file-sharing-agent.md](lld/19-file-sharing-agent.md) (§9), `4ee2370` |
| XR-031 | Доступ к шарам через инвайт: привязка шар к инвайтам, выбор галочками (selection), приём на устройстве | LLD | P2 | 2026-06-28 | [docs/lld/19-file-sharing-agent.md](lld/19-file-sharing-agent.md) (§9), `ace9e21` |
| XR-037 | xr-share install не затирает конфиг (--force для чистой переустановки) + манифест в spawn_blocking (агент не виснет на холодном кеше) | bug | P1 | 2026-06-28 | `fa8f174`, `8860226` |
| XR-039 | Расцепить листинг и хеширование (агентная часть): мгновенный листинг без хеширования, хеши лениво через прогрев | LLD | P1 | 2026-06-28 | `9912d26` |
| XR-053 | Офлайн-просмотр скачанных файлов шары: при недоступном агенте проводник строится из локальных файлов, открытие локальное | bug | P2 | 2026-06-29 | `d703adf`, [ui/files/FilesViewModel.kt](../xr-android/app/src/main/java/com/xrproxy/app/ui/files/FilesViewModel.kt) |
