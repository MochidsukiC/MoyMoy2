//! `/admin/api/*` — the read-only operator view, for the MochiOS Hub console.
//!
//! Every route here answers with JSON and nothing else. There is no HTML, no
//! asset and no template: the console UI is served by the Hub from a single
//! origin, and this backend only supplies its own numbers. Nothing in this
//! module writes to the database and nothing in it moves money — the one
//! operator action that does (`force_refund`) stays where `main.rs`'s `admin`
//! module put it, off the network entirely.
//!
//! # Mounted on a SECOND listener — and [`crate::api::router`] is still the
//! # wrong place for it
//!
//! [`router`] is served by [`crate::spawn_admin_listener`], on its own loopback
//! socket that is never handed to the tunnel. It must stay off
//! [`crate::api::router`], and the reason is the finding this module was written
//! to record.
//!
//! `main` binds a listener on `127.0.0.1:<port>` and hands that same address to
//! [`crate::tunnel::spawn`], which claims `moymoy.cs.mnn` and forwards inbound
//! overlay traffic to it — unfiltered, with no notion of paths
//! (`CsTunnelConfig::local_target` is a `SocketAddr` and nothing more). `main`'s
//! own docs say it: "`moymoy.cs.mnn` is our ONLY ingress". So a route added to
//! that router is reachable by anything on the MNN overlay that can resolve
//! `https://moymoy.cs.mnn/`, not only by a Hub proxying from the host.
//!
//! Three consequences, and the third is why this is a separate socket rather
//! than a guard on the shared one:
//!
//! 1. **What is behind these five routes** is every account balance and card
//!    face, the whole cross-account ledger, and every payment intent with its
//!    payer. Read-only, but not low-stakes.
//! 2. **This unit implements no authentication.** The Hub authenticates the
//!    operator and proxies; that is the whole design, and it only holds if the
//!    Hub is the one who can reach these.
//! 3. **A peer-address check cannot substitute.** Tunnel-forwarded requests
//!    arrive on the loopback socket like every other request, so `127.0.0.1`
//!    does not distinguish a Hub proxy from overlay traffic. The obvious guard
//!    does not exist.
//!
//! ## How this is wired
//!
//! A SECOND loopback listener that is never handed to the tunnel, so the overlay
//! has no route to it at all. It costs no second cs claim — the claim is made by
//! the tunnel at its handshake, and this listener is not part of one.
//!
//! The port is not this module's call and never was: the MochiOS launcher
//! allocates one and injects it as `MOYMOY_ADMIN_LISTEN`, declared by
//! `app.toml`'s `[admin] listen_env`. **No default is invented here** — with the
//! variable unset there is simply no admin listener, because a fallback port
//! chosen locally is exactly the kind of default that ends up in production
//! unreviewed.
//!
//! `admin_routes_are_not_mounted_on_the_public_router` keeps pinning the half
//! that matters: merge these onto the tunnel's router and it fails.
//!
//! # CORS (for whoever mounts this)
//!
//! These routes must carry NO CORS headers. Every other route in this process
//! inherits `allow_origin(Any)` from `api::router`'s `CorsLayer`, and an
//! operator surface listing every balance must not — that would let any page in
//! any browser read it with the reader's credentials. The console reaches these
//! through the Hub on a single origin, so there is no cross-origin caller to
//! answer a preflight for in the first place.
//!
//! On its own listener this is free: no layer, no headers. If it is ever merged
//! into a router that HAS a `CorsLayer`, merge it *after* the `.layer()` call —
//! axum's `Router::layer` wraps only routes registered before it ("Additional
//! routes added after `layer` is called will not have the middleware added" —
//! axum 0.7 `docs/routing/layer.md`).
//! `merging_before_the_layer_would_hand_admin_routes_cors` demonstrates the
//! failure that ordering avoids, so the hazard is pinned rather than described.

pub mod read;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{blocking, AppState};
use crate::db::now_ms;
use crate::error::ApiError;

use read::Cursor;

/// The operator routes, as their own `Router`.
///
/// Served by [`crate::spawn_admin_listener`] and by nothing else — read the
/// module docs before changing that, because the router this looks like it
/// belongs on ([`crate::api::router`]) is the cs tunnel's ingress.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/admin/api/overview", get(overview))
        .route("/admin/api/merchants", get(merchants))
        .route("/admin/api/ledger", get(ledger))
        .route("/admin/api/intents", get(intents))
        .route("/admin/api/accounts", get(accounts))
        .with_state(state)
}

