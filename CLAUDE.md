# CLAUDE.md

## Project Overview

xr-proxy — lightweight obfuscated proxy for bypassing regional internet blocks. Deployed on OpenWRT routers (`xr-client`) connected to a VPS (`xr-server`). All LAN devices get transparent access to blocked resources without per-device configuration. There is also an Android client (`xr-android`) that uses the same tunnel via the shared `xr-core` engine (via JNI in `xr-android-jni`).

Language: Rust (core / server / OpenWRT client) + Kotlin (Android). All communication in this project is in Russian.

**Полная архитектура — в [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).** Читай этот файл, когда нужна карта компонентов, модулей, протоколов, модели конфигурации или жизненного цикла соединения. Не импортируй его сюда автоматически — он большой. Планы крупных доработок лежат в [docs/lld/](docs/lld/); после реализации факты из LLD переносятся в `ARCHITECTURE.md`.

Остальные разделы этого файла — рабочие правила (build, кросс-компиляция, дизайн-решения по nftables/TPROXY, деплой), которые нужны под рукой в каждом сеансе.

## Build & Test

**IMPORTANT**: Before every commit, run `cargo test --workspace` AND verify zero warnings with `cargo test --workspace 2>&1 | grep "warning:" | grep -v "generated"`. Do NOT commit code with warnings.

```bash
# Run all tests
cargo test --workspace

# Build server (on VPS)
cargo build --release -p xr-server

# Cross-compile client for OpenWRT (requires Docker running)
cross build --release --target aarch64-unknown-linux-musl -p xr-client

# Client with GeoIP support
cross build --release --target aarch64-unknown-linux-musl -p xr-client --features geoip
```

## Cross-Compilation Notes (musl libc)

When targeting `*-unknown-linux-musl`, the `libc` crate does NOT export certain constants. These must be defined manually:
- `SOL_IP` = 0
- `IP_TRANSPARENT` = 19
- `IP_RECVORIGDSTADDR` / `IP_ORIGDSTADDR` = 20
- `SO_ORIGINAL_DST` = 80

`libc::msghdr` on musl has private padding fields (`__pad1`, `__pad2`) — cannot use struct literal syntax. Must use `std::mem::zeroed()` + field-by-field assignment.

Integer types differ across targets (`msg_controllen`, `iov_len`). Use `as _` for portable casting.

## Key Design Decisions

- **nftables `ip` family, not `inet`** — `inet` family conflicts with TPROXY + `ip saddr` in the same rule. Always use `ip` family for TPROXY rules.
- **Older nftables (OpenWRT)** — require explicit `add table`/`add chain`/`add rule` syntax; block syntax (`table { chain { ... } }`) only works for updating existing tables.
- **`meta l4proto udp`** must appear on the same rule as the `tproxy` statement, not on a separate line above.
- **TPROXY source filtering in nftables, not application code** — if the proxy is down, intercepted traffic is blackholed. Filter by source IP in firewall rules so only specific devices (e.g., game consoles) are affected.
- **Response spoofing (UDP relay)** — Switch expects UDP responses from the original server IP, not the router. The client creates per-destination sockets with `IP_TRANSPARENT` + `bind(server_ip:port)` to send spoofed-source responses.
- **Tokio AsyncFd for TPROXY socket** — DO NOT use `UdpSocket::from_std()` + `AsyncFd::new()` on the same fd. It causes `EEXIST` (double reactor registration). Use `AsyncFd` exclusively with raw `recvmsg`/`sendto`.
- **procd respawn** — `respawn 3600 15 0` (threshold=3600s, interval=15s, retry=0=unlimited)
- **Timeouts everywhere** — idle 5min, max lifetime 1h, TCP keepalive 60s. Prevents zombie connection memory leaks.
- **SO_REUSEADDR** on TCP listener — prevents "address already in use" on rapid restart.

## File Locations on Router

```
/usr/bin/xr-client              — binary
/usr/bin/xr-watchdog.sh         — cron watchdog (restart + crash log)
/usr/bin/udp-tproxy-setup.sh    — nftables TPROXY setup (reads config)
/etc/xr-proxy/config.toml       — configuration
/etc/xr-proxy/crash.log         — persistent crash diagnostics
/etc/init.d/xr-proxy            — procd init script
```

## Config Files

- `configs/client.toml` — reference client config with all options documented
- `configs/server.toml` — reference server config
- `configs/routing-russia.toml` — comprehensive routing rules for Russia (domains + CIDR for Telegram)

## Scripts

- `deploy/xr-proxy.init` — procd init: start (TCP + UDP TPROXY setup), stop (cleanup both), respawn
- `deploy/xr-watchdog.sh` — cron every minute: check process, log crash, cleanup rules, restart, set OOM protection
- `scripts/udp-tproxy-setup.sh` — reads source_ips from config, creates nftables TPROXY rules (ip family). Refuses to run with empty source_ips (safety).
- `scripts/udp-tproxy-cleanup.sh` — removes TPROXY rules and policy routes
- `scripts/diagnose.sh` — comprehensive diagnostics (binary, config, process, ports, firewall, connectivity)
- `scripts/generate-key.sh` — generate base64 obfuscation key

## Known Issues / Watch Out For

- `Connection reset by peer` in tunnel logs can mean VPS overloaded or semaphore full (max_connections=256)
- BusyBox crond logs all cron executions as `cron.err` — this is normal, not an actual error
- UDP relay `source_ips` MUST be specified — empty list intercepts ALL LAN UDP and breaks games/VoIP
- `bypass_ips` in client config excludes devices from TCP proxy only (nftables prerouting return)
- init script `stop_service` must clean both `ip xr_proxy` (TCP) and `ip xr_udp_relay` (UDP) tables + policy route
