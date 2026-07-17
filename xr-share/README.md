# xr-share: file-sharing agent (LLD-19)

Shares **any number of paths** (folders *and* individual files) over HTTP(S):
it serves a signed-hash **manifest** and verifies hub-minted **access tokens
offline**. Shares are **read by default**; a folder marked writable also accepts
uploads and deletes from invite holders who carry a write-binding (LLD-28). The
hub is only an index and a notary. It knows agent addresses and signs access
tokens, but **file bytes never pass through it** (legal cleanliness).

## How it works

Three roles. The **hub** is a phone book + notary. The **agent** (this binary)
holds your files and checks access tokens itself. The **consumer** pulls files
straight from the agent.

```mermaid
flowchart LR
    subgraph Owner["Your machine"]
        AG["xr-share agent<br/>serves N paths<br/>(folders and files)"]
    end
    subgraph HubBox["Hub (xr-hub)"]
        HK["ed25519 key:<br/>signs tokens"]
        IDX["share index:<br/>agent address and key<br/>(NOT the bytes)"]
    end
    CON["Consumer<br/>(Android app)"]

    AG -->|"1 registers a share"| IDX
    HK -->|"2 signs an access token"| AG
    AG -->|"3 share link with token"| CON
    CON ==>|"4 token and file request, direct"| AG
    AG ==>|"5 serves the bytes"| CON

    style HubBox fill:#eef,stroke:#88a
    style CON fill:#efe,stroke:#8a8
```

The bold arrows (4 and 5) are the file transfer: **agent ↔ consumer, bypassing the
hub**. A token is a hub-signed note *"access to share X until time T"*; the agent
verifies its signature with the hub key it pinned at install, **offline**, never
calling the hub. Revocation is the token's TTL.

Full design + sequence diagrams: [docs/lld/19-file-sharing-agent.md](../docs/lld/19-file-sharing-agent.md).

## Install

One command on any OS: downloads the binary from the hub, verifies its SHA-256,
installs the autostart service (systemd / Scheduled Task / launchd) with a
long-lived hub mandate, and shares a folder right away. Take a **setup token**
in the hub admin (**Shares** tab) and run as root/Administrator:

```sh
# Linux / macOS
curl -fsSL https://xr-hub.zoobr.top/share/install.sh | sudo sh -s -- \
  --setup <SETUP-TOKEN> --dir /srv/share
```
```powershell
# Windows (elevated PowerShell)
$env:XR_SETUP="<SETUP-TOKEN>"; $env:XR_DIR="C:\share"
irm https://xr-hub.zoobr.top/share/install.ps1 | iex
```

The setup token packs a registration token and an invite (XR-127): the share
gets attached to the invite, and the relay leg turns on by itself when the
mandate carries a relay descriptor, so a share behind NAT just works
(`--no-relay` opts a public-IP host out). With a plain **reg token**
(`--token <REG-TOKEN>` / `$env:XR_TOKEN`) the same line installs the mandated
service only; share any path anytime after:

```sh
sudo xr-share share /srv/photos              # a folder OR a single file
sudo xr-share share /srv/dropbox --writable  # invite holders can upload/delete (folders only)
sudo xr-share list
```

`--writable` opts a folder into the write path and adds a write-binding on the
attached invite; re-running `share` without the flag turns write back off. From a
laptop the desktop harness mirrors `pull` for sending:

```sh
xr-share push --invite <TOKEN> --share <id|name> report.pdf   # upload (--to <rel> to rename)
xr-share rm   --invite <TOKEN> --share <id|name> report.pdf   # delete
```

`push` refuses locally if the invite grants no write access, and on overwrite
sends `If-Match` with the hash it just read, so it cannot silently clobber a
newer version (`--force` drops that guard).

Run the installer with no token at all to just fetch or update the binary; an
already-installed service is restarted with the new one.

Re-running the installer keeps the existing agent: `install` looks for the
config at the requested path, then at the path recorded in the autostart
service, then at the OS default location, and reuses its identity, shares and
mandate (a fresh `--setup` only re-points the default invite). A fresh identity
would orphan every share registered under the old one on the hub (XR-134), so
it is minted only when no config is found anywhere (with a warning if service
traces remain) or on `xr-share install --force`, which also takes the previous
shares off the hub index.

> Self-hosting the hub? Point the installer elsewhere with
> `XR_SHARE_BASE=https://your-hub/share`.

