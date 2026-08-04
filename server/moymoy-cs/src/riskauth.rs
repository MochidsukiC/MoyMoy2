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
//! | anything above that | PIN |
//! | > 5,000 エメ once, > 10,000 エメ in 24h, or an unfamiliar device | PIN + email OTP |
//!
//! Constants, not configuration: a wallet whose spending limits can be moved by
//! an environment variable has limits set by whoever can edit the launcher.
//! DEV.md records the numbers for operational review.
//!
//! ## The second factor fails closed
//!
//! A movement in the top band from an account with no verified email is
//! **refused** (`otp_unavailable`), not quietly served on the PIN alone. Every
//! account the signup flow can create has an address; one without is a leftover
//! from development. Degrading instead would leave a threshold that decides
//! nothing, which is worse than having no threshold at all.

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::auth::{self, LockoutPolicy, PinAttempt, PinSettle};
use crate::db::now_ms;
use crate::error::ApiError;
use crate::otp::{self, VerifyOtp};

/// Single-movement ceiling below which nothing is asked.
pub const FRICTIONLESS_SINGLE: i64 = 200;
/// Rolling 24h outflow below which nothing is asked.
pub const FRICTIONLESS_DAILY: i64 = 1_000;
/// Single movement above which a second factor is required.
pub const STEPUP_SINGLE: i64 = 5_000;
/// Rolling 24h outflow above which a second factor is required.
pub const STEPUP_DAILY: i64 = 10_000;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// The OTP purpose a step-up code is issued and verified under.
///
/// Its own namespace, next to signup / login2fa / recovery: a code mailed to
/// confirm a login must not also authorize a 9,000 エメ payment, and
/// [`crate::otp`] keys every code by `(purpose, email)`.
pub const PURPOSE_STEPUP: &str = "stepup";

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
    PinAndOtp,
}

