# LLD-09 — Multi-mux pool: параллельные туннели для устранения HoL

**Статус:** Implemented
**Область:** `xr-proto::mux_pool`, `xr-proto::mux`, использование пула в `xr-client::handle_connection` и `xr-core::session`
**Связанные документы:** [ARCHITECTURE.md §3 (mux-протокол)](../ARCHITECTURE.md), [CLAUDE.md](../../CLAUDE.md)

Расширяем `MuxPool` с одного TCP-туннеля до N (4-8) параллельных
туннелей с балансировкой стримов между ними. Это устраняет
**Head-of-Line blocking** одного TCP — главный bottleneck, из-за
которого первый ролик YouTube открывается 5-7 секунд на быстром
клиенте и до 20 секунд на Android, и любой retransmit одного потока
тормозит все остальные.

---

## 1. Текущее состояние и доказанная регрессия

### 1.1 Что мы видим у пользователя

- Скачивание Cloudflare-CDN (`updates.signal.org` 30 МБ): 6 МБ/с
  стабильно — это уже после фикса буферов (см. §2). Канал не упирается.
- TLS-handshake к новому домену через прокси: 3-7 секунд на Mac,
  до 20 на Android.
- В логах xr-client под нагрузкой нет `mux stream channel full`,
  нет `Tunnel to ... failed`. Сетевой тракт здоровый.
- На VPS под нагрузкой `ss -tinp` показывает на mux-TCP:
  `cwnd:10, ssthresh:64076` после потери пакета — окно сжимается и
  не возвращается к до-инцидентному значению при cubic.

### 1.2 Архитектура `MuxPool` сейчас

