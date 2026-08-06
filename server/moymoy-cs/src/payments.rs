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

/// How long an escrowed payment waits after the merchant reports it fulfilled
/// before the money is released.
///
/// **The gate is the fulfilment report; this is the pause after it.** A shop that
/// reports an order fulfilled the moment it is placed still cannot have the money
/// for this long, which is the window in which a mistake can be caught before it
/// becomes unrecoverable. It is short because it is not the protection — holding
/// the money until the goods are reported delivered is.
///
/// A constant, not configuration, for the reason the risk thresholds are: a hold
/// period that an environment variable can set to zero is a hold period chosen by
/// whoever edits the launcher.
pub const RELEASE_GATE_MS: i64 = 10 * 60 * 1000;

/// How long an escrowed payment may sit with no fulfilment report at all before
/// it is considered abandoned by the merchant.
///
/// **Recorded but not yet acted on.** `escrow_deadline_unix_ms` is written on
/// every escrowed intent so the data exists from the first payment onward, but
/// nothing in this build reads it — deciding what happens to an order a shop
/// never reports on needs the shop's side of the conversation to exist first.
/// Writing the column now means the intents created in between are not a gap when
/// that arrives.
pub const ESCROW_DEADLINE_MS: i64 = 6 * 60 * 60 * 1000;

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
    // ── escrow (v9) ─────────────────────────────────────────────────────────
    // All `None` on an intent that never reached `paid`, and all `None` on the
    // pre-v9 `paid` intents too — except `released_unix_ms`, which the migration
    // stamped precisely so those rows are not mistaken for money still owed.
    /// When the payer's money reached the escrow account.
    pub escrowed_unix_ms: Option<i64>,
    /// The earliest the release sweep may pay out (`escrowed` + [`RELEASE_GATE_MS`]).
    pub release_due_unix_ms: Option<i64>,
    /// When this intent stops waiting for a fulfilment report (`escrowed` +
    /// [`ESCROW_DEADLINE_MS`]). Written, not yet acted on.
    pub escrow_deadline_unix_ms: Option<i64>,
    /// When the merchant reported the order fulfilled. `None` ⇒ still waiting,
    /// and the sweep will not release however long the gate has been past.
    pub fulfilled_unix_ms: Option<i64>,
    /// What the merchant is owed, in minor units. Never above [`Intent::amount`];
    /// the difference goes back to the payer.
    pub fulfilled_amount: Option<i64>,
    /// The shop's own account of why it fell short (v10), sanitized and bounded
    /// like every other merchant-supplied string. `None` when it gave none, which
    /// a fully fulfilled order has no reason to. It is the only explanation that
    /// exists for a movement made against the buyer, so it lives beside the amount
    /// it explains rather than only in a log that rotates away.
    pub fulfil_reason: Option<String>,
    /// When the no-report deadline ran out and the sweep set this intent aside
    /// for a person to decide (v12).
    ///
    /// **Written INSTEAD of moving money, and never together with
    /// `released_unix_ms`.** A parked intent is unresolved on purpose: its money
    /// is still in escrow, and the operator path that can move it
    /// ([`force_refund`]) works precisely because nothing here was closed.
    pub escrow_parked_unix_ms: Option<i64>,
    /// When escrow paid out. Doubles as the sweep's exactly-once claim.
    pub released_unix_ms: Option<i64>,
    /// The escrow → merchant ledger row, if that half moved anything.
    pub release_tx_id: Option<String>,
    /// The escrow → payer ledger row, if there was a shortfall to return.
    pub escrow_refund_tx_id: Option<String>,
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
     refund_tx_id, created_unix_ms, expires_unix_ms, \
     escrowed_unix_ms, release_due_unix_ms, escrow_deadline_unix_ms, fulfilled_unix_ms, \
     fulfilled_amount, fulfil_reason, escrow_parked_unix_ms, released_unix_ms, release_tx_id, \
     escrow_refund_tx_id";

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
        escrowed_unix_ms: r.get(14)?,
        release_due_unix_ms: r.get(15)?,
        escrow_deadline_unix_ms: r.get(16)?,
        fulfilled_unix_ms: r.get(17)?,
        fulfilled_amount: r.get(18)?,
        fulfil_reason: r.get(19)?,
        escrow_parked_unix_ms: r.get(20)?,
        released_unix_ms: r.get(21)?,
        release_tx_id: r.get(22)?,
        escrow_refund_tx_id: r.get(23)?,
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
        // Nothing is escrowed until somebody approves; `settle` fills these in.
        escrowed_unix_ms: None,
        release_due_unix_ms: None,
        escrow_deadline_unix_ms: None,
        fulfilled_unix_ms: None,
        fulfilled_amount: None,
        fulfil_reason: None,
        escrow_parked_unix_ms: None,
        released_unix_ms: None,
        release_tx_id: None,
        escrow_refund_tx_id: None,
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

// ── fulfilment report (merchant API key) ─────────────────────────────────────

/// Outcome of a merchant reporting how much of an order it actually delivered.
#[derive(Debug, PartialEq, Eq)]
pub enum FulfillOutcome {
    Ok {
        fulfilled_amount: i64,
        refund_amount: i64,
    },
    /// No such intent — or one belonging to another shop, which reads the same on
    /// purpose (the discipline `cancel` and `op_status` use, so this cannot be
    /// turned into an oracle for other shops' order flow).
    UnknownIntent,
    /// Reported before, and a fulfilment is stated once. `state` is what the
    /// earlier report said, so a retrying integrator can see it agrees.
    AlreadyFulfilled { fulfilled_amount: Option<i64> },
    /// Nothing is being held for this intent, so there is nothing to report on:
    /// it was never paid, or its money has already left escrow.
    NotHeld { stage: &'static str },
    /// Outside `0..=amount`.
    AmountOutOfRange { amount: i64 },
    /// The explanation was too long, or carried characters that do not render as
    /// themselves. Refused rather than truncated or stripped: the record has to
    /// say what the shop said, and a shortened string is neither what it said nor
    /// obviously not.
    BadReason(TextReject),
}

/// Record how much of a paid, escrowed order the merchant actually delivered.
///
/// ## This does not move money, and it cannot move money UP
///
/// **The invariant that `/merchant/v1/*` can move no funds is intact.** It is
/// worth being explicit, because an endpoint that takes an amount from an API key
/// holder looks at first glance like the thing that invariant forbids.
///
/// Two properties make it not that. First, the only number a shop can state here
/// is bounded above by `intent.amount` — a figure the CUSTOMER already saw and
/// approved — so the most this can do is confirm what was already authorized;
/// there is no value of `fulfilled_amount` that takes more. Second, it writes no
/// ledger row at all: it records a fact about the order, and the release sweep
/// moves money later, after the gate. A leaked API key can therefore under-report
/// its own takings (giving the customer their money back) and nothing else.
///
/// The direction is deliberately asymmetric: reporting is the shop's way of
/// giving up its claim on the part it could not deliver.
pub fn fulfill(
    conn: &mut Connection,
    merchant_id: &str,
    intent_id: &str,
    fulfilled_amount: i64,
    reason: Option<&str>,
) -> rusqlite::Result<FulfillOutcome> {
    let now = now_ms();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // The ownership filter is in the read, so another shop's intent never even
    // becomes a row this function has an opinion about.
    let Some(intent) = tx
        .query_row(
            &format!(
                "SELECT {INTENT_COLS} FROM payment_intents WHERE intent_id = ?1 AND merchant_id = ?2"
            ),
            params![intent_id, merchant_id],
            row_to_intent,
        )
        .optional()?
    else {
        tx.commit()?;
        return Ok(FulfillOutcome::UnknownIntent);
    };

    // Reported already: said before the stage check, because it is the more
    // useful answer for an intent that is both fulfilled and since released.
    if intent.fulfilled_unix_ms.is_some() {
        tx.commit()?;
        return Ok(FulfillOutcome::AlreadyFulfilled {
            fulfilled_amount: intent.fulfilled_amount,
        });
    }
    // Only money still being held can be reported on. `held` excludes both an
    // intent that was never paid and one whose money has left escrow — including
    // the pre-v9 payments the migration closed, which went straight to the
    // merchant and which the sweep will never look at again. Accepting a report
    // on one of those would leave the shop a record saying it reported, and no
    // effect anywhere.
    let stage = escrow_stage(&intent);
    if stage != "held" {
        tx.commit()?;
        return Ok(FulfillOutcome::NotHeld { stage });
    }
    // `0` is in range and means "nothing could be delivered" — the whole payment
    // goes back. The caller distinguishes that from an absent field, which is
    // refused before this is reached.
    if !(0..=intent.amount).contains(&fulfilled_amount) {
        tx.commit()?;
        return Ok(FulfillOutcome::AmountOutOfRange {
            amount: intent.amount,
        });
    }
    // Vetted like every other merchant-supplied string, and for the same reason:
    // the sales page renders it, so a bidi override in here would let a leaked API
    // key put words on a screen its owner reads as MoyMoy's. Absent and blank both
    // mean "no explanation given" — the `order_ref` pattern in `create` — because
    // a fully fulfilled order has nothing to explain and requiring one would only
    // teach integrators to send a placeholder.
    let reason = match reason.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => match merchant::sanitize_text(s, merchant::MAX_FULFIL_REASON_CHARS) {
            Ok(v) => Some(v),
            Err(e) => {
                tx.commit()?;
                return Ok(FulfillOutcome::BadReason(e));
            }
        },
        None => None,
    };