/// Trim a query string, treating an empty one as absent — `?q=` from a cleared
/// search box means "no filter", not "match the empty string".
fn present(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

// ── overview ─────────────────────────────────────────────────────────────────

async fn overview(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    let view = blocking(st.pool, move |conn| {
        read::overview(conn, now_ms()).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({ "ok": true, "overview": view })))
}

// ── merchants ────────────────────────────────────────────────────────────────

async fn merchants(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    let list = blocking(st.pool, move |conn| {
        read::merchants(conn).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({ "ok": true, "merchants": list })))
}

// ── ledger ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LedgerQuery {
    q: Option<String>,
    /// One of `read::LEDGER_KINDS`, or `all`. Absent folds `receive` out.
    kind: Option<String>,
    limit: Option<i64>,
    /// A `next_before` from a previous page, handed back untouched.
    before: Option<String>,
}

async fn ledger(
    State(st): State<AppState>,
    Query(q): Query<LedgerQuery>,
) -> Result<Json<Value>, ApiError> {
    // Refused rather than ignored. A `kind` this ledger has never written is a
    // filter that would silently return nothing, and the operator would read
    // that as "there is no such activity".
    if let Some(k) = present(&q.kind) {
        if k != "all" && !read::LEDGER_KINDS.contains(&k) {
            return Err(ApiError::bad_request(format!(
                "unknown kind `{k}` — this ledger writes {} (or `all`)",
                read::LEDGER_KINDS.join(", ")
            )));
        }
    }
    // Same reasoning: a cursor we cannot read would page back to the top forever
    // rather than end.
    let before = match present(&q.before) {
        Some(raw) => Some(
            Cursor::parse(raw)
                .ok_or_else(|| ApiError::bad_request("`before` is not a cursor this API issued"))?,
        ),
        None => None,
    };

    let page = blocking(st.pool, move |conn| {
        read::ledger(
            conn,
            present(&q.q),
            present(&q.kind),
            q.limit.unwrap_or(read::DEFAULT_LIMIT),
            before,
        )
        .map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({
        "ok": true,
        "rows": page.rows,
        "next_before": page.next_before,
        "truncated": page.truncated,
    })))
}

// ── intents ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct IntentQuery {
    /// One of `read::STAGES`. Absent ⇒ every stage.
    stage: Option<String>,
    /// A single intent, for the console's detail view.
    intent_id: Option<String>,
    limit: Option<i64>,
}

async fn intents(
    State(st): State<AppState>,
    Query(q): Query<IntentQuery>,
) -> Result<Json<Value>, ApiError> {
    if let Some(s) = present(&q.stage) {
        if !read::STAGES.contains(&s) {
            return Err(ApiError::bad_request(format!(
                "unknown stage `{s}` — escrow_stage returns {}",
                read::STAGES.join(", ")
            )));
        }
    }
    let value = blocking(st.pool, move |conn| {
        let now = now_ms();
        if let Some(id) = present(&q.intent_id) {
            // `null`, not a 404: "no such intent" is an ordinary answer to an
            // operator's search, the same way the domain outcomes elsewhere in
            // this process are 200s (see `error.rs`).
            let intent = read::intent_by_id(conn, id, now)?;
            return Ok::<Value, ApiError>(json!({ "ok": true, "intent": intent }));
        }
        let page = read::intents(
            conn,
            present(&q.stage),
            q.limit.unwrap_or(read::DEFAULT_LIMIT),
            now,
        )?;
        Ok(json!({
            "ok": true,
            "intents": page.intents,
            "truncated": page.truncated,
        }))
    })
    .await?;
    Ok(Json(value))
}

// ── accounts ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AccountQuery {
    q: Option<String>,
    limit: Option<i64>,
}

