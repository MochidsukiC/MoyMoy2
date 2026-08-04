//! Merchant lifecycle: self-serve registration, API-key credentials, and the
//! impersonation defences that let an approval screen's "who is asking you for
//! money" be believed.
//!
//! Two credentials, deliberately not interchangeable:
//!   - **API key** (`Authorization: Bearer moy_sk_…`, the [`MerchantAuth`]
//!     extractor) creates, reads and cancels payment intents. It moves no money
//!     and it cannot change the shop it belongs to, so a leaked key can annoy a
//!     merchant's customers but cannot take the shop over.
//!   - **Session + PIN** (`/merchant/portal/*`) registers a shop, rotates the
//!     key, stops the shop and raises its ceilings. A stolen session alone still
//!     cannot mint a key.
//!
//! The name rules are the load-bearing part. A merchant name is what a person
//! reads before typing their PIN, so `lower(name)` uniqueness is not enough: it
//! only settles who asked first, and `PiggleShoр2` (Cyrillic er) walks straight
//! through NFKC + lowercase. Uniqueness is therefore decided on the UTS #39
//! confusable skeleton, alphabets may not be mixed beyond what a real language
//! needs, and the operator's own vocabulary is reserved outright.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::extract::FromRequestParts;
use axum::http::header;
use axum::http::request::Parts;
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;
use unicode_security::{RestrictionLevel, RestrictionLevelDetection};
use uuid::Uuid;

use crate::api::AppState;
use crate::auth;
use crate::db::now_ms;
use crate::error::ApiError;

/// A merchant that may trade.
pub const STATUS_ACTIVE: &str = "active";
/// A merchant that has been stopped — by its owner or by an operator. Its key
/// stops working and its outstanding intents stop being payable.
pub const STATUS_DISABLED: &str = "disabled";

/// Length ceilings, in characters (not bytes). The approval screen cannot be the
/// thing that stops a 4 KB description: by the time it is rendering, the string
/// has already been stored and served as fact.
pub const MAX_NAME_CHARS: usize = 32;
pub const MAX_SUB_CHARS: usize = 48;
pub const MAX_DESCRIPTION_CHARS: usize = 120;
pub const MAX_ORDER_REF_CHARS: usize = 64;
/// Combining marks allowed on one base character. Enough for any real script,
/// far below what it takes to push a glyph out of its own line.
const MAX_COMBINING_PER_BASE: usize = 3;

/// Live merchants one login account may own. Registration is self-serve, so this
/// is what stops one account from farming names.
pub const MAX_ACTIVE_MERCHANTS: i64 = 3;

/// Issuance ceilings for a merchant that has never had them raised.
///
/// These exist because merchant revenue is NOT held in escrow (DEV.md: accepted
/// risk). Nothing can claw money back once it has been withdrawn to the MC world,
/// so what is limited instead is the speed at which a shop can ask for money:
/// how many bills it may have outstanding, and how much it may bill in a day.
pub const DEFAULT_MAX_OPEN_INTENTS: i64 = 20;
pub const DEFAULT_DAILY_ISSUE_CAP: i64 = 50_000;
/// The most the portal will raise those to. A raise costs session + PIN, so this
/// is the ceiling on what a leaked API key could ever be talked into.
pub const MAX_OPEN_INTENTS_CEILING: i64 = 500;
pub const MAX_DAILY_ISSUE_CEILING: i64 = 2_000_000;

/// Rate limits as `(calls, window_ms)`.
///
/// Creation is read as "30 per minute" — the 24h amount cap and the open-intent
/// cap above are the real brake on a runaway merchant, and this only needs to
/// stop a loop.
pub const RL_INTENT_CREATE: (usize, i64) = (30, 60_000);
pub const RL_INTENT_READ: (usize, i64) = (120, 60_000);
pub const RL_REGISTER: (usize, i64) = (1, 10 * 60_000);

/// API-key prefix. Greppable on purpose: a key pasted into a public repo should
/// be findable by the same scan that finds every other `*_sk_` credential.
pub const API_KEY_PREFIX: &str = "moy_sk_";

// ── text guard ───────────────────────────────────────────────────────────────

/// Why a merchant-supplied string was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextReject {
    Empty,
    TooLong,
    /// A character that renders as nothing, renders as something else, or is not
    /// a character yet.
    Invisible,
    /// Combining marks piled past what any script uses.
    Stacked,
}