[xr-proto/src/mux_pool.rs:28-32](../../xr-proto/src/mux_pool.rs#L28-L32):

```rust
pub struct MuxPool {
    connect_fn: ConnectFn,
    codec: Codec,
    current: Mutex<Option<Arc<Multiplexer>>>,  // ← один TCP на всех
}
```

`open_stream(target)` всегда идёт через тот же `Arc<Multiplexer>`,
пока он жив. То есть:

- **Все** клиенты LAN (Mac, Android, IoT) делят **один** TCP к VPS:8443.
- **Все** логические стримы пишут в **один** `writer_tx`
  (`WRITER_CHANNEL_SIZE=2048`) → encode → один TCP socket.

### 1.3 Почему это медленно (HoL)

Один TCP — один cwnd, один ordered byte stream. Любая потеря пакета:
1. Все frames всех стримов **за** потерянным сегментом ждут retransmit.
2. cubic: cwnd /= 2 → throughput всех стримов проседает.
3. Slow-start после long pause: cwnd начинается с 10 → медленный
   набор пропускной способности.

При просмотре YouTube запрос на чанк видео = новый mux-stream через
тот же TCP. Если TCP сейчас в восстановлении после потери, новый
stream ждёт. У нескольких клиентов параллельно — все ждут одного.

### 1.4 Что уже сделано (контекст для нового агента)

| Коммит | Эффект | Лимит |
|---|---|---|
| `104c268` mux split + буферы 1024/2048 | Убрал `channel full` при CDN-burst | Не увеличивает throughput одного TCP |
| `dde442b` ошибки LAN/Tunnel | Чистый лог, нет ложных fallback=Direct | Косметика, не bandwidth |
| `3a56e89` LAN-only catch-all + port-based override | Telegram-Android и Telegram-Desktop работают через MTProto на нестандартных портах | Не трогает mux pool |
| BBR + `tcp_rmem/wmem=8M` на роутере и VPS (sysctl.d) | 3 МБ/с → 6 МБ/с, throughput стабилизирован | Один TCP остался HoL-узким |

После всех этих фиксов **остался** ровно один симптом — медленный
старт нового стрима. Только multi-mux его решит.

---

## 2. Целевое поведение

### 2.1 Архитектура pool

```
┌─────────────────────────────────────┐
│ MuxPool                             │
│  ┌──────────┐   ┌──────────┐  ...   │
│  │ Mux #0   │   │ Mux #1   │       │
│  │ TCP:8443 │   │ TCP:8443 │       │
│  └──────────┘   └──────────┘       │
└─────────────────────────────────────┘
       ↑                ↑
   stream A         stream B
   (round-robin or least-loaded)
```

- Постоянно держим N (default = 4, конфигурируется) активных Multiplexer'ов.
- Каждый — отдельный TCP к VPS:8443 со своим MuxInit handshake.
- `open_stream(target)` выбирает один из живых mux'ов по политике
  балансировки (см. §2.3).

### 2.2 Failover

- Если выбранный mux умер (alive=false) на момент `open_stream` —
  выбираем следующий живой.
- Параллельно фоновая задача поднимает замену для умершего слота.
- Если все mux мертвы — попытка connect синхронно для одного слота,
  чтобы не блокировать запрос навсегда.

### 2.3 Балансировка

**Вариант A — round-robin по индексу.** Простой, predictable.
Минус: если один mux попал в slow-start, нагрузка на нём не упадёт
быстрее.

**Вариант B — least-loaded (по числу активных стримов).**
Каждый Multiplexer трекает счётчик активных стримов; новый стрим
идёт в mux с минимальным счётчиком. Минус: lock-contention.

**Рекомендация:** начать с round-robin (атомарный счётчик), посмотреть
профиль нагрузки. Least-loaded имеет смысл если distribution стримов
сильно асимметрична.

### 2.4 Конфигурация

В `[client]` (router) и `[android]` (Android-движок) добавить:

```toml
[client]
mux_pool_size = 4   # default
```

Поле опциональное, default=4. На малых сетях можно ставить 2.

---

## 3. Изменения в коде

### 3.1 `xr-proto/src/mux_pool.rs`

```rust
pub struct MuxPool {
    connect_fn: ConnectFn,
    codec: Codec,
    slots: Vec<Mutex<Option<Arc<Multiplexer>>>>,
    next: AtomicUsize,
}

impl MuxPool {
    pub fn new(connect_fn: ConnectFn, codec: Codec, size: usize) -> Arc<Self> { ... }

    pub async fn open_stream(&self, target: &TargetAddr) -> io::Result<MuxStream> {
        // try slots starting from `next % size`, advance on each call (RR)
        // for each candidate slot:
        //   - if slot has alive mux → mux_open_stream()
        //   - else: take lock, reconnect, retry
        // if all slots fail → return last error
    }

    pub async fn warmup(&self) -> io::Result<()> {
        // открываем все N mux'ов параллельно (FuturesUnordered)
    }
}
```

Тесты:
- `test_pool_size_zero_uses_default` — не паникуем при mux_pool_size=0.
- `test_round_robin_distribution` — N=3, открыть 9 стримов, проверить
  по 3 на каждом mux'е.
- `test_failover_to_next_slot` — mark slot #0 dead, open_stream
  идёт через slot #1 без задержки.

### 3.2 `xr-client::ProxyState` и `xr-core::SessionContext`

Добавить чтение `mux_pool_size` из конфига, передать в `MuxPool::new`.
Default — 4, если не указано.

### 3.3 Никаких изменений wire-протокола

`MuxInit/MuxInitAck/Connect/...` остаются. Сервер не различает,
сколько TCP подключений у одного клиента — для него это просто
несколько mux-сессий.

---

## 4. Ожидаемый эффект

### 4.1 TLS-handshake нового стрима

При N=4: с вероятностью 75% попадаем на TCP, который **не** сейчас
в slow-start или recovery. Среднее время handshake падает примерно
в N раз для случая «один мух в просадке».

### 4.2 Параллельные тяжёлые загрузки

Cwnd параллелится на N TCP. Aggregate throughput может вырасти, но
главное — **stability**: один большой стрим не убивает первый-байт-
latency маленьких параллельных запросов.

### 4.3 Failover

Сейчас разрыв единственного TCP = пауза для всех. После: один из 4
обрывается → новые стримы идут через 3 живых, агент чинит обрыв.

---

## 5. Метрики для подтверждения

До и после внедрения:

1. **TTFB нового стрима под нагрузкой**
   - С роутера или MacBook: `curl --max-time 30 -w "%{time_starttransfer}"`
     на новый домен через прокси.
   - 1 stream / 10 streams параллельно / 50 параллельно.
   - До: TTFB ≥ 5 сек на Mac. После: ожидаем ≤ 1.5 сек.

2. **Bandwidth aggregate**
   - 4 параллельных download'а файла 30МБ.
   - До: ~6 МБ/с total (упирается в один TCP). После: ожидаем 15-20 МБ/с total.

3. **`ss -tinp` на VPS под нагрузкой**
   - До: один TCP с cwnd скачет 10 ↔ 50.
   - После: 4 TCP, cwnd распределён.

---

## 6. Риски

- **Рост числа TCP-сессий на VPS** — с N=4 и K клиентов = 4×K активных
  TCP. На VPS лимиты ulimit/conntrack должны выдержать (256
  max_connections в `[server.limits]` стоит увеличить пропорционально).
- **TLS fingerprint** — больше параллельных TLS-handshake к VPS может
  быть сильнее заметно ТСПУ. Mitigation: `MUX_MAX_LIFETIME` уже
  ротирует mux каждые 4ч, можно сократить до 1ч.
- **Memory** — каждый Multiplexer держит per-stream channel buffers.
  STREAM_CHANNEL_SIZE=1024×Vec<u8> на стрим. С N=4 базовая
  per-mux память ×4. Для роутера с 256 МБ это 0.x МБ — нерелевантно.

---

## 7. Открытые вопросы

1. Стоит ли делать **affinity** (один LAN-IP всегда на один mux), чтобы
   стримы одного устройства шли через один TCP и не задерживали
   соседей? (Возможно нет — теряется балансировка.)
2. **Per-domain affinity**: все стримы к `*.googlevideo.com` через один
   mux, чтобы upstream-сервер мог использовать одно TCP к origin
   эффективнее? Усложняет код, выгода неочевидна.
3. Ставить ли `mux_pool_size` в `xr-hub` пресете (центральная настройка
   на флот) или только в локальном `client.toml`?

---

## 8. Не входит в этот LLD

- Изменения в xr-server (если только не упрёмся в `max_connections=256`
  на сервере — тогда отдельный коммит).
- Анти-fingerprint обфускация (отдельный LLD при необходимости).
- HTTP/3-стиль stream control (window updates) — пока не нужно.
