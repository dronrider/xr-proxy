//! Agent side of the relay (LLD-23 §2.1, §2.3). Feature-gated (`relay`): the
//! rcgen/rustls stack it pulls is heavy for the agent's Windows/musl cross-build,
//! so a default agent build has none of it and serves direct-only.
//!
//! The agent holds one **outgoing** obfuscated mux to the relay and reconnects
//! with exponential backoff. On each connection it registers via
//! challenge-response (proving its identity key against the hub-signed
//! credential), then serves every reverse-stream the relay opens by terminating
//! **identity-TLS** on it and handing the plaintext to the same axum router the
//! direct listener uses. The relay only ever moves ciphertext (§3.3).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::PrivateKeyDer;
use tokio_rustls::TlsAcceptor;
use xr_proto::mux::{mux_handshake_client, mux_open_stream, Multiplexer};
use xr_proto::protocol::{Command, TargetAddr};
use xr_proto::relay_client::{RELAY_HELLO_OK, RELAY_REGISTER_TARGET, RELAY_REVERSE_TARGET};
use xr_proto::share::{sign_relay_register, AgentCredential, RelayObf};

use crate::config::RelayAgentConfig;
use crate::server::AgentState;

/// Backoff bounds for the reconnect loop.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// How long we wait for the relay's registration verdict before giving up on
/// this connection and reconnecting.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(15);
/// Cap on the identity-TLS handshake of a reverse-stream, so a consumer that
/// opens a stream but never sends a ClientHello (slow-loris) can't hold the
/// stream and its registry slot open until the whole mux dies. Only the
/// handshake is bounded; a completed request may stream a large file for as long
/// as the relay's own splice-lifetime cap allows.
const REVERSE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Build the agent's self-signed identity certificate: an Ed25519 cert whose
/// public key **is** the agent's identity key, so the consumer's pinning
/// verifier (`SPKI == agent_pubkey`) accepts exactly this agent (LLD-23 §2.3).
pub fn identity_cert(identity: &SigningKey) -> Result<(Vec<u8>, PrivateKeyDer<'static>)> {
    let pkcs8 = identity.to_pkcs8_der().context("identity key to pkcs8")?;
    let kp = rcgen::KeyPair::try_from(pkcs8.as_bytes())
        .map_err(|e| anyhow!("rcgen keypair from identity: {e}"))?;
    let params = rcgen::CertificateParams::new(vec!["xr-share-agent".to_string()])
        .context("cert params")?;
    let cert = params.self_signed(&kp).map_err(|e| anyhow!("self-signed cert: {e}"))?;
    let cert_der = cert.der().to_vec();
    let key_der = PrivateKeyDer::try_from(kp.serialize_der())
        .map_err(|e| anyhow!("serialize identity key der: {e}"))?;
    Ok((cert_der, key_der))
}

/// Decode the agent's credential blob (base64url-nopad JSON, as the hub issued
/// it) into an [`AgentCredential`].
fn decode_credential(blob: &str) -> Result<AgentCredential> {
    use base64::Engine as _;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(blob.trim())
        .context("credential base64url")?;
    serde_json::from_slice(&json).context("credential json")
}

