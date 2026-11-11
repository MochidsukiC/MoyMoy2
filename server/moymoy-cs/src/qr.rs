//! QR payments: the public landing pages a printed or on-screen QR opens, and the
//! wallet endpoints that start a payment from a shop's static code.
//!
//! ```text
//! dynamic QR:  shop creates an intent (API key) ─ prints /qr/i/<intent_id>
//! static  QR:  shop prints /qr/s/<code> once    ─ customer picks the amount
//!
//!   scan ▶ landing page ▶ "open MoyMoy" ▶ app ▶ /wallet/payment/intent ▶ approve
//! ```
//!
//! **A QR code carries no authority.** Both landing pages are public and do no
//! database work: all they do is hand the app an id. What the customer sees and
//! pays is decided later, by the approval screen from this backend's own record —
//! the invariant the rest of `payments` keeps ("the only thing the client carries
//! is an `intent_id`"). That is why the pages show no amount, shop or id, and why
//! they never look the id up: a page that answered differently for ids that exist
//! would be an oracle for them.
//!
//! Nothing here moves money or touches the approval path. A static-QR payment is
//! created as an ordinary intent addressed to its customer alone, and is then
//! approved through exactly the PIN / step-up / escrow route every payment takes.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{blocking, replay, AppState};
use crate::auth::AuthedAccount;
use crate::db::{self, now_ms};
use crate::error::ApiError;
use crate::merchant::{self, ok_json, rate_limited, STATIC_QR_PREFIX};
use crate::payments::{self, CreateOutcome, IntentSource, NewIntent};
use crate::wallet;

// ── landing pages (public) ───────────────────────────────────────────────────

/// Marker and length of an intent id (`payments::gen_intent_id`: `pi_` + 128 bits
/// as unpadded base64url = 22 characters).
const INTENT_ID_PREFIX: &str = "pi_";
const TOKEN_BODY_LEN: usize = 22;

/// The script the landing pages load. Served by [`launch_js`]; kept in its own
/// file so the page needs no inline script (see [`CSP`]).
const LAUNCH_JS: &str = include_str!("qr_launch.js");

/// `default-src 'none'` and nothing but our own script: the page has one button
/// and must not be able to load, post to or be framed by anything else.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; \
                   base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Whether `s` is exactly `prefix` + 22 base64url characters. Hand-checked, byte