    // THE claim, in the shape `force_refund` uses: `fulfilled_unix_ms IS NULL` is
    // part of the UPDATE, so two reports arriving together produce one. The state
    // conditions are repeated from the checks above because those read the row
    // and this writes it — between them is where a concurrent release would land.
    //
    // The amount and the reason are set by the SAME statement. Writing the reason
    // separately would make a row with a shortfall and no explanation reachable —
    // which is exactly the state this column exists to prevent, and the one that
    // would appear whenever the second write was the one that failed.
    let claimed = tx.execute(
        "UPDATE payment_intents SET fulfilled_unix_ms = ?3, fulfilled_amount = ?4, \
                fulfil_reason = ?6, updated_unix_ms = ?3 \
         WHERE intent_id = ?1 AND merchant_id = ?2 AND fulfilled_unix_ms IS NULL \
           AND state = ?5 AND escrowed_unix_ms IS NOT NULL AND released_unix_ms IS NULL",
        params![intent_id, merchant_id, now, fulfilled_amount, STATE_PAID, reason],
    )?;
    if claimed == 0 {
        tx.commit()?;
        return Ok(FulfillOutcome::AlreadyFulfilled {
            fulfilled_amount: None,
        });
    }
    tx.commit()?;

    let refund_amount = intent.amount - fulfilled_amount;
    // Logged as well as stored — the row is the record, this is the operational
    // trace that says when it arrived.
    tracing::info!(
        intent_id, merchant_id, fulfilled_amount, refund_amount,
        reason = reason.as_deref().unwrap_or(""),
        "merchant reported an order fulfilled"
    );
    Ok(FulfillOutcome::Ok {
        fulfilled_amount,
        refund_amount,
    })
}

// ── the shop's own sales history (portal, session-authenticated) ─────────────

/// Rows one sales page returns when the caller does not say, and the ceiling it
/// will not go past whatever the caller says. The same clamp
/// [`crate::wallet::history`] uses, for the same reason: a page is a page.
pub const SALES_DEFAULT_LIMIT: i64 = 50;
pub const SALES_MAX_LIMIT: i64 = 200;

/// Everything a shop's owner sees about what has been paid to it.
///
/// ## `held_total_minor` is DERIVED, not a balance
///
/// It is `SUM(amount)` over this merchant's escrowed-and-unreleased intents, read
/// fresh on every call. There is deliberately no "pending balance" column, and
/// adding one would be a mistake worth naming: the money it would describe really
/// exists, in the escrow account, and a second number tracking the same thing is
/// a number that can disagree with it. This repository's precedent is the same —
/// `reserve_withdraw` does not add a "reserved" column, it moves the eme and lets
/// the `emerald_ops` row hold the claim.
///
/// It also cannot be answered from the escrow account's balance, because that one
/// account holds every shop's held money in a single pot. `merchant::close` makes
/// the same judgement for the same reason, and both count intents.
pub fn sales_page(
    conn: &Connection,
    m: &MerchantRow,
    limit: i64,
    now: i64,
) -> Result<Value, ApiError> {
    let limit = limit.clamp(1, SALES_MAX_LIMIT);
    // One more than asked for, purely to answer "is there more?". A page that
    // silently stops reads as "this is everything", which is how a shop concludes
    // it was paid less than it was.
    let rows: Vec<Intent> = {
        // `rowid DESC` breaks ties, because `created_unix_ms` is milliseconds and a
        // busy shop issues several bills inside one. Without it the order of a tie
        // group is unspecified, so the page reshuffles between reloads and — worse
        // — WHICH row falls off the end at the limit is arbitrary, which quietly
        // undermines the `truncated` flag below. rowid follows insertion, so it
        // agrees with "newest first" rather than being an arbitrary tiebreak on a
        // random id. The plan still drives the primary ordering off
        // `idx_intents_merchant_time`; only the tie groups are sorted.
        let mut stmt = conn.prepare(&format!(
            "SELECT {INTENT_COLS} FROM payment_intents WHERE merchant_id = ?1 \
             ORDER BY created_unix_ms DESC, rowid DESC LIMIT ?2"
        ))?;
        let v = stmt
            .query_map(params![m.merchant_id, limit + 1], row_to_intent)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };
    let truncated = rows.len() as i64 > limit;
    let sales = rows
        .iter()
        .take(limit as usize)
        .map(|i| merchant_view(m, i, now))
        .collect::<Result<Vec<_>, ApiError>>()?;

    // Counted from the intents, not from any balance — see the note above.
    let (held_count, held_total): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(amount), 0) FROM payment_intents \
         WHERE merchant_id = ?1 AND escrowed_unix_ms IS NOT NULL AND released_unix_ms IS NULL",
        [&m.merchant_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    Ok(json!({
        "merchant_id": m.merchant_id,
        "name": m.name,
        "held_count": held_count,
        "held_total_minor": held_total,
        "limit": limit,
        // Stated rather than implied: the caller can ask for more.
        "truncated": truncated,
        "sales": sales,
    }))
}

// ── escrow release ───────────────────────────────────────────────────────────

/// Label on the escrow → merchant payout.
const RELEASE_LABEL_PREFIX: &str = "売上";
/// Label on the escrow → payer return of whatever was not fulfilled.
const ESCROW_REFUND_LABEL_PREFIX: &str = "未履行分の返金";

/// Close out every escrowed intent that has reached one of its two ends.
///
/// Rides the same 30-second pass as [`expire_pass`] and the emerald
/// reconciliation.
///
/// | the intent | end |
/// |---|---|
/// | reported, and the gate has elapsed | pay the shop what it reported, return the rest |
/// | NOT reported, and the deadline has passed | **park it for a person; move nothing** |
/// | anything else | left alone |
///
/// The second end moves no money at all, and that is the decision it embodies:
/// **silence is not evidence.** A shop that has not reported has not said the
/// goods stayed put — it has said nothing — so refunding on a timer would decide
/// the question with no evidence, in a system where the shop-side delivery retry
/// is deliberately unbounded during an outage. This repository already answers
/// the same question the same way for emeralds: an ungrantable payout goes to
/// `stuck` and is never auto-refunded (`charge.rs`, R008), because "we do not
/// know" is its own outcome. It is the one branch that warns — see
/// [`release_one`].
///
/// **One row per transaction, not one bulk UPDATE.** `expire_pass` can be a
/// single statement because expiring an intent owes nobody anything; a release
/// moves money — up to two ways at once — and the row's state change and those
/// movements have to be the same atomic unit or a crash between them leaves the
/// intent marked paid-out with the payout missing. This is the shape
/// `charge::withdraw`'s dead-letter pass uses, for the same reason.
///
/// Returns how many intents were closed out.
pub fn release_pass(conn: &mut Connection, now: i64) -> rusqlite::Result<usize> {
    let due: Vec<String> = {
        // ONE scan for both ends. They share the partial index's predicate
        // (`released_unix_ms IS NULL AND escrowed_unix_ms IS NOT NULL`), so
        // splitting them into two queries would walk the same small set twice
        // and give the two ends separate batch limits for no reason.
        //
        // `release_due_unix_ms` orders the result because both deadlines are a
        // fixed offset from `escrowed_unix_ms`, which makes it oldest-first for
        // either end — and it is the indexed column, so the order is free.
        // `escrow_parked_unix_ms IS NULL` matches the partial index's predicate
        // (v12) AND stops the sweep re-parking what it has already parked — a
        // warning repeated every 30 seconds until a human acts is a log nobody can
        // read.
        let mut stmt = conn.prepare(
            "SELECT intent_id FROM payment_intents \
             WHERE released_unix_ms IS NULL AND escrowed_unix_ms IS NOT NULL \
               AND escrow_parked_unix_ms IS NULL \
               AND ( (fulfilled_unix_ms IS NOT NULL AND release_due_unix_ms <= ?1) \
                  OR (fulfilled_unix_ms IS NULL AND escrow_deadline_unix_ms <= ?1) ) \
             ORDER BY release_due_unix_ms ASC LIMIT 50",
        )?;
        // Bound to a local so the borrowing `MappedRows` temporary drops at the
        // `;` (before `stmt`), the same shape `wallet::history` uses.
        let v = stmt
            .query_map([now], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };

    let mut released = 0usize;
    for intent_id in due {
        // One clock for the pass, like `expire_pass` — the selection above already
        // decided due-ness against it, and re-reading it per row would let a row
        // selected as due be claimed against a later instant than the one that
        // chose it.
        match release_one(conn, &intent_id, now) {
            Ok(true) => released += 1,
            Ok(false) => {}
            Err(e) => {
                // The row is untouched (the transaction rolled back), so the next
                // pass sees it again. Logged rather than propagated: one bad row
                // must not stop the other 49, nor the emerald reconciliation this
                // pass shares a task with.
                tracing::error!(error = %e, %intent_id,
                    "escrow release failed; the intent stays unreleased and will be retried");
            }
        }
    }
    Ok(released)
}

/// Which end this intent reached. Decided from the row inside the claiming
/// transaction, never from the selection that queued it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReleaseEnd {
    /// The shop reported, and the gate elapsed. Money moves.
    Reported,
    /// The shop never reported, and [`ESCROW_DEADLINE_MS`] passed. **Nothing
    /// moves** — the intent is parked for a person.
    Unreported,
}

