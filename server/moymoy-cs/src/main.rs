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
mod merchant;
mod notify;
mod otp;
mod payments;
mod riskauth;
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

    // Operator commands run against the database and exit — they never bind a
    // port or claim the cs host. See [`admin`].
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("admin") {
        return admin::run(&argv[1..]);
    }

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
    //
    // Expiring payment intents rides along on this same pass rather than getting
    // a timer of its own: it is one indexed UPDATE, it has no deadline of its own
    // (approve carries `expires_unix_ms > now` in its claim, so a late sweep can
    // never let a stale intent be paid), and a second scheduler would be a second
    // thing to get wrong.
    {
        let charge_rec = charge.clone();
        let pool_rec = pool.clone();
        tokio::spawn(async move {
            loop {
                charge_rec.reconcile().await;
                let pool = pool_rec.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let conn = pool.get()?;
                    let n = payments::expire_pass(&conn, db::now_ms())?;
                    if n > 0 {
                        tracing::info!(count = n, "expired unanswered payment intents");
                    }
                    Ok(())
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
                {
                    tracing::error!(error = %e, "payment-intent expiry pass failed");
                }
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }

    // Email OTP (verify / 2FA / recovery) over MNN mail. Enabled only when this
    // process has its own identity token; otherwise the wallet degrades to
    // handle+PIN.
    let mailer = otp::Mailer::from_env();

    // Deposit notifications: drain the transactional outbox (wallet.rs writes
    // it inside each crediting transaction) to the OS notifications service.
    // Best-effort by design; without an identity token rows are discarded.
    notify::spawn(pool.clone());

    let state = AppState {
        pool: pool.clone(),
        charge,
        mailer,
        attest: attest_verifier,
        challenges,
        char_sessions,
        // Throttles, not boundaries: the merchant issuance ceilings and the PIN
        // lockout are what actually bound damage. Both live in this process for
        // the same reason the attestation challenge store does — there is one
        // moymoy-cs, and putting a counter in SQLite would mean a write on every
        // read of every intent.
        rate: Arc::new(merchant::RateLimiter::new()),
        pin_backoff: Arc::new(riskauth::PinBackoff::new()),
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

// ── operator commands ────────────────────────────────────────────────────────

/// Things only the operator does, reachable only from a shell on the host.
///
/// **Deliberately not an HTTP endpoint.** Forcing a refund is the strongest
/// authority in this system — it moves another account's money without that
/// account's consent — and the way to keep it from being reached by a stolen
/// session, a leaked API key or a bug in a route table is not to give it a
/// network surface at all. Whoever can run this binary against the wallet's
/// database can already do anything; nobody else can reach it.
///
/// It also takes no new configuration: the database is `MOYMOY_DB_PATH`, exactly
/// as the server reads it, and SQLite's WAL mode means this can be run while the
/// backend is serving.
mod admin {
    use crate::db;
    use crate::payments::{self, RefundOutcome};
    use crate::{env_or, merchant, wallet};

    const USAGE: &str = "\
usage: moymoy-cs admin <command>

  refund <intent_id> [reason...]   Return a paid intent's money to the payer.
                                   The intent stays `paid` — a refund is a second,
                                   opposite movement, not a rewind.
";

    pub fn run(args: &[String]) -> anyhow::Result<()> {
        match args.first().map(String::as_str) {
            Some("refund") => refund(&args[1..]),
            Some("help") | Some("--help") | Some("-h") | None => {
                print!("{USAGE}");
                Ok(())
            }
            Some(other) => {
                eprint!("{USAGE}");
                anyhow::bail!("unknown admin command `{other}`")
            }
        }
    }

    fn refund(args: &[String]) -> anyhow::Result<()> {
        let Some(intent_id) = args.first() else {
            eprint!("{USAGE}");
            anyhow::bail!("refund needs an intent_id");
        };
        let reason = match args[1..].join(" ") {
            s if s.trim().is_empty() => "運営措置".to_string(),
            s => s,
        };

        let db_path = env_or("MOYMOY_DB_PATH", "moymoy.db");
        let pool = db::open(&db_path)?;
        let mut conn = pool.get()?;

        // Read the whole picture BEFORE moving anything, so the report can name
        // the accounts even for the outcomes that change nothing.
        let Some(before) = payments::get(&conn, intent_id)? else {
            anyhow::bail!("no intent `{intent_id}` in {db_path}");
        };
        let shop = merchant::get(&conn, &before.merchant_id)?;
        let payer = before.payer_account_id.clone();

        let outcome = payments::force_refund(&mut conn, intent_id, &reason)?;
        match outcome {
            RefundOutcome::Ok { tx_id, amount } => {
                let payer = payer.unwrap_or_default();
                let shop_account = shop.as_ref().map(|m| m.account_id.as_str()).unwrap_or("?");
                // Every amount below is minor units (1/100 エメ) and is rendered,
                // not printed: this report is what an operator reads to decide
                // whether a refund did what they meant, and a raw integer states
                // it at a hundred times its value.
                println!("refunded {} エメ", wallet::format_eme(amount));
                println!("  intent      {intent_id}");
                println!(
                    "  merchant    {} {} (account {shop_account})",
                    before.merchant_id,
                    shop.as_ref().map(|m| m.name.as_str()).unwrap_or("?"),
                );
                println!("  payer       {payer}");
                println!("  reason      {reason}");
                println!("  refund tx   {tx_id}");
                // `paid` is terminal by design; the refund is recorded alongside
                // it rather than instead of it.
                println!("  intent state {} -> {} (refunded)", before.state, before.state);
                println!(
                    "  balances    merchant {} エメ / payer {} エメ",
                    wallet::format_eme(wallet::balance(&conn, shop_account)?),
                    wallet::format_eme(wallet::balance(&conn, &payer)?),
                );
                Ok(())
            }
            // Everything below leaves the wallet exactly as it was, and says so
            // rather than exiting 0 on a refund that did not happen.
            RefundOutcome::UnknownIntent => anyhow::bail!("no intent `{intent_id}`"),
            RefundOutcome::NotPaid { state } => anyhow::bail!(
                "intent `{intent_id}` is `{state}`, not `paid` — there is nothing to return"
            ),
            RefundOutcome::AlreadyRefunded => anyhow::bail!(
                "intent `{intent_id}` was already refunded at {} (refund tx {})",
                before.refunded_unix_ms.unwrap_or_default(),
                before.refund_tx_id.as_deref().unwrap_or("?")
            ),
            RefundOutcome::MerchantShort { balance } => anyhow::bail!(
                "merchant `{}` holds {} エメ but owes {} — merchant revenue is NOT escrowed \
                 (DEV.md: accepted risk), so a shop that withdrew its takings to the MC world \
                 leaves nothing to reverse. Nothing was moved; retry if it is funded again.",
                before.merchant_id,
                wallet::format_eme(balance),
                wallet::format_eme(before.amount)
            ),
        }
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