> The distributed binary serves **plain HTTP** (run behind a TLS terminator, or
> direct in a trusted circle). Direct HTTPS termination by the agent is an
> opt-in source build, `cargo build --release -p xr-share --features tls`
> (Linux only; its crypto backend doesn't cross-compile to Windows).

## Endpoints

The share id is in the URL (`GET /{share_id}/manifest`, `GET /{share_id}/file/...`);
the bare `/manifest` and `/file/...` are legacy aliases that select the share from
the token. The write routes are v2 only.

| Method / path                  | Scope         | Purpose                                            |
|--------------------------------|---------------|----------------------------------------------------|
| `GET /healthz`                 | none          | liveness                                           |
| `GET /{id}/manifest`           | `share:read`  | listing: `path`, `size`, `mtime`, `sha256`         |
| `GET /{id}/file/{*path}`       | `share:read`  | file bytes; supports `Range` (resume)              |
| `PUT /{id}/file/{*path}`       | `share:write` | upload a file; `201` new, `204` overwrite          |
| `DELETE /{id}/file/{*path}`    | `share:write` | remove a file; `204`, `404` missing, `409` a dir   |

Token is presented as a URL-safe base64 blob of the hub's `ShareToken` JSON, via
`Authorization: Bearer <blob>`, `X-Share-Token: <blob>`, or `?token=<blob>`
(best-effort for browsers). Verified offline against the pinned hub key (bound
`share_id`, not expired, valid signature, and carrying the route's scope);
otherwise `401` (no/garbled token) or `403` (wrong share, expired, bad signature,
or missing scope). Tokens are never logged.

### Scope model (LLD-28)

The token carries an OAuth-style `scope` string inside its signed bytes: today
`share:read` and `share:write`. Read routes need `share:read`, write routes
`share:write`; a grant with write access gets both names. Write scope is minted
by a single path only, `GET /api/v1/invite/{token}/shares` for an invite that has
a **write-binding** to a **writable** share; the share link and `/share/mint`
always hand out read-only tokens. A holder reads its own rights by decoding the
grant's token blob and looking for `share:write` in `scope`.

### Write path (PUT / DELETE)

The order of gates: the share exists (`404`), the agent config marks it
`writable` (`403`), the token carries `share:write` (`401`/`403`), and the path
resolves inside the share (`403`). Both master switches are the owner's: the hub
never mints `share:write` for a share the owner did not mark writable, and the
agent refuses even a valid `share:write` token unless its own config allows the
share, so a compromised hub still cannot write.

An upload streams into a reserved `.xr-part-<rand>` temp next to the target,
hashing on the fly, then `fsync` + atomic rename over the target, so a
half-written file never appears in the manifest or under the target name. The
`.xr-part-` prefix is reserved: no request path (including `GET`) may name a
component with it, and the manifest walk skips such files.

Optional headers:

- `X-Xr-Sha256: <hex>` on `PUT` verifies the received bytes before the rename;
  a mismatch is `422` and the target is untouched.
- `If-Match: <sha256>` runs the operation only if the target's current content
  hash equals that value (optimistic concurrency against a lost update); `PUT`
  also honours `If-None-Match: *` to require the target not to exist. A violated
  precondition is `412`, target untouched. Without these the default is
  last-write-wins on atomic operations.
- `max_file_mb` in the agent config caps an upload: over the cap is `413` (by
  `Content-Length` up front, else while streaming). A full disk is `507`; the
  temp is removed on any failure.

The manifest response is signed with the agent's identity key (XR-046): the
`x-xr-manifest-sig` / `x-xr-manifest-signed-at` headers carry an ed25519
signature over the exact body bytes plus the share id, and consumers verify it
against the `agent_pubkey` pinned from the grant. Without the identity key
(config `identity_key` or `identity.key` next to the config) the agent serves
unsigned and pinning consumers refuse the listing.

## Manual setup (no installer)

```sh
# 1. Generate the agent identity (once). Register the printed PUBLIC key in the
#    hub as the share's agent_pubkey (the consumer pins it, TOFU).
xr-share keygen

# 2. Register the share in the hub (Admin UI → Shares, or POST /admin/shares)
#    using addr:port + that public key; copy the returned share_id.

# 3. Fetch the hub's signing key (pin it): GET https://<hub>/api/v1/public-key

# 4. Fill /etc/xr-share/config.toml (see configs/share.toml), then run:
xr-share -c /etc/xr-share/config.toml
```

Direct access needs a public IP or a forwarded port. Behind NAT the relay leg
(LLD-23) carries the share instead: token installs pick the relay descriptor up
from the hub automatically, hand-rolled setups add a `[relay]` block.

## Build

Pure Rust, no platform-specific code in the binary, so it builds for Linux and
Windows alike.

```sh
# Linux (static musl)
cargo build --release -p xr-share --target x86_64-unknown-linux-musl

# Windows
cargo build --release -p xr-share --target x86_64-pc-windows-gnu
```

Release binaries in the hub's share-dist are built with `--features relay`
(the CI relay guard refuses a binary without it, XR-133); add the flag to a
source build if the share must work behind NAT.

## Autostart

`sudo xr-share service install` covers every OS: a systemd unit on Linux, a
SYSTEM Scheduled Task on Windows, a LaunchDaemon on macOS (XR-127);
`service status` / `service uninstall` to inspect and remove. The install
one-liner already did this for you. For a hand-rolled Linux setup there is
also [`deploy/xr-share.service`](../deploy/xr-share.service) to drop into
`/etc/systemd/system/` and `systemctl enable --now xr-share`.
