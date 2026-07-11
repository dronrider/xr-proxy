//! End-to-end TLS pinned to the agent's identity key (LLD-23 §2.3, §3.3).
//!
//! Inside the relay's blind splice (and on the direct path too) the consumer
//! runs TLS 1.3 straight to the agent. Trust is not a CA chain but the pin the
//! hub already hands out: the agent serves a self-signed certificate whose public
//! key **is** its ed25519 identity key, and the consumer accepts exactly the key
//! `agent_pubkey` from the grant. A relay (or any MITM) that swaps the cert
//! presents a different key and the handshake fails.
//!
//! This module is consumer-and-transport side only: the pinning verifier and the
//! rustls config builders, on the `ring` provider so they cross-compile to
//! Android and musl. Generating the certificate from the identity key needs
//! `rcgen` and lives in the agent (`xr-share`, feature `relay`); nothing here
//! pulls it.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring as provider, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};

/// DER OID of Ed25519 (`1.3.101.112`), the only algorithm we pin.
const ED25519_OID: &[u8] = &[0x2b, 0x65, 0x70];

/// Extract the raw 32-byte Ed25519 public key from a certificate's
/// SubjectPublicKeyInfo. Errors if the cert doesn't parse or isn't Ed25519.
pub fn cert_ed25519_spki(cert_der: &[u8]) -> Result<[u8; 32], String> {
    use x509_cert::der::Decode;
    let cert = x509_cert::Certificate::from_der(cert_der).map_err(|e| format!("parse cert: {e}"))?;
    let spki = cert.tbs_certificate.subject_public_key_info;
    let oid = spki.algorithm.oid.as_bytes();
    if oid != ED25519_OID {
        return Err(format!("not an Ed25519 cert (spki oid {oid:02x?})"));
    }
    let key = spki
        .subject_public_key
        .as_bytes()
        .ok_or_else(|| "spki bit string not byte-aligned".to_string())?;
    key.try_into()
        .map_err(|_| format!("Ed25519 key must be 32 bytes, got {}", key.len()))
}

