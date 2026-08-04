//! PaymentIntent: the record of a shop asking a specific wallet for a specific
//! amount, and the state machine that turns it into a transfer exactly once.
//!
//! ```text
//! created ──┬── approve (session + PIN, transfer in the same tx) ──▶ paid
//!           ├── decline (session) ─────────────────────────────────▶ declined
//!           ├── cancel  (merchant API key) ────────────────────────▶ canceled
//!           └── TTL / sweep ───────────────────────────────────────▶ expired
//! ```
//!
//! Every terminal state is final. `paid` in particular is never rewound — an
//! operator-forced refund ([`force_refund`]) is a second movement in the opposite
//! direction, so the ledger says what happened rather than pretending it did not.
//!
//! **What makes this safe is one statement.** Approve claims the intent with
//!
//! ```sql
//! UPDATE payment_intents SET state='paid', payer_account_id=?, updated_unix_ms=?
//!  WHERE intent_id=? AND state='created' AND expires_unix_ms > ?now
//! ```
//!
//! and moves money only when that changed exactly one row. Two phones approving
//! at once, an approval landing a millisecond before the deadline, a merchant
//! cancelling while the customer is typing their PIN — all of them are the same
//! race, and all of them are decided by whether this UPDATE found the row in the
//! state it requires. This mirrors `charge/withdraw.rs`, where the refund lives
//! inside the state transition for exactly the same reason.
//!
//! **The amount and the recipient are read from the stored intent**, never from
//! the approving request. The client carries an `intent_id` and nothing else that
//! is believed, so there is no field it could tamper with.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::extract::{Query, State};
use axum::Json;
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{blocking, AppState};
use crate::auth::{self, AuthedAccount};
use crate::db::now_ms;
use crate::error::ApiError;
use crate::merchant::{self, IssueGuard, MerchantRow, TextReject};
use crate::riskauth::{self, PinBackoff, Requirement};
use crate::wallet::{self, TxResult};

/// Issued, unanswered, still inside its deadline.
pub const STATE_CREATED: &str = "created";
/// Paid. Terminal.
pub const STATE_PAID: &str = "paid";
/// The payer refused it. Terminal.
pub const STATE_DECLINED: &str = "declined";
/// The merchant withdrew it. Terminal.
pub const STATE_CANCELED: &str = "canceled";
/// The deadline passed unanswered. Terminal.
pub const STATE_EXPIRED: &str = "expired";

/// How long an unanswered intent stays payable, and the range a merchant may ask
/// for. Bounded at both ends: too short and a customer cannot finish typing a
/// PIN, too long and an approval screen can be raised hours after the shop that
/// asked for it was closed.
pub const DEFAULT_TTL_SECS: i64 = 600;
pub const MIN_TTL_SECS: i64 = 60;
pub const MAX_TTL_SECS: i64 = 1_800;

/// The idempotency namespace one merchant's `idem_key`s live in.
///
/// Scoped per merchant on the same reasoning schema v5 applied to charges: a bare
/// `"pay"` scope lets one caller's key collide with another's and replay a
/// response that was never theirs. (`send`/`pay` still use bare scopes — that is
/// the bad precedent, not the pattern to copy.)
pub fn intent_scope(merchant_id: &str) -> String {
    format!("mi:{merchant_id}")
}

/// A payment intent as stored.
#[derive(Debug, Clone)]
pub struct Intent {
    pub intent_id: String,
    pub merchant_id: String,
    pub amount: i64,
    pub description: String,
    pub order_ref: Option<String>,
    pub state: String,
    pub payer_account_id: Option<String>,
    pub payer_hint_account_id: Option<String>,
    pub launch_app_id: Option<String>,
    pub tx_id: Option<String>,
    pub refunded_unix_ms: Option<i64>,
    pub refund_tx_id: Option<String>,
    pub created_unix_ms: i64,
    pub expires_unix_ms: i64,
}

impl Intent {
    /// The state to *report*, which is not always the state stored: the sweep
    /// runs every 30 seconds, and an intent whose deadline passed 5 seconds ago
    /// is already unpayable (approve's guard says so) whether or not a row has
    /// been rewritten. Time is the authority here; the sweep is housekeeping.
    pub fn effective_state(&self, now: i64) -> &str {
        if self.state == STATE_CREATED && self.expires_unix_ms <= now {
            STATE_EXPIRED
        } else {
            &self.state
        }
    }
}

const INTENT_COLS: &str = "intent_id, merchant_id, amount, description, order_ref, state, \
     payer_account_id, payer_hint_account_id, launch_app_id, tx_id, refunded_unix_ms, \
     refund_tx_id, created_unix_ms, expires_unix_ms";

fn row_to_intent(r: &rusqlite::Row<'_>) -> rusqlite::Result<Intent> {
    Ok(Intent {
        intent_id: r.get(0)?,
        merchant_id: r.get(1)?,
        amount: r.get(2)?,
        description: r.get(3)?,
        order_ref: r.get(4)?,
        state: r.get(5)?,
        payer_account_id: r.get(6)?,
        payer_hint_account_id: r.get(7)?,
        launch_app_id: r.get(8)?,
        tx_id: r.get(9)?,
        refunded_unix_ms: r.get(10)?,
        refund_tx_id: r.get(11)?,
        created_unix_ms: r.get(12)?,
        expires_unix_ms: r.get(13)?,
    })
}

/// Fetch an intent by id.
pub fn get(conn: &Connection, intent_id: &str) -> rusqlite::Result<Option<Intent>> {
    conn.query_row(
        &format!("SELECT {INTENT_COLS} FROM payment_intents WHERE intent_id = ?1"),
        [intent_id],
        row_to_intent,
    )
    .optional()
}

/// A 128-bit intent id. Unguessable on purpose: holding one is what lets a wallet
/// see the amount and the shop behind a purchase.
fn gen_intent_id() -> String {
    let mut buf = [0u8; 16];
    OsRng.fill_bytes(&mut buf);
    format!(
        "pi_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
    )
}

// ── creation (merchant API key) ──────────────────────────────────────────────

/// What a merchant asked for.
#[derive(Debug)]
pub struct NewIntent<'a> {
    pub idem_key: &'a str,
    pub amount: i64,
    pub description: &'a str,
    pub order_ref: Option<&'a str>,
    pub launch_app_id: Option<&'a str>,
    /// Resolved from `payer_hint_handle` by the caller, so this module never has
    /// to decide what a handle means.
    pub payer_hint_account_id: Option<&'a str>,
    pub expires_in_secs: Option<i64>,
}

/// Outcome of creating an intent. Only `Ok` is a success.
#[derive(Debug)]
pub enum CreateOutcome {
    Ok(Intent),
    BadAmount,
    BadDescription(TextReject),
    BadOrderRef(TextReject),
    BadTtl,
    Capped(IssueGuard),
}

/// Create an intent inside the caller's transaction (the issuance ceilings and
/// the insert have to be one unit, or a burst of concurrent creates all pass a
/// ceiling none of them are individually over).
pub fn create(
    tx: &rusqlite::Transaction<'_>,
    m: &MerchantRow,
    req: &NewIntent<'_>,
) -> Result<CreateOutcome, ApiError> {
    if req.amount <= 0 || req.amount > wallet::MAX_AMOUNT {
        return Ok(CreateOutcome::BadAmount);
    }
    let description = match merchant::sanitize_text(req.description, merchant::MAX_DESCRIPTION_CHARS)
    {
        Ok(d) => d,
        Err(e) => return Ok(CreateOutcome::BadDescription(e)),
    };
    let order_ref = match req.order_ref.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => match merchant::sanitize_text(s, merchant::MAX_ORDER_REF_CHARS) {
            Ok(v) => Some(v),
            Err(e) => return Ok(CreateOutcome::BadOrderRef(e)),
        },
        None => None,
    };
    let ttl = req.expires_in_secs.unwrap_or(DEFAULT_TTL_SECS);
    if !(MIN_TTL_SECS..=MAX_TTL_SECS).contains(&ttl) {
        return Ok(CreateOutcome::BadTtl);
    }
    let now = now_ms();
    match merchant::check_issuance(tx, m, req.amount, now)? {
        IssueGuard::Ok => {}
        capped => return Ok(CreateOutcome::Capped(capped)),
    }

    let intent = Intent {
        intent_id: gen_intent_id(),
        merchant_id: m.merchant_id.clone(),
        amount: req.amount,
        description,
        order_ref,
        state: STATE_CREATED.to_string(),
        payer_account_id: None,
        payer_hint_account_id: req.payer_hint_account_id.map(str::to_string),
        launch_app_id: req.launch_app_id.map(str::to_string),
        tx_id: None,
        refunded_unix_ms: None,
        refund_tx_id: None,
        created_unix_ms: now,
        expires_unix_ms: now + ttl * 1_000,
    };
    tx.execute(
        "INSERT INTO payment_intents \
           (intent_id, merchant_id, amount, description, order_ref, state, payer_hint_account_id, \
            launch_app_id, idem_key, created_unix_ms, updated_unix_ms, expires_unix_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11)",
        params![
            intent.intent_id,
            intent.merchant_id,
            intent.amount,
            intent.description,
            intent.order_ref,
            STATE_CREATED,
            intent.payer_hint_account_id,
            intent.launch_app_id,
            req.idem_key,
            now,
            intent.expires_unix_ms,
        ],
    )?;
    Ok(CreateOutcome::Ok(intent))
}