/// Release one escrowed intent. `true` when this call is the one that did it.
///
/// The claim is `released_unix_ms IS NULL` inside the UPDATE — the same shape as
/// `force_refund`'s and the withdraw settler's — so two passes (or a pass racing a
/// restart) produce one payout. Everything after the claim runs in the same
/// transaction, so "the row says released" and "the money moved" cannot come
/// apart.
fn release_one(conn: &mut Connection, intent_id: &str, now: i64) -> rusqlite::Result<bool> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let Some(intent) = tx
        .query_row(
            &format!("SELECT {INTENT_COLS} FROM payment_intents WHERE intent_id = ?1"),
            [intent_id],
            row_to_intent,
        )
        .optional()?
    else {
        tx.commit()?;
        return Ok(false);
    };
    let (Some(payer), Some(m)) = (
        intent.payer_account_id.clone(),
        merchant::get(&tx, &intent.merchant_id)?,
    ) else {
        // An escrowed intent always has both. Left unreleased for a human rather
        // than paid to a guess — the money is safe in escrow meanwhile, which is
        // the whole point of holding it.
        tx.commit()?;
        tracing::error!(intent_id, "escrowed intent has no payer or no merchant; not releasing");
        return Ok(false);
    };

    // Which end applies is read from the row, not carried from the selection: the
    // shop may have reported in the seconds between the scan and this transaction,
    // which turns a deadline expiry into an ordinary release.
    let end = match (intent.fulfilled_unix_ms, intent.escrow_deadline_unix_ms) {
        (Some(_), _) => ReleaseEnd::Reported,
        (None, Some(deadline)) if deadline <= now => ReleaseEnd::Unreported,
        // Neither: the scan queued it and the row moved on. Nothing to do.
        _ => {
            tx.commit()?;
            return Ok(false);
        }
    };

    // THE claim. Every condition the selection used is repeated here, because the
    // selection ran outside this transaction and any of them could have changed
    // since (a late fulfilment edit, another pass, a restart mid-batch). The two
    // ends carry DIFFERENT conditions, so each claims on its own — a row that
    // slipped from one end to the other between the scan and here fails its claim
    // and is picked up by the next pass under the end it now belongs to.
    //
    // They also write DIFFERENT columns. The unreported end sets
    // `escrow_parked_unix_ms` and leaves `released_unix_ms` NULL, because parking
    // is not a resolution: the money is still in escrow and an operator still has
    // to be able to move it. Stamping it released would close the row to
    // `force_refund` — the one route that can.
    if let ReleaseEnd::Unreported = end {
        // `fulfilled_unix_ms IS NULL` is part of THIS claim, so a report landing
        // between the read above and this statement wins: the UPDATE matches
        // nothing, and the next pass releases it properly instead of parking a
        // delivery that did happen.
        let parked = tx.execute(
            "UPDATE payment_intents SET escrow_parked_unix_ms = ?2, updated_unix_ms = ?2 \
             WHERE intent_id = ?1 AND released_unix_ms IS NULL AND escrowed_unix_ms IS NOT NULL \
               AND escrow_parked_unix_ms IS NULL \
               AND fulfilled_unix_ms IS NULL AND escrow_deadline_unix_ms <= ?2",
            params![intent_id, now],
        )?;
        if parked == 0 {
            tx.commit()?;
            return Ok(false);
        }
        tx.commit()?;
        // **A warning, not an info line.** Reaching here means a shop went silent
        // for six hours on an order a customer had already paid for. Nothing has
        // been decided and nobody has been paid — which is the point — but that
        // also means the money sits until a person acts, and this line is the only
        // thing that says so.
        tracing::warn!(
            intent_id, amount = intent.amount, merchant = %m.merchant_id, payer,
            deadline_ms = ESCROW_DEADLINE_MS,
            "escrow deadline passed with no fulfilment report — the payment is PARKED, not \
             refunded: nobody has said whether the goods went out, so the money stays in escrow \
             for an operator to decide (`moymoy-cs admin refund` returns it to the buyer)"
        );
        return Ok(true);
    }

    let claimed = tx.execute(
        "UPDATE payment_intents SET released_unix_ms = ?2, updated_unix_ms = ?2 \
         WHERE intent_id = ?1 AND released_unix_ms IS NULL AND escrowed_unix_ms IS NOT NULL \
           AND fulfilled_unix_ms IS NOT NULL AND release_due_unix_ms <= ?2",
        params![intent_id, now],
    )?;
    if claimed == 0 {
        tx.commit()?;
        return Ok(false);
    }

    // What the merchant is owed, bounded by what the customer actually paid. The
    // fulfilment endpoint already refuses anything outside the range; clamping
    // here as well means a row edited by any other route still cannot pay out more
    // than escrow received for it.
    let owed = intent.fulfilled_amount.unwrap_or(0).clamp(0, intent.amount);
    let refund = intent.amount - owed;
    let escrow = wallet::escrow_account_id();

    let mut release_tx_id: Option<String> = None;
    if owed > 0 {
        let label = format!("{RELEASE_LABEL_PREFIX}: {}", intent.description);
        match wallet::transfer(&tx, escrow, &m.account_id, owed, "pay", &label)? {
            TxResult::Ok { tx_id, .. } => release_tx_id = Some(tx_id),
            other => {
                // Escrow holds this money by construction, so a refusal here is a
                // broken invariant. Rolling back un-claims the row so the next
                // pass retries it — releasing the refund half alone would hand the
                // customer their money back for goods the shop already sent.
                drop(tx);
                tracing::error!(intent_id, owed, merchant = %m.merchant_id, ?other,
                    "escrow could not pay the merchant; NOTHING was released (the intent stays \
                     claimable and the money stays in escrow)");
                return Ok(false);
            }
        }
    }

    let mut escrow_refund_tx_id: Option<String> = None;
    if refund > 0 {
        let label = format!("{ESCROW_REFUND_LABEL_PREFIX}: {}", intent.description);
        match wallet::transfer(&tx, escrow, &payer, refund, "pay", &label)? {
            TxResult::Ok { tx_id, .. } => escrow_refund_tx_id = Some(tx_id),
            other => {
                // Same rollback for the same reason, from the other side: the
                // merchant must not be paid while the customer's share is stranded
                // in escrow with the row marked released.
                drop(tx);
                tracing::error!(intent_id, refund, payer, ?other,
                    "escrow could not return the unfulfilled share; NOTHING was released");
                return Ok(false);
            }
        }
    }

    tx.execute(
        "UPDATE payment_intents SET release_tx_id = ?2, escrow_refund_tx_id = ?3 \
         WHERE intent_id = ?1",
        params![intent_id, release_tx_id, escrow_refund_tx_id],
    )?;
    tx.commit()?;
    tracing::info!(intent_id, owed, refund, merchant = %m.merchant_id, "escrow released");
    Ok(true)
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

/// Where a paid intent's money currently is, as one word.
///
/// Distinct from `state`, which is about the ORDER (`created` → `paid` …). A
/// `paid` intent can be any of these, and to the shop they are three different
/// situations: money it cannot have yet, money on its way, and money it has.
pub fn escrow_stage(intent: &Intent) -> &'static str {
    // Parked outranks everything except released, and is reported as its own
    // stage rather than folded into `held`. Both mean "escrow has the money", but
    // only one of them is waiting for a person — a shop shown `held` for a payment
    // that is actually stuck has no way to learn why it never arrived.
    if intent.escrow_parked_unix_ms.is_some() && intent.released_unix_ms.is_none() {
        return "parked";
    }
    match (
        intent.escrowed_unix_ms,
        intent.fulfilled_unix_ms,
        intent.released_unix_ms,
    ) {
        // Never escrowed: unpaid, or one of the pre-v9 payments the migration
        // closed. Neither is money this mechanism is holding.
        (None, _, _) => "none",
        (Some(_), _, Some(_)) => "released",
        (Some(_), Some(_), None) => "fulfilled",
        (Some(_), None, None) => "held",
    }
}

