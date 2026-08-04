//! Risk-based step-up authentication — the single gate every outflow of eme goes
//! through.
//!
//! The alternative was to sprinkle a PIN prompt over each feature as it was
//! added. That fails in both directions: whichever endpoint is written next
//! forgets it, and the ones that remember charge the user for a 20 エメ purchase
//! at the same rate as a 20,000 エメ one. So the decision is made in one place,
//! from the size of the movement rather than from which endpoint it arrived at,
//! and `/wallet/send`, `/wallet/withdraw` and `/wallet/payment/approve` all ask
//! here.
//!
//! `/wallet/charge` deliberately does not: it moves eme IN. Nothing a stolen
//! session can do with it takes anything away from the account holder.
//!
//! ## Thresholds
//!
//! | | requirement |
//! |---|---|
//! | ≤ 200 エメ, ≤ 1,000 エメ in 24h, familiar device | none |
//! | anything above that, or an unfamiliar device | PIN |
//!
//! Constants, not configuration: a wallet whose spending limits can be moved by
//! an environment variable has limits set by whoever can edit the launcher.
//! DEV.md records the numbers for operational review.
//!
//! ## Why there is no second factor
//!
//! **A PIN is as far as MoyMoy authenticates, and that is a stated position
//! rather than an omission.** An earlier revision escalated large movements to an
//! emailed code. The deployed backend has no mail configuration at all, so that
//! code could never be delivered — and since a withdrawal is capped at 20,736 エメ
//! ([`crate::wallet::MAX_WITHDRAW_PER_OP`]), the tier would have blocked most of
//! the withdrawal range outright. The choice was between a factor that silently
//! degraded to a PIN (a threshold that decides nothing, which is worse than no
//! threshold) and saying plainly that there is one factor. This is the second.
//!
//! Reintroducing a second factor means reintroducing it here, in `assess`, and
//! nowhere else.

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::auth::{self, LockoutPolicy, PinAttempt, PinSettle};
use crate::db::now_ms;
use crate::error::ApiError;

/// Single-movement ceiling below which nothing is asked.
pub const FRICTIONLESS_SINGLE: i64 = 200;
/// Rolling 24h outflow below which nothing is asked.
pub const FRICTIONLESS_DAILY: i64 = 1_000;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Consecutive wrong PINs on one session before the backoff bites.
const BACKOFF_FREE_ATTEMPTS: u32 = 2;
/// First penalty, doubling per further failure.
const BACKOFF_BASE_MS: i64 = 1_000;
/// Cap. Long enough to make guessing pointless, short enough that a customer who
/// fat-fingered their PIN at a checkout is not stranded.
const BACKOFF_MAX_MS: i64 = 60_000;

/// What the caller must present before the money moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Small, routine, from the usual device.
    None,
    Pin,
}

impl Requirement {
    pub fn needs_pin(self) -> bool {
        matches!(self, Requirement::Pin)
    }
    /// The stricter of the two. Payments start at [`Requirement::Pin`] by
    /// standing decision, and the amount may only hold that, never lower it.
    pub fn max(self, other: Requirement) -> Requirement {
        if self.needs_pin() || other.needs_pin() {
            Requirement::Pin
        } else {
            Requirement::None
        }
    }
    /// What the client is told it must collect. Distinct from `invalid_pin`: one
    /// says "ask the user", the other says "the user got it wrong".
    pub fn code(self) -> &'static str {
        match self {
            Requirement::None => "none",
            Requirement::Pin => "pin",
        }
    }
}

/// Is this session coming from the device the account has been using?
///
/// A **signal, not a control.** `phone_id` is chosen by the client, so anyone who
/// can steal a session can send the matching device id; what this catches is the
/// ordinary case where they did not bother. It only ever raises friction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceTrust {
    Familiar,
    Unfamiliar,
}

