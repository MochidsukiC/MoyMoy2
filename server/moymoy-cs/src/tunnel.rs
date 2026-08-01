//! Embedded cs tunnel — the "tunnel-embedded SDK" path (DEV.md §5.4.3,
//! app.toml `tunnel = "self"`).
//!
//! Instead of running the generic `mochi-tunnel-agent` sidecar as a separate
//! process, this backend embeds the exact same tunnel logic in-process via
//! `mochi-hub-cs-sdk`. We are already on a Tokio runtime (`#[tokio::main]`), so we
//! drive the async core [`run_cs_tunnel`] directly with `tokio::spawn` (rather
//! than `CsTunnel::start`, which would spin up a second, redundant runtime).
//!
//! A `<host>.cs.mnn` domain is *claim-bound*: the tunnel claims `<host>` at the
//! reverse-tunnel handshake (no `PUT /nodes`). The Hub gateway then routes
//! `moymoy.cs.mnn` HTTPS through this tunnel to our loopback TLS listener.
//!
//! When `tunnel = "self"` the launcher injects NO `MOCHI_TUNNEL_*`; we source the
//! Hub endpoints from env (dev defaults below) and the loopback target from the
//! address we actually bound (`MOCHI_APP_LISTEN`).
//!
//! ## Who this authenticates as (DEV.md §7.3.9 Stage 6-iv)
//!
//! The handshake bearer is THIS process's own per-process identity token
//! (`MOCHI_SVC_IDENTITY_TOKEN`, minted and injected by the hub launcher), so the
//! Hub attributes the `wallet.moymoy` claim to `ServiceProcess("app.moymoy")` —
//! the principal the launcher registered this backend's cs-host under. The legacy
//! shared `MOCHI_TUNNEL_BEARER` is gone: since Stage 6-iv-c2 the Hub derives no
//! principal from it at all, so presenting it can only ever be rejected (that is
//! the `tunnel WS handshake failed` loop this replaced). No token ⇒ no tunnel,
//! reported as an error rather than retried against a placeholder.

use std::net::SocketAddr;
use std::path::PathBuf;

use mochi_hub_cs_sdk::{run_cs_tunnel, CsTunnelConfig};
use tokio::sync::watch;
use uuid::Uuid;

use crate::env_or;

/// Build the tunnel config for `mnn_domain`, forwarding inbound traffic to
/// `local_target` (our loopback TLS listener). Hub endpoints come from env with
/// dev defaults that match a local `mochi-inworld` devstack. `bearer` is our own
/// identity token (see the module docs) — the caller has already verified it is
/// non-empty.
fn config(
    mnn_domain: &str,
    local_target: SocketAddr,
    bearer: String,
) -> anyhow::Result<CsTunnelConfig> {
    let hub_quic_addr = {
        let raw = env_or("MOCHI_TUNNEL_HUB_QUIC", "127.0.0.1:7420");
        match raw.parse::<SocketAddr>() {
            Ok(addr) => Some(addr),
            Err(e) => {
                // The default always parses; failure means an invalid env override.
                // QUIC remains disabled (None) but surface the misconfiguration.
                tracing::warn!(error = %e, value = %raw,
                    "MOCHI_TUNNEL_HUB_QUIC is not a valid socket address; QUIC transport disabled");
                None
            }
        }
    };
    Ok(CsTunnelConfig {
        hub_ws_base: env_or("MOCHI_TUNNEL_HUB_WS", "ws://127.0.0.1:7411"),
        hub_quic_addr,
        router_url: env_or("MOCHI_IPVM_ROUTER_URL", "http://127.0.0.1:7400"),
        bearer,
        mnn_domain: mnn_domain.to_string(),
        local_target,
        // Stable node id derived from the domain so a reconnect re-claims the same
        // cs host (never a fresh random claim that would race the prior one).
        node_id: Uuid::new_v5(&Uuid::NAMESPACE_DNS, mnn_domain.as_bytes()),
        capabilities: vec!["moymoy".to_string()],
        egress_addr: None,
        trust_anchor_pem: quic_trust_anchor_pem()?,
        // Never the silent MITM-open fallback for a missing anchor: with QUIC
        // configured and no anchor, the SDK honest-fails at connect instead
        // (DEV.md §7.3.9 R5). Unset MOCHI_TUNNEL_HUB_QUIC for a WSS-only smoke.
        insecure_tls: false,
        // Non-enrolled: our credential is the launcher-minted identity token, not
        // an auto-enrolled endpoint bearer (same posture as the hub's own
        // in-process tunnels).
        require_quic: false,
        join_token: None,
        credential_dir: None,
    })
}