/// The escrow half of an intent, for the shop that owns it.
///
/// Amount fields carry `_minor` for the reason `/merchant/v1` renamed `amount` in
/// v8: a number whose unit is only known by convention is a number the next
/// migration silently changes the meaning of.
///
/// The ledger row ids (`release_tx_id`, `escrow_refund_tx_id`) are deliberately
/// NOT here. This view already withholds the payer's identity behind `payer_ref`;
/// MoyMoy's internal ledger ids are the same kind of thing — nothing a shop can
/// act on, and a handle onto the wallet's internals. An operator sees them
/// through the admin refund report instead.
fn escrow_view(intent: &Intent) -> Value {
    json!({
        "stage": escrow_stage(intent),
        "escrowed_unix_ms": intent.escrowed_unix_ms,
        "release_due_unix_ms": intent.release_due_unix_ms,
        "escrow_deadline_unix_ms": intent.escrow_deadline_unix_ms,
        "fulfilled_unix_ms": intent.fulfilled_unix_ms,
        // Both `null` until the shop reports; afterwards they sum to `amount_minor`.
        "fulfilled_amount_minor": intent.fulfilled_amount,
        "refunded_amount_minor": intent.fulfilled_amount.map(|f| intent.amount - f.clamp(0, intent.amount)),
        // The shop's own words, echoed back so it can see what was recorded — and
        // read by the sales page, which is where the explanation is actually
        // wanted.
        "fulfil_reason": intent.fulfil_reason,
        // Non-null means the deadline ran out with no report and a person has to
        // decide. The money is still here; nothing was refunded.
        "parked_unix_ms": intent.escrow_parked_unix_ms,
        "released_unix_ms": intent.released_unix_ms,
    })
}