/// Compare this session's device against the one the account established.
///
/// The reference is the account's OLDEST session that carries a device id — the
/// phone it registered or first logged in on. An account with no such session
/// (every client that ever logged in omitted the field) has established nothing
/// to differ from, and is reported `Familiar`: inventing a mismatch out of
/// missing data would put every existing user behind an OTP.
pub fn device_trust(
    conn: &Connection,
    account_id: &str,
    session_phone_id: Option<&str>,
) -> rusqlite::Result<DeviceTrust> {
    // `.optional()` and not `.unwrap_or(None)`: "no session has ever named a
    // device" and "the database failed to answer" must not collapse into the same
    // value, because that value is the one that asks the user for LESS.
    let established: Option<String> = conn
        .query_row(
            "SELECT phone_id FROM moymoy_sessions \
             WHERE account_id = ?1 AND phone_id IS NOT NULL \
             ORDER BY created_unix_ms ASC LIMIT 1",
            [account_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    match (established.as_deref(), session_phone_id) {
        (None, _) => Ok(DeviceTrust::Familiar),
        (Some(known), Some(now)) if known == now => Ok(DeviceTrust::Familiar),
        _ => Ok(DeviceTrust::Unfamiliar),
    }
}

/// Everything that has left this account in the last 24 hours, as a positive
/// number.
///
/// Read off the ledger rather than from a counter, so it covers sends, payments
/// and withdrawal reserves alike — including any outflow a future feature adds,
/// which is the point of putting the question here instead of in each endpoint.
pub fn outflow_24h(conn: &Connection, account_id: &str, now: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(-SUM(amount), 0) FROM transactions \
         WHERE account_id = ?1 AND amount < 0 AND ts_unix_ms > ?2",
        params![account_id, now - DAY_MS],
        |r| r.get(0),
    )
}

/// Decide what this movement has to be authenticated with.
///
/// The device check is not folded into the amount test on purpose: a session that
/// has moved to another handset is worth a PIN even for a movement small enough
/// to be waved through, because the thing in doubt is who is holding it rather
/// than how much they are asking for.
pub fn assess(amount: i64, outflow_24h: i64, device: DeviceTrust) -> Requirement {
    if device == DeviceTrust::Unfamiliar {
        return Requirement::Pin;
    }
    let after = outflow_24h.saturating_add(amount);
    if amount <= FRICTIONLESS_SINGLE && after <= FRICTIONLESS_DAILY {
        return Requirement::None;
    }
    Requirement::Pin
}

/// Read the account's recent history and decide, in one call.
pub fn assess_for(
    conn: &Connection,
    account_id: &str,
    phone_id: Option<&str>,
    amount: i64,
    now: i64,
) -> rusqlite::Result<Requirement> {
    let spent = outflow_24h(conn, account_id, now)?;
    let device = device_trust(conn, account_id, phone_id)?;
    Ok(assess(amount, spent, device))
}

// ── the gate ─────────────────────────────────────────────────────────────────

/// What the money transaction still owes the authentication machinery.
///
/// Carried out of [`step_up`] and handed to [`settle`] inside the caller's
/// transaction. Empty when the movement needed nothing.
#[derive(Debug, Default)]
pub struct StepUpTicket {
    /// The failure-counter epoch stage 1 wrote, when a PIN was checked.
    epoch: Option<i64>,
}

/// The gate's answer.
pub enum StepUp {
    /// Authenticated (or nothing was required). Proceed to the money.
    Cleared(StepUpTicket),
    /// Refused, with the `{ok:false,…}` body to return. Every one of these is an
    /// ordinary domain answer, not a fault.
    Refused(Value),
}

/// Who is asking for a movement, and what they brought to prove it.
///
/// One struct for every outflow path, so a new endpoint cannot quietly assemble
/// a caller that is missing the session key the backoff counts against.
pub struct Caller<'a> {
    pub account_id: &'a str,
    /// From the session row, not from the request body.
    pub phone_id: Option<&'a str>,
    pub session_key: &'a str,
    pub pin: Option<&'a str>,
}