/// Spawn the reverse-tunnel task: keep an agent registration live on the relay
/// and serve reverse-streams over identity-TLS, reconnecting forever with
/// backoff. Returns immediately; the work runs in the background.
pub fn spawn(
    state: Arc<AgentState>,
    relay: RelayAgentConfig,
    credential_blob: String,
    identity: SigningKey,
) -> Result<()> {
    let cred = decode_credential(&credential_blob)
        .context("relay: agent credential unusable (re-run `xr-share install --token`)")?;
    let (cert_der, key_der) = identity_cert(&identity)?;
    let server_cfg = xr_proto::relay_tls::identity_server_config(cert_der, key_der)
        .map_err(|e| anyhow!("identity TLS server config: {e}"))?;
    let acceptor = TlsAcceptor::from(Arc::new(server_cfg));

    tokio::spawn(async move {
        let mut backoff = BACKOFF_MIN;
        loop {
            match connect_and_serve(&state, &relay, &cred, &identity, &acceptor).await {
                Ok(()) => {
                    // Even a clean end gets the floor delay, so a relay that
                    // accepts then instantly drops the mux can't spin a hot
                    // reconnect loop.
                    tracing::info!("relay uplink ended, reconnecting");
                    tokio::time::sleep(BACKOFF_MIN).await;
                    backoff = BACKOFF_MIN;
                }
                Err(e) => {
                    tracing::warn!("relay uplink failed: {e:#}; retry in {:?}", backoff);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    });
    Ok(())
}

/// One connection lifetime: dial, handshake, register, then serve reverse-streams
/// until the mux dies. Returns `Ok` on a clean end (reconnect promptly), `Err`
/// on a failure to establish (back off).
async fn connect_and_serve(
    state: &Arc<AgentState>,
    relay: &RelayAgentConfig,
    cred: &AgentCredential,
    identity: &SigningKey,
    acceptor: &TlsAcceptor,
) -> Result<()> {
    let codec = relay.obf.codec().map_err(|e| anyhow!("relay obfuscation: {e}"))?;
    let mut tcp = TcpStream::connect(relay.dial())
        .await
        .with_context(|| format!("dial relay {}", relay.dial()))?;
    tcp.set_nodelay(true).ok();
    let Some(caps) = mux_handshake_client(&mut tcp, &codec).await? else {
        return Err(anyhow!("relay rejected mux init"));
    };
    let mux = Multiplexer::new_client(tcp, codec, caps);

    // Registration is inline on the first stream: relay acks, sends a nonce, we
    // answer with the credential + a signature over the nonce, and hold the
    // stream open as our liveness signal for the whole connection.
    let reg_stream = register(&mux, cred, identity).await?;
    tracing::info!("registered on relay {}", relay.dial());

    let mut reg_stream = reg_stream;
    let mut new_stream_rx = mux
        .take_new_stream_rx()
        .await
        .ok_or_else(|| anyhow!("relay mux new_stream_rx already taken"))?;

    // Serve reverse-streams until the mux dies (rx yields None) or the relay
    // closes our register stream (deregistration without dropping the whole mux):
    // keep the register stream in the select so its end ends the connection and
    // triggers a fresh registration, instead of serving into a dead registry.
    loop {
        tokio::select! {
            ns = new_stream_rx.recv() => {
                let Some(ns) = ns else { break };
                let stream_id = ns.stream_id;
                let is_reverse = matches!(
                    TargetAddr::decode(&ns.payload),
                    Ok((TargetAddr::Domain(d, _), _)) if d == RELAY_REVERSE_TARGET
                );
                if !is_reverse {
                    let _ = mux.send_frame(stream_id, Command::Close, Vec::new()).await;
                    continue;
                }
                let mux = mux.clone();
                let acceptor = acceptor.clone();
                let router = crate::server::router(state.clone());
                tokio::spawn(async move {
                    if let Err(e) = serve_reverse(mux, stream_id, acceptor, router).await {
                        tracing::debug!("reverse stream {stream_id} ended: {e}");
                    }
                });
            }
            // Register-stream traffic: `None` means the relay dropped our
            // registration, so end the connection and re-register. Stray bytes
            // are drained and ignored.
            r = reg_stream.recv() => {
                if r.is_none() {
                    break;
                }
            }
        }
    }

    mux.shutdown();
    Ok(())
}

/// Registration challenge-response (LLD-23 §2.1). Opens the register stream,
/// reads the relay's nonce, answers with credential + nonce signature, and
/// requires the `OK` verdict. Returns the still-open register stream.
async fn register(
    mux: &Arc<Multiplexer>,
    cred: &AgentCredential,
    identity: &SigningKey,
) -> Result<xr_proto::mux::MuxStream> {
    let target = TargetAddr::Domain(RELAY_REGISTER_TARGET.to_string(), 0);
    let mut stream = mux_open_stream(mux, &target).await.context("open register stream")?;

    let deadline = tokio::time::timeout(REGISTER_TIMEOUT, async {
        let nonce = stream.recv().await.ok_or_else(|| anyhow!("relay closed before nonce"))?;
        let answer = sign_relay_register(identity, cred, &nonce);
        let bytes = serde_json::to_vec(&answer).context("encode registration")?;
        stream.send(&bytes).await.context("send registration")?;
        match stream.recv().await {
            Some(v) if v == [RELAY_HELLO_OK] => Ok(()),
            Some(_) => Err(anyhow!("relay rejected registration")),
            None => Err(anyhow!("relay closed during registration")),
        }
    })
    .await
    .map_err(|_| anyhow!("registration timed out"))?;
    deadline?;
    Ok(stream)
}

/// Terminate identity-TLS on one reverse-stream and serve the agent's HTTP
/// router over it. The bytes on the mux stream are TLS ciphertext end-to-end to
/// the consumer; the relay never saw plaintext.
async fn serve_reverse(
    mux: Arc<Multiplexer>,
    stream_id: u32,
    acceptor: TlsAcceptor,
    router: axum::Router,
) -> Result<()> {
    let stream = mux.register_stream(stream_id).await;
    mux.send_frame(stream_id, Command::ConnectAck, vec![0])
        .await
        .context("ack reverse stream")?;
    let io = stream.into_io();
    let tls = tokio::time::timeout(REVERSE_HANDSHAKE_TIMEOUT, acceptor.accept(io))
        .await
        .map_err(|_| anyhow!("identity TLS handshake timed out"))?
        .context("identity TLS accept")?;

    use tower::ServiceExt;
    let hyper_service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let router = router.clone();
        async move {
            let req = req.map(axum::body::Body::new);
            router.oneshot(req).await
        }
    });
    hyper::server::conn::http1::Builder::new()
        .serve_connection(hyper_util::rt::TokioIo::new(tls), hyper_service)
        .await
        .map_err(|e| anyhow!("serve reverse http: {e}"))
}

/// True if a relay is configured but its obfuscation params are malformed, so
/// `main` can warn instead of silently skipping the uplink. Cheap validation
/// mirroring what [`connect_and_serve`] does per dial.
pub fn relay_obf_ok(obf: &RelayObf) -> bool {
    obf.codec().is_ok()
}

/// Fetch the hub's current relay descriptor (XR-123). A plain binary update then
/// switches an agent onto relay without re-exchanging a token or hand-editing the
/// config. Returns `None` if the hub advertises no relay or is unreachable, so the
/// caller falls back to the config `[relay]`. The descriptor is not secret (every
/// consumer grant carries it), so the fetch is unauthenticated.
pub fn fetch_relay_descriptor(hub_url: &str) -> Option<xr_proto::share::RelayDescriptor> {
    let url = format!("{}/api/v1/relay", hub_url.trim_end_matches('/'));
    let resp = ureq::get(&url).timeout(std::time::Duration::from_secs(10)).call().ok()?;
    let body = resp.into_string().ok()?;
    serde_json::from_str::<Option<xr_proto::share::RelayDescriptor>>(&body).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use xr_proto::relay_tls::cert_ed25519_spki;

    #[test]
    fn identity_cert_spki_is_agent_pubkey() {
        // LLD-23 §2.3: the cert's SPKI must equal the agent's identity key, or the
        // consumer's pin (SPKI == agent_pubkey) could never match.
        let identity = SigningKey::from_bytes(&[3u8; 32]);
        let (cert_der, _key) = identity_cert(&identity).unwrap();
        let spki = cert_ed25519_spki(&cert_der).unwrap();
        assert_eq!(&spki, identity.verifying_key().as_bytes());
    }

    #[test]
    fn credential_blob_roundtrips() {
        use base64::Engine as _;
        let cred = AgentCredential {
            agent_pubkey: "QQ==".into(),
            exp: 42,
            signature: "sig".into(),
        };
        let blob = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&cred).unwrap());
        assert_eq!(decode_credential(&blob).unwrap(), cred);
    }

    // ── full data-path e2e: agent + relay + pinned-TLS consumer ─────────

    use std::sync::RwLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    use base64::Engine as _;
    use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
    use xr_proto::protocol::Codec;
    use xr_proto::relay_client::{LoopbackForwarder, RelayEndpoint};
    use xr_proto::relay_tls::pinned_client_config;
    use xr_proto::share::{
        sign_agent_credential, sign_relay_token, sign_share_token, RelayGrant, RelayObf, ShareToken,
    };

    use crate::manifest::HashCache;
    use crate::server::{AgentState, ShareRoot, SharesMap};

    fn now() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn test_obf() -> RelayObf {
        RelayObf {
            key: b64(b"test-key-32-bytes-long-enough!!!"),
            salt: 0xDEADBEEF,
            modifier: "positional_xor_rotate".into(),
            padding_min: 0,
            padding_max: 0,
        }
    }

    fn test_codec() -> Codec {
        let key = b"test-key-32-bytes-long-enough!!!".to_vec();
        Codec::new(Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate), 0, 0)
    }

    fn token_blob(t: &ShareToken) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(t).unwrap())
    }

    /// LLD-23 §4 integration: an agent behind the relay serves its manifest and a
    /// file to a consumer over end-to-end pinned TLS through the blind splice; the
    /// SHA-256 matches, and a consumer pinning the wrong key fails the handshake
    /// (the relay-swaps-the-cert attack).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn agent_serves_over_relay_with_pinned_tls() {
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let identity = SigningKey::from_bytes(&[7u8; 32]);
        let agent_pk = b64(identity.verifying_key().as_bytes());

        // Relay on an ephemeral port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = listener.local_addr().unwrap();
        let relay_state = xr_relay::RelayState::new(hub.verifying_key(), 64, 8, Duration::from_secs(30));
        {
            let s = relay_state.clone();
            tokio::spawn(async move { xr_relay::serve(listener, test_codec(), s, 64).await });
        }

        // Agent: one file share + reverse tunnel to the relay.
        let dir = tempfile::tempdir().unwrap();
        let file_bytes = b"relayed file bytes, end to end".to_vec();
        std::fs::write(dir.path().join("hello.txt"), &file_bytes).unwrap();
        let mut shares = SharesMap::new();
        shares.insert(
            "S".into(),
            ShareRoot { path: dir.path().canonicalize().unwrap(), is_file: false, writable: false },
        );
        let state = Arc::new(AgentState {
            shares: RwLock::new(Arc::new(shares)),
            hub_key: hub.verifying_key(),
            hash_cache: HashCache::new(),
            identity: Some(identity.clone()),
            max_file_mb: None,
        });
        let cred = sign_agent_credential(&hub, &agent_pk, now() + 3600);
        let cred_blob =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cred).unwrap());
        let relay_cfg = RelayAgentConfig {
            addr: relay_addr.ip().to_string(),
            port: relay_addr.port(),
            obf: test_obf(),
        };
        spawn(state, relay_cfg, cred_blob, identity.clone()).unwrap();

        // Wait until the agent has registered on the relay.
        let mut registered = false;
        for _ in 0..100 {
            if relay_state.registry.get(&agent_pk).await.is_some() {
                registered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(registered, "agent must register on the relay");

        // Consumer: loopback forwarder over a relay grant, reqwest with the pin.
        let relay_token = sign_relay_token(&hub, "S", &agent_pk, now() + 3600);
        let grant = RelayGrant {
            addr: relay_addr.ip().to_string(),
            port: relay_addr.port(),
            obf: test_obf(),
            relay_token,
        };
        let endpoint = Arc::new(RelayEndpoint::from_grant(&grant).unwrap());
        let fwd = LoopbackForwarder::spawn(endpoint).await.unwrap();
        let base = format!("https://{}", fwd.local_addr());

        let client = reqwest::Client::builder()
            .use_preconfigured_tls(pinned_client_config(&agent_pk).unwrap())
            .build()
            .unwrap();
        let share_token = sign_share_token(&hub, "S", "share:read", now() + 3600);

        // Manifest over the relay.
        let resp = client
            .get(format!("{base}/S/manifest"))
            .bearer_auth(token_blob(&share_token))
            .send()
            .await
            .expect("manifest request over relay");
        assert!(resp.status().is_success(), "manifest status {}", resp.status());
        let manifest: xr_proto::share::ShareManifest = resp.json().await.unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].path, "hello.txt");

        // File over the relay, bytes intact end to end.
        let resp = client
            .get(format!("{base}/S/file/hello.txt"))
            .bearer_auth(token_blob(&share_token))
            .send()
            .await
            .expect("file request over relay");
        assert!(resp.status().is_success());
        let got = resp.bytes().await.unwrap();
        assert_eq!(&got[..], &file_bytes[..], "file bytes must survive the E2E splice");

        // MITM: a consumer pinning the wrong key must fail the TLS handshake.
        let mut wrong = identity.verifying_key().to_bytes();
        wrong[0] ^= 0xFF;
        let mitm = reqwest::Client::builder()
            .use_preconfigured_tls(pinned_client_config(&b64(&wrong)).unwrap())
            .build()
            .unwrap();
        let res = mitm
            .get(format!("{base}/S/manifest"))
            .bearer_auth(token_blob(&share_token))
            .send()
            .await;
        assert!(res.is_err(), "wrong pin must break the E2E handshake");
    }
}