/// Withdraw an unanswered intent (merchant API key).
///
/// The `state='created'` guard is what makes this safe to race against an
/// approval: whichever statement finds the row first owns the outcome. An intent
/// that is already paid stays paid and says so — the shop owes the goods.
pub fn cancel(conn: &Connection, intent_id: &str, merchant_id: &str) -> rusqlite::Result<Value> {
    let now = now_ms();
    let changed = conn.execute(
        "UPDATE payment_intents SET state = ?4, updated_unix_ms = ?3 \
         WHERE intent_id = ?1 AND merchant_id = ?2 AND state = ?5",
        params![
            intent_id,
            merchant_id,
            now,
            STATE_CANCELED,
            STATE_CREATED
        ],
    )?;
    if changed == 1 {
        return Ok(json!({ "ok": true, "intent_id": intent_id, "state": STATE_CANCELED }));
    }
    // Ownership is checked by re-reading rather than trusted from the failed
    // UPDATE: another merchant's intent must be indistinguishable from one that
    // does not exist (the discipline `op_status` uses).
    match get(conn, intent_id)? {
        Some(i) if i.merchant_id == merchant_id => {
            let state = i.effective_state(now);
            Ok(json!({
                "ok": false,
                "error": if state == STATE_PAID { "already_paid" } else { "not_cancelable" },
                "state": state,
            }))
        }
        _ => Ok(json!({ "ok": false, "error": "unknown_intent" })),
    }
}

// ── expiry ───────────────────────────────────────────────────────────────────

/// Move every intent past its deadline to `expired`. One indexed UPDATE over
/// `(state, expires_unix_ms)`.
///
/// This is housekeeping, not enforcement. Approve's own guard carries
/// `expires_unix_ms > now`, so a sweep that is late — or that never runs — cannot
/// let a stale intent be paid. What it does is stop the merchant's open-intent
/// ceiling from filling up with corpses and keep reported states honest.
pub fn expire_pass(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE payment_intents SET state = ?1, updated_unix_ms = ?2 \
         WHERE state = ?3 AND expires_unix_ms <= ?2",
        params![STATE_EXPIRED, now, STATE_CREATED],
    )
}

// ── views ────────────────────────────────────────────────────────────────────

/// The shop behind an intent, as the approval screen shows it.
#[derive(Debug, Serialize)]
pub struct MerchantFace {
    pub merchant_id: String,
    pub name: String,
    pub sub: Option<String>,
    /// The `@handle` of the login account that registered the shop. The screen
    /// leads with THIS, not with `name`: a name can be squatted or imitated, but
    /// a handle is the wallet's existing unique id space.
    pub owner_handle: Option<String>,
    /// When the shop registered — the approval screen badges a new one.
    pub created_unix_ms: i64,
}

/// The face of an intent, for the payer's approval screen. This is the ONLY thing
/// the screen may render: everything on it comes from this backend's record, not
/// from whatever the launching app passed along.
pub fn payer_view(conn: &Connection, intent: &Intent, now: i64) -> rusqlite::Result<Value> {
    let m = merchant::get(conn, &intent.merchant_id)?;
    let face = match &m {
        Some(m) => {
            let owner_handle = match &m.owner_account_id {
                Some(owner) => auth::account_view(conn, owner)?
                    .map(|a| a.handle)
                    .filter(|h| !h.is_empty()),
                None => None,
            };
            Some(MerchantFace {
                merchant_id: m.merchant_id.clone(),
                name: m.name.clone(),
                sub: m.sub.clone(),
                owner_handle,
                created_unix_ms: m.created_unix_ms,
            })
        }
        None => None,
    };
    Ok(json!({
        "intent_id": intent.intent_id,
        "amount": intent.amount,
        "description": intent.description,
        "state": intent.effective_state(now),
        "expires_unix_ms": intent.expires_unix_ms,
        "created_unix_ms": intent.created_unix_ms,
        "launch_app_id": intent.launch_app_id,
        "merchant": face,
        // Non-active merchants are surfaced rather than hidden: the screen has to
        // be able to say why a payment it is showing cannot be made.
        "merchant_active": m.map(|m| m.is_active()).unwrap_or(false),
    }))
}

/// The face of an intent for the merchant that owns it. Carries `payer_ref` once
/// paid — a pseudonym stable within this shop and uncorrelatable outside it.
pub fn merchant_view(m: &MerchantRow, intent: &Intent, now: i64) -> Result<Value, ApiError> {
    let payer_ref = match (&intent.payer_account_id, &m.payer_ref_salt) {
        (Some(payer), Some(salt)) => Some(merchant::payer_ref(salt, payer)?),
        // A merchant with no salt is a pre-v6 demo row, which has no API key and
        // so cannot reach this. Reported as absent rather than fabricated.
        _ => None,
    };
    Ok(json!({
        "intent_id": intent.intent_id,
        "merchant_id": intent.merchant_id,
        "amount": intent.amount,
        "description": intent.description,
        "order_ref": intent.order_ref,
        "state": intent.effective_state(now),
        "payer_ref": payer_ref,
        "refunded": intent.refunded_unix_ms.is_some(),
        "created_unix_ms": intent.created_unix_ms,
        "expires_unix_ms": intent.expires_unix_ms,
    }))
}

// ── decline (session, no PIN) ────────────────────────────────────────────────

/// Refuse an intent. No PIN: nothing moves, and making a customer authenticate to
/// say "no" is how people learn to type their PIN into whatever asks.
pub fn decline(conn: &Connection, intent_id: &str, account_id: &str) -> rusqlite::Result<Value> {
    let now = now_ms();
    let Some(intent) = get(conn, intent_id)? else {
        return Ok(json!({ "ok": false, "error": "unknown_intent" }));
    };
    if let Some(hint) = &intent.payer_hint_account_id {
        if hint != account_id {
            return Ok(json!({ "ok": false, "error": "payer_mismatch" }));
        }
    }
    let changed = conn.execute(
        "UPDATE payment_intents SET state = ?3, payer_account_id = ?2, updated_unix_ms = ?4 \
         WHERE intent_id = ?1 AND state = ?5 AND expires_unix_ms > ?4",
        params![
            intent_id,
            account_id,
            STATE_DECLINED,
            now,
            STATE_CREATED
        ],
    )?;
    if changed == 1 {
        return Ok(json!({ "ok": true, "intent_id": intent_id, "state": STATE_DECLINED }));
    }
    let state = get(conn, intent_id)?
        .map(|i| i.effective_state(now).to_string())
        .unwrap_or_else(|| STATE_EXPIRED.to_string());
    Ok(json!({ "ok": false, "error": "not_declinable", "state": state }))
}

// ── approve (session + PIN, the money path) ──────────────────────────────────