async fn accounts(
    State(st): State<AppState>,
    Query(q): Query<AccountQuery>,
) -> Result<Json<Value>, ApiError> {
    let (list, truncated) = blocking(st.pool, move |conn| {
        read::accounts(conn, present(&q.q), q.limit.unwrap_or(read::DEFAULT_LIMIT))
            .map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({
        "ok": true,
        "accounts": list,
        "truncated": truncated,
    })))
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::AppState;
    use axum::body::Body;
    use axum::http::Request;
    use tower::Service as _;

    /// A real [`AppState`], built the way `merchant.rs`'s own router test builds
    /// one. Nothing here reaches the tunnel: these routes only ever touch the
    /// pool.
    /// Shared with `main`'s own tests (which serve this router on a real socket),
    /// so there is one construction of the operator plane's state rather than two
    /// that can drift.
    pub(crate) fn app_state() -> AppState {
        let pool = crate::db::open_memory().expect("in-memory pool");
        {
            let conn = pool.get().unwrap();
            crate::wallet::seed_escrow_account(&conn).unwrap();
        }
        let mc = crate::mc::McLink::new(mochi_hub_cs_sdk::CsHttpSender::default());
        AppState {
            pool: pool.clone(),
            charge: std::sync::Arc::new(crate::charge::ChargeCoordinator::new(
                pool.clone(),
                mc.clone(),
            )),
            mailer: crate::otp::Mailer::from_env(),
            attest: std::sync::Arc::new(crate::attest::AttestVerifier::new(mc)),
            challenges: std::sync::Arc::new(crate::attest::ChallengeStore::new()),
            char_sessions: std::sync::Arc::new(crate::attest::CharSessionStore::new()),
            rate: std::sync::Arc::new(crate::merchant::RateLimiter::new()),
            pin_backoff: std::sync::Arc::new(crate::riskauth::PinBackoff::new()),
        }
    }

    /// Every operator route, so a new one cannot be added without the guards
    /// below covering it too.
    const ADMIN_PATHS: [&str; 5] = [
        "/admin/api/overview",
        "/admin/api/merchants",
        "/admin/api/ledger",
        "/admin/api/intents",
        "/admin/api/accounts",
    ];

    /// Drive a router exactly as `main::serve_tls` drives it (`app.call(req)`),
    /// as a cross-origin caller, and report the status and the CORS header.
    async fn get(app: &mut axum::Router, path: &str) -> (axum::http::StatusCode, Option<String>) {
        let req = Request::builder()
            .uri(path)
            .header("origin", "https://evil.example")
            .body(Body::empty())
            .unwrap();
        let res = app.call(req).await.expect("the router answers");
        let header = res
            .headers()
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap().to_string());
        (res.status(), header)
    }

    /// **The exposure guard.** These routes must not be on the router the cs
    /// tunnel forwards to.
    ///
    /// `api::router` is what `main` hands to the listener that
    /// `tunnel::spawn` publishes as `moymoy.cs.mnn`, so a route reachable here
    /// is a route reachable from the whole MNN overlay — unauthenticated, and
    /// listing every balance in the wallet. `404` is the assertion: merge
    /// `admin_api::router()` back into `api::router` and this fails.
    #[tokio::test]
    async fn admin_routes_are_not_mounted_on_the_public_router() {
        let mut app = crate::api::router(app_state());
        for path in ADMIN_PATHS {
            let (status, _) = get(&mut app, path).await;
            assert_eq!(
                status,
                axum::http::StatusCode::NOT_FOUND,
                "{path} is reachable on the tunnel's router — it is exposed to the \
                 MNN overlay with no authentication"
            );
        }
    }

    /// The wallet routes must keep exactly the CORS they had.
    ///
    /// The guard above would pass just as well if the whole `CorsLayer` had been
    /// deleted, which is the change this unit was told not to make. This is the
    /// half that notices.
    #[tokio::test]
    async fn the_wallet_routes_still_carry_their_cors() {
        let mut app = crate::api::router(app_state());
        for path in ["/healthz", "/wallet/status", "/auth/config"] {
            let (status, cors) = get(&mut app, path).await;
            assert!(status.is_success(), "{path} answered {status}");
            assert_eq!(
                cors.as_deref(),
                Some("*"),
                "{path} lost the CORS it had before admin_api existed"
            );
        }
    }

    /// Mounted alone — the shape it will actually be served in — every route
    /// answers, and none of them carries a CORS header.
    ///
    /// Without this the handlers would have no HTTP-level coverage at all while
    /// they are unmounted: a route that 500s on its own listener would look
    /// exactly like a route that is correctly absent from the public one.
    #[tokio::test]
    async fn the_admin_router_answers_on_its_own_and_carries_no_cors() {
        let mut app = super::router(app_state());
        for path in ADMIN_PATHS {
            let (status, cors) = get(&mut app, path).await;
            assert!(status.is_success(), "{path} answered {status}");
            assert_eq!(cors, None, "{path} answered a cross-origin caller");
        }
    }

    /// A trap left armed for whoever eventually mounts these.
    ///
    /// On their own listener these routes need no CORS reasoning — there is no
    /// layer to inherit. But if they are ever merged into a router that HAS a
    /// `CorsLayer`, the merge has to come after the `.layer()` call, and getting
    /// that backwards is silent: the routes work, and every browser on the
    /// internet can read them.
    ///
    /// So this builds the WRONG wiring on purpose and asserts it does the wrong
    /// thing. It is what makes the module docs' claim about axum's ordering a
    /// checked fact rather than a remembered one — if a future axum applied
    /// `layer` to routes registered after it as well, this fails and the advice
    /// gets revisited before somebody follows it.
    #[tokio::test]
    async fn merging_before_the_layer_would_hand_admin_routes_cors() {
        let cors = tower_http::cors::CorsLayer::new().allow_origin(tower_http::cors::Any);
        let mut app = axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .merge(super::router(app_state()))
            .layer(cors);
        let req = Request::builder()
            .uri("/admin/api/overview")
            .header("origin", "https://evil.example")
            .body(Body::empty())
            .unwrap();
        let res = app.call(req).await.unwrap();
        assert_eq!(
            res.headers()
                .get("access-control-allow-origin")
                .map(|v| v.to_str().unwrap()),
            Some("*"),
            "merge-before-layer no longer applies the layer — the module docs' \
             advice about `.merge()` ordering is now wrong, and anyone mounting \
             these routes onto a CORS-bearing router would be misled by it"
        );
    }
}
