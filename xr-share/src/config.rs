//! Agent configuration (LLD-19 §2.7, §9.1).
//!
//! v2 makes the agent a **multishare**: it serves an unbounded list of shares,
//! each a `share_id` plus a path that is **either a directory or a single file**.
//! The config pins the **hub's** public key so access tokens are verified offline
//! (the agent never calls the hub at access time), and optionally holds the
//! agent's `agent_credential` + `hub_url` + identity so the `share`/`list`/
//! `unshare` subcommands can talk to the hub on the operator's behalf.
//!
//! The legacy single-share form (`dir` + `share_id` at top level) is still
//! accepted and folded into the share list, so a v1 config keeps working.

use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentConfig {
    /// Listen address, e.g. `0.0.0.0:8443`.
    pub listen: String,
    /// Base64 (standard) ed25519 public key of the hub — pinned. Tokens are
    /// verified against this offline.
    pub hub_pubkey: String,
    /// Hub base URL, used only by the `share`/`list`/`unshare` subcommands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_url: Option<String>,
    /// The long-lived bearer mandate from the hub (base64url blob), obtained once
    /// at install. Lets `xr-share share` register shares without admin action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_credential: Option<String>,
    /// This agent's own ed25519 private key (base64 standard). Identity the hub
    /// bound the credential to; kept for future proof-of-possession.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_key: Option<String>,
    /// Optional TLS (provided cert + key PEM). Without it the agent serves plain
    /// HTTP (dev / behind a TLS terminator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
    /// Relay for shares behind NAT (LLD-23 §2.4). Handed to the agent by the hub
    /// at `install`/`share`; when present the agent keeps an outgoing reverse
    /// tunnel to the relay and serves reverse-streams over identity-TLS. Absent
    /// means direct-only. Served only in a build with the `relay` feature; a
    /// default build parses it but logs that it's ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayAgentConfig>,
    /// Invite this agent attaches new shares to when `share` gets no explicit
    /// `--invite` (XR-127). Set from a `--setup` token at install, so onboarding
    /// is one command: install once, and every later `share` lands on the invite
    /// its holders already carry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_invite: Option<String>,
    /// Optional cap on an accepted upload's size, in mebibytes (LLD-28). `None`
    /// means no limit (the trusted circle default). A body over the cap is
    /// refused with `413` before it is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_mb: Option<u64>,
    /// URL-import settings + plugin registry (LLD-29). Absent block means no
    /// import anywhere: the local opt-in on top of the `share:import` scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import: Option<ImportConfig>,
    /// The shares this agent serves. Each `[[share]]` is a `share_id` + path.
    #[serde(default, rename = "share")]
    pub shares: Vec<ShareEntry>,

    // ── legacy single-share (v1) ──────────────────────────────────────
    /// Legacy single served directory. Folded into [`AgentConfig::resolved_shares`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Legacy single share id paired with `dir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_id: Option<String>,
}

/// One share entry: an opaque `share_id` (the token binding) and a path that is
/// either a directory (serve its tree) or a single file (a one-entry manifest).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShareEntry {
    pub share_id: String,
    pub path: String,
    /// Optional human label (echoed back to the operator by `list`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The agent accepts `PUT`/`DELETE` into this share (LLD-28). Off by default:
    /// the local switch is a second gate on top of the hub-minted `share:write`
    /// scope, so a compromised hub still cannot write here. Only directories are
    /// writable; a file share stays read-only. Set by `xr-share share --writable`.
    #[serde(default)]
    pub writable: bool,
    /// The agent accepts URL-import jobs into this share (LLD-29). Off by
    /// default and only valid together with `writable`: import is a kind of
    /// write. Set by `xr-share share --writable --import`.
    #[serde(default)]
    pub import: bool,
}

/// Job limits and the plugin registry for URL import (LLD-29 п. 2.3). The block
/// is agent-global; each share still opts in with its own `import` flag.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImportConfig {
    /// Hard cap on one job's lifetime, minutes. The process is killed past it.
    #[serde(default = "default_import_timeout_min")]
    pub timeout_min: u64,
    /// Optional cap on a job's total output, in mebibytes, checked while it
    /// downloads. `None` means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_mb: Option<u64>,
    /// `auto` wraps the plugin in systemd-run with private ranges denied
    /// (Linux); `none` disables the wrapper explicitly.
    #[serde(default = "default_import_sandbox")]
    pub sandbox: String,
    #[serde(default, rename = "plugin")]
    pub plugins: Vec<ImportPlugin>,
}