/// Approve an intent: authenticate the payer, claim the intent and move the money.
///
/// Runs on a blocking connection and opens **three separate short transactions**
/// (see [`crate::auth`]'s PIN notes) — never one long one, because the Argon2id
/// comparison in the middle would otherwise hold SQLite's only write lock for
/// hundreds of milliseconds and stall every other wallet operation.
pub fn approve(
    conn: &mut Connection,
    backoff: &PinBackoff,
    email_enabled: bool,
    a: &riskauth::Caller<'_>,
    intent_id: &str,
) -> Result<Value, ApiError> {
    let now = now_ms();
    let Some(intent) = get(conn, intent_id)? else {
        return Ok(json!({ "ok": false, "error": "unknown_intent" }));
    };

    // A replay is answered BEFORE any PIN work. The app retries an approval whose
    // response was lost, and charging that retry an attempt would let a flaky
    // connection lock somebody out of a purchase they already made — the same
    // reasoning that makes a charge replay skip its attestation.
    if intent.state == STATE_PAID {
        return Ok(if intent.payer_account_id.as_deref() == Some(a.account_id) {
            let mut v = paid_response(conn, &intent, a.account_id)?;
            if let Some(o) = v.as_object_mut() {
                o.insert("duplicate".to_string(), json!(true));
            }
            v
        } else {
            json!({ "ok": false, "error": "already_paid", "state": STATE_PAID })
        });
    }
    if intent.state != STATE_CREATED {
        return Ok(json!({ "ok": false, "error": "not_payable", "state": intent.state }));
    }
    if intent.expires_unix_ms <= now {
        // Lazy expiry: report the truth now rather than waiting for the sweep.
        // Best-effort — the row's state is not what stops the payment, the guard
        // inside the claim is.
        if let Err(e) = expire_pass(conn, now) {
            tracing::warn!(error = %e, "approve: lazy expiry sweep failed (harmless — the claim guard still refuses)");
        }
        return Ok(json!({ "ok": false, "error": "expired", "state": STATE_EXPIRED }));
    }

    let Some(m) = merchant::get(conn, &intent.merchant_id)? else {
        return Err(ApiError::internal(format!(
            "intent {intent_id} references merchant {} which does not exist",
            intent.merchant_id
        )));
    };
    // A stopped shop may not collect on the bills it issued before it was
    // stopped. Without this an operator freezing a fraudulent merchant would
    // still be watching it take money for the next half hour.
    if !m.is_active() {
        return Ok(json!({ "ok": false, "error": "merchant_disabled" }));
    }
    if let Some(hint) = &intent.payer_hint_account_id {
        if hint != a.account_id {
            return Ok(json!({ "ok": false, "error": "payer_mismatch" }));
        }
    }
    if m.account_id == a.account_id {
        return Ok(json!({ "ok": false, "error": "self_transfer" }));
    }

    // A payment is a PIN by standing decision (the user's explicit choice); the
    // amount may only raise that, never lower it.
    let ticket = match riskauth::step_up(conn, backoff, email_enabled, a, intent.amount, Requirement::Pin)? {
        riskauth::StepUp::Cleared(t) => t,
        riskauth::StepUp::Refused(v) => return Ok(v),
    };

    let settled = settle(conn, &intent, &m, a.account_id, &ticket)?;
    if !settled.committed {
        // The PIN was right; the operation just did not happen (short balance, a
        // lost race, a wrong code). Give the attempt back, or five honest retries
        // against an empty wallet would lock the account out of its own money.
        riskauth::refund_attempt(conn, a.account_id, &ticket)?;
    }
    Ok(settled.value)
}

struct Settled {
    committed: bool,
    value: Value,
}

/// The one transaction that claims the intent and moves the eme.
fn settle(
    conn: &mut Connection,
    intent: &Intent,
    m: &MerchantRow,
    payer: &str,
    ticket: &riskauth::StepUpTicket,
) -> Result<Settled, ApiError> {
    // Read the clock HERE, not in the caller. Between `approve`'s first look at
    // the intent and this line sits an Argon2id comparison, a wait for a pooled
    // connection and a wait for SQLite's single write lock — hundreds of
    // milliseconds under load. Carrying the arrival time down would evaluate the
    // deadline against when the request showed up rather than when it is being
    // acted on, and an intent that expired mid-PIN would still be payable so long
    // as the sweep had not caught it. The deadline is what stops that; the sweep
    // is only housekeeping.
    let now = now_ms();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    // The lockout is re-checked here, inside the transaction that moves the
    // money, so an account locked out between the PIN check and this point does
    // not pay anyway.
    if let Some(refused) = riskauth::settle(&tx, payer, ticket, now)? {
        return Ok(Settled {
            committed: false,
            value: refused,
        });
    }

    // THE claim. Double approval, an approval landing after the deadline, and a
    // merchant cancelling mid-PIN are one race, decided here.
    let changed = tx.execute(
        "UPDATE payment_intents SET state = ?3, payer_account_id = ?2, updated_unix_ms = ?4 \
         WHERE intent_id = ?1 AND state = ?5 AND expires_unix_ms > ?4",
        params![intent.intent_id, payer, STATE_PAID, now, STATE_CREATED],
    )?;
    if changed == 0 {
        drop(tx);
        // Somebody else owns this intent's outcome now. Read what they made of it
        // rather than guessing.
        let state = get(conn, &intent.intent_id)?
            .map(|i| i.effective_state(now).to_string())
            .unwrap_or_else(|| STATE_EXPIRED.to_string());
        let error = if state == STATE_PAID {
            "already_paid"
        } else if state == STATE_EXPIRED {
            "expired"
        } else {
            "not_payable"
        };
        return Ok(Settled {
            committed: false,
            value: json!({ "ok": false, "error": error, "state": state }),
        });
    }

    // Amount and recipient come from the stored intent, never from the request.
    let result = wallet::transfer(&tx, payer, &m.account_id, intent.amount, "pay", &m.name)?;
    match result {
        TxResult::Ok {
            tx_id,
            balance_after,
            ..
        } => {
            tx.execute(
                "UPDATE payment_intents SET tx_id = ?2 WHERE intent_id = ?1",
                params![intent.intent_id, tx_id],
            )?;
            tx.commit()?;
            Ok(Settled {
                committed: true,
                value: json!({
                    "ok": true,
                    "intent_id": intent.intent_id,
                    "state": STATE_PAID,
                    "amount": intent.amount,
                    "tx_id": tx_id,
                    "balance": balance_after,
                    "merchant_name": m.name,
                }),
            })
        }
        // Rolling back unwinds the claim as well, which is deliberate: the intent
        // goes back to `created`, so a customer who was short can charge their
        // wallet and approve the very same intent instead of asking the shop for
        // a new one.
        other => {
            drop(tx);
            Ok(Settled {
                committed: false,
                value: tx_result_json(other),
            })
        }
    }
}

/// The response a paid intent replays.
fn paid_response(conn: &Connection, intent: &Intent, account_id: &str) -> rusqlite::Result<Value> {
    // The intent row IS the idempotency record — state, payer and tx_id are
    // enough to rebuild the answer, so approve never writes to the idempotency
    // table at all.
    let name = merchant::get(conn, &intent.merchant_id)?.map(|m| m.name);
    Ok(json!({
        "ok": true,
        "intent_id": intent.intent_id,
        "state": STATE_PAID,
        "amount": intent.amount,
        "tx_id": intent.tx_id,
        "balance": wallet::balance(conn, account_id)?,
        "merchant_name": name,
    }))
}

fn tx_result_json(r: TxResult) -> Value {
    match r {
        TxResult::Ok { .. } => json!({ "ok": true }),
        TxResult::BadAmount => json!({ "ok": false, "error": "bad_amount" }),
        TxResult::SelfTransfer => json!({ "ok": false, "error": "self_transfer" }),
        TxResult::UnknownTarget => json!({ "ok": false, "error": "unknown_target" }),
        TxResult::Insufficient { balance } => {
            json!({ "ok": false, "error": "insufficient", "balance": balance })
        }
    }
}

// ── operator-forced refund ───────────────────────────────────────────────────

/// Outcome of forcing a refund.
#[derive(Debug)]
pub enum RefundOutcome {
    Ok { tx_id: String, amount: i64 },
    UnknownIntent,
    NotPaid { state: String },
    AlreadyRefunded,
    /// The merchant no longer holds the money. Merchant revenue is not escrowed
    /// (DEV.md: accepted risk), so a shop that collected and withdrew to the MC
    /// world leaves nothing to reverse. Reported honestly rather than papered
    /// over with a partial refund from nowhere.
    MerchantShort { balance: i64 },
}

