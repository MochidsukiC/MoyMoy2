//! Dev TLS for the `moymoy.cs.mnn` endpoint (path C: end-to-end through the
//! gateway CONNECT tunnel — the gateway never decrypts, DEV.md §7.3.3).
//!
//! A dev Mochi CA signs a server cert covering the `.mnn` host (and
//! `localhost`/`127.0.0.1` for the browser-direct debug loop). The CA + cert are
//! persisted under a dev dir so the client trusts the CA once (stable across
//! restarts). Adapted from `PiggleShop2/server/piggleshop-cs/src/tls.rs` (env
//! prefix + CA CN renamed). DEV-grade PKI — production CA is a later phase.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;

fn tls_dir() -> PathBuf {
    std::env::var("MOYMOY_CS_TLS_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".devstack/moymoy-cs-tls"))
}

fn server_sans(mnn_host: &str) -> Vec<String> {
    let mut v = vec![mnn_host.to_string()];
    for n in ["localhost", "127.0.0.1"] {
        if !v.iter().any(|s| s == n) {
            v.push(n.to_string());
        }
    }
    v
}

fn load_or_mint(dir: &Path, mnn_host: &str) -> anyhow::Result<(String, String)> {
    let ca_pem = dir.join("ca.pem");
    let ca_key_pem = dir.join("ca.key.pem");
    let srv_pem = dir.join("server.pem");
    let srv_key_pem = dir.join("server.key.pem");

    if srv_pem.exists() && srv_key_pem.exists() && ca_pem.exists() && ca_key_pem.exists() {
        let cert = fs::read_to_string(&srv_pem)?;
        let key = fs::read_to_string(&srv_key_pem)?;
        return Ok((cert, key));
    }

    fs::create_dir_all(dir)?;

    // --- CA ---
    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Mochi Dev CA (MoyMoy cs)");
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_cert = ca_params.self_signed(&ca_key)?;

    // --- server (leaf) cert signed by the CA ---
    let srv_key = KeyPair::generate()?;
    let mut srv_params = CertificateParams::new(server_sans(mnn_host))?;
    srv_params
        .distinguished_name
        .push(DnType::CommonName, mnn_host);
    srv_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    srv_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let issuer = Issuer::from_params(&ca_params, &ca_key);
    let srv_cert = srv_params.signed_by(&srv_key, &issuer)?;

    let ca_cert_pem = ca_cert.pem();
    let srv_cert_pem = srv_cert.pem();
    let srv_key_pem_str = srv_key.serialize_pem();

    fs::write(&ca_pem, &ca_cert_pem)?;
    fs::write(&ca_key_pem, ca_key.serialize_pem())?;
    fs::write(&srv_pem, &srv_cert_pem)?;
    fs::write(&srv_key_pem, &srv_key_pem_str)?;

    tracing::info!(
        ca = %ca_pem.display(),
        "moymoy.cs.mnn dev TLS: minted Mochi dev CA + server cert (import ca.pem to trust)"
    );

    Ok((srv_cert_pem, srv_key_pem_str))
}

/// Build a rustls [`ServerConfig`] for `moymoy.cs.mnn`, minting a dev CA +
/// server cert on first run.
pub fn server_config(mnn_host: &str) -> anyhow::Result<Arc<ServerConfig>> {
    // Pin the ring crypto provider (the workspace + quinn standardize on ring).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let dir = tls_dir();
    let (cert_pem, key_pem) = load_or_mint(&dir, mnn_host)?;

    let certs: Vec<CertificateDer<'static>> = pemfile_certs(cert_pem.as_bytes())?;
    let key: PrivateKeyDer<'static> = pemfile_key(key_pem.as_bytes())?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("rustls server config: {e}"))?;
    Ok(Arc::new(config))
}

// --- minimal PEM parsing (avoid an extra rustls-pemfile dep) ---

fn pemfile_certs(pem: &[u8]) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let text = std::str::from_utf8(pem)?;
    let mut out = Vec::new();
    for block in pem_blocks(text, "CERTIFICATE") {
        out.push(CertificateDer::from(block));
    }
    if out.is_empty() {
        anyhow::bail!("no CERTIFICATE block in server cert PEM");
    }
    Ok(out)
}

fn pemfile_key(pem: &[u8]) -> anyhow::Result<PrivateKeyDer<'static>> {
    let text = std::str::from_utf8(pem)?;
    // rcgen serialize_pem emits a PKCS#8 "PRIVATE KEY" block.
    for label in ["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"] {
        if let Some(der) = pem_blocks(text, label).into_iter().next() {
            return PrivateKeyDer::try_from(der)
                .map_err(|e| anyhow::anyhow!("parse private key ({label}): {e}"));
        }
    }
    anyhow::bail!("no PRIVATE KEY block in server key PEM")
}

/// Extract the base64 DER bytes of every `-----BEGIN <label>-----` block.
fn pem_blocks(text: &str, label: &str) -> Vec<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(b) = rest.find(&begin) {
        let after = &rest[b + begin.len()..];
        if let Some(e) = after.find(&end) {
            let body: String = after[..e].split_whitespace().collect();
            if let Ok(der) = base64_decode(&body) {
                out.push(der);
            }
            rest = &after[e + end.len()..];
        } else {
            break;
        }
    }
    out
}

/// Tiny standard-base64 decoder (PEM bodies only; no external dep).
fn base64_decode(s: &str) -> Result<Vec<u8>, &'static str> {
    fn val(c: u8) -> Result<u8, &'static str> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err("invalid base64 char"),
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 {
            return Err("truncated base64");
        }
        let b0 = val(chunk[0])?;
        let b1 = val(chunk[1])?;
        out.push((b0 << 2) | (b1 >> 4));
        if chunk.len() >= 3 && chunk[2] != b'=' {
            let b2 = val(chunk[2])?;
            out.push((b1 << 4) | (b2 >> 2));
            if chunk.len() == 4 && chunk[3] != b'=' {
                let b3 = val(chunk[3])?;
                out.push((b2 << 6) | b3);
            }
        }
    }
    Ok(out)
}