/// The face of an intent for the merchant that owns it. Carries `payer_ref` once
/// paid — a pseudonym stable within this shop and uncorrelatable outside it.
///
/// The amount goes out as `amount_minor`, matching what `/merchant/v1` now
/// accepts — this and `intent_create`'s reply are the two places a third party
/// reads an amount from this wallet, and both name the unit rather than assuming
/// the reader shares it. See `merchant::IntentCreateReq::amount_minor`.
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
        "amount_minor": intent.amount,
        "description": intent.description,
        "order_ref": intent.order_ref,
        "state": intent.effective_state(now),
        "payer_ref": payer_ref,
        "refunded": intent.refunded_unix_ms.is_some(),
        "created_unix_ms": intent.created_unix_ms,
        "expires_unix_ms": intent.expires_unix_ms,
        // Since v9 `state = paid` no longer means "the shop has the money", so the
        // shop is told where it actually is rather than being left to assume.
        "escrow": escrow_view(intent),
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

    // Amount comes from the stored intent, never from the request. The
    // DESTINATION is the escrow account rather than the shop: the payer is
    // debited now, exactly as before, but the money is MoyMoy's to hold until the
    // merchant reports the order fulfilled and the release gate elapses (see
    // `release_pass`). Nothing else about this transaction changes — the claim
    // above still decides the race, and the buyer still sees one `pay` row
    // labelled with the shop's name.
    //
    // The label stays `m.name` deliberately. It is what the app renders in the
    // history, and to the buyer this IS a payment to that shop; where the wallet
    // parks the money in the meantime is not a fact about their purchase.
    let result = wallet::transfer(
        &tx,
        payer,
        wallet::escrow_account_id(),
        intent.amount,
        "pay",
        &m.name,
    )?;
    match result {
        TxResult::Ok {
            tx_id,
            balance_after,
            ..
        } => {
            tx.execute(
                "UPDATE payment_intents SET tx_id = ?2, escrowed_unix_ms = ?3, \
                        release_due_unix_ms = ?4, escrow_deadline_unix_ms = ?5 \
                 WHERE intent_id = ?1",
                params![
                    intent.intent_id,
                    tx_id,
                    now,
                    now + RELEASE_GATE_MS,
                    now + ESCROW_DEADLINE_MS
                ],
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
            // `UnknownTarget` used to mean "this shop's account is gone". Since the
            // money goes to escrow it means the PAYER's row or the escrow row is
            // missing, and the escrow row missing would fail every payment in the
            // deployment — a startup that did not seed, not a fact about this
            // customer. It fails closed either way, but an operator has to be able
            // to see the difference between that and an ordinary refusal.
            if matches!(other, TxResult::UnknownTarget) {
                tracing::error!(
                    intent_id = %intent.intent_id, payer,
                    escrow = wallet::escrow_account_id(),
                    "settle: a payment could not be escrowed because an account row is missing — \
                     if this is the escrow account, EVERY payment is failing and the startup seed \
                     (wallet::seed_escrow_account) did not run"
                );
            }
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
    /// The merchant no longer holds the money.
    ///
    /// **Still reachable, and still the honest answer — for RELEASED payments
    /// only.** Escrow closed the window rather than the hole: while a payment is
    /// held, the money is MoyMoy's and a refund always has a source. Once the
    /// shop has reported the order fulfilled and been paid, its revenue is its
    /// own and it can withdraw the takings to the MC world, at which point there
    /// is nothing to reverse and this says so rather than papering over it with a
    /// refund from nowhere. See [`force_refund`] for which source is used when.
    MerchantShort { balance: i64 },
}

/// Return a paid intent's money to the payer.
///
/// **Not reachable from the merchant API**, by construction: it takes no
/// credential and lives here for an operator path to call. `paid` is not rewound
/// — the refund is its own transfer, and both movements stay in the ledger.
///
/// Exactly-once comes from the claim, in the same shape as the approval above:
/// `refunded_unix_ms IS NULL` is part of the UPDATE, so two operators pressing
/// the button together produce one refund.
///
/// ## Which account pays
///
/// Since v9 that depends on where the money currently is, and the intent row says
/// so without anyone having to look at balances:
///
/// * `released_unix_ms IS NULL` — still escrowed. MoyMoy holds it, so the refund
///   comes out of the escrow account and **cannot** report
///   [`RefundOutcome::MerchantShort`]: escrow received exactly this amount for
///   exactly this intent and has not paid it to anyone.
/// * released — the shop has been paid. The refund comes from the shop, exactly
///   as it did before escrow existed, and `MerchantShort` is a real outcome
///   again if the takings have been withdrawn to the MC world.
///
/// The rows the v9 migration stamped released carry no `escrowed_unix_ms`, which
/// is right: they were settled directly to the merchant and the merchant is where
/// their refund has to come from.
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
    // Where the money is decides where it comes back from. Both halves of the
    // predicate matter: `escrowed_unix_ms IS NOT NULL` says escrow ever received
    // it (the pre-v9 rows the migration stamped released did not), and
    // `released_unix_ms IS NULL` says escrow has not passed it on.
    //
    // **A PARKED intent satisfies both**, which is deliberate and load-bearing:
    // parking writes `escrow_parked_unix_ms` and leaves `released_unix_ms` alone
    // precisely so this path still reaches it. This is the only route by which a
    // parked payment can be returned to the buyer, so a future change that starts
    // treating parked rows as closed would strand them.
    let escrowed = intent.escrowed_unix_ms.is_some() && intent.released_unix_ms.is_none();
    let source = if escrowed {
        wallet::escrow_account_id()
    } else {
        m.account_id.as_str()
    };
    let label = format!("返金（運営措置）: {reason}");
    match wallet::transfer(&tx, source, &payer, intent.amount, "pay", &label)? {
        TxResult::Ok { tx_id, .. } => {
            tx.execute(
                "UPDATE payment_intents SET refund_tx_id = ?2 WHERE intent_id = ?1",
                params![intent_id, tx_id],
            )?;
            tx.commit()?;
            tracing::warn!(intent_id, amount = intent.amount, reason, escrowed,
                "operator-forced refund applied");
            Ok(RefundOutcome::Ok {
                tx_id,
                amount: intent.amount,
            })
        }
        TxResult::Insufficient { balance } => {
            // Rolls back, so the claim unwinds and the refund can be retried once
            // the source has funds again.
            //
            // From ESCROW this is a broken invariant, not an accepted risk: escrow
            // took in exactly this amount for this intent and the release sweep is
            // the only thing that pays it out, so a shortfall means something else
            // spent it. From the MERCHANT it is the accepted risk DEV.md records —
            // a shop that was paid and withdrew its takings to the MC world leaves
            // nothing to reverse.
            if escrowed {
                tracing::error!(intent_id, amount = intent.amount, balance, reason,
                    "operator-forced refund NOT applied — the ESCROW account is short, which it \
                     cannot legitimately be for an unreleased intent (invariant violated)");
            } else {
                tracing::error!(intent_id, amount = intent.amount, balance, reason,
                    "operator-forced refund NOT applied — this payment was already released and \
                     the merchant no longer holds the money (the accepted risk in DEV.md)");
            }
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
        // What `main` does before it serves anything. A wallet without this
        // account cannot take a payment at all (every approval transfers into
        // it), so a fixture without it is not a wallet under test.
        wallet::seed_escrow_account(&conn).expect("escrow account");
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

    /// What [`stepup_fixture`] funds its payer with.
    ///
    /// Derived from the threshold it has to clear rather than written as a
    /// literal: amounts are minor units now, so a fixed number that used to
    /// cover the top band is a fixture that quietly starts testing
    /// "insufficient" instead of the second factor it was written for.
    const STEPUP_BALANCE: i64 = riskauth::STEPUP_SINGLE * 2;

    /// An intent past the step-up threshold, on a wallet that can pay for it.
    fn stepup_fixture(email: Option<&str>) -> (Pool, String, i64) {
        let (pool, _, m) = fixture(300, STEPUP_BALANCE);
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

    fn stepup_verified_at(pool: &Pool) -> Option<i64> {
        pool.get()
            .unwrap()
            .query_row(
                "SELECT stepup_verified_unix_ms FROM accounts WHERE account_id = 'acct-a'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn windowed_outflow(pool: &Pool) -> i64 {
        riskauth::outflow_in_window(&pool.get().unwrap(), "acct-a", now_ms()).unwrap()
    }

    /// Clearing an emailed code restarts the account's outflow window.
    ///
    /// Without it, the payment just authenticated stays in the total, and the
    /// NEXT one — however small — meets that same total and is asked for another
    /// code. That is the defect this exists to remove, and it was live.
    #[test]
    fn clearing_a_code_restarts_the_outflow_window() {
        let (pool, intent_id, amount) = stepup_fixture(Some("stepup-window@disc.mnn"));
        assert_eq!(stepup_verified_at(&pool), None, "the fixture starts unverified");

        let code = issue_stepup_code(&pool, "stepup-window@disc.mnn");
        let v = approve_stepup(&pool, &intent_id, PIN, Some(&code));
        assert_eq!(v["ok"], json!(true), "{v}");

        let verified = stepup_verified_at(&pool).expect("the window was not restarted");
        let paid_at: i64 = pool
            .get()
            .unwrap()
            .query_row(
                "SELECT ts_unix_ms FROM transactions WHERE account_id = 'acct-a' \
                   AND kind = 'pay' ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Stamped when the code cleared, which is BEFORE the transfer — so the
        // payment it authorized falls inside its own new window instead of the
        // next one starting from zero.
        assert!(
            verified <= paid_at,
            "the stamp ({verified}) came after the movement ({paid_at}) it authorized"
        );
        assert_eq!(
            windowed_outflow(&pool),
            amount,
            "the authorized movement fell outside its own window"
        );
    }

    /// A PIN is not a second factor, and restarts nothing.
    ///
    /// What justifies the reset is the holder producing something a stolen session
    /// cannot. A PIN travels with the session it is typed into, so resetting on
    /// one would let the thing being guarded clear its own guard.
    #[test]
    fn a_pin_only_approval_leaves_the_window_alone() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        assert_eq!(
            stepup_verified_at(&pool),
            None,
            "a PIN restarted the outflow window"
        );
        // …and the movement still counts, which is the point of it not resetting.
        assert_eq!(windowed_outflow(&pool), 300);
    }

    #[test]
    fn a_wrong_code_leaves_the_window_where_it_was() {
        let (pool, intent_id, _) = stepup_fixture(Some("stepup-badwin@disc.mnn"));
        let real = issue_stepup_code(&pool, "stepup-badwin@disc.mnn");
        let wrong = if real == "000000" { "111111" } else { "000000" };

        let v = approve_stepup(&pool, &intent_id, PIN, Some(wrong));
        assert_eq!(v["error"], json!("invalid_code"), "{v}");
        assert_eq!(
            stepup_verified_at(&pool),
            None,
            "a rejected code restarted the window"
        );
    }

    fn state_of(pool: &Pool, intent_id: &str) -> String {
        get(&pool.get().unwrap(), intent_id)
            .unwrap()
            .unwrap()
            .state
    }

    fn intent_of(pool: &Pool, intent_id: &str) -> Intent {
        get(&pool.get().unwrap(), intent_id).unwrap().unwrap()
    }

    fn escrow_balance(pool: &Pool) -> i64 {
        balance_of(pool, wallet::escrow_account_id())
    }

    /// Record a fulfilment report the way `/merchant/v1/intent/fulfill` will.
    ///
    /// Written against the columns because the endpoint is a later unit; the
    /// claim it will use (`fulfilled_unix_ms IS NULL`) is the same one, so the
    /// release behaviour these tests pin does not depend on which writes it.
    fn fulfil(pool: &Pool, intent_id: &str, fulfilled_amount: i64) {
        let n = pool
            .get()
            .unwrap()
            .execute(
                "UPDATE payment_intents SET fulfilled_unix_ms = ?2, fulfilled_amount = ?3 \
                 WHERE intent_id = ?1 AND fulfilled_unix_ms IS NULL",
                params![intent_id, now_ms(), fulfilled_amount],
            )
            .unwrap();
        assert_eq!(n, 1, "{intent_id} was already fulfilled");
    }

    /// Run the release sweep as it would run once the gate has elapsed.
    ///
    /// The gate is ten minutes, so the clock is advanced rather than waited on —
    /// `release_pass` takes `now` for exactly this reason, and passing the real
    /// clock (as `sweep_now` does) is how the "too early" case is exercised.
    fn sweep_after_gate(pool: &Pool) -> usize {
        let mut conn = pool.get().unwrap();
        release_pass(&mut conn, now_ms() + RELEASE_GATE_MS + 1).unwrap()
    }

    fn sweep_now(pool: &Pool) -> usize {
        let mut conn = pool.get().unwrap();
        release_pass(&mut conn, now_ms()).unwrap()
    }

    /// The sweep as it would run once the no-report deadline has passed. Six
    /// hours, so the clock is advanced rather than waited on.
    fn sweep_after_deadline(pool: &Pool) -> usize {
        let mut conn = pool.get().unwrap();
        release_pass(&mut conn, now_ms() + ESCROW_DEADLINE_MS + 1).unwrap()
    }

    /// Approve, report fulfilled in full, and release — an ordinary completed
    /// purchase, end to end.
    fn approve_and_release(pool: &Pool, intent_id: &str, amount: i64) {
        assert_eq!(do_approve(pool, intent_id, PIN)["ok"], json!(true));
        fulfil(pool, intent_id, amount);
        assert_eq!(sweep_after_gate(pool), 1);
    }

    /// An approval takes exactly the intent's amount from the buyer — and, since
    /// v9, hands it to ESCROW rather than to the shop.
    ///
    /// The buyer's half of this test is unchanged on purpose: the debit, the
    /// reported amount and the intent's state are what a customer experiences, and
    /// escrow was not allowed to alter any of them. What changed is the second
    /// balance — the shop is owed the money, not holding it — and that is the
    /// whole point of the mechanism, so it is asserted rather than dropped.
    #[test]
    fn an_approval_moves_the_amount_the_intent_says_and_nothing_else() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        let v = do_approve(&pool, &intent_id, PIN);
        assert_eq!(v["ok"], json!(true), "{v}");
        assert_eq!(v["amount"], json!(300));
        assert_eq!(balance_of(&pool, "acct-a"), 700);
        assert_eq!(balance_of(&pool, "acct-m"), 0, "the shop was paid before it delivered");
        assert_eq!(escrow_balance(&pool), 300);
        assert_eq!(state_of(&pool, &intent_id), STATE_PAID);

        // …and the money reaches the shop once it reports the order fulfilled and
        // the gate elapses. Nothing else moves: 300 in, 300 out.
        fulfil(&pool, &intent_id, 300);
        assert_eq!(sweep_after_gate(&pool), 1);
        assert_eq!(balance_of(&pool, "acct-m"), 300);
        assert_eq!(escrow_balance(&pool), 0);
        assert_eq!(balance_of(&pool, "acct-a"), 700);
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

        // The ledger is the final word: the money moved exactly once. It is in
        // escrow rather than the shop's account (v9), which does not weaken the
        // claim this test guards — one debit of exactly one AMOUNT happened, and
        // where it landed is a separate decision.
        assert_eq!(balance_of(&db.pool, "acct-a"), START - AMOUNT);
        assert_eq!(escrow_balance(&db.pool), AMOUNT);
        assert_eq!(balance_of(&db.pool, &m.account_id), 0);
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
        assert_eq!(balance_of(&pool, "acct-a"), STEPUP_BALANCE - amount);
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
        assert_eq!(balance_of(&pool, "acct-a"), STEPUP_BALANCE);
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
        assert_eq!(balance_of(&pool, "acct-a"), STEPUP_BALANCE);
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
        assert_eq!(balance_of(&pool, "acct-a"), STEPUP_BALANCE);
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
        assert_eq!(balance_of(&pool, "acct-a"), STEPUP_BALANCE);
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
        //
        // "Settled" now means released, not merely paid: a paid-but-escrowed bill
        // IS an outstanding claim (see the test below), so the history has to be
        // finished before this asserts anything about it.
        let (pool, intent_id, m) = fixture(300, 1_000);
        approve_and_release(&pool, &intent_id, 300);
        assert_eq!(close_shop(&pool, &m.merchant_id), merchant::CloseOutcome::Ok);
        assert_eq!(state_of(&pool, &intent_id), STATE_PAID);
    }

    /// A shop cannot walk away from money MoyMoy is holding for it.
    ///
    /// Closing releases the name and the owner's slot, and the release sweep
    /// resolves a merchant to its account — so a shop closed with escrow
    /// outstanding would leave the money suspended with the row it would be paid
    /// through retired. Same reasoning as the open-intent refusal: finish what is
    /// outstanding, then close.
    #[test]
    fn a_shop_cannot_close_while_money_is_held_for_it() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        assert_eq!(
            close_shop(&pool, &m.merchant_id),
            merchant::CloseOutcome::HasEscrowedFunds {
                count: 1,
                total: 300
            }
        );
        // Reporting the order fulfilled is not enough on its own — the money is
        // still in escrow until the gate elapses and the sweep moves it.
        fulfil(&pool, &intent_id, 300);
        assert_eq!(
            close_shop(&pool, &m.merchant_id),
            merchant::CloseOutcome::HasEscrowedFunds {
                count: 1,
                total: 300
            }
        );
        assert_eq!(sweep_after_gate(&pool), 1);
        assert_eq!(close_shop(&pool, &m.merchant_id), merchant::CloseOutcome::Ok);
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
        // The money came back out of escrow, where the approval had put it, and
        // the pot is square again — the refund neither invented eme nor left any
        // of this payment behind for the sweep to find.
        assert_eq!(escrow_balance(&pool), 0);
        // `paid` is not rewound — the record still says the purchase happened.
        assert_eq!(state_of(&pool, &intent_id), STATE_PAID);
    }

    /// The accepted risk in DEV.md, in the window escrow does NOT cover.
    ///
    /// Escrow shortened this window; it did not close it. Once a shop has reported
    /// an order fulfilled and been paid, the revenue is its own and it may withdraw
    /// it to the MC world — after which a forced refund has no source, and the
    /// honest answer is to say so rather than to conjure the money from somewhere.
    /// That is what this test has always protected, and it protects it on the
    /// released path now that the escrowed path cannot reach it.
    #[test]
    fn a_refund_the_merchant_can_no_longer_fund_is_reported_not_faked() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        approve_and_release(&pool, &intent_id, 300);
        assert_eq!(balance_of(&pool, "acct-m"), 300);

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
        // Escrow is not raided to cover it either: this intent's money left escrow
        // legitimately, and paying the refund out of the pot would be taking it
        // from whichever other payments are being held there.
        assert_eq!(escrow_balance(&pool), 0, "another payment's escrow funded this refund");

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

    fn report(pool: &Pool, merchant_id: &str, intent_id: &str, amount: i64) -> FulfillOutcome {
        let mut conn = pool.get().unwrap();
        fulfill(&mut conn, merchant_id, intent_id, amount, Some("test")).unwrap()
    }

    /// A fulfilment is stated once, and the second attempt is told so.
    ///
    /// The claim decides what the customer is refunded, so "already reported" and
    /// "recorded" must not look alike to a retrying integrator — an at-least-once
    /// caller that read a repeat as success would believe its second figure took.
    #[test]
    fn an_order_can_be_reported_fulfilled_exactly_once() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));

        assert_eq!(
            report(&pool, &m.merchant_id, &intent_id, 200),
            FulfillOutcome::Ok {
                fulfilled_amount: 200,
                refund_amount: 100
            }
        );
        // A second report — even one claiming a different figure — cannot revise it.
        assert_eq!(
            report(&pool, &m.merchant_id, &intent_id, 300),
            FulfillOutcome::AlreadyFulfilled {
                fulfilled_amount: Some(200)
            }
        );
        assert_eq!(intent_of(&pool, &intent_id).fulfilled_amount, Some(200));

        // …and the sweep pays out the figure the FIRST report set.
        assert_eq!(sweep_after_gate(&pool), 1);
        assert_eq!(balance_of(&pool, "acct-m"), 200);
        assert_eq!(balance_of(&pool, "acct-a"), 800);
    }

    #[test]
    fn a_report_cannot_claim_more_than_the_customer_approved() {
        // The bound that keeps this endpoint from being a way to move money UP:
        // the ceiling is a figure the buyer already saw and agreed to.
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));

        for bad in [301, 1_000, i64::MAX, -1] {
            assert_eq!(
                report(&pool, &m.merchant_id, &intent_id, bad),
                FulfillOutcome::AmountOutOfRange { amount: 300 },
                "{bad} was accepted"
            );
        }
        // Nothing was recorded by any of them.
        assert_eq!(intent_of(&pool, &intent_id).fulfilled_unix_ms, None);
        // The bound itself is reachable.
        assert_eq!(
            report(&pool, &m.merchant_id, &intent_id, 300),
            FulfillOutcome::Ok {
                fulfilled_amount: 300,
                refund_amount: 0
            }
        );
    }

    #[test]
    fn reporting_zero_is_a_valid_report_of_total_failure() {
        // Distinct from an absent field, which the handler refuses: `0` says the
        // shop delivered nothing and gives up its whole claim.
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));

        assert_eq!(
            report(&pool, &m.merchant_id, &intent_id, 0),
            FulfillOutcome::Ok {
                fulfilled_amount: 0,
                refund_amount: 300
            }
        );
        let i = intent_of(&pool, &intent_id);
        assert_eq!(i.fulfilled_amount, Some(0));
        // …and it IS a report: the row is `fulfilled`, not still waiting.
        assert!(i.fulfilled_unix_ms.is_some());
        assert_eq!(escrow_stage(&i), "fulfilled");
    }

    #[test]
    fn a_report_moves_no_money_until_the_gate_elapses() {
        // The invariant this endpoint has to keep: an API key writes a fact, and
        // the sweep is the only thing that pays.
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));

        report(&pool, &m.merchant_id, &intent_id, 300);
        assert_eq!(escrow_balance(&pool), 300, "the report itself paid out");
        assert_eq!(balance_of(&pool, "acct-m"), 0);
        assert_eq!(sweep_now(&pool), 0);
        assert_eq!(balance_of(&pool, "acct-m"), 0);
    }

    #[test]
    fn one_shop_cannot_report_on_another_shops_order() {
        // Indistinguishable from a missing intent, so this cannot be used to probe
        // whether another shop's intent id exists.
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));

        assert_eq!(
            report(&pool, "m-somebody-else", &intent_id, 300),
            FulfillOutcome::UnknownIntent
        );
        assert_eq!(intent_of(&pool, &intent_id).fulfilled_unix_ms, None);
    }

    #[test]
    fn a_payment_that_predates_escrow_cannot_be_reported_on() {
        // The rows the v9 migration closed: their money went straight to the shop
        // and the sweep will never look at them again. Accepting a report would
        // leave the shop a record saying it reported, with no effect anywhere.
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        pool.get()
            .unwrap()
            .execute(
                "UPDATE payment_intents SET escrowed_unix_ms = NULL, release_due_unix_ms = NULL, \
                        released_unix_ms = ?2 WHERE intent_id = ?1",
                params![intent_id, now_ms()],
            )
            .unwrap();

        assert_eq!(
            report(&pool, &m.merchant_id, &intent_id, 300),
            FulfillOutcome::NotHeld { stage: "none" }
        );
    }

    #[test]
    fn an_unpaid_or_already_released_order_cannot_be_reported_on() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        // Never approved: nothing is being held, so there is nothing to report.
        assert_eq!(
            report(&pool, &m.merchant_id, &intent_id, 300),
            FulfillOutcome::NotHeld { stage: "none" }
        );

        // Paid, reported and released — the money has left escrow, so a further
        // report cannot change what anybody received.
        approve_and_release(&pool, &intent_id, 300);
        assert_eq!(
            report(&pool, &m.merchant_id, &intent_id, 0),
            FulfillOutcome::AlreadyFulfilled {
                fulfilled_amount: Some(300)
            }
        );
        assert_eq!(balance_of(&pool, "acct-m"), 300);
    }

    fn report_with(
        pool: &Pool,
        merchant_id: &str,
        intent_id: &str,
        amount: i64,
        reason: Option<&str>,
    ) -> FulfillOutcome {
        let mut conn = pool.get().unwrap();
        fulfill(&mut conn, merchant_id, intent_id, amount, reason).unwrap()
    }

    /// The shop's explanation is stored beside the amount it explains.
    #[test]
    fn a_shortfall_keeps_the_words_that_explain_it() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        report_with(
            &pool,
            &m.merchant_id,
            &intent_id,
            200,
            Some("2 of 3 lines undeliverable"),
        );

        let i = intent_of(&pool, &intent_id);
        assert_eq!(i.fulfilled_amount, Some(200));
        assert_eq!(i.fulfil_reason.as_deref(), Some("2 of 3 lines undeliverable"));
        // …and it outlives the release, because it explains a movement that is now
        // in the ledger for good.
        assert_eq!(sweep_after_gate(&pool), 1);
        assert_eq!(
            intent_of(&pool, &intent_id).fulfil_reason.as_deref(),
            Some("2 of 3 lines undeliverable")
        );
    }

    #[test]
    fn an_explanation_is_optional_but_never_half_written() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        // A full delivery has nothing to explain, and blank means the same as
        // absent — requiring one would only produce placeholder strings.
        report_with(&pool, &m.merchant_id, &intent_id, 300, Some("   "));
        let i = intent_of(&pool, &intent_id);
        assert_eq!(i.fulfilled_amount, Some(300));
        assert_eq!(i.fulfil_reason, None);
    }

    #[test]
    fn an_unusable_explanation_is_refused_and_records_nothing() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));

        let long = "あ".repeat(merchant::MAX_FULFIL_REASON_CHARS + 1);
        assert_eq!(
            report_with(&pool, &m.merchant_id, &intent_id, 200, Some(&long)),
            FulfillOutcome::BadReason(merchant::TextReject::TooLong)
        );
        // A bidi override would let a leaked key put words on the sales page its
        // owner reads as MoyMoy's — the guard every merchant string passes.
        assert_eq!(
            report_with(&pool, &m.merchant_id, &intent_id, 200, Some("在庫\u{202E}切れ")),
            FulfillOutcome::BadReason(merchant::TextReject::Invisible)
        );

        // **Refused means nothing happened**: not the amount, not the claim. The
        // shop can fix its string and report properly.
        let i = intent_of(&pool, &intent_id);
        assert_eq!(i.fulfilled_unix_ms, None);
        assert_eq!(i.fulfilled_amount, None);
        assert_eq!(i.fulfil_reason, None);
        assert!(matches!(
            report_with(&pool, &m.merchant_id, &intent_id, 200, Some("在庫切れ")),
            FulfillOutcome::Ok { .. }
        ));
        assert_eq!(
            intent_of(&pool, &intent_id).fulfil_reason.as_deref(),
            Some("在庫切れ")
        );
    }

    // ── the sales page ──────────────────────────────────────────────────────

    #[test]
    fn the_sales_page_shows_what_is_held_and_what_has_landed() {
        let (pool, first, m) = fixture(300, 10_000);
        // Three sales at different points in their lives.
        let (held, partial) = {
            let mut conn = pool.get().unwrap();
            (
                new_intent(&mut conn, &m, 500, None, 600),
                new_intent(&mut conn, &m, 400, None, 600),
            )
        };
        approve_and_release(&pool, &first, 300); // done: the shop has this
        assert_eq!(do_approve(&pool, &held, PIN)["ok"], json!(true)); // held
        assert_eq!(do_approve(&pool, &partial, PIN)["ok"], json!(true));
        report_with(&pool, &m.merchant_id, &partial, 100, Some("在庫切れ"));

        let page = {
            let conn = pool.get().unwrap();
            sales_page(&conn, &m, SALES_DEFAULT_LIMIT, now_ms()).unwrap()
        };

        // Held = escrowed and not yet released, whatever stage they are at: the
        // reported-but-not-released one counts too, because the money is still
        // MoyMoy's until the sweep moves it.
        assert_eq!(page["held_count"], json!(2));
        assert_eq!(page["held_total_minor"], json!(900));
        assert_eq!(page["truncated"], json!(false));

        let sales = page["sales"].as_array().unwrap();
        assert_eq!(sales.len(), 3, "{page:#}");
        // Newest first.
        assert_eq!(sales[0]["intent_id"], json!(partial));
        assert_eq!(sales[2]["intent_id"], json!(first));

        // The stage judgement is `escrow_stage`'s, not a second copy of it.
        assert_eq!(sales[0]["escrow"]["stage"], json!("fulfilled"));
        assert_eq!(sales[1]["escrow"]["stage"], json!("held"));
        assert_eq!(sales[2]["escrow"]["stage"], json!("released"));

        // The three money figures a shop needs, and the words behind the gap.
        assert_eq!(sales[0]["amount_minor"], json!(400));
        assert_eq!(sales[0]["escrow"]["fulfilled_amount_minor"], json!(100));
        assert_eq!(sales[0]["escrow"]["refunded_amount_minor"], json!(300));
        assert_eq!(sales[0]["escrow"]["fulfil_reason"], json!("在庫切れ"));
        // The customer stays behind the per-shop pseudonym.
        assert!(sales[0]["payer_ref"].is_string());
    }

    #[test]
    fn a_truncated_sales_page_says_so() {
        // Silently stopping reads as "this is everything", which is how a shop
        // concludes it was paid less than it was.
        let (pool, _first, m) = fixture(300, 10_000);
        {
            let mut conn = pool.get().unwrap();
            for _ in 0..4 {
                new_intent(&mut conn, &m, 100, None, 600);
            }
        }
        let conn = pool.get().unwrap();

        let page = sales_page(&conn, &m, 2, now_ms()).unwrap();
        assert_eq!(page["sales"].as_array().unwrap().len(), 2);
        assert_eq!(page["limit"], json!(2));
        assert_eq!(page["truncated"], json!(true));

        // Asking for everything there is says so too.
        let all = sales_page(&conn, &m, 50, now_ms()).unwrap();
        assert_eq!(all["sales"].as_array().unwrap().len(), 5);
        assert_eq!(all["truncated"], json!(false));

        // The ceiling is the wallet's, not the caller's.
        let huge = sales_page(&conn, &m, 10_000, now_ms()).unwrap();
        assert_eq!(huge["limit"], json!(SALES_MAX_LIMIT));
    }

    #[test]
    fn a_held_total_of_nothing_is_zero_not_missing() {
        // COALESCE, not an absent key: a page that omits the figure when a shop
        // has never traded is a page the UI has to special-case.
        let (pool, _intent_id, m) = fixture(300, 1_000);
        let conn = pool.get().unwrap();
        let page = sales_page(&conn, &m, SALES_DEFAULT_LIMIT, now_ms()).unwrap();
        assert_eq!(page["held_count"], json!(0));
        assert_eq!(page["held_total_minor"], json!(0));
    }

    /// **THE test for the sweep.** It runs every 30 seconds for the life of the
    /// process and restarts land it back on the same rows, so a release that is
    /// not idempotent pays the shop again on every tick.
    ///
    /// The same shape as the withdraw settler's double-refund guard: the claim is
    /// part of the UPDATE, so only the transaction that moved the row moves money.
    #[test]
    fn the_release_sweep_pays_once_however_many_times_it_runs() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        fulfil(&pool, &intent_id, 300);

        // Ten passes — a restart is not a special case here, it is just another
        // pass arriving at a row the previous one already handled.
        let mut releases = 0;
        for _ in 0..10 {
            releases += sweep_after_gate(&pool);
        }
        assert_eq!(releases, 1, "the sweep released the same payment more than once");
        assert_eq!(balance_of(&pool, "acct-m"), 300);
        assert_eq!(escrow_balance(&pool), 0);
        assert_eq!(balance_of(&pool, "acct-a"), 700);

        // One ledger row for the payout, not ten.
        let credits: i64 = pool
            .get()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE account_id = 'acct-m' AND kind = 'receive'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(credits, 1);
    }

    /// The gate is a floor the sweep will not cross, and the fulfilment report is
    /// a condition it will not assume.
    #[test]
    fn the_sweep_waits_for_both_the_report_and_the_gate() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));

        // Fulfilled but inside the gate: not yet.
        fulfil(&pool, &intent_id, 300);
        assert_eq!(sweep_now(&pool), 0, "the sweep paid out before the gate elapsed");
        assert_eq!(escrow_balance(&pool), 300);
        assert_eq!(intent_of(&pool, &intent_id).released_unix_ms, None);

        // Past the gate: released.
        assert_eq!(sweep_after_gate(&pool), 1);
        assert_eq!(balance_of(&pool, "acct-m"), 300);
    }

    #[test]
    fn no_amount_of_time_alone_ever_decides_who_gets_the_money() {
        // The distinction that makes escrow worth having, in its strongest form.
        //
        // It has been narrowed twice and is now back to the whole claim. Before
        // the deadline existed it read "time releases nothing". 3-1b weakened it
        // to "time never pays the SHOP", because the deadline refunded the buyer.
        // Parking restores it: a clock is not evidence about anything, so however
        // long a shop stays silent, elapsed time on its own moves the money
        // NEITHER way. What it produces is a question for a person.
        //
        // A timer-only hold — the design escrow replaced — pays the shop.
        // Refund-on-timer — the design B replaced — pays the buyer. Both decide a
        // question nobody answered.
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));

        let mut conn = pool.get().unwrap();
        // A year: far past the gate, and far past the deadline.
        let far_future = now_ms() + 365 * 24 * 60 * 60 * 1000;
        assert_eq!(release_pass(&mut conn, far_future).unwrap(), 1);
        drop(conn);

        assert_eq!(
            balance_of(&pool, &m.account_id),
            0,
            "waiting long enough paid a shop that never said it delivered anything"
        );
        assert_eq!(
            balance_of(&pool, "acct-a"),
            700,
            "waiting long enough refunded a buyer whose goods may well have arrived"
        );
        assert_eq!(escrow_balance(&pool), 300, "the money left escrow on a clock");
        let i = intent_of(&pool, &intent_id);
        assert_eq!(i.release_tx_id, None);
        assert_eq!(i.escrow_refund_tx_id, None);
        assert_eq!(escrow_stage(&i), "parked");
    }

    /// A partial fulfilment splits the escrowed money two ways, and both ways
    /// happen together or not at all.
    #[test]
    fn a_partial_fulfilment_pays_the_shop_and_returns_the_rest_in_one_step() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        assert_eq!(balance_of(&pool, "acct-a"), 700);

        // Two of three lines shipped.
        fulfil(&pool, &intent_id, 200);
        assert_eq!(sweep_after_gate(&pool), 1);

        assert_eq!(balance_of(&pool, "acct-m"), 200, "the shop was paid the wrong share");
        assert_eq!(balance_of(&pool, "acct-a"), 800, "the buyer was not returned the rest");
        // Nothing is left behind in the pot: the two halves account for the whole.
        assert_eq!(escrow_balance(&pool), 0);

        let i = intent_of(&pool, &intent_id);
        assert_eq!(escrow_stage(&i), "released");
        assert!(i.release_tx_id.is_some(), "no ledger row for the payout");
        assert!(i.escrow_refund_tx_id.is_some(), "no ledger row for the return");
    }

    #[test]
    fn a_wholly_unfulfilled_payment_returns_everything_and_pays_the_shop_nothing() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));

        // Nothing could be delivered at all.
        fulfil(&pool, &intent_id, 0);
        assert_eq!(sweep_after_gate(&pool), 1);

        assert_eq!(balance_of(&pool, "acct-a"), 1_000);
        assert_eq!(balance_of(&pool, "acct-m"), 0);
        assert_eq!(escrow_balance(&pool), 0);
        let i = intent_of(&pool, &intent_id);
        // No payout row was written, because no payout happened — an empty
        // transfer would be a ledger row for nothing.
        assert_eq!(i.release_tx_id, None);
        assert!(i.escrow_refund_tx_id.is_some());
    }

    // ── the no-report deadline ──────────────────────────────────────────────

    /// A shop's silence runs out, and the sweep decides NOTHING.
    ///
    /// **Silence is not evidence.** The shop has not said the goods stayed put —
    /// it has said nothing — and the shop-side retry is deliberately unbounded
    /// during an infrastructure outage, so a delivery can still succeed hours
    /// after this fires. Refunding on the timer would have handed the buyer the
    /// goods and the money. Escrow keeps it and asks for a person, which is the
    /// answer `emerald_ops` already gives an unprovable payout (`stuck`, R008).
    #[test]
    fn a_payment_no_one_ever_reported_on_is_parked_not_refunded() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        assert_eq!(escrow_balance(&pool), 300);

        assert_eq!(sweep_after_deadline(&pool), 1);

        // Not one エメ moved, in either direction.
        assert_eq!(escrow_balance(&pool), 300, "the deadline moved the money");
        assert_eq!(balance_of(&pool, "acct-a"), 700, "the buyer was refunded on a timer");
        assert_eq!(balance_of(&pool, &m.account_id), 0, "a silent shop was paid");

        let i = intent_of(&pool, &intent_id);
        assert!(i.escrow_parked_unix_ms.is_some(), "the intent was not parked");
        // NOT released: parking is not a resolution, and a released row is closed
        // to the operator path that has to be able to move this money.
        assert_eq!(i.released_unix_ms, None, "a parked intent was marked resolved");
        assert_eq!(i.release_tx_id, None);
        assert_eq!(i.escrow_refund_tx_id, None);
        // Reported as its own stage, so a shop is not shown `held` for a payment
        // that is actually waiting on a human.
        assert_eq!(escrow_stage(&i), "parked");
        // The order itself is untouched — `paid` is still what happened.
        assert_eq!(state_of(&pool, &intent_id), STATE_PAID);
        assert_eq!(i.fulfilled_unix_ms, None);
    }

    /// The way out of park: an operator returns it, from escrow.
    ///
    /// Parked money that nobody can move would be worse than refunding it. This
    /// is the route, and it works only because parking leaves
    /// `released_unix_ms` NULL.
    #[test]
    fn an_operator_can_return_a_parked_payment_from_escrow() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        assert_eq!(sweep_after_deadline(&pool), 1);

        let mut conn = pool.get().unwrap();
        let out = force_refund(&mut conn, &intent_id, "shop never reported").unwrap();
        assert!(matches!(out, RefundOutcome::Ok { amount: 300, .. }), "{out:?}");
        drop(conn);

        assert_eq!(balance_of(&pool, "acct-a"), 1_000);
        assert_eq!(escrow_balance(&pool), 0);
        // Taken from escrow, where the money actually was — not from a shop that
        // was never paid.
        assert_eq!(balance_of(&pool, &m.account_id), 0);
    }

    #[test]
    fn a_parked_payment_is_still_counted_as_held_against_its_shop() {
        // The money has not gone anywhere, so both places that ask "what is
        // suspended for this shop" must keep saying so: the sales page total, and
        // the refusal to close a shop with money outstanding.
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        assert_eq!(sweep_after_deadline(&pool), 1);

        let page = {
            let conn = pool.get().unwrap();
            sales_page(&conn, &m, SALES_DEFAULT_LIMIT, now_ms()).unwrap()
        };
        assert_eq!(page["held_count"], json!(1));
        assert_eq!(page["held_total_minor"], json!(300));
        assert_eq!(page["sales"][0]["escrow"]["stage"], json!("parked"));
        assert!(page["sales"][0]["escrow"]["parked_unix_ms"].is_i64());

        assert_eq!(
            close_shop(&pool, &m.merchant_id),
            merchant::CloseOutcome::HasEscrowedFunds {
                count: 1,
                total: 300
            }
        );
        let _ = intent_id;
    }

    /// The gate and the deadline are different clocks, and the sweep respects
    /// both. An unreported payment is NOT released when the ten-minute gate
    /// elapses — only when the six-hour deadline does.
    #[test]
    fn an_unreported_payment_waits_for_the_deadline_not_the_gate() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));

        assert_eq!(sweep_now(&pool), 0);
        // Well past the release gate, nowhere near the deadline: still held, which
        // is the whole point — a delivery that takes an hour keeps its money where
        // a refund can reach it.
        assert_eq!(sweep_after_gate(&pool), 0);
        assert_eq!(escrow_balance(&pool), 300);
        assert_eq!(escrow_stage(&intent_of(&pool, &intent_id)), "held");

        assert_eq!(sweep_after_deadline(&pool), 1);
    }

    /// A reported payment past the deadline goes out through the REPORTED end.
    ///
    /// Both conditions are true for it at that point, so the branch is chosen by
    /// what the row says rather than by which clock ran out — otherwise a shop
    /// that reported and then waited six hours would have its earnings refunded.
    #[test]
    fn a_reported_payment_is_never_taken_by_the_deadline_branch() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        fulfil(&pool, &intent_id, 200);

        // Long past both the gate and the deadline.
        assert_eq!(sweep_after_deadline(&pool), 1);

        assert_eq!(balance_of(&pool, &m.account_id), 200, "the shop lost earnings it reported");
        assert_eq!(balance_of(&pool, "acct-a"), 800);
        assert_eq!(escrow_balance(&pool), 0);
        let i = intent_of(&pool, &intent_id);
        assert!(i.release_tx_id.is_some(), "the reported share was not paid");
        assert!(i.escrow_refund_tx_id.is_some());
    }

    /// A report that lands after the deadline but before the sweep still wins.
    ///
    /// `release_one` re-reads the row inside its own transaction, so the end is
    /// decided on what is true when the money moves — not on what the scan saw.
    #[test]
    fn a_report_arriving_before_the_sweep_takes_precedence_over_the_deadline() {
        let (pool, intent_id, m) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        // The deadline has passed…
        assert_eq!(escrow_stage(&intent_of(&pool, &intent_id)), "held");
        // …and the shop reports before the sweep next runs.
        fulfil(&pool, &intent_id, 300);
        assert_eq!(sweep_after_deadline(&pool), 1);

        assert_eq!(balance_of(&pool, &m.account_id), 300);
        assert_eq!(balance_of(&pool, "acct-a"), 700);
    }

    #[test]
    fn the_deadline_parks_once_however_many_times_the_sweep_runs() {
        // The sweep revisits every 30 seconds for the life of the process, and a
        // parked intent stays parked until a person acts. Re-parking would move no
        // money — but it would re-emit the warning on every tick, and a line that
        // repeats forever is a line nobody reads. The `escrow_parked_unix_ms IS
        // NULL` filter is what stops it, in the selection AND in the claim.
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));

        let mut acted = 0;
        for _ in 0..10 {
            acted += sweep_after_deadline(&pool);
        }
        assert_eq!(acted, 1, "the sweep parked the same payment more than once");

        // Ten passes, and the ledger never moved.
        assert_eq!(escrow_balance(&pool), 300);
        assert_eq!(balance_of(&pool, "acct-a"), 700);
        let rows: i64 = pool
            .get()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE account_id = 'acct-a' \
                   AND kind = 'receive'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
        // …and the stamp is the first pass's, not the tenth's.
        assert!(intent_of(&pool, &intent_id).escrow_parked_unix_ms.is_some());
    }

    /// The counterpart, and the reason escrow exists: while the money is held, a
    /// forced refund always has a source and `MerchantShort` cannot happen.
    #[test]
    fn a_refund_of_an_escrowed_payment_comes_from_moymoy_not_the_shop() {
        let (pool, intent_id, _) = fixture(300, 1_000);
        assert_eq!(do_approve(&pool, &intent_id, PIN)["ok"], json!(true));
        // The shop holds nothing at all — under the old model this was exactly the
        // situation that made a refund impossible.
        assert_eq!(balance_of(&pool, "acct-m"), 0);

        let mut conn = pool.get().unwrap();
        let out = force_refund(&mut conn, &intent_id, "chargeback").unwrap();
        assert!(matches!(out, RefundOutcome::Ok { amount: 300, .. }), "{out:?}");
        drop(conn);

        assert_eq!(balance_of(&pool, "acct-a"), 1_000, "the buyer was not made whole");
        assert_eq!(escrow_balance(&pool), 0);
        assert_eq!(balance_of(&pool, "acct-m"), 0, "the shop was debited for money it never had");
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