/// Return a paid intent's money from the merchant to the payer.
///
/// **Not reachable from the merchant API**, by construction: it takes no
/// credential and lives here for an operator path to call. `paid` is not rewound
/// — the refund is its own transfer, and both movements stay in the ledger.
///
/// Exactly-once comes from the claim, in the same shape as the approval above:
/// `refunded_unix_ms IS NULL` is part of the UPDATE, so two operators pressing
/// the button together produce one refund.
pub fn force_refund(
    conn: &mut Connection,
    intent_id: &str,
    reason: &str,
) -> Result<RefundOutcome, ApiError> {
    let now = now_ms();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let Some(intent) = tx
        .query_row(
            &format!("SELECT {INTENT_COLS} FROM payment_intents WHERE intent_id = ?1"),
            [intent_id],
            row_to_intent,
        )
        .optional()?
    else {
        return Ok(RefundOutcome::UnknownIntent);
    };
    if intent.state != STATE_PAID {
        return Ok(RefundOutcome::NotPaid {
            state: intent.state,
        });
    }
    let (Some(payer), Some(m)) = (
        intent.payer_account_id.clone(),
        merchant::get(&tx, &intent.merchant_id)?,
    ) else {
        return Err(ApiError::internal(format!(
            "paid intent {intent_id} has no payer or no merchant"
        )));
    };
    let claimed = tx.execute(
        "UPDATE payment_intents SET refunded_unix_ms = ?2, updated_unix_ms = ?2 \
         WHERE intent_id = ?1 AND state = ?3 AND refunded_unix_ms IS NULL",
        params![intent_id, now, STATE_PAID],
    )?;
    if claimed == 0 {
        return Ok(RefundOutcome::AlreadyRefunded);
    }
    let label = format!("返金（運営措置）: {reason}");
    match wallet::transfer(&tx, &m.account_id, &payer, intent.amount, "pay", &label)? {
        TxResult::Ok { tx_id, .. } => {
            tx.execute(
                "UPDATE payment_intents SET refund_tx_id = ?2 WHERE intent_id = ?1",
                params![intent_id, tx_id],
            )?;
            tx.commit()?;
            tracing::warn!(intent_id, amount = intent.amount, reason,
                "operator-forced refund applied");
            Ok(RefundOutcome::Ok {
                tx_id,
                amount: intent.amount,
            })
        }
        TxResult::Insufficient { balance } => {
            // Rolls back, so the claim unwinds and the refund can be retried once
            // the merchant has funds again.
            tracing::error!(intent_id, amount = intent.amount, balance, reason,
                "operator-forced refund NOT applied — the merchant no longer holds the money \
                 (revenue is not escrowed; see the accepted risk in DEV.md)");
            Ok(RefundOutcome::MerchantShort { balance })
        }
        other => Err(ApiError::internal(format!(
            "forced refund of {intent_id} could not be paid: {other:?}"
        ))),
    }
}

// ── EC payment, payer side ───────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct IntentQuery {
    pub(crate) intent_id: String,
}

