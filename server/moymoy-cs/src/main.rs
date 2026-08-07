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

// `admin_api` and the `admin` module further down are different things and the
// names are not interchangeable: `admin` is the operator CLI (it moves money and
// has no network surface at all), `admin_api` is the read-only console API. The
// suffix is also forced — `mod admin` below already owns that name in this crate.
mod admin_api;
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
        // Before anything can be paid, and fatal if it fails: every approval
        // transfers into this account, so a wallet without it would refuse every
        // payment with `unknown_target`. Better to not come up at all than to come
        // up unable to take money.
        wallet::seed_escrow_account(&conn)?;
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
    //
    // Releasing escrowed payments rides along for the same reason, and is late by
    // at most one cycle — which costs nothing, because the money is already the
    // merchant's claim and `release_due_unix_ms` is a floor, not a schedule.
    // Unlike expiry it moves money, so it works one intent at a time inside its
    // own transaction rather than as a bulk UPDATE (`payments::release_pass`).
    {
        let charge_rec = charge.clone();
        let pool_rec = pool.clone();
        tokio::spawn(async move {
            loop {
                charge_rec.reconcile().await;
                let pool = pool_rec.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let mut conn = pool.get()?;
                    let n = payments::expire_pass(&conn, db::now_ms())?;
                    if n > 0 {
                        tracing::info!(count = n, "expired unanswered payment intents");
                    }
                    // After the expiry pass, not before: an expired intent was
                    // never paid, so it can never be one of the rows this releases,
                    // and running the cheap bulk statement first keeps the write
                    // lock held for the shortest time.
                    let released = payments::release_pass(&mut conn, db::now_ms())?;
                    if released > 0 {
                        tracing::info!(count = released, "released escrowed payments");
                    }
                    Ok(())
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
                {
                    tracing::error!(error = %e, "payment-intent housekeeping pass failed");
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

    // --- operator plane (SECOND listener) ---
    // Never reachable from the overlay, and not because of where this sits in
    // `main`: [`spawn_admin_listener`] binds its listener and moves it straight
    // into its serving task, so no admin `SocketAddr` is ever produced that
    // `tunnel::spawn` could be handed — at any position in this function. The
    // address below exists for the log line.
    // Unset ⇒ no operator plane at all (see [`admin_listen`]).
    if let Some(admin) = admin_listen(std::env::var("MOYMOY_ADMIN_LISTEN").ok()) {
        let admin_local = spawn_admin_listener(&admin, state.clone()).await?;
        tracing::info!(
            %admin_local,
            "operator plane online — admin_api only, no TLS, no CORS, not on the tunnel"
        );
    }

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
/// tunnel — the gateway never decrypts). "path C" is local shorthand, defined
/// nowhere else in this repo; [`tls`] spells out what it means and which
/// MochiOS2.0 DEV.md section covers it. Adapted from `services/rein/src/main.rs`.
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

// ── operator plane ───────────────────────────────────────────────────────────

/// Where the operator plane should listen, from whatever the environment
/// supplied — or `None`, meaning this process serves no operator plane at all.
///
/// **No default is invented**, and that is the decision, not an omission: the
/// port belongs to the MochiOS launcher, which allocates a free one and injects
/// it as `MOYMOY_ADMIN_LISTEN` because `app.toml` declares
/// `[admin] listen_env = "MOYMOY_ADMIN_LISTEN"`. A fallback chosen here would be
/// a port nobody reviewed, listening on a surface that lists every balance in the
/// wallet. So this reads the value the way [`tunnel::spawn`] reads
/// `MOCHI_SVC_IDENTITY_TOKEN` (present and non-empty, or absent) rather than
/// through [`env_or`], which demands a default.
///
/// Takes the value instead of reading the variable so both branches are testable:
/// cargo runs a binary's tests as threads in ONE process, where a `set_var` would
/// race every other test in it.
fn admin_listen(configured: Option<String>) -> Option<String> {
    configured.filter(|s| !s.is_empty())
}

/// Parse `listen` and refuse anything that is not a loopback address.
///
/// **This is the enforcement behind every "loopback-only" claim in this module.**
/// [`admin_api`] has no authentication of any kind — it answers every balance,
/// every card face, and the cross-account ledger to whoever connects. What makes
/// that safe is that only the Hub, on this same host, can reach it. Nothing else
/// checks that. Without this function, `MOYMOY_ADMIN_LISTEN=0.0.0.0:9999` would
/// publish the whole wallet to the network, and the code would have been doing
/// exactly what it was told.
///
/// The launcher always injects `127.0.0.1:<port>` (MochiOS2.0's
/// `launcher/spawn.rs` applies it AFTER `app.toml`'s `[env]`, so the manifest
/// cannot override it). A value that arrives here non-loopback therefore means
/// something other than the launcher started this process — which is precisely
/// the case that needs stopping, not accommodating.
///
/// Checked BEFORE binding. Binding first and inspecting `local_addr()` after
/// would open the socket, however briefly, on every interface.
///
/// # Two deliberate narrownesses
///
/// A hostname (`localhost:9999`) does not parse as a `SocketAddr` and is
/// refused. Accepting one would require resolving it, and what a name resolves
/// to is not ours to decide — a check that consults DNS is a check an attacker
/// can influence. The launcher sends a literal address, so nothing real needs
/// this.
///
/// An IPv4-mapped IPv6 address (`::ffff:127.0.0.1`) is NOT loopback to
/// [`std::net::Ipv6Addr::is_loopback`], so it is refused too, even though it
/// names a loopback host. Same reasoning as the console's CIDR matching in
/// MochiOS2.0's `console/net.rs`: silently unwrapping the mapping makes one rule
/// span two address families. Refusing costs nothing here.
fn require_loopback(listen: &str) -> anyhow::Result<SocketAddr> {
    let addr: SocketAddr = listen.parse().map_err(|_| {
        anyhow::anyhow!(
            "bind operator plane {listen}: not a host:port address. The operator \
             plane takes a literal loopback socket address (e.g. 127.0.0.1:0); \
             hostnames are not resolved here on purpose"
        )
    })?;
    if !addr.ip().is_loopback() {
        anyhow::bail!(
            "bind operator plane {listen}: refusing to serve the operator plane \
             off loopback. It carries every account balance, card face and the \
             cross-account ledger with NO authentication — being reachable only \
             from this host is the whole security boundary. The Hub launcher \
             injects 127.0.0.1:<port>; a different value means this process was \
             started some other way"
        );
    }
    Ok(addr)
}

/// Bind the operator plane on `listen`, serve [`admin_api::router`] there, and
/// return the address actually bound (so `127.0.0.1:0` resolves to a real port).
///
/// **The listener never leaves this function**, and that — not the call's
/// position in `main` — is what keeps the operator plane off the overlay. It is
/// created here and moved straight into the serving task, so the only thing that
/// escapes is a `SocketAddr` for the log line and for tests. [`tunnel::spawn`]
/// takes one `SocketAddr` and `main` hands it the PUBLIC listener's; moving this
/// call earlier or later cannot change that, because no admin listener exists
/// anywhere for it to be given.
///
/// **No TLS and no CORS, both deliberately.** `MOYMOY_CS_TLS` gates the wallet
/// listener only, and this function sits outside that branch, so the flag cannot
/// reach here — deployments DO set `MOYMOY_CS_TLS = "1"` (see
/// `app_backends/moymoy/app.toml`) while this plane stays plaintext. That is the
/// intended shape: it is loopback-only — enforced by [`require_loopback`] rather
/// than merely intended — reached by the Hub on the same host, and
/// the Hub's side dials `http://` unconditionally
/// (`hub/server/src/launcher/admin_proxy.rs`'s `admin_url`, which hardcodes the
/// scheme and re-checks it). Do not "fix" one half without the other. CORS is
/// covered by
/// `admin_api::tests::the_admin_router_answers_on_its_own_and_carries_no_cors`.
///
/// A bind failure is a hard error, matching [`tunnel::spawn`]'s posture: the
/// operator declared an admin plane in `app.toml`, and one the console cannot
/// reach is worse silent than loud.
async fn spawn_admin_listener(listen: &str, state: AppState) -> anyhow::Result<SocketAddr> {
    let addr = require_loopback(listen)?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("bind operator plane {listen}: {e}"))?;
    let local = listener
        .local_addr()
        .map_err(|e| anyhow::anyhow!("operator plane local_addr: {e}"))?;
    let app = admin_api::router(state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "operator plane listener stopped");
        }
    });
    Ok(local)
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
                // WHERE the money came back from, and the ledger rows to reconcile
                // it against. Since v9 that is not always the shop: a payment still
                // in escrow is returned by MoyMoy, and an operator checking their
                // own books needs to know which account moved and which rows the
                // release (if any) had already written.
                println!("  refunded from {}", payments::escrow_stage(&before));
                if let Some(id) = &before.release_tx_id {
                    println!("  release tx  {id} (escrow -> merchant, already paid out)");
                }
                if let Some(id) = &before.escrow_refund_tx_id {
                    println!("  escrow refund tx {id} (unfulfilled share, already returned)");
                }
                // `paid` is terminal by design; the refund is recorded alongside
                // it rather than instead of it.
                println!("  intent state {} -> {} (refunded)", before.state, before.state);
                println!(
                    "  balances    merchant {} エメ / payer {} エメ / escrow {} エメ",
                    wallet::format_eme(wallet::balance(&conn, shop_account)?),
                    wallet::format_eme(wallet::balance(&conn, &payer)?),
                    wallet::format_eme(wallet::balance(&conn, wallet::escrow_account_id())?),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An unset (or blank) `MOYMOY_ADMIN_LISTEN` means NO operator plane — the
    /// deployment that never declared `[admin]` in its `app.toml` must start
    /// exactly as it did before this existed, with no port invented for it.
    ///
    /// Driven by value rather than by `set_var`: cargo runs this binary's tests as
    /// threads in one process, so an env write here would race every other test.
    /// `main`'s one-line `if let` over `std::env::var(..).ok()` is the only part
    /// not covered, and it is inspected rather than tested.
    #[test]
    fn without_the_env_var_there_is_no_operator_plane() {
        assert_eq!(admin_listen(None), None);
        assert_eq!(admin_listen(Some(String::new())), None);
        assert_eq!(
            admin_listen(Some("127.0.0.1:51999".to_string())),
            Some("127.0.0.1:51999".to_string()),
            "a launcher-injected address is taken verbatim"
        );
    }

    /// With an address, the operator plane really answers on it — asserted over a
    /// real socket with a real HTTP client, not by inspecting a `Router`.
    ///
    /// The second half is the one that matters as much: this listener serves
    /// `admin_api` and NOTHING else. If it ever also carried `api::router`, the
    /// wallet API would be duplicated onto a port with no TLS, and `/healthz`
    /// answering here is how that would first show up.
    #[tokio::test]
    async fn the_operator_plane_answers_where_it_was_told_to_and_serves_only_admin_api() {
        let addr = spawn_admin_listener("127.0.0.1:0", crate::admin_api::tests::app_state())
            .await
            .expect("the operator plane binds");
        // NOTE: this asserts a property of the address THIS TEST chose to pass
        // in. It enforces nothing — pass "0.0.0.0:0" and it would simply assert
        // about that instead. The invariant is enforced by `require_loopback`
        // and pinned by `a_non_loopback_operator_plane_is_refused_before_binding`
        // below; do not read this line as the guard.
        assert!(addr.ip().is_loopback(), "must not bind off loopback: {addr}");

        let client = reqwest::Client::new();
        let admin = client
            .get(format!("http://{addr}/admin/api/overview"))
            .send()
            .await
            .expect("the operator plane answers");
        assert_eq!(admin.status(), 200);

        let wallet = client
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .expect("the operator plane answers");
        assert_eq!(
            wallet.status(),
            404,
            "the operator plane must serve admin_api alone — a wallet route here \
             means api::router was merged onto it"
        );
    }

    /// A declared operator plane that cannot bind stops the backend, rather than
    /// leaving it running with a console surface nobody can reach. Same posture as
    /// `tunnel::spawn`'s missing-identity error.
    #[tokio::test]
    async fn a_declared_operator_plane_that_cannot_bind_is_a_hard_error() {
        let err = spawn_admin_listener("not-an-address", crate::admin_api::tests::app_state())
            .await
            .expect_err("an unusable address must not be shrugged off");
        assert!(
            err.to_string().contains("bind operator plane"),
            "the error must say what failed: {err}"
        );
    }

    /// The loopback rule itself, with no socket involved.
    ///
    /// `admin_api` has no authentication, so "only this host can reach it" is the
    /// entire security boundary. This is the function that makes that true.
    #[test]
    fn only_a_literal_loopback_address_is_accepted() {
        for ok in ["127.0.0.1:0", "127.0.0.1:51999", "127.9.9.9:80", "[::1]:0"] {
            assert!(
                require_loopback(ok).is_ok(),
                "the launcher's own shape must be accepted: {ok}"
            );
        }
        // The wildcard is the dangerous one: it is what an operator types when
        // they mean "let me reach it from my laptop", and it publishes every
        // balance to the network.
        for bad in ["0.0.0.0:0", "0.0.0.0:9999", "192.0.2.1:0", "[::]:0"] {
            // `expect_err` prints the accepted address, so the failure names the
            // offender without interpolating here.
            let err = require_loopback(bad).expect_err("a non-loopback address must be refused");
            assert!(
                err.to_string().contains("off loopback"),
                "the refusal must name the reason: {err}"
            );
        }
        // Refused on purpose, both of them — see `require_loopback`'s docs.
        assert!(
            require_loopback("localhost:9999").is_err(),
            "a hostname is not resolved here"
        );
        assert!(
            require_loopback("[::ffff:127.0.0.1]:0").is_err(),
            "an IPv4-mapped v6 address is not unwrapped into a v4 rule"
        );
    }

    /// The rule reaches the real code path, and it fires BEFORE any socket opens.
    ///
    /// `0.0.0.0:0` binds successfully on every machine this could run on, so an
    /// `Err` here can only have come from [`require_loopback`] — if the check were
    /// removed, this call would succeed rather than fail differently. That is what
    /// makes this test discriminating rather than merely passing.
    #[tokio::test]
    async fn a_non_loopback_operator_plane_is_refused_before_binding() {
        let err = spawn_admin_listener("0.0.0.0:0", crate::admin_api::tests::app_state())
            .await
            .expect_err("the wallet must never be published off loopback");
        assert!(
            err.to_string().contains("off loopback"),
            "must be refused by the loopback rule, not by a bind failure: {err}"
        );
    }
}