impl TextReject {
    pub fn code(self) -> &'static str {
        match self {
            TextReject::Empty => "empty",
            TextReject::TooLong => "too_long",
            TextReject::Invisible => "invisible_char",
            TextReject::Stacked => "stacked_marks",
        }
    }
}

/// Normalize and vet one merchant-supplied display string (`name`, `sub`,
/// `description`). The **normalized** form is what callers must store: validating
/// one string and persisting another would put a string on the approval screen
/// that never passed this function.
///
/// "No control characters" is not the rule, because U+202E RIGHT-TO-LEFT OVERRIDE
/// is Cf, not Cc. One of those in a description is enough to render
/// `"MoyMoy 公式確認: PIN を再入力してください"` on the approval screen out of a
/// string that reads innocently in the merchant's own dashboard. So the whole
/// invisible half of category C is refused by what it is: Cc, Cf, unassigned and
/// private use.
pub fn sanitize_text(input: &str, max_chars: usize) -> Result<String, TextReject> {
    let normalized: String = input.nfkc().collect();
    let text = normalized.trim();
    if text.is_empty() {
        return Err(TextReject::Empty);
    }
    if text.chars().count() > max_chars {
        return Err(TextReject::TooLong);
    }
    let mut combining = 0usize;
    let mut has_base = false;
    for c in text.chars() {
        match get_general_category(c) {
            GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::Unassigned
            | GeneralCategory::PrivateUse
            | GeneralCategory::Surrogate
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator => return Err(TextReject::Invisible),
            GeneralCategory::NonspacingMark
            | GeneralCategory::SpacingMark
            | GeneralCategory::EnclosingMark => {
                // A mark with nothing in front of it attaches to whatever the UI
                // drew last — the label, not the value.
                if !has_base {
                    return Err(TextReject::Stacked);
                }
                combining += 1;
                if combining > MAX_COMBINING_PER_BASE {
                    return Err(TextReject::Stacked);
                }
            }
            _ => {
                has_base = true;
                combining = 0;
            }
        }
    }
    Ok(text.to_string())
}

/// The confusable skeleton a name resolves to — the value merchant names are
/// unique on.
///
/// Each step closes one way of claiming a name somebody else already has:
/// `ＰＩＧＧＬＥ` (NFKC), `piggleshop` (case), `Piggle Shop` (spacing), and
/// `PiggleShoр` with a Cyrillic er (the skeleton itself, UTS #39 §4).
pub fn name_skeleton(name: &str) -> String {
    let folded: String = name
        .nfkc()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace())
        .collect();
    unicode_security::skeleton(&folded).collect()
}

/// Words nobody may build a shop name out of. Matched as substrings of the
/// *skeleton*, so `Мoymoy` (Cyrillic em) is caught by the same list.
const RESERVED_NAME_PARTS: &[&str] = &[
    "moymoy", "mochi", "admin", "official", "support", "staff", "system", "運営", "公式", "管理",
    "サポート", "事務局",
];

fn is_reserved(skeleton: &str) -> bool {
    RESERVED_NAME_PARTS
        .iter()
        .any(|w| skeleton.contains(&name_skeleton(w)))
}

/// Do the letters in `name` come from a coherent set of alphabets?
///
/// UTS #39's restriction level is defined over identifiers, so a space, a digit,
/// an `&` or an emoji makes it report `Unrestricted` — it would refuse
/// `"Piggle Shop 2"`. The question actually being asked is "does this name mix
/// alphabets", and a space belongs to no alphabet, so only letters and marks are
/// judged. `HighlyRestrictive` is what admits normal Japanese (Han + Hiragana +
/// Katakana + Latin) while refusing the Latin/Cyrillic hybrid that exists only to
/// look like something else.
fn scripts_are_coherent(name: &str) -> bool {
    let letters: String = name
        .chars()
        .filter(|c| {
            matches!(
                get_general_category(*c),
                GeneralCategory::UppercaseLetter
                    | GeneralCategory::LowercaseLetter
                    | GeneralCategory::TitlecaseLetter
                    | GeneralCategory::ModifierLetter
                    | GeneralCategory::OtherLetter
                    | GeneralCategory::NonspacingMark
                    | GeneralCategory::SpacingMark
                    | GeneralCategory::EnclosingMark
            )
        })
        .collect();
    // A name made only of digits and punctuation mixes nothing.
    letters.is_empty()
        || letters.detect_restriction_level() <= RestrictionLevel::HighlyRestrictive
}