/// Everything the approval screen may show. Deliberately the only source: what
/// the launching app passed along is an `intent_id` and nothing else.
pub(crate) async fn payment_intent(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Query(q): Query<IntentQuery>,
) -> Result<Json<Value>, ApiError> {
    let v = blocking(st.pool, move |conn| {
        let Some(intent) = get(conn, &q.intent_id)? else {
            return Ok::<Value, ApiError>(json!({ "ok": false, "error": "unknown_intent" }));
        };
        // An intent addressed to somebody else is not this account's to look at.
        if let Some(hint) = &intent.payer_hint_account_id {
            if hint != &acct.account_id {
                return Ok(json!({ "ok": false, "error": "payer_mismatch" }));
            }
        }
        Ok(json!({ "ok": true, "intent": payer_view(conn, &intent, now_ms())? }))
    })
    .await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
pub(crate) struct ApproveReq {
    intent_id: String,
    /// Optional in the wire schema so a client that has not collected one yet
    /// gets `{ok:false,error:"pin_required"}` — a domain answer it can act on —
    /// rather than a deserialization rejection.
    pin: Option<String>,
    /// Supplied on the retry after `otp_required`.
    otp: Option<String>,
}

pub(crate) async fn payment_approve(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<ApproveReq>,
) -> Result<Json<Value>, ApiError> {
    let email_enabled = st.email_enabled();
    let backoff = st.pin_backoff.clone();
    let value = blocking(st.pool, move |conn| {
        approve(
            conn,
            &backoff,
            email_enabled,
            &riskauth::Caller {
                account_id: &acct.account_id,
                phone_id: acct.phone_id.as_deref(),
                session_key: &acct.session_key,
                pin: req.pin.as_deref(),
                otp: req.otp.as_deref(),
            },
            &req.intent_id,
        )
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
pub(crate) struct DeclineReq {
    intent_id: String,
}

pub(crate) async fn payment_decline(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<DeclineReq>,
) -> Result<Json<Value>, ApiError> {
    let value = blocking(st.pool, move |conn| {
        decline(conn, &req.intent_id, &acct.account_id).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Pool;
    use rusqlite::TransactionBehavior;

    const PIN: &str = "1234";

    /// A wallet with a customer (`acct-a`, funded) and a registered shop owned by
    /// `acct-m`, plus one open intent.
    fn fixture(amount: i64, balance: i64) -> (Pool, String, MerchantRow) {
        let pool = crate::db::open_memory().expect("in-memory pool");
        let (intent_id, m) = seed(&pool, amount, balance);
        (pool, intent_id, m)
    }

    /// Everything [`fixture`] sets up, against whichever pool it is handed — the
    /// concurrency test needs the same world on a file-backed database.
    fn seed(pool: &Pool, amount: i64, balance: i64) -> (String, MerchantRow) {
        let mut conn = pool.get().expect("checkout");
        let hash = auth::hash_pin(PIN).unwrap();
        for (id, handle) in [("acct-a", "payer"), ("acct-m", "shopkeep")] {
            auth::insert_account(&conn, id, handle, handle, handle, &hash, None).unwrap();
        }
        conn.execute(
            "UPDATE accounts SET balance = ?1 WHERE account_id = 'acct-a'",
            [balance],
        )
        .unwrap();
        let m = {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            let out = merchant::register(&tx, "acct-m", "Piggle Shop", None, None, None).unwrap();
            let merchant::RegisterOutcome::Ok { merchant_id, .. } = out else {
                panic!("registration failed: {out:?}");
            };
            let m = merchant::get(&tx, &merchant_id).unwrap().unwrap();
            tx.commit().unwrap();
            m
        };
        let intent_id = new_intent(&mut conn, &m, amount, None, DEFAULT_TTL_SECS);
        drop(conn);
        (intent_id, m)
    }

    /// A file-backed database, deleted when the test ends.
    ///
    /// `open_memory()` cannot be used for anything genuinely concurrent: it pins
    /// `max_size(1)`, and `SqliteConnectionManager::memory()` hands every
    /// connection a database of its OWN — two threads would not even be looking
    /// at the same rows. A real file gives the pool several connections onto one
    /// database, which is what makes threads contend the way they do in the
    /// server.
    struct TempDb {
        path: String,
        pool: Pool,
    }

    impl TempDb {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("moymoy-test-{}.db", uuid::Uuid::new_v4().simple()))
                .to_string_lossy()
                .into_owned();
            let pool = crate::db::open(&path).expect("file-backed pool");
            TempDb { path, pool }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.path));
            }
        }
    }

    fn new_intent(
        conn: &mut Connection,
        m: &MerchantRow,
        amount: i64,
        hint: Option<&str>,
        ttl: i64,
    ) -> String {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let out = create(
            &tx,
            m,
            &NewIntent {
                idem_key: &format!("k-{amount}-{ttl}-{}", now_ms()),
                amount,
                description: "りんご 1個",
                order_ref: None,
                launch_app_id: None,
                payer_hint_account_id: hint,
                expires_in_secs: Some(ttl),
            },
        )
        .unwrap();
        let CreateOutcome::Ok(i) = out else {
            panic!("create failed: {out:?}");
        };
        tx.commit().unwrap();
        i.intent_id
    }

    fn approver<'a>(pin: Option<&'a str>, otp: Option<&'a str>) -> riskauth::Caller<'a> {
        riskauth::Caller {
            account_id: "acct-a",
            phone_id: None,
            session_key: "sess-a",
            pin,
            otp,
        }
    }

    fn do_approve(pool: &Pool, intent_id: &str, pin: &str) -> Value {
        approve_with(pool, intent_id, Some(pin))
    }

    fn approve_with(pool: &Pool, intent_id: &str, pin: Option<&str>) -> Value {
        let mut conn = pool.get().unwrap();
        approve(
            &mut conn,
            &PinBackoff::new(),
            false,
            &approver(pin, None),
            intent_id,
        )
        .unwrap()
    }

    fn balance_of(pool: &Pool, account_id: &str) -> i64 {
        wallet::balance(&pool.get().unwrap(), account_id).unwrap()
    }

    /// Give the payer a verified address, which is what makes the top tier
    /// reachable at all.
    fn verify_email(pool: &Pool, email: &str) {
        pool.get()
            .unwrap()
            .execute(
                "UPDATE accounts SET email = ?1, email_lower = ?1,                  email_verified_unix_ms = 1 WHERE account_id = 'acct-a'",
                [email],
            )
            .unwrap();
    }

    /// Mail a step-up code the way `/wallet/stepup/otp` does.
    fn issue_stepup_code(pool: &Pool, email: &str) -> String {
        let conn = pool.get().unwrap();
        match crate::otp::create(&conn, riskauth::PURPOSE_STEPUP, email, Some("acct-a"), None)
            .unwrap()
        {
            crate::otp::CreateOtp::Issued(code) => code,
            crate::otp::CreateOtp::TooSoon { retry_after_ms } => {
                panic!("unexpected resend cooldown: {retry_after_ms} ms")
            }
        }
    }

    /// Wrong guesses recorded against the live step-up code.
    fn stepup_code_attempts(pool: &Pool) -> i64 {
        pool.get()
            .unwrap()
            .query_row(
                "SELECT COALESCE(MAX(attempts), 0) FROM moymoy_otps WHERE purpose = ?1",
                [riskauth::PURPOSE_STEPUP],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn live_stepup_codes(pool: &Pool) -> i64 {
        pool.get()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM moymoy_otps WHERE purpose = ?1",
                [riskauth::PURPOSE_STEPUP],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Approve with mail enabled, as a deploy with an identity token behaves.
    fn approve_stepup(pool: &Pool, intent_id: &str, pin: &str, code: Option<&str>) -> Value {
        let mut conn = pool.get().unwrap();
        approve(
            &mut conn,
            &PinBackoff::new(),
            true,
            &approver(Some(pin), code),
            intent_id,
        )
        .unwrap()
    }

    /// An intent past the step-up threshold, on a wallet that can pay for it.
    fn stepup_fixture(email: Option<&str>) -> (Pool, String, i64) {
        let (pool, _, m) = fixture(300, 100_000);
        if let Some(e) = email {
            verify_email(&pool, e);
        }
        let amount = riskauth::STEPUP_SINGLE + 1;
        let intent_id = {
            let mut conn = pool.get().unwrap();
            new_intent(&mut conn, &m, amount, None, 600)
        };
        (pool, intent_id, amount)
    }

    fn failed_attempts(pool: &Pool) -> i64 {
        pool.get()
            .unwrap()
            .query_row(
                "SELECT failed_pin_attempts FROM accounts WHERE account_id = 'acct-a'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn state_of(pool: &Pool, intent_id: &str) -> String {
        get(&pool.get().unwrap(), intent_id)
            .unwrap()
            .unwrap()
            .state
    }

    #[test]
    fn an_approval_moves_the_amount_the_intent_says_and_nothing_else() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        let v = do_approve(&pool, &intent_id, PIN);
        assert_eq!(v["ok"], json!(true), "{v}");
        assert_eq!(v["amount"], json!(300));
        assert_eq!(balance_of(&pool, "acct-a"), 700);
        assert_eq!(balance_of(&pool, "acct-m"), 300);
        assert_eq!(state_of(&pool, &intent_id), STATE_PAID);
    }

    /// The claim is the whole safety argument, so it is exercised through
    /// `approve()` on real threads against one real database — not by replaying
    /// a copy of its SQL on a single connection, which would keep passing if the
    /// guard were deleted from the code that actually runs.
    ///
    /// Four threads, not more: `begin_pin_attempt` records each attempt up front,
    /// so a fifth concurrent approval would trip the account lockout and return
    /// `locked` before ever reaching the claim — correct behaviour, but it would
    /// stop this test from testing what it is here for.
    #[test]
    fn only_one_of_several_concurrent_approvals_can_pay() {
        use std::sync::{Arc, Barrier};

        const THREADS: usize = 4;
        const AMOUNT: i64 = 300;
        const START: i64 = 10_000;

        let db = TempDb::new();
        let (intent_id, m) = seed(&db.pool, AMOUNT, START);
        let barrier = Arc::new(Barrier::new(THREADS));
        // One shared backoff, as the server has: the threads are one session.
        let backoff = Arc::new(PinBackoff::new());

        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let pool = db.pool.clone();
            let backoff = backoff.clone();
            let barrier = barrier.clone();
            let intent_id = intent_id.clone();
            handles.push(std::thread::spawn(move || {
                let mut conn = pool.get().expect("checkout");
                // Released together, so every thread reads the intent while it is
                // still `created` and they all arrive at the claim.
                barrier.wait();
                approve(
                    &mut conn,
                    &backoff,
                    false,
                    &approver(Some(PIN), None),
                    &intent_id,
                )
                .unwrap()
            }));
        }
        let results: Vec<Value> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Which thread wins is not the property under test — that exactly one
        // does, and that the others come back as something that moved no money.
        let winners: Vec<&Value> = results
            .iter()
            .filter(|v| v["ok"] == json!(true) && v.get("duplicate").is_none())
            .collect();
        assert_eq!(winners.len(), 1, "not exactly one payment: {results:#?}");
        assert_eq!(winners[0]["amount"], json!(AMOUNT));

        let claim_losers = results
            .iter()
            .filter(|v| v["error"] == json!("already_paid"))
            .count();
        let replays = results
            .iter()
            .filter(|v| v.get("duplicate") == Some(&json!(true)))
            .count();
        assert_eq!(
            claim_losers + replays,
            THREADS - 1,
            "a loser came back as something other than a replay or a lost claim: {results:#?}"
        );
        // The branch this test exists for: a thread that got past the replay
        // check and lost the claim. The barrier plus the ~100 ms Argon2id
        // comparison makes the window enormous compared to thread start-up, so
        // every loser should land here.
        assert!(
            claim_losers >= 1,
            "no thread reached the claim guard — the race did not happen: {results:#?}"
        );

        // The ledger is the final word: the money moved exactly once.
        assert_eq!(balance_of(&db.pool, "acct-a"), START - AMOUNT);
        assert_eq!(balance_of(&db.pool, &m.account_id), AMOUNT);
        assert_eq!(state_of(&db.pool, &intent_id), STATE_PAID);
        let paid_rows: i64 = db
            .pool
            .get()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE account_id = ?1 AND kind = 'pay'",
                [&m.account_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(paid_rows, 0, "the merchant was debited by its own sale");
        let payer_rows: i64 = db
            .pool
            .get()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE account_id = 'acct-a' AND kind = 'pay'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(payer_rows, 1, "the payer was charged more than once");
    }

    /// An intent whose deadline passes DURING the PIN check must not be paid.
    ///
    /// This is the test that pins down *which clock* the claim uses. The deadline
    /// is set a few milliseconds past the moment the approval starts — so it is
    /// still live when the request arrives, and lapsed by the time the Argon2id
    /// comparison finishes. A claim judging the deadline against the arrival time
    /// pays it; one that reads the clock where the money actually moves does not.
    #[test]
    fn an_intent_that_lapses_during_the_pin_check_is_not_paid() {
        use std::sync::{Arc, Barrier};

        /// Comfortably longer than the gap between two threads leaving a barrier,
        /// and comfortably shorter than an Argon2id verification.
        const GRACE_MS: i64 = 20;

        let db = TempDb::new();
        let (intent_id, _) = seed(&db.pool, 300, 1_000);
        let barrier = Arc::new(Barrier::new(2));

        let approver_thread = {
            let pool = db.pool.clone();
            let barrier = barrier.clone();
            let intent_id = intent_id.clone();
            std::thread::spawn(move || {
                let mut conn = pool.get().unwrap();
                barrier.wait();
                approve(
                    &mut conn,
                    &PinBackoff::new(),
                    false,
                    &approver(Some(PIN), None),
                    &intent_id,
                )
                .unwrap()
            })
        };
        barrier.wait();
        let deadline = now_ms() + GRACE_MS;
        db.pool
            .get()
            .unwrap()
            .execute(
                "UPDATE payment_intents SET expires_unix_ms = ?2 WHERE intent_id = ?1",
                params![intent_id, deadline],
            )
            .unwrap();

        let v = approver_thread.join().unwrap();
        // The PIN check has to have outlived the deadline, or this test proved
        // nothing about which clock was consulted.
        assert!(
            now_ms() > deadline,
            "the approval finished before the deadline it was supposed to outlive"
        );
        assert_eq!(v["ok"], json!(false), "a lapsed intent was paid: {v}");
        assert_eq!(balance_of(&db.pool, "acct-a"), 1_000);
        assert_ne!(state_of(&db.pool, &intent_id), STATE_PAID);
    }

    #[test]
    fn a_payment_asks_for_a_pin_however_small_it_is() {
        // riskauth would wave a 5-eme send through untouched. A payment is the
        // one movement with a standing PIN requirement, so the floor has to
        // survive all the way to the endpoint, not just to the assessment.
        let (pool, _, m) = fixture(300, 1_000);
        let intent_id = {
            let mut conn = pool.get().unwrap();
            new_intent(&mut conn, &m, 5, None, 600)
        };
        assert_eq!(
            riskauth::assess(5, 0, riskauth::DeviceTrust::Familiar),
            Requirement::None
        );
        let v = approve_with(&pool, &intent_id, None);
        assert_eq!(v["error"], json!("pin_required"), "{v}");
        assert_eq!(v["required"], json!("pin"));
        assert_eq!(balance_of(&pool, "acct-a"), 1_000);
    }

    /// The top tier, end to end. Without this, a mismatch between the purpose
    /// `/wallet/stepup/otp` issues under and the one `riskauth` verifies under
    /// would look exactly like a green suite.
    #[test]
    fn the_step_up_tier_pays_when_the_emailed_code_is_right() {
        let (pool, intent_id, amount) = stepup_fixture(Some("stepup-ok@disc.mnn"));
        // Asked for the code first, before it is supplied.
        let v = approve_stepup(&pool, &intent_id, PIN, None);
        assert_eq!(v["error"], json!("otp_required"), "{v}");
        assert_eq!(v["required"], json!("pin_otp"));

        let code = issue_stepup_code(&pool, "stepup-ok@disc.mnn");
        let v = approve_stepup(&pool, &intent_id, PIN, Some(&code));

        assert_eq!(v["ok"], json!(true), "{v}");
        assert_eq!(balance_of(&pool, "acct-a"), 100_000 - amount);
        assert_eq!(state_of(&pool, &intent_id), STATE_PAID);
        // Consumed, so it cannot be spent again.
        assert_eq!(live_stepup_codes(&pool), 0, "the code survived its use");
    }

    /// An account with no verified address cannot make a movement in the top
    /// band at all. Deliberately NOT degraded to a PIN — that would leave the
    /// threshold deciding nothing.
    #[test]
    fn a_step_up_payment_is_refused_when_no_second_factor_can_be_produced() {
        let (pool, intent_id, _) = stepup_fixture(None);
        let v = approve_stepup(&pool, &intent_id, PIN, None);
        assert_eq!(v["error"], json!("otp_unavailable"), "{v}");
        assert_eq!(v["required"], json!("pin_otp"));
        assert_eq!(balance_of(&pool, "acct-a"), 100_000);
        // …and it costs no PIN attempt: the request was never going to be enough
        // whatever PIN it carried.
        assert_eq!(failed_attempts(&pool), 0);
    }

    /// The regression guard for the defect this shape exists to prevent.
    ///
    /// A wrong code must leave its attempt behind. It is verified in a
    /// transaction of its own precisely so the count survives the rollback of
    /// the payment it failed to authorize — folded into the money transaction,
    /// every wrong guess was undone and the five-attempt limit never arrived.
    #[test]
    fn a_wrong_emailed_code_pays_nothing_and_spends_no_pin_attempt() {
        let (pool, intent_id, _) = stepup_fixture(Some("stepup-bad@disc.mnn"));
        let real = issue_stepup_code(&pool, "stepup-bad@disc.mnn");
        let wrong = if real == "000000" { "111111" } else { "000000" };

        let v = approve_stepup(&pool, &intent_id, PIN, Some(wrong));

        assert_eq!(v["error"], json!("invalid_code"), "{v}");
        assert_eq!(balance_of(&pool, "acct-a"), 100_000);
        assert_eq!(state_of(&pool, &intent_id), STATE_CREATED);
        // The PIN was right; a wrong code has its own counter and must not eat
        // into the PIN's, or a fumbled code would walk somebody into a lockout.
        assert_eq!(failed_attempts(&pool), 0);
        // The count that bounds guessing survived the failed payment.
        assert_eq!(
            stepup_code_attempts(&pool),
            1,
            "a wrong code left no trace — it could be guessed indefinitely"
        );
        // The real code is still live, so the customer can just try again.
        assert_eq!(live_stepup_codes(&pool), 1);
        let v = approve_stepup(&pool, &intent_id, PIN, Some(&real));
        assert_eq!(v["ok"], json!(true), "{v}");
    }

    #[test]
    fn guessing_a_code_runs_out_of_attempts_rather_than_out_of_codes() {
        let (pool, intent_id, _) = stepup_fixture(Some("stepup-brute@disc.mnn"));
        let real = issue_stepup_code(&pool, "stepup-brute@disc.mnn");
        let wrong = if real == "000000" { "111111" } else { "000000" };
        for _ in 0..5 {
            let v = approve_stepup(&pool, &intent_id, PIN, Some(wrong));
            assert_eq!(v["error"], json!("invalid_code"), "{v}");
        }
        // Five wrong guesses burn the code itself — after which even the right
        // one is worthless and a fresh one has to be mailed.
        assert_eq!(live_stepup_codes(&pool), 0);
        let v = approve_stepup(&pool, &intent_id, PIN, Some(&real));
        assert_eq!(v["error"], json!("invalid_code"), "{v}");
        assert_eq!(balance_of(&pool, "acct-a"), 100_000);
    }

    #[test]
    fn a_code_issued_for_a_login_is_not_a_code_for_a_payment() {
        // Separate namespaces on purpose: a code mailed to confirm signing in
        // must not authorize a 5,001 エメ transfer.
        let (pool, intent_id, _) = stepup_fixture(Some("stepup-purpose@disc.mnn"));
        let code = {
            let conn = pool.get().unwrap();
            match crate::otp::create(
                &conn,
                crate::otp::PURPOSE_LOGIN2FA,
                "stepup-purpose@disc.mnn",
                Some("acct-a"),
                None,
            )
            .unwrap()
            {
                crate::otp::CreateOtp::Issued(c) => c,
                crate::otp::CreateOtp::TooSoon { .. } => panic!("cooldown"),
            }
        };
        let v = approve_stepup(&pool, &intent_id, PIN, Some(&code));
        assert_eq!(v["error"], json!("invalid_code"), "{v}");
        assert_eq!(balance_of(&pool, "acct-a"), 100_000);
    }

    #[test]
    fn a_second_approval_replays_instead_of_paying_twice() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        let again = do_approve(&pool, &intent_id, PIN);
        assert_eq!(again["ok"], json!(true), "{again}");
        assert_eq!(again["duplicate"], json!(true));
        assert_eq!(balance_of(&pool, "acct-a"), 700, "paid twice");
    }

    #[test]
    fn a_replay_costs_no_pin_attempt() {
        // A retry after a lost response arrives with whatever the app cached. It
        // must not be able to lock the customer out of a purchase they already
        // completed, so the replay is answered before any PIN work happens.
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        assert_eq!(failed_attempts(&pool), 0);
        let v = do_approve(&pool, &intent_id, "9999");
        assert_eq!(v["duplicate"], json!(true), "{v}");
        assert_eq!(failed_attempts(&pool), 0, "a replay burned an attempt");
    }

    #[test]
    fn a_wrong_pin_leaves_its_failure_recorded_and_moves_nothing() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        let v = do_approve(&pool, &intent_id, "9999");
        assert_eq!(v["error"], json!("invalid_pin"), "{v}");
        // The counter must survive: it is written in its own committed
        // transaction precisely so no later rollback can erase it.
        assert_eq!(failed_attempts(&pool), 1);
        assert_eq!(balance_of(&pool, "acct-a"), 1_000);
        assert_eq!(state_of(&pool, &intent_id), STATE_CREATED);
    }

    #[test]
    fn a_correct_pin_that_cannot_be_paid_does_not_cost_an_attempt() {
        // Otherwise five honest retries against a short balance would lock a
        // customer out of their own wallet while they went to charge it.
        let (pool, intent_id, _) = fixture(300, 100);
        for _ in 0..6 {
            let v = do_approve(&pool, &intent_id, PIN);
            assert_eq!(v["error"], json!("insufficient"), "{v}");
        }
        assert_eq!(failed_attempts(&pool), 0);
        // …and the intent is still payable once the money is there, rather than
        // consumed by the failed attempts.
        assert_eq!(state_of(&pool, &intent_id), STATE_CREATED);
        pool.get()
            .unwrap()
            .execute(
                "UPDATE accounts SET balance = 500 WHERE account_id = 'acct-a'",
                [],
            )
            .unwrap();
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
    }

    #[test]
    fn an_intent_a_moment_past_its_deadline_cannot_be_paid() {
        let (pool, _, m) = fixture(300, 1_000);
        let intent_id = {
            let mut conn = pool.get().unwrap();
            new_intent(&mut conn, &m, 300, None, MIN_TTL_SECS)
        };
        // One millisecond past: the guard is `>`, not `>=`.
        pool.get()
            .unwrap()
            .execute(
                "UPDATE payment_intents SET expires_unix_ms = ?2 WHERE intent_id = ?1",
                params![intent_id, now_ms() - 1],
            )
            .unwrap();
        let v = do_approve(&pool, &intent_id, PIN);
        assert_eq!(v["error"], json!("expired"), "{v}");
        assert_eq!(balance_of(&pool, "acct-a"), 1_000);
        // A wrong PIN was never even asked for, so nothing was spent on it.
        assert_eq!(failed_attempts(&pool), 0);
    }

    #[test]
    fn a_stopped_shop_cannot_collect_on_the_bills_it_already_issued() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        {
            let mut conn = pool.get().unwrap();
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            assert!(
                merchant::set_status(&tx, &m.merchant_id, "acct-m", merchant::STATUS_DISABLED)
                    .unwrap()
            );
            tx.commit().unwrap();
        }
        let v = do_approve(&pool, &intent_id, PIN);
        assert_eq!(v["error"], json!("merchant_disabled"), "{v}");
        assert_eq!(balance_of(&pool, "acct-a"), 1_000);
    }

    #[test]
    fn an_intent_addressed_to_someone_else_is_refused() {
        let (pool, _, m) = fixture(300, 1_000);
        let intent_id = {
            let mut conn = pool.get().unwrap();
            new_intent(&mut conn, &m, 300, Some("acct-someone-else"), 600)
        };
        let v = do_approve(&pool, &intent_id, PIN);
        assert_eq!(v["error"], json!("payer_mismatch"), "{v}");
        // Declining it is refused on the same grounds — a third party holding an
        // intent_id must not be able to cancel somebody's purchase either.
        let d = decline(&pool.get().unwrap(), &intent_id, "acct-a").unwrap();
        assert_eq!(d["error"], json!("payer_mismatch"), "{d}");
    }

    #[test]
    fn declining_is_terminal_and_costs_no_pin() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        {
            // Scoped: the in-memory pool holds exactly one connection, so the
            // helpers below cannot check it out while this one is alive.
            let conn = pool.get().unwrap();
            assert_eq!(decline(&conn, &intent_id, "acct-a").unwrap()["ok"], json!(true));
            let again = decline(&conn, &intent_id, "acct-a").unwrap();
            assert_eq!(again["error"], json!("not_declinable"), "{again}");
        }
        assert_eq!(state_of(&pool, &intent_id), STATE_DECLINED);
        let v = do_approve(&pool, &intent_id, PIN);
        assert_eq!(v["error"], json!("not_payable"), "{v}");
    }

    #[test]
    fn a_cancel_and_an_approval_cannot_both_win() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        let conn = pool.get().unwrap();
        assert_eq!(
            cancel(&conn, &intent_id, &m.merchant_id).unwrap()["ok"],
            json!(true)
        );
        // Cancelling twice is not an error the merchant can act on differently…
        let again = cancel(&conn, &intent_id, &m.merchant_id).unwrap();
        assert_eq!(again["error"], json!("not_cancelable"), "{again}");
        // …and another merchant's intent is indistinguishable from a missing one.
        let theirs = cancel(&conn, &intent_id, "mr_someone_else").unwrap();
        assert_eq!(theirs["error"], json!("unknown_intent"), "{theirs}");
        drop(conn);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["error"], json!("not_payable"));
    }

    #[test]
    fn a_paid_intent_cannot_be_cancelled_out_from_under_the_customer() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        let v = cancel(&pool.get().unwrap(), &intent_id, &m.merchant_id).unwrap();
        assert_eq!(v["error"], json!("already_paid"), "{v}");
        assert_eq!(v["state"], json!(STATE_PAID));
    }

    #[test]
    fn the_sweep_only_touches_what_is_actually_overdue() {
        let (pool, live, m) = fixture(300, 1_000);
        let stale = {
            let mut conn = pool.get().unwrap();
            new_intent(&mut conn, &m, 50, None, 600)
        };
        let conn = pool.get().unwrap();
        conn.execute(
            "UPDATE payment_intents SET expires_unix_ms = ?2 WHERE intent_id = ?1",
            params![stale, now_ms() - 1],
        )
        .unwrap();
        assert_eq!(expire_pass(&conn, now_ms()).unwrap(), 1);
        assert_eq!(expire_pass(&conn, now_ms()).unwrap(), 0, "not idempotent");
        drop(conn);
        assert_eq!(state_of(&pool, &stale), STATE_EXPIRED);
        assert_eq!(state_of(&pool, &live), STATE_CREATED);
    }

    #[test]
    fn one_merchants_idem_key_cannot_replay_into_another_merchants_namespace() {
        assert_eq!(intent_scope("mr_a"), "mi:mr_a");
        assert_ne!(intent_scope("mr_a"), intent_scope("mr_b"));
        let pool = crate::db::open_memory().unwrap();
        let conn = pool.get().unwrap();
        crate::db::idem_put(&conn, "order-1", &intent_scope("mr_a"), "{\"ok\":true}").unwrap();
        assert!(crate::db::idem_get(&conn, "order-1", &intent_scope("mr_a"))
            .unwrap()
            .is_some());
        assert!(crate::db::idem_get(&conn, "order-1", &intent_scope("mr_b"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn issuance_ceilings_bound_what_one_shop_can_bill_in_a_day() {
        // The fixture already issued one 10-eme intent, which the window counts.
        let (pool, _, m) = fixture(10, 1_000);
        let headroom = merchant::DEFAULT_DAILY_ISSUE_CAP - 10;
        let mut conn = pool.get().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        // The cap counts what has been issued plus what is being asked for, so
        // the last eme under it is allowed and the next one is not.
        assert_eq!(
            merchant::check_issuance(&tx, &m, headroom, now_ms()).unwrap(),
            IssueGuard::Ok
        );
        assert!(matches!(
            merchant::check_issuance(&tx, &m, headroom + 1, now_ms()).unwrap(),
            IssueGuard::DailyCapExceeded { issued: 10, .. }
        ));
        // A shop may not park more unanswered bills than its open-intent ceiling
        // however small they are.
        let filled = MerchantRow {
            max_open_intents: 1,
            ..m.clone()
        };
        assert!(matches!(
            merchant::check_issuance(&tx, &filled, 1, now_ms()).unwrap(),
            IssueGuard::TooManyOpen { limit: 1 }
        ));
        tx.commit().unwrap();
    }

    /// Register a shop owned by `acct-m`, reporting the outcome.
    fn register_shop(pool: &Pool, name: &str) -> merchant::RegisterOutcome {
        register_shop_as(pool, "acct-m", name)
    }

    fn register_shop_as(pool: &Pool, owner: &str, name: &str) -> merchant::RegisterOutcome {
        let mut conn = pool.get().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let out = merchant::register(&tx, owner, name, None, None, None).unwrap();
        tx.commit().unwrap();
        out
    }

    /// Withdraw the unanswered bill the fixture leaves outstanding, so a shop can
    /// be closed.
    fn clear_open_intent(pool: &Pool, intent_id: &str, merchant_id: &str) {
        assert_eq!(
            cancel(&pool.get().unwrap(), intent_id, merchant_id).unwrap()["ok"],
            json!(true)
        );
    }

    fn set_shop_status(pool: &Pool, merchant_id: &str, status: &str) {
        let mut conn = pool.get().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert!(merchant::set_status(&tx, merchant_id, "acct-m", status).unwrap());
        tx.commit().unwrap();
    }

    fn close_shop(pool: &Pool, merchant_id: &str) -> merchant::CloseOutcome {
        let mut conn = pool.get().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let out = merchant::close(&tx, merchant_id, "acct-m").unwrap();
        tx.commit().unwrap();
        out
    }

    /// The squatting hole, closed: stopping a shop does NOT hand the slot back,
    /// because stopping does not hand the NAME back either. Counting only active
    /// rows would have let register → disable → register run forever.
    #[test]
    fn a_stopped_shop_keeps_occupying_its_owners_slot() {
        // The fixture already registered "Piggle Shop", so one slot is spent.
        let (pool, _, m) = fixture(300, 1_000);
        assert!(matches!(
            register_shop(&pool, "Second Shop"),
            merchant::RegisterOutcome::Ok { .. }
        ));
        assert!(matches!(
            register_shop(&pool, "Third Shop"),
            merchant::RegisterOutcome::Ok { .. }
        ));
        assert!(matches!(
            register_shop(&pool, "Fourth Shop"),
            merchant::RegisterOutcome::TooManyMerchants
        ));
        // Stopping the first one frees nothing.
        set_shop_status(&pool, &m.merchant_id, merchant::STATUS_DISABLED);
        assert!(matches!(
            register_shop(&pool, "Fourth Shop"),
            merchant::RegisterOutcome::TooManyMerchants
        ));
        // …and it still holds its name against everyone else, which is why the
        // slot must not come back either: otherwise this pair of moves would
        // accumulate names without bound.
        assert!(matches!(
            register_shop_as(&pool, "acct-a", "PiggleShop"),
            merchant::RegisterOutcome::NameTaken
        ));
    }

    #[test]
    fn closing_a_shop_gives_back_both_the_slot_and_the_name() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        clear_open_intent(&pool, &intent_id, &m.merchant_id);
        assert_eq!(close_shop(&pool, &m.merchant_id), merchant::CloseOutcome::Ok);
        // The name is claimable again — by anyone, including a different account.
        assert!(matches!(
            register_shop(&pool, "Piggle Shop"),
            merchant::RegisterOutcome::Ok { .. }
        ));
        // Closing twice is not a second slot.
        assert_eq!(
            close_shop(&pool, &m.merchant_id),
            merchant::CloseOutcome::NotFound
        );
        // The closed row survives, so the ledger can still say who was paid…
        let closed = merchant::get(&pool.get().unwrap(), &m.merchant_id)
            .unwrap()
            .expect("the row is kept for history");
        assert_eq!(closed.status, merchant::STATUS_DELETED);
        assert!(!closed.is_active());
        // …and its credential is gone, so the old key authenticates nothing.
        let key_hash: Option<String> = pool
            .get()
            .unwrap()
            .query_row(
                "SELECT api_key_hash FROM merchants WHERE merchant_id = ?1",
                [&m.merchant_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(key_hash, None);
    }

    #[test]
    fn a_shop_with_customers_still_holding_bills_cannot_be_closed() {
        // The fixture leaves one unanswered intent outstanding.
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(
            close_shop(&pool, &m.merchant_id),
            merchant::CloseOutcome::HasOpenIntents { count: 1 }
        );
        // Withdrawing the bill clears the way.
        clear_open_intent(&pool, &intent_id, &m.merchant_id);
        assert_eq!(close_shop(&pool, &m.merchant_id), merchant::CloseOutcome::Ok);
    }

    #[test]
    fn a_settled_history_does_not_block_closing() {
        // Paid, declined and expired bills are nobody's outstanding claim — and
        // the `payment_intents` foreign key is exactly why closing is a status
        // change rather than a DELETE.
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        assert_eq!(close_shop(&pool, &m.merchant_id), merchant::CloseOutcome::Ok);
        assert_eq!(state_of(&pool, &intent_id), STATE_PAID);
    }

    #[test]
    fn a_forced_refund_happens_once_and_leaves_both_movements_in_the_ledger() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        let mut conn = pool.get().unwrap();
        let out = force_refund(&mut conn, &intent_id, "chargeback").unwrap();
        let RefundOutcome::Ok { amount, .. } = out else {
            panic!("refund refused: {out:?}");
        };
        assert_eq!(amount, 300);
        // The second press finds the claim taken.
        assert!(matches!(
            force_refund(&mut conn, &intent_id, "chargeback").unwrap(),
            RefundOutcome::AlreadyRefunded
        ));
        drop(conn);
        assert_eq!(balance_of(&pool, "acct-a"), 1_000);
        assert_eq!(balance_of(&pool, "acct-m"), 0);
        // `paid` is not rewound — the record still says the purchase happened.
        assert_eq!(state_of(&pool, &intent_id), STATE_PAID);
    }

    #[test]
    fn a_refund_the_merchant_can_no_longer_fund_is_reported_not_faked() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        let mut conn = pool.get().unwrap();
        // The shop withdrew its takings to the MC world — the accepted risk.
        conn.execute(
            "UPDATE accounts SET balance = 0 WHERE account_id = 'acct-m'",
            [],
        )
        .unwrap();
        let out = force_refund(&mut conn, &intent_id, "fraud").unwrap();
        assert!(matches!(out, RefundOutcome::MerchantShort { balance: 0 }), "{out:?}");
        drop(conn);
        assert_eq!(balance_of(&pool, "acct-a"), 700, "money appeared from nowhere");
        // The claim rolled back with the transfer, so a retry is possible once the
        // merchant has funds again.
        let mut conn = pool.get().unwrap();
        conn.execute(
            "UPDATE accounts SET balance = 300 WHERE account_id = 'acct-m'",
            [],
        )
        .unwrap();
        assert!(matches!(
            force_refund(&mut conn, &intent_id, "fraud").unwrap(),
            RefundOutcome::Ok { .. }
        ));
    }

    #[test]
    fn an_unpaid_intent_has_nothing_to_refund() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        let mut conn = pool.get().unwrap();
        assert!(matches!(
            force_refund(&mut conn, &intent_id, "x").unwrap(),
            RefundOutcome::NotPaid { .. }
        ));
        assert!(matches!(
            force_refund(&mut conn, "pi_nope", "x").unwrap(),
            RefundOutcome::UnknownIntent
        ));
    }

    #[test]
    fn a_description_that_could_rewrite_the_approval_screen_is_refused_at_creation() {
        let (pool, _, m) = fixture(300, 1_000);
        let mut conn = pool.get().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let out = create(
            &tx,
            &m,
            &NewIntent {
                idem_key: "k-bidi",
                amount: 10,
                description: "りんご\u{202E}MoyMoy 公式確認",
                order_ref: None,
                launch_app_id: None,
                payer_hint_account_id: None,
                expires_in_secs: None,
            },
        )
        .unwrap();
        assert!(
            matches!(out, CreateOutcome::BadDescription(TextReject::Invisible)),
            "{out:?}"
        );
    }

    #[test]
    fn a_ttl_outside_the_allowed_band_is_refused() {
        let (pool, _, m) = fixture(300, 1_000);
        let mut conn = pool.get().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        for bad in [MIN_TTL_SECS - 1, MAX_TTL_SECS + 1, 0, -60] {
            let out = create(
                &tx,
                &m,
                &NewIntent {
                    idem_key: "k-ttl",
                    amount: 10,
                    description: "x",
                    order_ref: None,
                    launch_app_id: None,
                    payer_hint_account_id: None,
                    expires_in_secs: Some(bad),
                },
            )
            .unwrap();
            assert!(matches!(out, CreateOutcome::BadTtl), "{bad} was accepted");
        }
    }
}
