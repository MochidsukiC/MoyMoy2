//! `moymoy-cs` — the MoyMoy wallet backend, reachable at `https://moymoy.cs.mnn`
//! over the MNN overlay.
//!
//! Responsibilities (design-derived from "MochiOS Mobile.html"):
//!   - Serve the wallet HTTP API the app calls (balance / send / pay / charge /
//!     history), persisted to SQLite. The wallet is the single source of truth
//!     for balances and works WITHOUT the Minecraft mod.
//!   - Be reachable as `moymoy.cs.mnn` via an EMBEDDED cs tunnel
//!     (`mochi-hub-cs-sdk`, app.toml `tunnel = "self"`) — no sidecar process.
//!   - Drive emerald charging against the in-world mod as HTTP in MNN over that
//!     SAME tunnel (`crate::mc`); it degrades to wallet-only whenever the tunnel
//!     is down, which is the only "unavailable" state left.

mod api;
mod attest;
mod auth;
mod charge;
mod db;
mod error;
mod identity;
mod mc;
mod otp;
mod tls;
mod tunnel;
mod wallet;

use std::net::SocketAddr;
use std::sync::Arc;

use mochi_hub_cs_sdk::CsHttpSender;

use api::AppState;
use charge::ChargeCoordinator;
use mc::McLink;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // The launcher injects MOCHI_APP_LISTEN=127.0.0.1:<port>; fall back to a dev
    // default for a standalone smoke (tools/run-cs.ps1).
    let listen = env_or(
        "MOCHI_APP_LISTEN",
        &env_or("MOYMOY_CS_LISTEN", "127.0.0.1:7433"),
    );
    // Our single ingress. The wallet and emerald charging share ONE cs claim now
    // that the charge path is HTTP over this same tunnel — the sibling-sub-host
    // split (`wallet.moymoy` / `charge.moymoy`) existed only to give the old
    // command-bus connection a claim of its own, and is gone with it.
    let mnn = env_or("MOYMOY_CS_MNN", "moymoy.cs.mnn");
    let db_path = env_or("MOYMOY_DB_PATH", "moymoy.db");
    let tls_on = env_flag("MOYMOY_CS_TLS", true);
    let tunnel_on = env_flag("MOYMOY_CS_TUNNEL", true);

    // --- persistence ---
    db::ensure_parent_dir(&db_path)?;
    let pool = db::open(&db_path)?;
    {
        let mut conn = pool.get()?;
        wallet::seed_demo_merchants(&mut conn)?;
    }
    tracing::info!(db = %db_path, "sqlite ready");

    // --- emerald charge over the cs tunnel ---
    // The outbound half, held before the tunnel is spawned: it is published with
    // the live connection on connect (and cleared on drop), so the charge path can
    // hold it from the start and simply report "not connected" until then.
    let tunnel_sender = CsHttpSender::default();
    let mc = McLink::new(tunnel_sender.clone());
    let charge = Arc::new(ChargeCoordinator::new(pool.clone(), mc.clone()));

    // Host attestation (MochiOS DEV.md §7.3.10 G4): which in-world character a
    // wallet request may spend the emeralds of. The verifier fetches the Hub's
    // public key over the SAME tunnel, lazily — at this point the tunnel has not
    // even been spawned, so fetching now could only fail.
    //
    // WHICH connectors deserve to be believed is the HUB's policy
    // (`[attestation] trusted_exsoft_attesters`); it refuses to sign for an
    // attester it does not trust, so this backend keeps no second allowlist that
    // could drift out of step with it. See `attest.rs` for the full reasoning.
    let attest_verifier = Arc::new(attest::AttestVerifier::new(mc));
    let challenges = Arc::new(attest::ChallengeStore::new());
    let char_sessions = Arc::new(attest::CharSessionStore::new());

    // Reconciliation: re-send non-terminal emerald ops so a dropped request/ack
    // eventually settles (at-least-once + op-idempotent mod), and age out ops too
    // old to keep retrying. Once at startup, then on a timer — unconditionally,
    // since the tunnel may connect (or drop) at any point in this process's life.
    {
        let charge_rec = charge.clone();
        tokio::spawn(async move {
            loop {
                charge_rec.reconcile().await;
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }

    // Email OTP (verify / 2FA / recovery) over MNN mail. Enabled only when this
    // process has its own identity token; otherwise the wallet degrades to
    // handle+PIN.
    let mailer = otp::Mailer::from_env();

    let state = AppState {
        pool: pool.clone(),
        charge,
        mailer,
        attest: attest_verifier,
        challenges,
        char_sessions,
    };

    // --- bind loopback listener ---
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| anyhow::anyhow!("bind {listen}: {e}"))?;
    let local: SocketAddr = listener
        .local_addr()
        .map_err(|e| anyhow::anyhow!("local_addr: {e}"))?;
    tracing::info!(%local, %mnn, tls = tls_on, "moymoy.cs.mnn wallet backend online");

    // --- embedded cs tunnel (tunnel = "self") ---
    // Held for the process lifetime; dropping the sender winds the tunnel down.
    let _tunnel = if tunnel_on {
        Some(tunnel::spawn(&mnn, local, tunnel_sender)?)
    } else {
        // No tunnel ⇒ no ingress and no charge path: `can_charge` reports false
        // for as long as it is down, which is exactly what this smoke wants.
        tracing::info!("MOYMOY_CS_TUNNEL=0 — embedded tunnel disabled (loopback-only smoke)");
        None
    };

    // --- serve ---
    let app = api::router(state);
    if tls_on {
        let cfg = tls::server_config(&mnn)?;
        serve_tls(listener, app, cfg).await
    } else {
        axum::serve(listener, app)
            .await
            .map_err(|e| anyhow::anyhow!("serve: {e}"))
    }
}

/// Serve the router over TLS (path C: end-to-end through the gateway CONNECT
/// tunnel — the gateway never decrypts). Adapted from `services/rein/src/main.rs`.
async fn serve_tls(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    config: Arc<rustls::ServerConfig>,
) -> anyhow::Result<()> {
    use tower::Service as _;
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "TLS accept failed");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let tls = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(error = %e, "TLS handshake failed");
                    return;
                }
            };
            let io = hyper_util::rt::TokioIo::new(tls);
            let service =
                hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let mut app = app.clone();
                    async move { app.call(req).await }
                });
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                tracing::debug!(error = %e, "connection ended");
            }
        });
    }
}

// ── env helpers ──────────────────────────────────────────────────────────────

/// Read an env var, falling back to `default` when unset or empty.
pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Read a boolean env flag (`1/true/yes/on`), falling back to `default`.
pub fn env_flag(key: &str, default: bool) -> bool {
    match std::env::var(key).ok().filter(|s| !s.is_empty()) {
        Some(v) => matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "on"),
        None => default,
    }
}