/// Why a proposed merchant name was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameReject {
    Text(TextReject),
    /// Contains the operator's own vocabulary.
    Reserved,
    /// Mixes alphabets no language mixes.
    MixedScript,
    /// Normalizes to nothing that can be told apart from anything else.
    NoSkeleton,
}

impl NameReject {
    pub fn code(self) -> &'static str {
        match self {
            NameReject::Text(t) => t.code(),
            NameReject::Reserved => "reserved_name",
            NameReject::MixedScript => "mixed_script",
            NameReject::NoSkeleton => "unnameable",
        }
    }
}

/// Vet a proposed merchant name, returning `(normalized name, skeleton)`.
///
/// There is deliberately **no rename API** anywhere in this module: the name is
/// fixed at registration. An approval screen that can be showing one name while
/// the merchant changes it to another is not showing a fact.
pub fn valid_merchant_name(input: &str) -> Result<(String, String), NameReject> {
    let name = sanitize_text(input, MAX_NAME_CHARS).map_err(NameReject::Text)?;
    if !scripts_are_coherent(&name) {
        return Err(NameReject::MixedScript);
    }
    let skeleton = name_skeleton(&name);
    if skeleton.is_empty() {
        return Err(NameReject::NoSkeleton);
    }
    if is_reserved(&skeleton) {
        return Err(NameReject::Reserved);
    }
    Ok((name, skeleton))
}

// ── credentials ──────────────────────────────────────────────────────────────

/// A fresh API key. The plaintext is returned to the merchant exactly once; only
/// [`api_key_hash`] of it is ever stored.
fn gen_api_key() -> String {
    format!("{API_KEY_PREFIX}{}", auth::gen_token())
}

/// SHA-256(key) as base64 — the same discipline session tokens use, and for the
/// same reason: a lookup has to be fast per request, and a dump of `merchants`
/// must not hand anyone a working key.
pub fn api_key_hash(key: &str) -> String {
    auth::token_hash(key)
}

/// The leading chars of a key, so the portal can say *which* key is installed
/// without being able to show it.
fn key_prefix_of(key: &str) -> String {
    key.chars().take(API_KEY_PREFIX.len() + 6).collect()
}

/// A 128-bit per-merchant salt for [`payer_ref`].
fn gen_payer_salt() -> String {
    let mut buf = [0u8; 16];
    OsRng.fill_bytes(&mut buf);
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(buf)
}

/// The stable, per-merchant pseudonym for one payer.
///
/// `sha256(salt ‖ account_id)` with a salt this merchant alone holds: the same
/// customer is the same ref every time they buy from this shop (so repeat fraud
/// and a customer's own claim can be matched), and two shops comparing their
/// books learn nothing — the refs for one person do not correlate across salts.
/// The real `@handle` never leaves the wallet.
pub fn payer_ref(salt_b64: &str, account_id: &str) -> Result<String, ApiError> {
    let salt = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(salt_b64)
        .map_err(|e| ApiError::internal(format!("merchant payer_ref_salt is not base64: {e}")))?;
    let mut h = Sha256::new();
    h.update(&salt);
    h.update(account_id.as_bytes());
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.finalize()))
}

// ── rows ─────────────────────────────────────────────────────────────────────

/// A merchant as the payment paths need it.
#[derive(Debug, Clone)]
pub struct MerchantRow {
    pub merchant_id: String,
    /// The account the money lands in.
    pub account_id: String,
    /// The login account that registered it. Equal to `account_id` for every
    /// self-serve merchant; NULL only for the pre-v6 demo rows.
    pub owner_account_id: Option<String>,
    pub name: String,
    pub sub: Option<String>,
    pub status: String,
    pub payer_ref_salt: Option<String>,
    pub max_open_intents: i64,
    pub daily_issue_cap: i64,
    pub created_unix_ms: i64,
}

impl MerchantRow {
    pub fn is_active(&self) -> bool {
        self.status == STATUS_ACTIVE
    }
}

const MERCHANT_COLS: &str = "merchant_id, account_id, owner_account_id, name, sub, status, \
     payer_ref_salt, max_open_intents, daily_issue_cap, created_unix_ms";