/// Decide what this outflow needs and prove it — stages 1 and 2.
///
/// `floor` is the minimum the calling operation insists on regardless of size:
/// [`Requirement::None`] for a send or a withdrawal, [`Requirement::Pin`] for a
/// payment, which is a PIN by standing decision.
///
/// Takes `&mut Connection` and no transaction, and that is the point: stage 1
/// commits on its own and the Argon2id comparison happens with no write lock
/// held. See the notes in [`crate::auth`].
pub fn step_up(
    conn: &mut Connection,
    backoff: &PinBackoff,
    caller: &Caller<'_>,
    amount: i64,
    floor: Requirement,
) -> Result<StepUp, ApiError> {
    let now = now_ms();
    let account_id = caller.account_id;
    let requirement = floor.max(assess_for(conn, account_id, caller.phone_id, amount, now)?);
    if !requirement.needs_pin() {
        return Ok(StepUp::Cleared(StepUpTicket::default()));
    }
    if let Err(retry_after_ms) = backoff.check(caller.session_key, now) {
        return Ok(StepUp::Refused(
            json!({ "ok": false, "error": "too_many_attempts", "retry_after_ms": retry_after_ms }),
        ));
    }
    let Some(pin) = caller.pin.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(StepUp::Refused(
            json!({ "ok": false, "error": "pin_required", "required": requirement.code() }),
        ));
    };

    let (pin_hash, epoch) = match auth::begin_pin_attempt(conn, account_id, LockoutPolicy::Enforce)?
    {
        PinAttempt::Ready { pin_hash, epoch } => (pin_hash, epoch),
        PinAttempt::Locked { retry_after_ms } => {
            return Ok(StepUp::Refused(
                json!({ "ok": false, "error": "locked", "retry_after_ms": retry_after_ms }),
            ))
        }
        PinAttempt::NoPin => {
            tracing::error!(account = %account_id,
                "step-up: a live session points at an account with no PIN — refusing");
            return Ok(StepUp::Refused(json!({ "ok": false, "error": "invalid_pin" })));
        }
    };
    // No transaction is open across this comparison.
    if !auth::verify_pin_hash(pin, &pin_hash) {
        let retry_after_ms = backoff.record_failure(caller.session_key, now);
        return Ok(StepUp::Refused(
            json!({ "ok": false, "error": "invalid_pin", "retry_after_ms": retry_after_ms }),
        ));
    }
    backoff.clear(caller.session_key);
    Ok(StepUp::Cleared(StepUpTicket { epoch: Some(epoch) }))
}

/// Stage 3, inside the caller's money transaction: re-check the lockout and clear
/// the attempt this authentication spent. `Some(body)` means do not proceed —
/// roll back and return it.
pub fn settle(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    ticket: &StepUpTicket,
    now: i64,
) -> Result<Option<Value>, ApiError> {
    let Some(epoch) = ticket.epoch else {
        return Ok(None);
    };
    match auth::settle_pin_success(tx, account_id, epoch, now)? {
        PinSettle::Ok => Ok(None),
        PinSettle::Locked { retry_after_ms } => Ok(Some(
            json!({ "ok": false, "error": "locked", "retry_after_ms": retry_after_ms }),
        )),
    }
}

/// Give back the attempt a **correct** PIN spent, for the paths where the
/// operation it authorized did not commit.
///
/// Without this, five honest retries against a short balance would lock somebody
/// out of their own wallet — the failure counter is written up front (fail-closed)
/// and the money transaction's rollback takes the clearing with it.
pub fn refund_attempt(
    conn: &Connection,
    account_id: &str,
    ticket: &StepUpTicket,
) -> rusqlite::Result<()> {
    if let Some(epoch) = ticket.epoch {
        auth::clear_pin_failures(conn, account_id, epoch)?;
    }
    Ok(())
}

// ── per-session backoff ──────────────────────────────────────────────────────