/// One external fetcher: a command plus the host suffixes it takes.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImportPlugin {
    pub name: String,
    /// Host suffixes matched at label boundaries; `"*"` makes it the catch-all.
    pub patterns: Vec<String>,
    /// The owner's quality ceiling: a job never downloads above this height,
    /// whatever the device asked (LLD-29 п. 3.9).
    #[serde(default = "default_import_max_height")]
    pub max_height: u32,
    pub cmd: String,
    /// argv template. The element equal to `{url}` is replaced by the link as
    /// one literal argument (no shell); `{height}` inside any element is
    /// replaced by the validated effective height.
    pub args: Vec<String>,
}

fn default_import_timeout_min() -> u64 {
    30
}

fn default_import_sandbox() -> String {
    "auto".into()
}

fn default_import_max_height() -> u32 {
    1080
}

impl ImportConfig {
    /// The reference block `share --import` bootstraps into a config that has no
    /// `[import]` yet (LLD-29 п. 3.10): a catch-all yt-dlp wrapper capped at
    /// 1080p. Mirrors `configs/share.toml`.
    pub fn reference() -> Self {
        Self {
            timeout_min: default_import_timeout_min(),
            max_total_mb: Some(4096),
            sandbox: default_import_sandbox(),
            plugins: vec![ImportPlugin {
                name: "yt-dlp".into(),
                patterns: vec!["youtube.com".into(), "youtu.be".into(), "*".into()],
                max_height: default_import_max_height(),
                cmd: "yt-dlp".into(),
                args: vec![
                    "--no-playlist".into(),
                    "--newline".into(),
                    "-f".into(),
                    "bv*[height<={height}]+ba/b[height<={height}]".into(),
                    "--progress-template".into(),
                    "download:xr-progress %(progress._percent_str)s".into(),
                    "-o".into(),
                    "%(title).200B [%(id)s].%(ext)s".into(),
                    "{url}".into(),
                ],
            }],
        }
    }
}

/// The relay this agent tunnels through for NAT'd shares (LLD-23 §2.4). Mirror
/// of the hub's relay descriptor: dial address plus the mux obfuscation params
/// (shared with the relay and the consumer so all three build the same codec).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RelayAgentConfig {
    pub addr: String,
    pub port: u16,
    /// Named `obfuscation` in TOML (matching the hub/relay configs), carried as
    /// `obf` on the wire descriptor.
    #[serde(rename = "obfuscation")]
    pub obf: xr_proto::share::RelayObf,
}

impl RelayAgentConfig {
    /// `host:port` for dialing the relay (used by the reverse-tunnel uplink).
    #[cfg(feature = "relay")]
    pub fn dial(&self) -> String {
        format!("{}:{}", self.addr, self.port)
    }

    /// Build from a hub-issued relay descriptor (captured at `install`/`share`).
    pub fn from_descriptor(d: &xr_proto::share::RelayDescriptor) -> Self {
        Self { addr: d.addr.clone(), port: d.port, obf: d.obf.clone() }
    }
}

/// Read only by the `tls` feature; kept parseable in HTTP-only builds so a
/// `[tls]` block produces a clear error rather than an unknown-field failure.
#[cfg_attr(not(feature = "tls"), allow(dead_code))]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsConfig {
    pub cert: String,
    pub key: String,
}