/// The PEM the QUIC data plane pins the Hub's server cert against (DEV.md §7.3.9
/// R5). `MOCHI_TUNNEL_TRUST_ANCHOR` names the file explicitly (an unreadable
/// override is a hard error — the operator pointed at a specific path); otherwise
/// the launcher-injected `MOCHI_STATE_DIR` gives the zero-config default
/// `<state_dir>/mnn-trust-anchor.pem`, which is exactly where the Hub exports its
/// own Overlay CA root. Absent ⇒ `None`, and `run_cs_tunnel` decides (it refuses
/// QUIC without an anchor rather than connecting unverified).
fn quic_trust_anchor_pem() -> anyhow::Result<Option<String>> {
    if let Some(path) = std::env::var("MOCHI_TUNNEL_TRUST_ANCHOR")
        .ok()
        .filter(|s| !s.is_empty())
    {
        let pem = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading MOCHI_TUNNEL_TRUST_ANCHOR file {path:?}: {e}"))?;
        return Ok(Some(pem));
    }
    let Some(state_dir) = std::env::var("MOCHI_STATE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
    else {
        return Ok(None);
    };
    let default_path = state_dir.join("mnn-trust-anchor.pem");
    match std::fs::read_to_string(&default_path) {
        Ok(pem) => Ok(Some(pem)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!(
            "reading default QUIC trust anchor {}: {e}",
            default_path.display()
        )),
    }
}

/// Spawn the embedded cs tunnel on the current Tokio runtime. Returns the
/// shutdown [`watch::Sender`]; hold it for the process lifetime (dropping it
/// signals the tunnel to wind down). The tunnel reconnects on its own until then.
///
/// Errors when this process has no identity token to authenticate the claim with
/// (`MOCHI_SVC_IDENTITY_TOKEN` unset ⇒ it was not started by the hub launcher).
/// `wallet.moymoy.cs.mnn` is our ONLY ingress, so a tunnel that can never be
/// accepted is a dead backend — fail loudly at boot instead of looping on a
/// handshake the Hub is guaranteed to reject.
pub fn spawn(mnn_domain: &str, local_target: SocketAddr) -> anyhow::Result<watch::Sender<()>> {
    let bearer = std::env::var("MOCHI_SVC_IDENTITY_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "MOCHI_SVC_IDENTITY_TOKEN is unset — this backend has no identity to claim \
                 {mnn_domain} with. It is minted and injected by the MochiOS hub launcher, so \
                 run moymoy-cs from app_backends/moymoy/ via the hub (set MOYMOY_CS_TUNNEL=0 \
                 for a loopback-only smoke)."
            )
        })?;
    let (tx, rx) = watch::channel(());
    let cfg = config(mnn_domain, local_target, bearer)?;
    let domain = mnn_domain.to_string();
    tokio::spawn(async move {
        tracing::info!(%domain, %local_target, "embedded cs tunnel starting (tunnel-embedded SDK)");
        if let Err(e) = run_cs_tunnel(cfg, rx).await {
            tracing::error!(error = %e, %domain, "embedded cs tunnel exited with error");
        }
    });
    Ok(tx)
}