/// for byte, because it gates what is written into a page.
fn is_token(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix).is_some_and(|body| {
        body.len() == TOKEN_BODY_LEN
            && body
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// Escape a value for a double-quoted HTML attribute. The values reaching it are
/// already restricted to `[A-Za-z0-9_-]` JSON, so this is the second layer, not
/// the only one.
fn html_attr_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A response with the hardening headers every page here carries.
///
/// `Referrer-Policy: no-referrer` because the URL contains the bill's id, and
/// `no-store` for the same reason — neither should outlive the visit in a
/// referrer header or a shared cache.
fn hardened(status: StatusCode, content_type: &'static str, body: String) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// The same answer for every malformed id. Fixed text, never the input.
fn not_found() -> Response {
    hardened(
        StatusCode::NOT_FOUND,
        "text/plain; charset=utf-8",
        "not found".to_string(),
    )
}

/// The landing page for one launch. `params` is the JSON the button hands the app.
fn landing_html(params: &Value) -> String {
    let params = html_attr_escape(&params.to_string());
    format!(
        "<!doctype html>\n\
         <html lang=\"ja\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>MoyMoy で支払う</title>\n\
         <style>\n\
         body{{margin:0;font-family:sans-serif;background:#f4f4f6;color:#222}}\n\
         main{{max-width:28rem;margin:0 auto;padding:2rem 1.25rem;text-align:center}}\n\
         h1{{font-size:1.4rem;margin:0 0 1rem}}\n\
         p{{line-height:1.6;margin:0 0 1.5rem}}\n\
         button{{width:100%;padding:1rem;font-size:1.1rem;font-weight:bold;color:#fff;\
         background:#2f6fed;border:0;border-radius:.6rem}}\n\
         </style>\n\
         </head>\n\
         <body>\n\
         <main>\n\
         <h1>MoyMoy で支払う</h1>\n\
         <p>ボタンを押すと MoyMoy アプリが開きます。\
         お店と金額は、アプリの画面で確認してから支払ってください。</p>\n\
         <button id=\"pay\" type=\"button\" data-app-params=\"{params}\">MoyMoy アプリを開く</button>\n\
         </main>\n\
         <script src=\"/qr/launch.js\" defer></script>\n\
         </body>\n\
         </html>\n"
    )
}

/// `GET /qr/i/:intent_id` — landing page for a dynamic QR.
///
/// Validates the id's shape and nothing more. It does not check that the intent
/// exists: the answer must not depend on it (see the module docs).
pub(crate) async fn landing_intent(Path(intent_id): Path<String>) -> Response {
    if !is_token(&intent_id, INTENT_ID_PREFIX) {
        return not_found();
    }
    hardened(
        StatusCode::OK,
        "text/html; charset=utf-8",
        landing_html(&json!({ "intent_id": intent_id })),
    )
}

/// `GET /qr/s/:code` — landing page for a shop's static QR. Same discipline as
/// [`landing_intent`]: shape only, no lookup.
pub(crate) async fn landing_static(Path(code): Path<String>) -> Response {
    if !is_token(&code, STATIC_QR_PREFIX) {
        return not_found();
    }
    hardened(
        StatusCode::OK,
        "text/html; charset=utf-8",
        landing_html(&json!({ "static_qr": code })),
    )
}

/// `GET /qr/launch.js` — the landing pages' only script.
pub(crate) async fn launch_js() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        LAUNCH_JS,
    )
        .into_response()
}

// ── static QR, customer side (session) ───────────────────────────────────────

/// Intent lifetime for a bill a customer creates at the till. Shorter than the
/// 600 s default for API intents: the customer is standing at the counter and a
/// stale bill only prolongs the one-open-bill-per-shop rule below.
pub const STATIC_QR_INTENT_TTL_SECS: i64 = 300;

/// What the approval screen shows as the line item. Fixed, not customer-supplied:
/// the customer chooses the amount and nothing else.
pub const STATIC_QR_DESCRIPTION: &str = "店頭 QR でのお支払い";

/// Bills one account may create from static codes: 10 per 60 s, across all shops.
pub const RL_STATIC_QR_INTENT: (usize, i64) = (10, 60_000);

#[derive(Deserialize)]
pub(crate) struct StaticQrQuery {
    code: String,
}

/// `GET /wallet/qr/static?code=` — whose code is this?
///
/// What the app shows between the scan and the amount entry. The shop is
/// described by [`payments::merchant_face`], the very function the approval screen
/// uses, so the two cannot show a different name, `@owner_handle` or "new shop"
/// signal for the same shop.
///
/// An unknown code and a stopped or closed shop's code answer identically.
pub(crate) async fn static_qr_lookup(
    State(st): State<AppState>,
    _acct: AuthedAccount,
    Query(q): Query<StaticQrQuery>,
) -> Result<Json<Value>, ApiError> {
    let value = blocking(st.pool, move |conn| {
        Ok::<Value, ApiError>(match merchant::get_active_by_static_qr(conn, &q.code)? {
            Some(m) => json!({ "ok": true, "merchant": payments::merchant_face(conn, &m)? }),
            None => json!({ "ok": false, "error": "unknown_static_qr" }),
        })
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
pub(crate) struct StaticQrIntentReq {
    code: String,
    idem_key: String,
    /// Optional in the struct so `merchant::amount_minor_of` can refuse a missing
    /// one — and the pre-v8 `amount` below — with the same 400s `/merchant/v1`
    /// gives.
    amount_minor: Option<i64>,
    /// The pre-v8 field. Accepted by the parser purely in order to refuse it.
    amount: Option<i64>,
}

/// `POST /wallet/qr/static/intent {code, amount_minor, idem_key}` — create the
/// bill for a static-QR payment, addressed to the caller.
///
/// This creates a request for money; it moves none. The customer then approves it
/// through `/wallet/payment/intent` + `/wallet/payment/approve`, which is
/// unchanged and demands the PIN, step-up and escrow as for any payment. The
/// intent is addressed to the caller (`payer_hint_account_id`), so nobody else can
/// see or approve it.
///
/// One transaction: replay → resolve the shop → check the amount → cancel this
/// customer's earlier open bills at this shop → create. A customer has at most one
/// open static-QR bill per shop, so re-scanning replaces the last bill instead of
/// piling up, and nobody can fill a shop's list with open bills.
pub(crate) async fn static_qr_intent(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<StaticQrIntentReq>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if req.idem_key.trim().is_empty() {
        return Err(ApiError::bad_request("idem_key required"));
    }
    // Before the limiter and the replay, as in `intent_create`: a request this API
    // cannot read has not used anyone's allowance, and must never reach a frozen
    // success.
    let amount_minor = match merchant::amount_minor_of(req.amount_minor, req.amount) {
        Ok(a) => a,
        Err(refused) => return Ok(refused),
    };
    let scope = format!("sqi:{}", acct.account_id);
    if let Err(retry_after_ms) = st.rate.check(
        &scope,
        RL_STATIC_QR_INTENT.0,
        RL_STATIC_QR_INTENT.1,
        now_ms(),
    ) {
        return Ok(rate_limited(retry_after_ms));
    }

    let watch = st.watch.clone();
    let result = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(prev) = db::idem_get(&tx, &req.idem_key, &scope)? {
            return Ok(replay(prev));
        }
        let Some(m) = merchant::get_active_by_static_qr(&tx, &req.code)? else {
            return Ok(json!({ "ok": false, "error": "unknown_static_qr" }));
        };
        // Same bound as every other payment, and BEFORE anything is cancelled: a
        // refused request must leave the customer's earlier bill alone.
        // (Deliberately no `% 100` test, as for sends and approvals.)
        if !(1..=wallet::MAX_AMOUNT).contains(&amount_minor) {
            return Ok(json!({ "ok": false, "error": "bad_amount" }));
        }

        // This customer's earlier STILL-PAYABLE bills at this shop. Selected, then
        // updated, in the one IMMEDIATE transaction: the ids are what gets notified
        // to long-pollers once this commits. One `now` for both statements, and
        // only bills inside their deadline: a lapsed one is the expiry sweep's to
        // mark `expired` — cancelling it would record "withdrawn" for a lapse.
        let now = now_ms();
        let stale: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT intent_id FROM payment_intents \
                 WHERE merchant_id = ?1 AND payer_hint_account_id = ?2 \
                   AND source = ?3 AND state = ?4 AND expires_unix_ms > ?5",
            )?;
            let ids = stmt
                .query_map(
                    params![
                        m.merchant_id,
                        acct.account_id,
                        IntentSource::StaticQr.as_db(),
                        payments::STATE_CREATED,
                        now
                    ],
                    |r| r.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        tx.execute(
            "UPDATE payment_intents SET state = ?5, updated_unix_ms = ?6 \
             WHERE merchant_id = ?1 AND payer_hint_account_id = ?2 \
               AND source = ?3 AND state = ?4 AND expires_unix_ms > ?6",
            params![
                m.merchant_id,
                acct.account_id,
                IntentSource::StaticQr.as_db(),
                payments::STATE_CREATED,
                payments::STATE_CANCELED,
                now
            ],
        )?;

        let out = payments::create(
            &tx,
            &m,
            &NewIntent {
                idem_key: &req.idem_key,
                amount: amount_minor,
                description: STATIC_QR_DESCRIPTION,
                order_ref: None,
                launch_app_id: None,
                payer_hint_account_id: Some(acct.account_id.as_str()),
                expires_in_secs: Some(STATIC_QR_INTENT_TTL_SECS),
                source: IntentSource::StaticQr,
            },
        )?;
        // Anything but `Ok` means the cancellations above must not stand: dropping
        // `tx` rolls them back, so a refused create never leaves the customer
        // without the bill they had. (Every refusal `create` has is checked above
        // or fixed here, so this is a defect, not a user error.)
        let CreateOutcome::Ok(i) = out else {
            return Err(ApiError::internal(format!(
                "static-QR intent for shop {} was refused by create: {out:?}",
                m.merchant_id
            )));
        };
        let v = json!({
            "ok": true,
            "intent_id": i.intent_id,
            "amount_minor": i.amount,
            "expires_unix_ms": i.expires_unix_ms,
        });
        db::idem_put(&tx, &req.idem_key, &scope, &v.to_string())?;
        tx.commit()?;
        // Right after the commit, on this blocking task: a shop long-polling one
        // of the bills just replaced must hear that it is cancelled, and that must
        // not depend on the HTTP handler still being alive to run a later line
        // (a client disconnect drops the handler, never this task).
        for intent_id in &stale {
            watch.notify(intent_id);
        }
        Ok::<Value, ApiError>(v)
    })
    .await;
    Ok(ok_json(result?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_valid_landing(response: Response) {
        assert_eq!(response.status(), 200);
        let headers = response.headers();
        assert!(headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'none'"));
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert!(headers["cache-control"]
            .to_str()
            .unwrap()
            .contains("no-store"));
    }

    #[tokio::test]
    async fn intent_landing_accepts_only_twenty_two_base64url_characters() {
        for id in ["pi_AAAAAAAAAAAAAAAAAAAAAA", "pi_a-b_c0123456789XYZabcd"] {
            assert_valid_landing(landing_intent(Path(id.to_string())).await);
        }
        for id in [
            "pi_AAAAAAAAAAAAAAAAAAAAA",
            "pi_AAAAAAAAAAAAAAAAAAAAAAA",
            "pi_AAAAAAAAAAAAAAAAAAAAA=",
            "pi_AAAAAAAAAAAAAAAAAAAAA.",
            "pi_AAAAAAAAAAAAAAAAAAAAA/",
            "sq_AAAAAAAAAAAAAAAAAAAAAA",
            "",
        ] {
            assert_eq!(
                landing_intent(Path(id.to_string())).await.status(),
                404,
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn static_landing_accepts_only_twenty_two_base64url_characters() {
        for id in ["sq_AAAAAAAAAAAAAAAAAAAAAA", "sq_a-b_c0123456789XYZabcd"] {
            assert_valid_landing(landing_static(Path(id.to_string())).await);
        }
        for id in [
            "sq_AAAAAAAAAAAAAAAAAAAAA",
            "sq_AAAAAAAAAAAAAAAAAAAAAAA",
            "sq_AAAAAAAAAAAAAAAAAAAAA=",
            "sq_AAAAAAAAAAAAAAAAAAAAA.",
            "sq_AAAAAAAAAAAAAAAAAAAAA/",
            "pi_AAAAAAAAAAAAAAAAAAAAAA",
            "",
        ] {
            assert_eq!(
                landing_static(Path(id.to_string())).await.status(),
                404,
                "{id}"
            );
        }
    }
}