impl Requirement {
    pub fn needs_pin(self) -> bool {
        !matches!(self, Requirement::None)
    }
    pub fn needs_otp(self) -> bool {
        matches!(self, Requirement::PinAndOtp)
    }
    /// The strictest of the two. Payments start at [`Requirement::Pin`] by
    /// standing decision and are raised from there by the amount.
    pub fn max(self, other: Requirement) -> Requirement {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }
    fn rank(self) -> u8 {
        match self {
            Requirement::None => 0,
            Requirement::Pin => 1,
            Requirement::PinAndOtp => 2,
        }
    }
    /// What the client is told it must collect. Distinct from `invalid_pin`: one
    /// says "ask the user", the other says "the user got it wrong".
    pub fn code(self) -> &'static str {
        match self {
            Requirement::None => "none",
            Requirement::Pin => "pin",
            Requirement::PinAndOtp => "pin_otp",
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
/// An unfamiliar device lands in the top band whatever the amount, and that is
/// not a rounding-up: the thing in doubt is who is holding the session, not how
/// much they are asking for, and a code sent to the account's own address is the
/// only check here that the holder of the session cannot answer by themselves.
pub fn assess(amount: i64, outflow_24h: i64, device: DeviceTrust) -> Requirement {
    let after = outflow_24h.saturating_add(amount);
    if amount > STEPUP_SINGLE || after > STEPUP_DAILY || device == DeviceTrust::Unfamiliar {
        return Requirement::PinAndOtp;
    }
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
    /// The emailed code, once the caller has been told `otp_required`.
    pub otp: Option<&'a str>,
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
    email_enabled: bool,
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

    // Resolve the second factor before spending a PIN attempt on a request that
    // was never going to be enough.
    let mut otp_check: Option<(String, String)> = None;
    if requirement.needs_otp() {
        let info = auth::account_full(conn, account_id)?
            .ok_or_else(|| ApiError::unauthorized("account no longer exists"))?;
        let verified_email = info
            .email_lower
            .filter(|_| email_enabled && info.email_verified);
        let Some(email_lower) = verified_email else {
            // Fail closed. This account (or this deploy) cannot produce the
            // factor a movement of this size requires, and settling for the PIN
            // instead would make the threshold decide nothing at all.
            tracing::warn!(account = %account_id, amount,
                "step-up refused: this movement requires an email code and the account has no \
                 verified address (or mail is disabled in this deploy)");
            return Ok(StepUp::Refused(json!({
                "ok": false, "error": "otp_unavailable", "required": requirement.code(),
            })));
        };
        let Some(code) = caller.otp.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(StepUp::Refused(
                json!({ "ok": false, "error": "otp_required", "required": requirement.code() }),
            ));
        };
        otp_check = Some((email_lower, code.to_string()));
    }

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

    // ── DO NOT FOLD THIS INTO THE CALLER'S MONEY TRANSACTION ────────────────
    //
    // The code is verified and consumed HERE, in a transaction of its own that
    // commits on BOTH outcomes. It looks tidier to hand the code down and check
    // it alongside the transfer — that is exactly the bug this shape exists to
    // prevent, and it was live once.
    //
    // `otp::verify` records a wrong code by incrementing that code's attempt
    // counter, and that counter is the ONLY thing bounding guesses at a six-digit
    // number. Verified inside the money transaction, every wrong guess was rolled
    // back along with the payment it failed to authorize, so the five-attempt
    // limit never arrived and the second factor could be guessed indefinitely —
    // i.e. it stopped being a factor at all.
    //
    // The price of committing here is that a valid code is spent even when the
    // transfer then fails for want of funds. The customer asks for another one.
    // That is the cheaper of the two prices by a wide margin.
    if let Some((email_lower, code)) = otp_check {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let verdict = otp::verify(&tx, PURPOSE_STEPUP, &email_lower, &code)?;
        tx.commit()?;
        match verdict {
            // A code issued to a different account is not a code for this one,
            // however valid it is in its own right.
            VerifyOtp::Ok { account_id: a, .. } if a.as_deref() == Some(account_id) => {}
            _ => {
                // The PIN was right — only the code was wrong, and the code now
                // carries that failure on its own counter. Give the PIN attempt
                // back, or five mistyped codes would lock somebody out of their
                // account rather than just out of that code.
                auth::clear_pin_failures(conn, account_id, epoch)?;
                return Ok(StepUp::Refused(
                    json!({ "ok": false, "error": "invalid_code" }),
                ));
            }
        }
    }

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

    #[test]
    fn the_step_up_band_starts_one_eme_past_each_threshold() {
        let f = DeviceTrust::Familiar;
        assert_eq!(assess(STEPUP_SINGLE, 0, f), Requirement::Pin);
        assert_eq!(assess(STEPUP_SINGLE + 1, 0, f), Requirement::PinAndOtp);
        // The daily ceiling counts this movement too: 10,000 already spent plus
        // one more eme is over it, even though the movement itself is trivial.
        assert_eq!(assess(1, STEPUP_DAILY - 1, f), Requirement::Pin);
        assert_eq!(assess(1, STEPUP_DAILY, f), Requirement::PinAndOtp);
        // The largest single withdrawal the wallet permits is in the top band,
        // so a full payout always needs the code.
        assert_eq!(
            assess(crate::wallet::MAX_WITHDRAW_PER_OP, 0, f),
            Requirement::PinAndOtp
        );
    }

    #[test]
    fn an_unfamiliar_device_steps_up_however_small_the_amount() {
        // The signal is worth nothing if it only applies to amounts that were
        // already going to ask for something: what is in doubt is who holds the
        // session, and a code mailed to the account is the one check here that
        // the holder of a stolen session cannot answer alone.
        assert_eq!(assess(1, 0, DeviceTrust::Unfamiliar), Requirement::PinAndOtp);
        assert_eq!(
            assess(FRICTIONLESS_SINGLE, 0, DeviceTrust::Unfamiliar),
            Requirement::PinAndOtp
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
        for (amount, spent) in [(1, 0), (FRICTIONLESS_SINGLE, 0)] {
            assert_eq!(
                Requirement::Pin.max(assess(amount, spent, DeviceTrust::Familiar)),
                Requirement::Pin,
                "amount {amount} after {spent}"
            );
        }
        // …and the amount may still raise it further.
        assert_eq!(
            Requirement::Pin.max(assess(9_000, 0, DeviceTrust::Familiar)),
            Requirement::PinAndOtp
        );
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
            otp: None,
        };
        // The established handset, well inside the frictionless band: nothing.
        let cleared = step_up(
            &mut conn,
            &backoff,
            false,
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
            false,
            &caller("phone-2", None),
            1,
            Requirement::None,
        )
        .unwrap();
        let StepUp::Refused(body) = refused else {
            panic!("an unfamiliar handset was waved through");
        };
        assert_eq!(body["error"], json!("pin_required"), "{body}");
        // …and it is the TOP band it is being asked for, not merely a PIN: a
        // session that has moved handsets is exactly the case a code mailed to
        // the account's own address is there to catch.
        assert_eq!(body["required"], json!("pin_otp"));

        // This fixture's account has no verified address, so the movement is
        // refused outright rather than served on the PIN alone.
        let refused = step_up(
            &mut conn,
            &backoff,
            true,
            &caller("phone-2", Some("1234")),
            1,
            Requirement::None,
        )
        .unwrap();
        let StepUp::Refused(body) = refused else {
            panic!("a movement needing a code was cleared without one");
        };
        assert_eq!(body["error"], json!("otp_unavailable"), "{body}");
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