fn row_to_merchant(r: &rusqlite::Row<'_>) -> rusqlite::Result<MerchantRow> {
    Ok(MerchantRow {
        merchant_id: r.get(0)?,
        account_id: r.get(1)?,
        owner_account_id: r.get(2)?,
        name: r.get(3)?,
        sub: r.get(4)?,
        status: r.get(5)?,
        payer_ref_salt: r.get(6)?,
        max_open_intents: r.get::<_, Option<i64>>(7)?.unwrap_or(DEFAULT_MAX_OPEN_INTENTS),
        daily_issue_cap: r.get::<_, Option<i64>>(8)?.unwrap_or(DEFAULT_DAILY_ISSUE_CAP),
        created_unix_ms: r.get(9)?,
    })
}

/// Fetch a merchant by id, whatever its status. Callers that move money must
/// check [`MerchantRow::is_active`] themselves — see `payments::approve`, where
/// letting a disabled shop keep collecting on intents it already issued is the
/// exact bug this replaced.
pub fn get(conn: &Connection, merchant_id: &str) -> rusqlite::Result<Option<MerchantRow>> {
    conn.query_row(
        &format!("SELECT {MERCHANT_COLS} FROM merchants WHERE merchant_id = ?1"),
        [merchant_id],
        row_to_merchant,
    )
    .optional()
}

/// A merchant as its owner sees it in the portal.
#[derive(Debug, Serialize)]
pub struct MerchantSummary {
    pub merchant_id: String,
    pub name: String,
    pub sub: Option<String>,
    pub status: String,
    pub listed: bool,
    pub api_key_prefix: Option<String>,
    pub api_key_created_unix_ms: Option<i64>,
    /// When this key was last accepted. The one field that makes a leak visible:
    /// a merchant who has not traded today, seeing today's timestamp, knows.
    pub api_key_last_used_unix_ms: Option<i64>,
    pub max_open_intents: i64,
    pub daily_issue_cap: i64,
    pub created_unix_ms: i64,
}