impl AgentConfig {
    /// Decode and validate the pinned hub public key.
    pub fn hub_verifying_key(&self) -> Result<VerifyingKey> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(self.hub_pubkey.trim())
            .context("decoding hub_pubkey base64")?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("hub_pubkey must be 32 bytes, got {}", v.len()))?;
        VerifyingKey::from_bytes(&arr).context("invalid hub_pubkey")
    }

    /// The agent's identity signing key, used to sign manifests (XR-046). Two
    /// storage places exist historically: `identity_key` inside the config (the
    /// `install` flow) and an `identity.key` file next to the config (the `init`
    /// flow). `Ok(None)` when neither is present (a hand-written legacy config):
    /// the agent then serves unsigned manifests and a pinning consumer rejects
    /// them. A key that is present but undecodable is an error, not a silent
    /// downgrade to unsigned.
    pub fn identity_signing_key(&self, config_path: &Path) -> Result<Option<SigningKey>> {
        let b64 = match &self.identity_key {
            Some(k) => k.clone(),
            None => {
                let file = config_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("identity.key");
                match std::fs::read_to_string(&file) {
                    Ok(s) => s,
                    Err(_) => return Ok(None),
                }
            }
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .context("decoding identity key base64")?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("identity key must be 32 bytes, got {}", v.len()))?;
        Ok(Some(SigningKey::from_bytes(&arr)))
    }

    /// The full share list: `[[share]]` entries plus the legacy `dir`+`share_id`
    /// pair (if present and not already listed), so v1 and v2 configs both work.
    pub fn resolved_shares(&self) -> Vec<ShareEntry> {
        let mut shares = self.shares.clone();
        if let (Some(dir), Some(id)) = (&self.dir, &self.share_id) {
            if !shares.iter().any(|s| &s.share_id == id) {
                shares.push(ShareEntry {
                    share_id: id.clone(),
                    path: dir.clone(),
                    name: None,
                    // Legacy single-share configs predate writable shares.
                    writable: false,
                    import: false,
                });
            }
        }
        shares
    }

    /// Fail-fast checks for the import surface (LLD-29 п. 2.3): a broken plugin
    /// template or an `import` flag without `writable` must stop the agent at
    /// startup, not fail every job at runtime.
    pub fn validate_import(&self) -> Result<()> {
        if let Some(import) = &self.import {
            if import.sandbox != "auto" && import.sandbox != "none" {
                anyhow::bail!("[import] sandbox must be \"auto\" or \"none\", got {:?}", import.sandbox);
            }
            for p in &import.plugins {
                if p.patterns.is_empty() {
                    anyhow::bail!("import plugin {:?}: patterns must not be empty", p.name);
                }
                // {url} carries untrusted input, so it may only ever be a whole
                // argv element; embedded in a string it would invite quoting
                // games in whatever the command does with it.
                if !p.args.iter().any(|a| a == "{url}") {
                    anyhow::bail!("import plugin {:?}: args must contain \"{{url}}\" as its own element", p.name);
                }
                if p.args.iter().any(|a| a != "{url}" && a.contains("{url}")) {
                    anyhow::bail!("import plugin {:?}: \"{{url}}\" must be a whole argv element, not part of one", p.name);
                }
                if !(crate::import::HEIGHT_MIN..=crate::import::HEIGHT_MAX).contains(&p.max_height) {
                    anyhow::bail!(
                        "import plugin {:?}: max_height must be {}..{}",
                        p.name,
                        crate::import::HEIGHT_MIN,
                        crate::import::HEIGHT_MAX
                    );
                }
            }
        }
        for s in self.resolved_shares() {
            if s.import && !s.writable {
                anyhow::bail!("share {}: import = true requires writable = true", s.share_id);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multishare_config() {
        let toml = r#"
            listen = "0.0.0.0:8443"
            hub_pubkey = "QQ=="
            agent_credential = "blob"
            [[share]]
            share_id = "a"
            path = "/srv/photos"
            [[share]]
            share_id = "b"
            path = "/srv/report.pdf"
        "#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        let shares = cfg.resolved_shares();
        assert_eq!(shares.len(), 2);
        assert_eq!(shares[0].share_id, "a");
        assert_eq!(shares[1].path, "/srv/report.pdf");
        assert_eq!(cfg.agent_credential.as_deref(), Some("blob"));
    }

    #[test]
    fn folds_legacy_single_share() {
        let toml = r#"
            listen = "0.0.0.0:8443"
            hub_pubkey = "QQ=="
            dir = "/srv/share"
            share_id = "legacy"
        "#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        let shares = cfg.resolved_shares();
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].share_id, "legacy");
        assert_eq!(shares[0].path, "/srv/share");
    }

    #[test]
    fn identity_key_from_config_or_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let b64 = base64::engine::general_purpose::STANDARD.encode(key.to_bytes());

        let mut cfg: AgentConfig =
            toml::from_str("listen = \"0.0.0.0:8443\"\nhub_pubkey = \"QQ==\"").unwrap();

        // Neither config field nor identity.key file -> None (unsigned legacy).
        assert!(cfg.identity_signing_key(&cfg_path).unwrap().is_none());

        // The init flow's identity.key file next to the config.
        std::fs::write(dir.path().join("identity.key"), format!("{b64}\n")).unwrap();
        let loaded = cfg.identity_signing_key(&cfg_path).unwrap().unwrap();
        assert_eq!(loaded.to_bytes(), key.to_bytes());

        // An inline identity_key (the install flow) wins over the file.
        cfg.identity_key = Some(b64);
        assert!(cfg.identity_signing_key(&cfg_path).unwrap().is_some());

        // A present-but-broken key is an error, not silent unsigned serving.
        cfg.identity_key = Some("@@@".into());
        assert!(cfg.identity_signing_key(&cfg_path).is_err());
    }

    #[test]
    fn roundtrips_through_serialization() {
        // `xr-share share`/`unshare` rewrite the config, so it must survive a
        // serialize → parse cycle without losing or inventing fields.
        let cfg = AgentConfig {
            listen: "0.0.0.0:8443".into(),
            hub_pubkey: "QQ==".into(),
            hub_url: Some("https://hub".into()),
            agent_credential: Some("blob".into()),
            identity_key: Some("priv".into()),
            tls: None,
            relay: None,
            default_invite: Some("inv123".into()),
            max_file_mb: Some(100),
            import: None,
            shares: vec![ShareEntry { share_id: "a".into(), path: "/srv/x".into(), name: Some("X".into()), writable: true, import: true }],
            dir: None,
            share_id: None,
        };
        let text = toml::to_string(&cfg).unwrap();
        let back: AgentConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.resolved_shares().len(), 1);
        assert!(back.resolved_shares()[0].writable, "writable flag must survive the roundtrip");
        assert!(back.resolved_shares()[0].import, "import flag must survive the roundtrip");
        assert_eq!(back.max_file_mb, Some(100));
        assert_eq!(back.hub_url.as_deref(), Some("https://hub"));
        assert_eq!(back.default_invite.as_deref(), Some("inv123"));
        assert!(back.dir.is_none());
    }

    #[test]
    fn test_import_config_parse() {
        // The [import] block with a plugin parses, survives a roundtrip (the
        // bootstrap in `share --import` rewrites the config), and the reference
        // block is itself valid.
        let toml_text = r#"
            listen = "0.0.0.0:8443"
            hub_pubkey = "QQ=="
            [import]
            timeout_min = 10
            max_total_mb = 512
            [[import.plugin]]
            name = "yt-dlp"
            patterns = ["youtube.com", "*"]
            max_height = 720
            cmd = "yt-dlp"
            args = ["-f", "b[height<={height}]", "{url}"]
            [[share]]
            share_id = "w"
            path = "/srv/x"
            writable = true
            import = true
        "#;
        let cfg: AgentConfig = toml::from_str(toml_text).unwrap();
        cfg.validate_import().unwrap();
        let import = cfg.import.as_ref().unwrap();
        assert_eq!(import.timeout_min, 10);
        assert_eq!(import.max_total_mb, Some(512));
        assert_eq!(import.sandbox, "auto");
        assert_eq!(import.plugins[0].max_height, 720);
        assert!(cfg.resolved_shares()[0].import);

        let back: AgentConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.import.as_ref().unwrap().plugins.len(), 1);
        assert!(back.resolved_shares()[0].import);

        // {url} inside a string, not its own argv element -> startup error.
        let mut broken = cfg.clone();
        broken.import.as_mut().unwrap().plugins[0].args = vec!["--get={url}".into()];
        assert!(broken.validate_import().is_err());

        // No {url} at all is just as dead.
        let mut broken = cfg.clone();
        broken.import.as_mut().unwrap().plugins[0].args = vec!["-f".into(), "b".into()];
        assert!(broken.validate_import().is_err());

        // import = true without writable -> startup error.
        let mut broken = cfg.clone();
        broken.shares[0].writable = false;
        assert!(broken.validate_import().is_err());

        // The bootstrap reference block passes its own validation.
        let mut with_ref = cfg;
        with_ref.import = Some(ImportConfig::reference());
        with_ref.validate_import().unwrap();
    }
}
