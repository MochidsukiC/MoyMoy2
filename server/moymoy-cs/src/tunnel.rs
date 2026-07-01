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

use std::net::SocketAddr;

use mochi_hub_cs_sdk::{run_cs_tunnel, CsTunnelConfig};
use tokio::sync::watch;
use uuid::Uuid;

use crate::env_or;

/// Build the tunnel config for `mnn_domain`, forwarding inbound traffic to
/// `local_target` (our loopback TLS listener). Hub endpoints come from env with
/// dev defaults that match a local `mochi-inworld` devstack.
fn config(mnn_domain: &str, local_target: SocketAddr) -> CsTunnelConfig {
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
    CsTunnelConfig {
        hub_ws_base: env_or("MOCHI_TUNNEL_HUB_WS", "ws://127.0.0.1:7411"),
        hub_quic_addr,
        router_url: env_or("MOCHI_IPVM_ROUTER_URL", "http://127.0.0.1:7400"),
        bearer: env_or("MOCHI_TUNNEL_BEARER", "dev-moymoy"),
        mnn_domain: mnn_domain.to_string(),
        local_target,
        // Stable node id derived from the domain so a reconnect re-claims the same
        // cs host (never a fresh random claim that would race the prior one).
        node_id: Uuid::new_v5(&Uuid::NAMESPACE_DNS, mnn_domain.as_bytes()),
        capabilities: vec!["moymoy".to_string()],
        egress_addr: None,
    }
}

/// Spawn the embedded cs tunnel on the current Tokio runtime. Returns the
/// shutdown [`watch::Sender`]; hold it for the process lifetime (dropping it
/// signals the tunnel to wind down). The tunnel reconnects on its own until then.
pub fn spawn(mnn_domain: &str, local_target: SocketAddr) -> watch::Sender<()> {
    let (tx, rx) = watch::channel(());
    let cfg = config(mnn_domain, local_target);
    let domain = mnn_domain.to_string();
    tokio::spawn(async move {
        tracing::info!(%domain, %local_target, "embedded cs tunnel starting (tunnel-embedded SDK)");
        if let Err(e) = run_cs_tunnel(cfg, rx).await {
            tracing::error!(error = %e, %domain, "embedded cs tunnel exited with error");
        }
    });
    tx
}