/// Every merchant this login account owns.
pub fn list_for_owner(
    conn: &Connection,
    owner_account_id: &str,
) -> rusqlite::Result<Vec<MerchantSummary>> {
    let mut stmt = conn.prepare(
        "SELECT merchant_id, name, sub, status, listed, api_key_prefix, api_key_created_unix_ms, \
                api_key_last_used_unix_ms, max_open_intents, daily_issue_cap, created_unix_ms \
         FROM merchants WHERE owner_account_id = ?1 ORDER BY created_unix_ms ASC",
    )?;
    let v = stmt
        .query_map([owner_account_id], |r| {
            Ok(MerchantSummary {
                merchant_id: r.get(0)?,
                name: r.get(1)?,
                sub: r.get(2)?,
                status: r.get(3)?,
                listed: r.get::<_, i64>(4)? != 0,
                api_key_prefix: r.get(5)?,
                api_key_created_unix_ms: r.get(6)?,
                api_key_last_used_unix_ms: r.get(7)?,
                max_open_intents: r.get::<_, Option<i64>>(8)?.unwrap_or(DEFAULT_MAX_OPEN_INTENTS),
                daily_issue_cap: r.get::<_, Option<i64>>(9)?.unwrap_or(DEFAULT_DAILY_ISSUE_CAP),
                created_unix_ms: r.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(v)
}

// ── registration / portal operations ─────────────────────────────────────────

/// Outcome of a self-serve registration. Only `Ok` is a success; the rest are
/// ordinary validation answers (HTTP 200, `ok:false`).
#[derive(Debug)]
pub enum RegisterOutcome {
    Ok {
        merchant_id: String,
        name: String,
        /// The plaintext key. Returned once, here, and never again.
        api_key: String,
        api_key_prefix: String,
    },
    BadName(NameReject),
    BadSub(TextReject),
    NameTaken,
    TooManyMerchants,
}

/// Register a shop owned by `owner_account_id`, inside the caller's transaction
/// (the uniqueness check and the insert must be one unit against a concurrent
/// registration of the same name).
///
/// `merchants.account_id` is set to the owner's own login account, not to a
/// generated one: that is what makes the takings withdrawable. The pre-v6 demo
/// merchants had backing accounts with no handle and no PIN, so money paid to one
/// could never be moved again — which is why they are `listed = 0` now.
pub fn register(
    tx: &rusqlite::Transaction<'_>,
    owner_account_id: &str,
    name_in: &str,
    sub_in: Option<&str>,
    glyph: Option<&str>,
    pal: Option<&str>,
) -> Result<RegisterOutcome, ApiError> {
    let (name, skeleton) = match valid_merchant_name(name_in) {
        Ok(v) => v,
        Err(e) => return Ok(RegisterOutcome::BadName(e)),
    };
    let sub = match sub_in.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => match sanitize_text(s, MAX_SUB_CHARS) {
            Ok(v) => Some(v),
            Err(e) => return Ok(RegisterOutcome::BadSub(e)),
        },
        None => None,
    };

    let owned: i64 = tx.query_row(
        "SELECT COUNT(*) FROM merchants WHERE owner_account_id = ?1 AND status = ?2",
        params![owner_account_id, STATUS_ACTIVE],
        |r| r.get(0),
    )?;
    if owned >= MAX_ACTIVE_MERCHANTS {
        return Ok(RegisterOutcome::TooManyMerchants);
    }
    let taken = tx
        .query_row(
            "SELECT 1 FROM merchants WHERE name_skeleton = ?1",
            [&skeleton],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if taken {
        return Ok(RegisterOutcome::NameTaken);
    }

    let merchant_id = format!("mr_{}", Uuid::new_v4().simple());
    let api_key = gen_api_key();
    let prefix = key_prefix_of(&api_key);
    let now = now_ms();
    tx.execute(
        "INSERT INTO merchants \
           (merchant_id, account_id, owner_account_id, name, name_skeleton, sub, glyph, pal, \
            status, api_key_hash, api_key_prefix, api_key_created_unix_ms, listed, \
            payer_ref_salt, created_unix_ms, updated_unix_ms) \
         VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, ?12, ?11, ?11)",
        params![
            merchant_id,
            owner_account_id,
            name,
            skeleton,
            sub,
            glyph,
            pal,
            STATUS_ACTIVE,
            api_key_hash(&api_key),
            prefix,
            now,
            gen_payer_salt(),
        ],
    )?;
    Ok(RegisterOutcome::Ok {
        merchant_id,
        name,
        api_key,
        api_key_prefix: prefix,
    })
}

/// Issue a new API key for a merchant this account owns, invalidating the old one
/// in the same statement (there is no grace period — a rotation is what a
/// merchant does when they believe the old key is in someone else's hands).
/// Returns `None` when the merchant is not theirs.
pub fn rotate_key(
    tx: &rusqlite::Transaction<'_>,
    merchant_id: &str,
    owner_account_id: &str,
) -> rusqlite::Result<Option<(String, String)>> {
    let api_key = gen_api_key();
    let prefix = key_prefix_of(&api_key);
    let now = now_ms();
    let changed = tx.execute(
        "UPDATE merchants SET api_key_hash = ?3, api_key_prefix = ?4, \
                api_key_created_unix_ms = ?5, api_key_last_used_unix_ms = NULL, \
                updated_unix_ms = ?5 \
         WHERE merchant_id = ?1 AND owner_account_id = ?2",
        params![merchant_id, owner_account_id, api_key_hash(&api_key), prefix, now],
    )?;
    Ok((changed == 1).then_some((api_key, prefix)))
}

/// Set a merchant's status. `false` when the merchant is not this account's.
pub fn set_status(
    tx: &rusqlite::Transaction<'_>,
    merchant_id: &str,
    owner_account_id: &str,
    status: &str,
) -> rusqlite::Result<bool> {
    let changed = tx.execute(
        "UPDATE merchants SET status = ?3, updated_unix_ms = ?4 \
         WHERE merchant_id = ?1 AND owner_account_id = ?2",
        params![merchant_id, owner_account_id, status, now_ms()],
    )?;
    Ok(changed == 1)
}

/// Raise (or lower) a merchant's issuance ceilings, clamped to the hard maxima.
/// `false` when the merchant is not this account's.
pub fn set_limits(
    tx: &rusqlite::Transaction<'_>,
    merchant_id: &str,
    owner_account_id: &str,
    max_open_intents: Option<i64>,
    daily_issue_cap: Option<i64>,
) -> rusqlite::Result<bool> {
    let open = max_open_intents.map(|v| v.clamp(1, MAX_OPEN_INTENTS_CEILING));
    let cap = daily_issue_cap.map(|v| v.clamp(1, MAX_DAILY_ISSUE_CEILING));
    let changed = tx.execute(
        "UPDATE merchants SET max_open_intents = COALESCE(?3, max_open_intents), \
                daily_issue_cap = COALESCE(?4, daily_issue_cap), updated_unix_ms = ?5 \
         WHERE merchant_id = ?1 AND owner_account_id = ?2",
        params![merchant_id, owner_account_id, open, cap, now_ms()],
    )?;
    Ok(changed == 1)
}

// ── issuance ceilings ────────────────────────────────────────────────────────

/// Whether a merchant may issue one more intent right now.
#[derive(Debug, PartialEq, Eq)]
pub enum IssueGuard {
    Ok,
    TooManyOpen { limit: i64 },
    DailyCapExceeded { limit: i64, issued: i64 },
}

/// Check the issuance ceilings inside the caller's transaction.
///
/// The daily total counts every intent created in the window whatever became of
/// it — canceled and expired ones included. The ceiling is on how fast a shop can
/// *ask* for money, and an intent that was created and abandoned still went in
/// front of a customer.
pub fn check_issuance(
    tx: &rusqlite::Transaction<'_>,
    m: &MerchantRow,
    amount: i64,
    now: i64,
) -> rusqlite::Result<IssueGuard> {
    let open: i64 = tx.query_row(
        "SELECT COUNT(*) FROM payment_intents \
         WHERE merchant_id = ?1 AND state = ?2 AND expires_unix_ms > ?3",
        params![m.merchant_id, crate::payments::STATE_CREATED, now],
        |r| r.get(0),
    )?;
    if open >= m.max_open_intents {
        return Ok(IssueGuard::TooManyOpen {
            limit: m.max_open_intents,
        });
    }
    let issued: i64 = tx.query_row(
        "SELECT COALESCE(SUM(amount), 0) FROM payment_intents \
         WHERE merchant_id = ?1 AND created_unix_ms > ?2",
        params![m.merchant_id, now - 24 * 60 * 60 * 1000],
        |r| r.get(0),
    )?;
    if issued.saturating_add(amount) > m.daily_issue_cap {
        return Ok(IssueGuard::DailyCapExceeded {
            limit: m.daily_issue_cap,
            issued,
        });
    }
    Ok(IssueGuard::Ok)
}

// ── rate limiting ────────────────────────────────────────────────────────────

/// Sliding-window call counters, in this process.
///
/// Per-process is the right scope and not a shortcut: there is one moymoy-cs, and
/// a limiter in SQLite would put a write on every read of every intent. Nothing
/// here is a security boundary — the ceilings above are — this only stops a loop.
#[derive(Default)]
pub struct RateLimiter {
    inner: Mutex<HashMap<String, VecDeque<i64>>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Ok(())` when this call fits within `limit` calls per `window_ms`, else
    /// `Err(retry_after_ms)`.
    ///
    /// A refused call is NOT recorded, so a client hammering a closed door cannot
    /// keep extending its own penalty past the window it already earned.
    pub fn check(&self, key: &str, limit: usize, window_ms: i64, now: i64) -> Result<(), i64> {
        let mut map = self.lock();
        if map.len() > 1024 {
            map.retain(|_, hits| hits.back().is_some_and(|t| *t > now - window_ms));
        }
        let hits = map.entry(key.to_string()).or_default();
        while hits.front().is_some_and(|t| *t <= now - window_ms) {
            hits.pop_front();
        }
        if hits.len() >= limit {
            let oldest = hits.front().copied().unwrap_or(now);
            return Err((oldest + window_ms - now).max(1));
        }
        hits.push_back(now);
        Ok(())
    }

    /// Same recovery posture as `attest::CharSessionStore`: the map is held only
    /// for its own surgery, so a poisoned lock means another thread panicked
    /// mid-update. The contents are decaying counters, and refusing every
    /// merchant call for the rest of the process's life would be the worse
    /// outcome.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, VecDeque<i64>>> {
        self.inner.lock().unwrap_or_else(|e| {
            tracing::error!("RateLimiter mutex was poisoned; recovering the counters");
            e.into_inner()
        })
    }
}

// ── API-key extractor ────────────────────────────────────────────────────────

/// The merchant behind an `Authorization: Bearer moy_sk_…` request.
///
/// This authenticates a *shop*, never a person, and it is never accepted in place
/// of a session: nothing reachable with a [`MerchantAuth`] moves a balance.
#[derive(Debug, Clone)]
pub struct MerchantAuth {
    pub merchant: MerchantRow,
}

#[axum::async_trait]
impl FromRequestParts<AppState> for MerchantAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let key = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|s| s.starts_with(API_KEY_PREFIX) && s.len() > API_KEY_PREFIX.len())
            .ok_or_else(|| ApiError::unauthorized("missing merchant API key"))?
            .to_string();
        let pool = state.pool.clone();
        let found = tokio::task::spawn_blocking(move || -> Result<Option<MerchantRow>, ApiError> {
            let conn = pool.get()?;
            let row = conn
                .query_row(
                    &format!("SELECT {MERCHANT_COLS} FROM merchants WHERE api_key_hash = ?1"),
                    [api_key_hash(&key)],
                    row_to_merchant,
                )
                .optional()?;
            // Stamped for a presented key whatever its merchant's status: the
            // point of this column is noticing that somebody else is using the
            // key, and a merchant who stopped their shop is exactly who is
            // watching for that.
            if let Some(m) = &row {
                conn.execute(
                    "UPDATE merchants SET api_key_last_used_unix_ms = ?2 WHERE merchant_id = ?1",
                    params![m.merchant_id, now_ms()],
                )?;
            }
            Ok(row)
        })
        .await??;
        match found {
            Some(m) if m.is_active() => Ok(MerchantAuth { merchant: m }),
            // A stopped shop gets told so rather than "bad key": it is the answer
            // its owner needs, and it reveals nothing to anyone who did not
            // already hold the key.
            Some(_) => Err(ApiError::forbidden("merchant is disabled")),
            None => Err(ApiError::unauthorized("invalid merchant API key")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_bidi_override_never_reaches_the_approval_screen() {
        // U+202E is Cf, not Cc, so `is_control()` would pass it — and a
        // description that reverses everything after it is how an approval screen
        // gets talked into displaying MoyMoy's own words.
        for bad in [
            "MoyMoy\u{202E}確認",
            "shop\u{200B}name",  // ZWSP
            "shop\u{200D}name",  // ZWJ
            "shop\u{2066}name",  // LRI
            "shop\u{FEFF}name",  // BOM as a word joiner
            "shop\u{E000}name",  // private use
            "line\nbreak",
            "tab\there",
        ] {
            assert_eq!(
                sanitize_text(bad, 64),
                Err(TextReject::Invisible),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn ordinary_shop_text_survives_the_guard() {
        assert_eq!(sanitize_text("  Piggle Shop 2  ", 32).unwrap(), "Piggle Shop 2");
        assert_eq!(sanitize_text("鉱石商会 本店", 32).unwrap(), "鉱石商会 本店");
        // NFKC folds the halfwidth/fullwidth forms together, and the folded form
        // is what comes back — storing the raw input would display a string this
        // function never saw.
        assert_eq!(sanitize_text("ﾋﾟｸﾞﾙ①", 32).unwrap(), "ピグル1");
        assert_eq!(sanitize_text("   ", 32), Err(TextReject::Empty));
        assert_eq!(sanitize_text("abcdef", 5), Err(TextReject::TooLong));
    }

    #[test]
    fn marks_may_not_be_piled_up_or_left_unanchored() {
        assert!(sanitize_text("がぎ", 32).is_ok());
        // NFKC composes the first mark into the base, so what is counted is what
        // will actually be drawn on top of a glyph.
        assert_eq!(sanitize_text("e\u{0301}", 32).unwrap(), "é");
        assert!(sanitize_text("e\u{0301}\u{0302}\u{0303}\u{0304}", 32).is_ok());
        assert_eq!(
            sanitize_text("e\u{0301}\u{0302}\u{0303}\u{0304}\u{0305}", 32),
            Err(TextReject::Stacked)
        );
        assert_eq!(sanitize_text("\u{0301}shop", 32), Err(TextReject::Stacked));
    }

    #[test]
    fn a_cyrillic_lookalike_lands_on_the_skeleton_of_the_name_it_imitates() {
        // The whole reason uniqueness is not `lower(name)`: these two differ in
        // one code point and are indistinguishable on screen.
        assert_eq!(
            name_skeleton("PiggleShoр2"), // Cyrillic er
            name_skeleton("PiggleShop2")
        );
        // …as are the case, spacing and fullwidth variants.
        assert_eq!(name_skeleton("piggle shop"), name_skeleton("PiggleShop"));
        assert_eq!(name_skeleton("ＰｉｇｇｌｅShop"), name_skeleton("PiggleShop"));
        assert_ne!(name_skeleton("PiggleShop"), name_skeleton("WiggleShop"));
    }

    #[test]
    fn alphabets_may_not_be_mixed_beyond_what_a_language_needs() {
        // Japanese is Han + Hiragana + Katakana + Latin, all at once, and normal.
        for ok in [
            "鉱石商会",
            "鉱石商会 本店",
            "ピグルショップ Piggle 2",
            "Piggle Shop 2",
            "ЗАО Пиггле", // wholly Cyrillic: a real name, not a disguise
            "123",
        ] {
            assert!(valid_merchant_name(ok).is_ok(), "{ok:?} was refused");
        }
        // Latin with one Cyrillic letter smuggled in exists only to look like
        // something else.
        assert_eq!(
            valid_merchant_name("PiggleShoр2").unwrap_err(),
            NameReject::MixedScript
        );
        assert_eq!(
            valid_merchant_name("Ｍoymoy").unwrap_err(),
            NameReject::Reserved
        );
    }

    #[test]
    fn the_operators_own_words_cannot_be_worn_as_a_name() {
        for bad in [
            "MoyMoy サポート",
            "moymoy",
            "MOCHI STORE",
            "公式ストア",
            "運営",
            "Official Shop",
            "admin",
        ] {
            assert_eq!(
                valid_merchant_name(bad).map(|(n, _)| n).unwrap_err(),
                NameReject::Reserved,
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn a_payer_ref_is_stable_per_shop_and_uncorrelatable_across_shops() {
        let (s1, s2) = (gen_payer_salt(), gen_payer_salt());
        let a = payer_ref(&s1, "acct-a").unwrap();
        assert_eq!(a, payer_ref(&s1, "acct-a").unwrap());
        assert_ne!(a, payer_ref(&s1, "acct-b").unwrap());
        // Same customer, different shop: nothing the two shops can join on.
        assert_ne!(a, payer_ref(&s2, "acct-a").unwrap());
    }

    #[test]
    fn a_key_is_never_recoverable_from_what_is_stored() {
        let k = gen_api_key();
        assert!(k.starts_with(API_KEY_PREFIX));
        assert_ne!(api_key_hash(&k), k);
        assert_eq!(api_key_hash(&k), api_key_hash(&k));
        assert_ne!(api_key_hash(&k), api_key_hash(&gen_api_key()));
        // The prefix identifies a key without being enough to use one.
        assert!(k.starts_with(&key_prefix_of(&k)));
        assert!(key_prefix_of(&k).len() < k.len());
    }

    #[test]
    fn a_refused_call_does_not_extend_its_own_penalty() {
        let rl = RateLimiter::new();
        for i in 0..3 {
            assert!(rl.check("m1", 3, 1_000, 100 + i).is_ok());
        }
        // Over the limit, and hammering at t=900 must not push the reopening past
        // when the first of those three calls ages out (100 + 1000).
        assert!(rl.check("m1", 3, 1_000, 900).is_err());
        assert!(rl.check("m1", 3, 1_000, 1_099).is_err());
        assert!(rl.check("m1", 3, 1_000, 1_101).is_ok());
        // Buckets are per key.
        assert!(rl.check("m2", 3, 1_000, 900).is_ok());
    }

    /// The migration's collision rule, exercised on the shape it actually
    /// guards: two pre-v6 names that resolve to one skeleton.
    #[test]
    fn a_skeleton_collision_is_detectable_without_failing_anything() {
        let mut claimed = HashSet::new();
        assert!(claimed.insert(name_skeleton("PiggleShop")));
        assert!(!claimed.insert(name_skeleton("PiggleShoр"))); // Cyrillic er
        assert!(claimed.insert(name_skeleton("鉱石商会")));
    }
}