/// Exponential backoff on wrong PINs, counted per session.
///
/// This exists alongside the account-wide lockout in [`crate::auth`], because
/// that lockout can be tripped by anybody who knows a handle: five wrong PINs at
/// `/auth/login` and the victim cannot pay for anything for fifteen minutes, over
/// and over. A session token is held only by its owner, so a counter attached to
/// one cannot be pushed onto somebody else — which makes it the right place to
/// slow guessing down without handing an attacker a way to freeze a stranger's
/// wallet.
///
/// In-process, like `attest::ChallengeStore`: it throttles, and a restart
/// forgetting a few seconds of penalty costs nothing the account lockout does not
/// already cover.
#[derive(Default)]
pub struct PinBackoff {
    inner: Mutex<HashMap<String, Entry>>,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    fails: u32,
    blocked_until: i64,
}

impl PinBackoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Err(retry_after_ms)` while this session is serving a penalty.
    pub fn check(&self, session_key: &str, now: i64) -> Result<(), i64> {
        match self.lock().get(session_key) {
            Some(e) if e.blocked_until > now => Err(e.blocked_until - now),
            _ => Ok(()),
        }
    }

    /// Record a wrong PIN and return how long this session is now held off for.
    pub fn record_failure(&self, session_key: &str, now: i64) -> i64 {
        let mut map = self.lock();
        map.retain(|_, e| e.blocked_until > now - BACKOFF_MAX_MS);
        let entry = map.entry(session_key.to_string()).or_insert(Entry {
            fails: 0,
            blocked_until: 0,
        });
        entry.fails += 1;
        let penalty = entry
            .fails
            .saturating_sub(BACKOFF_FREE_ATTEMPTS)
            .min(u32::BITS - 1);
        let delay = if penalty == 0 {
            0
        } else {
            BACKOFF_BASE_MS
                .saturating_mul(1i64 << (penalty - 1))
                .min(BACKOFF_MAX_MS)
        };
        entry.blocked_until = now + delay;
        delay
    }

    /// A correct PIN clears the session's record.
    pub fn clear(&self, session_key: &str) {
        self.lock().remove(session_key);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.inner.lock().unwrap_or_else(|e| {
            tracing::error!("PinBackoff mutex was poisoned; recovering the counters");
            e.into_inner()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frictionless_band_is_small_recent_and_familiar_all_at_once() {
        let f = DeviceTrust::Familiar;
        assert_eq!(assess(200, 0, f), Requirement::None);
        assert_eq!(assess(200, 800, f), Requirement::None);
        // Exactly on both ceilings is still inside them.
        assert_eq!(assess(200, FRICTIONLESS_DAILY - 200, f), Requirement::None);
        // One eme over either one leaves it.
        assert_eq!(assess(201, 0, f), Requirement::Pin);
        assert_eq!(
            assess(200, FRICTIONLESS_DAILY - 199, f),
            Requirement::Pin
        );
    }

    /// A PIN is the ceiling. Above the frictionless band every size asks for the
    /// same thing, right up to the largest movement the wallet permits — there is
    /// deliberately no second tier for a backend that has no way to deliver one.
    #[test]
    fn a_pin_is_as_far_as_the_requirement_ever_goes() {
        let f = DeviceTrust::Familiar;
        for amount in [
            FRICTIONLESS_SINGLE + 1,
            5_000,
            5_001,
            crate::wallet::MAX_WITHDRAW_PER_OP,
            crate::wallet::MAX_AMOUNT,
        ] {
            assert_eq!(assess(amount, 0, f), Requirement::Pin, "amount {amount}");
        }
        // …and a huge running total does not invent a stricter answer either.
        assert_eq!(assess(1, 10_000_000, f), Requirement::Pin);
    }

    #[test]
    fn an_unfamiliar_device_asks_for_a_pin_however_small_the_amount() {
        // The signal is worth nothing if it only applies to amounts that were
        // already going to ask for something: the thing in doubt is who holds the
        // session, not how much they want.
        assert_eq!(assess(1, 0, DeviceTrust::Unfamiliar), Requirement::Pin);
        assert_eq!(
            assess(FRICTIONLESS_SINGLE, 0, DeviceTrust::Unfamiliar),
            Requirement::Pin
        );
        // The same movement from the familiar handset is waved through, so it is
        // the device that made the difference and nothing else.
        assert_eq!(
            assess(FRICTIONLESS_SINGLE, 0, DeviceTrust::Familiar),
            Requirement::None
        );
    }

    #[test]
    fn a_running_total_cannot_be_split_into_frictionless_pieces() {
        // Five 200-eme movements: the sixth crosses the daily ceiling and starts
        // asking, which is the whole point of counting the window.
        let f = DeviceTrust::Familiar;
        let mut spent = 0;
        for _ in 0..5 {
            assert_eq!(assess(200, spent, f), Requirement::None);
            spent += 200;
        }
        assert_eq!(assess(200, spent, f), Requirement::Pin);
    }

    #[test]
    fn a_payment_never_drops_below_a_pin() {
        // Payments are PIN-by-standing-decision, so the floor holds even for the
        // amounts the assessment would otherwise wave through.
        for (amount, spent) in [(1, 0), (FRICTIONLESS_SINGLE, 0), (9_000, 0), (1, 500_000)] {
            assert_eq!(
                Requirement::Pin.max(assess(amount, spent, DeviceTrust::Familiar)),
                Requirement::Pin,
                "amount {amount} after {spent}"
            );
        }
    }

    #[test]
    fn the_backoff_grows_only_after_a_fumble_or_two_and_stops_growing() {
        let b = PinBackoff::new();
        assert!(b.check("s1", 0).is_ok());
        // The first couple cost nothing: a mistyped PIN at a checkout is normal.
        for _ in 0..BACKOFF_FREE_ATTEMPTS {
            assert_eq!(b.record_failure("s1", 0), 0);
        }
        let mut last = 0;
        for _ in 0..12 {
            let d = b.record_failure("s1", 0);
            assert!(d >= last, "the penalty went backwards: {last} -> {d}");
            assert!(d <= BACKOFF_MAX_MS, "penalty {d} exceeded the cap");
            last = d;
        }
        assert_eq!(last, BACKOFF_MAX_MS);
        assert!(b.check("s1", 0).is_err());
        // Another session is unaffected — an attacker cannot push a penalty onto
        // somebody else, which is exactly what the account-wide lockout allows.
        assert!(b.check("s2", 0).is_ok());
        // …and getting it right clears the record.
        b.clear("s1");
        assert!(b.check("s1", 0).is_ok());
    }

    #[test]
    fn a_device_that_never_identified_itself_is_not_treated_as_a_new_one() {
        let pool = crate::db::open_memory().unwrap();
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO accounts (account_id, created_unix_ms, updated_unix_ms) \
             VALUES ('acct-a', 0, 0)",
            [],
        )
        .unwrap();
        // No sessions at all yet: nothing has been established.
        assert_eq!(
            device_trust(&conn, "acct-a", Some("phone-1")).unwrap(),
            DeviceTrust::Familiar
        );
        let add = |sid: &str, phone: Option<&str>, at: i64| {
            conn.execute(
                "INSERT INTO moymoy_sessions \
                   (session_id, account_id, token_hash, phone_id, created_unix_ms, \
                    last_seen_unix_ms, expires_unix_ms) \
                 VALUES (?1, 'acct-a', ?1, ?2, ?3, ?3, 99999999)",
                params![sid, phone, at],
            )
            .unwrap();
        };
        // A client that sends no device id establishes nothing.
        add("s0", None, 10);
        assert_eq!(
            device_trust(&conn, "acct-a", None).unwrap(),
            DeviceTrust::Familiar
        );
        add("s1", Some("phone-1"), 20);
        assert_eq!(
            device_trust(&conn, "acct-a", Some("phone-1")).unwrap(),
            DeviceTrust::Familiar
        );
        // A later login from a different handset is the case worth friction…
        add("s2", Some("phone-2"), 30);
        assert_eq!(
            device_trust(&conn, "acct-a", Some("phone-2")).unwrap(),
            DeviceTrust::Unfamiliar
        );
        // …and so is one that suddenly stops naming itself.
        assert_eq!(
            device_trust(&conn, "acct-a", None).unwrap(),
            DeviceTrust::Unfamiliar
        );
    }

    /// The device rule, through the real gate rather than through `assess`
    /// alone: an amount small enough to be waved through still has to produce a
    /// PIN once the session is coming from a handset the account has not used.
    #[test]
    fn the_gate_asks_for_a_pin_from_an_unfamiliar_handset_even_for_pocket_change() {
        let pool = crate::db::open_memory().unwrap();
        let mut conn = pool.get().unwrap();
        let hash = auth::hash_pin("1234").unwrap();
        auth::insert_account(&conn, "acct-a", "payer", "payer", "payer", &hash, None).unwrap();
        conn.execute(
            "INSERT INTO moymoy_sessions \
               (session_id, account_id, token_hash, phone_id, created_unix_ms, \
                last_seen_unix_ms, expires_unix_ms) \
             VALUES ('s1', 'acct-a', 's1', 'phone-1', 10, 10, 99999999999999)",
            [],
        )
        .unwrap();
        let backoff = PinBackoff::new();
        let caller = |phone, pin| Caller {
            account_id: "acct-a",
            phone_id: Some(phone),
            session_key: "sess",
            pin,
        };
        // The established handset, well inside the frictionless band: nothing.
        let cleared = step_up(
            &mut conn,
            &backoff,
            &caller("phone-1", None),
            1,
            Requirement::None,
        )
        .unwrap();
        assert!(matches!(cleared, StepUp::Cleared(_)));

        // Same eme, different handset: the gate asks.
        let refused = step_up(
            &mut conn,
            &backoff,
            &caller("phone-2", None),
            1,
            Requirement::None,
        )
        .unwrap();
        let StepUp::Refused(body) = refused else {
            panic!("an unfamiliar handset was waved through");
        };
        assert_eq!(body["error"], json!("pin_required"), "{body}");
        assert_eq!(body["required"], json!("pin"));

        // …and a correct PIN from that handset is enough — there is no further
        // factor to produce.
        let cleared = step_up(
            &mut conn,
            &backoff,
            &caller("phone-2", Some("1234")),
            1,
            Requirement::None,
        )
        .unwrap();
        assert!(matches!(cleared, StepUp::Cleared(_)), "a PIN was not enough");
    }

    #[test]
    fn the_window_counts_debits_and_only_debits() {
        let pool = crate::db::open_memory().unwrap();
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO accounts (account_id, created_unix_ms, updated_unix_ms) \
             VALUES ('acct-a', 0, 0)",
            [],
        )
        .unwrap();
        let now = 10 * DAY_MS;
        let row = |kind: &str, amount: i64, ts: i64| {
            conn.execute(
                "INSERT INTO transactions (id, account_id, kind, label, amount, balance_after, ts_unix_ms) \
                 VALUES (?1, 'acct-a', ?2, 'x', ?3, 0, ?4)",
                params![format!("t{ts}{amount}"), kind, amount, ts],
            )
            .unwrap();
        };
        row("send", -300, now - 1_000);
        row("pay", -50, now - 2_000);
        row("withdraw", -100, now - DAY_MS + 1_000);
        // Credits are not outflow, and a debit older than the window has aged out.
        row("charge", 9_000, now - 1_000);
        row("send", -7_000, now - DAY_MS - 1);
        assert_eq!(outflow_24h(&conn, "acct-a", now).unwrap(), 450);
    }
}
