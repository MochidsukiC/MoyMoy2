//! `moymoy-cs` — the MoyMoy wallet backend, reachable at `https://moymoy.cs.mnn`
//! over the MNN overlay.
//!
//! Responsibilities (design-derived from "MochiOS Mobile.html"):
//!   - Serve the wallet HTTP API the app calls (balance / send / pay / charge /
//!     history), persisted to SQLite. The wallet is the single source of truth
//!     for balances and works WITHOUT the Minecraft mod.
//!   - Be reachable as `moymoy.cs.mnn` via an EMBEDDED cs tunnel
//!     (`mochi-hub-cs-sdk`, app.toml `tunnel = "self"`) — no sidecar process.
//!   - (Optional) drive emerald deposit against the in-world mod over the command
//!     bus; degrades to wallet-only when no MC cert is configured.

mod api;
mod charge;
mod command_bus;
mod db;
mod error;
mod identity;
mod tls;
mod tunnel;
mod wallet;

use std::net::SocketAddr;
use std::sync::Arc;

use api::AppState;
use charge::ChargeCoordinator;
use command_bus::CommandBus;

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
    let listen = env_or("MOCHI_APP_LISTEN", &env_or("MOYMOY_CS_LISTEN", "127.0.0.1:7433"));
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

    // --- optional command bus (emerald charge) ---
    let bus = CommandBus::connect().await?;
    let can_charge = bus.is_some();
    let charge = Arc::new(ChargeCoordinator::new(pool.clone(), bus));

    let state = AppState {
        pool: pool.clone(),
        charge,
    };

    // --- bind loopback listener ---
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| anyhow::anyhow!("bind {listen}: {e}"))?;
    let local: SocketAddr = listener
        .local_addr()
        .map_err(|e| anyhow::anyhow!("local_addr: {e}"))?;
    tracing::info!(%local, %mnn, tls = tls_on, can_charge, "moymoy.cs.mnn wallet backend online");

    // --- embedded cs tunnel (tunnel = "self") ---
    // Held for the process lifetime; dropping the sender winds the tunnel down.
    let _tunnel = if tunnel_on {
        Some(tunnel::spawn(&mnn, local))
    } else {
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