/// A rustls verifier that accepts exactly one server: the one whose certificate
/// carries the pinned Ed25519 key (LLD-23 §2.3). The hostname/SNI is ignored on
/// purpose (the pin is on the key, not the address); the handshake signature is
/// still verified through the crypto provider, so presenting the cert without
/// holding its private key does not pass.
#[derive(Debug)]
pub struct PinnedAgentVerifier {
    expected: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl PinnedAgentVerifier {
    pub fn new(expected_agent_pubkey: [u8; 32]) -> Self {
        Self { expected: expected_agent_pubkey, provider: Arc::new(provider::default_provider()) }
    }
}

impl ServerCertVerifier for PinnedAgentVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = cert_ed25519_spki(end_entity.as_ref())
            .map_err(|e| rustls::Error::General(format!("agent cert: {e}")))?;
        if got == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("agent key pin mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// A `ClientConfig` that trusts exactly the agent identified by `agent_pubkey`
/// (base64 standard, 32-byte Ed25519 key), for pinned E2E over the relay or the
/// direct path. No ALPN is set; the agent speaks HTTP/1.1 over TLS.
pub fn pinned_client_config(agent_pubkey: &str) -> Result<ClientConfig, String> {
    use base64::Engine as _;
    let key = base64::engine::general_purpose::STANDARD
        .decode(agent_pubkey.trim())
        .map_err(|e| format!("agent_pubkey base64: {e}"))?;
    let key: [u8; 32] = key
        .try_into()
        .map_err(|v: Vec<u8>| format!("agent_pubkey must be 32 bytes, got {}", v.len()))?;
    let cfg = ClientConfig::builder_with_provider(Arc::new(provider::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("rustls versions: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedAgentVerifier::new(key)))
        .with_no_client_auth();
    Ok(cfg)
}

/// A `ServerConfig` presenting the agent's self-signed identity certificate.
/// `cert_der` and `key_der` come from the agent's `rcgen` cert built off the
/// identity key (LLD-23 §2.3); the consumer's [`PinnedAgentVerifier`] pins the
/// key inside `cert_der`. No client auth (authorization is the `ShareToken`
/// inside the tunnel, §2.3).
pub fn identity_server_config(
    cert_der: Vec<u8>,
    key_der: PrivateKeyDer<'static>,
) -> Result<ServerConfig, String> {
    let cfg = ServerConfig::builder_with_provider(Arc::new(provider::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("rustls versions: {e}"))?
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert_der)], key_der)
        .map_err(|e| format!("server cert: {e}"))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    /// Build a self-signed Ed25519 cert the way the agent will, returning the DER
    /// cert, DER key and the pinned public key (its SPKI).
    fn agent_cert() -> (Vec<u8>, PrivateKeyDer<'static>, [u8; 32]) {
        let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let params = rcgen::CertificateParams::new(vec!["xr-share-agent".to_string()]).unwrap();
        let cert = params.self_signed(&kp).unwrap();
        let cert_der = cert.der().to_vec();
        let spki = cert_ed25519_spki(&cert_der).unwrap();
        let key_der = PrivateKeyDer::try_from(kp.serialize_der()).unwrap();
        (cert_der, key_der, spki)
    }

    #[test]
    fn spki_extracted_from_generated_cert() {
        let (_c, _k, spki) = agent_cert();
        assert_eq!(spki.len(), 32);
    }

    #[test]
    fn pinned_config_rejects_wrong_key() {
        // A verifier pinned to key X must not accept a cert carrying key Y.
        let (cert_der, _k, spki) = agent_cert();
        let mut wrong = spki;
        wrong[0] ^= 0xFF;
        let v = PinnedAgentVerifier::new(wrong);
        let end = CertificateDer::from(cert_der);
        let err = v.verify_server_cert(
            &end,
            &[],
            &ServerName::try_from("x").unwrap(),
            &[],
            UnixTime::now(),
        );
        assert!(err.is_err(), "wrong pin must be rejected");
    }

    /// Full TLS 1.3 handshake over a real socket: the pinned client completes the
    /// handshake with the agent's cert and rejects a MITM cert (different key),
    /// which is exactly the relay-swaps-the-cert attack (LLD-23 §3.3).
    #[tokio::test]
    async fn pinned_handshake_accepts_agent_rejects_mitm() {
        let (cert_der, key_der, spki) = agent_cert();
        let server_cfg = identity_server_config(cert_der, key_der).unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_cfg));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Serve two connections: the honest client and the MITM-pinned one.
            for _ in 0..2 {
                let (sock, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut tls) = acceptor.accept(sock).await {
                        let mut b = [0u8; 4];
                        let _ = tls.read_exact(&mut b).await;
                        let _ = tls.write_all(b"pong").await;
                        let _ = tls.shutdown().await;
                    }
                });
            }
        });

        // Honest client pins the real key -> handshake + echo succeed.
        let good = PinnedAgentVerifier::new(spki);
        let connector = connector_with(good);
        let sock = TcpStream::connect(addr).await.unwrap();
        let mut tls = connector
            .connect(ServerName::try_from("agent").unwrap(), sock)
            .await
            .expect("pinned handshake to the agent succeeds");
        tls.write_all(b"ping").await.unwrap();
        let mut got = [0u8; 4];
        tls.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"pong");

        // MITM-pinned client (wrong key) -> handshake fails.
        let mut wrong = spki;
        wrong[0] ^= 0xFF;
        let connector = connector_with(PinnedAgentVerifier::new(wrong));
        let sock = TcpStream::connect(addr).await.unwrap();
        let res = connector.connect(ServerName::try_from("agent").unwrap(), sock).await;
        assert!(res.is_err(), "a swapped cert must break the pin");
    }

    fn connector_with(v: PinnedAgentVerifier) -> TlsConnector {
        let cfg = ClientConfig::builder_with_provider(Arc::new(provider::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(v))
            .with_no_client_auth();
        TlsConnector::from(Arc::new(cfg))
    }
}
