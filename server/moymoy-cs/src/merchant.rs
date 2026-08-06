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
use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::Json;
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;
use unicode_security::{RestrictionLevel, RestrictionLevelDetection};
use uuid::Uuid;

use crate::api::{blocking, replay, AppState};
use crate::auth::{self, AuthedAccount, LockoutPolicy, PinAttempt};
use crate::db::{self, now_ms};
use crate::error::ApiError;
use crate::payments::{self, CreateOutcome, IntentQuery, NewIntent};

/// A merchant that may trade.
pub const STATUS_ACTIVE: &str = "active";
/// A merchant that has been stopped — by its owner or by an operator. Its key
/// stops working and its outstanding intents stop being payable. It keeps its
/// name: a shop that is stopped for an afternoon has not given the name up.
pub const STATUS_DISABLED: &str = "disabled";
/// A merchant its owner has closed for good.
///
/// A soft state and not a `DELETE`, because `payment_intents.merchant_id` is a
/// foreign key: removing the row would either fail for any shop that ever traded
/// or take its order history with it. Closing releases what closing is *for* —
/// the name (`name_skeleton` → NULL), the credential, and the owner's slot — and
/// leaves the ledger able to say who was paid.
pub const STATUS_DELETED: &str = "deleted";

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

/// Merchants one login account may hold at once — **whatever their status**,
/// closed ones excepted.
///
/// Counting only `active` rows was a hole, not a shortcut: a disabled merchant
/// keeps its `name_skeleton`, so register → disable → register would have let one
/// account accumulate names without bound (throttled only by the registration
/// rate limit, ≈144 a day). Since the only way to give a slot back is
/// [`close`], which also releases the name, names and slots are now the same
/// budget and this number is the real ceiling on how many a person can hold.
pub const MAX_MERCHANTS_PER_ACCOUNT: i64 = 3;

/// Issuance ceilings for a merchant that has never had them raised.
///
/// These exist because merchant revenue is NOT held in escrow (DEV.md: accepted
/// risk). Nothing can claw money back once it has been withdrawn to the MC world,
/// so what is limited instead is the speed at which a shop can ask for money:
/// how many bills it may have outstanding, and how much it may bill in a day.
///
/// `*_INTENTS` are counts of bills; `*_ISSUE_CAP`/`*_ISSUE_CEILING` are amounts
/// and so are in minor units (1/100 エメ) like everything else — 5,000,000 minor
/// is 50,000 エメ a day. The v8 unit migration scaled the amounts and left the
/// counts alone, which is the whole distinction to keep in mind here: the two
/// kinds of ceiling sit side by side in the same struct and the same request body.
pub const DEFAULT_MAX_OPEN_INTENTS: i64 = 20;
pub const DEFAULT_DAILY_ISSUE_CAP: i64 = 5_000_000;
/// The most the portal will raise those to. A raise costs session + PIN, so this
/// is the ceiling on what a leaked API key could ever be talked into.
pub const MAX_OPEN_INTENTS_CEILING: i64 = 500;
pub const MAX_DAILY_ISSUE_CEILING: i64 = 200_000_000;

/// Rate limits as `(calls, window_ms)`.
///
/// Creation is read as "30 per minute" — the 24h amount cap and the open-intent
/// cap above are the real brake on a runaway merchant, and this only needs to
/// stop a loop.
pub const RL_INTENT_CREATE: (usize, i64) = (30, 60_000);
pub const RL_INTENT_READ: (usize, i64) = (120, 60_000);

/// How often an account may actually REGISTER a shop.
///
/// **Spent only when a merchant row is created** — see `portal_register`. A
/// registration has several ways to fail (name taken, name refused, wrong PIN,
/// three shops already, email not verified), and charging ten minutes for a
/// mistyped name would punish nobody but the honest user: an attempt that
/// creates no shop has not used up any of what this protects.
///
/// This is the opposite of the call the OS makes for `os.apps.launch`, where a
/// REFUSED launch is counted too. There, the refusal reason is itself the leak
/// (it tells the caller which apps are installed), so the asking has to cost
/// something. Here the only thing an attempt discloses is whether a shop name is
/// taken, and [`MAX_MERCHANTS_PER_ACCOUNT`] already bounds creation. Different
/// thing being protected, opposite answer.
pub const RL_REGISTER: (usize, i64) = (1, 10 * 60_000);

/// A short burst guard on registration ATTEMPTS, spent whether or not a shop is
/// created.
///
/// Needed precisely because [`RL_REGISTER`] is no longer spent on failure: every
/// attempt costs this process an Argon2id verification, so without a ceiling on
/// attempts a caller holding a valid session could loop deliberately-invalid
/// names and spend the wallet's CPU on PIN hashes. Loose enough that a human
/// correcting a typo never meets it.
pub const RL_REGISTER_BURST: (usize, i64) = (5, 10_000);

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

    // Every row the account still holds, disabled ones included — see
    // [`MAX_MERCHANTS_PER_ACCOUNT`] for why the `active` filter that used to be
    // here was a name-squatting hole.
    let owned: i64 = tx.query_row(
        "SELECT COUNT(*) FROM merchants WHERE owner_account_id = ?1 AND status != ?2",
        params![owner_account_id, STATUS_DELETED],
        |r| r.get(0),
    )?;
    if owned >= MAX_MERCHANTS_PER_ACCOUNT {
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
         WHERE merchant_id = ?1 AND owner_account_id = ?2 AND status != ?6",
        params![
            merchant_id,
            owner_account_id,
            api_key_hash(&api_key),
            prefix,
            now,
            STATUS_DELETED
        ],
    )?;
    Ok((changed == 1).then_some((api_key, prefix)))
}

/// Set a merchant's status. `false` when the merchant is not this account's.
///
/// A closed merchant is not reachable from here: it has already given its name
/// and its slot back, so re-opening it would produce a nameless shop holding a
/// slot nothing counted. Re-opening is registering again.
pub fn set_status(
    tx: &rusqlite::Transaction<'_>,
    merchant_id: &str,
    owner_account_id: &str,
    status: &str,
) -> rusqlite::Result<bool> {
    let changed = tx.execute(
        "UPDATE merchants SET status = ?3, updated_unix_ms = ?4 \
         WHERE merchant_id = ?1 AND owner_account_id = ?2 AND status != ?5",
        params![
            merchant_id,
            owner_account_id,
            status,
            now_ms(),
            STATUS_DELETED
        ],
    )?;
    Ok(changed == 1)
}

/// Outcome of closing a merchant for good.
#[derive(Debug, PartialEq, Eq)]
pub enum CloseOutcome {
    Ok,
    /// Not this account's, or already closed.
    NotFound,
    /// Customers are still holding bills from this shop.
    HasOpenIntents { count: i64 },
    /// Money this shop has been paid is still sitting in escrow, waiting to be
    /// released to it.
    HasEscrowedFunds { count: i64, total: i64 },
}

/// Close a merchant permanently, releasing its name and its owner's slot.
///
/// Refused in two situations, for the same reason: something of somebody else's
/// is still attached to this shop.
///
/// * **Unanswered intents.** Somebody may be looking at an approval screen for
///   this shop right now, and closing it under them would turn a payment they are
///   part-way through into `merchant_disabled` with no shop left to ask about it.
///   Cancel the bills first, or let them expire.
/// * **Unreleased escrow (v9).** A customer has already paid and MoyMoy is
///   holding the money for this shop. Closing now would leave it there with
///   nothing left to pay it to — the release sweep resolves a merchant to its
///   account, and a closed shop is exactly the row it would be resolving. Report
///   the order fulfilled (or have the payment refunded) first, and the money
///   stops being suspended.
///
/// Neither refusal loses anything; both are "finish what is outstanding".
pub fn close(
    tx: &rusqlite::Transaction<'_>,
    merchant_id: &str,
    owner_account_id: &str,
) -> rusqlite::Result<CloseOutcome> {
    let now = now_ms();
    let owned = tx
        .query_row(
            "SELECT 1 FROM merchants \
             WHERE merchant_id = ?1 AND owner_account_id = ?2 AND status != ?3",
            params![merchant_id, owner_account_id, STATUS_DELETED],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !owned {
        return Ok(CloseOutcome::NotFound);
    }
    let open: i64 = tx.query_row(
        "SELECT COUNT(*) FROM payment_intents \
         WHERE merchant_id = ?1 AND state = ?2 AND expires_unix_ms > ?3",
        params![merchant_id, crate::payments::STATE_CREATED, now],
        |r| r.get(0),
    )?;
    if open > 0 {
        return Ok(CloseOutcome::HasOpenIntents { count: open });
    }
    // Money already taken for this shop and not yet paid over to it. Counted from
    // `payment_intents` rather than from any balance, because that is where the
    // claim lives — the escrow account holds every shop's suspended money in one
    // pot, so a balance could not answer "how much of it is this shop's".
    let (held, total): (i64, i64) = tx.query_row(
        "SELECT COUNT(*), COALESCE(SUM(amount), 0) FROM payment_intents \
         WHERE merchant_id = ?1 AND escrowed_unix_ms IS NOT NULL AND released_unix_ms IS NULL",
        [merchant_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if held > 0 {
        return Ok(CloseOutcome::HasEscrowedFunds {
            count: held,
            total,
        });
    }
    // The name and the credential go at the same moment as the status: a closed
    // shop that kept its skeleton would hold a name nothing can trade under, and
    // one that kept its key hash would still authenticate.
    tx.execute(
        "UPDATE merchants SET status = ?3, name_skeleton = NULL, api_key_hash = NULL, \
                api_key_prefix = NULL, listed = 0, updated_unix_ms = ?4 \
         WHERE merchant_id = ?1 AND owner_account_id = ?2",
        params![merchant_id, owner_account_id, STATUS_DELETED, now],
    )?;
    Ok(CloseOutcome::Ok)
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
    // The floors differ because the units do: one open intent is the smallest
    // meaningful count, and one エメ (MINOR_PER_EME) is the smallest daily cap
    // worth having. A floor of `1` here would be a cap of one hundredth of an エメ
    // a day, which is indistinguishable from a shop that cannot trade at all.
    let cap = daily_issue_cap.map(|v| v.clamp(crate::wallet::MINOR_PER_EME, MAX_DAILY_ISSUE_CEILING));
    let changed = tx.execute(
        "UPDATE merchants SET max_open_intents = COALESCE(?3, max_open_intents), \
                daily_issue_cap = COALESCE(?4, daily_issue_cap), updated_unix_ms = ?5 \
         WHERE merchant_id = ?1 AND owner_account_id = ?2 AND status != ?6",
        params![
            merchant_id,
            owner_account_id,
            open,
            cap,
            now_ms(),
            STATUS_DELETED
        ],
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
        self.peek(key, limit, window_ms, now)?;
        self.record(key, window_ms, now);
        Ok(())
    }

    /// Ask whether a call would be allowed **without counting it**.
    ///
    /// Paired with [`RateLimiter::record`] by operations whose failures must not
    /// spend the allowance — see `portal_register`, where the limit is on
    /// creating shops and a mistyped name creates none.
    pub fn peek(&self, key: &str, limit: usize, window_ms: i64, now: i64) -> Result<(), i64> {
        let mut map = self.lock();
        Self::prune(&mut map, key, window_ms, now);
        let hits = map.entry(key.to_string()).or_default();
        if hits.len() >= limit {
            let oldest = hits.front().copied().unwrap_or(now);
            return Err((oldest + window_ms - now).max(1));
        }
        Ok(())
    }

    /// Count one call against `key`.
    pub fn record(&self, key: &str, window_ms: i64, now: i64) {
        let mut map = self.lock();
        Self::prune(&mut map, key, window_ms, now);
        map.entry(key.to_string()).or_default().push_back(now);
    }

    fn prune(
        map: &mut HashMap<String, VecDeque<i64>>,
        key: &str,
        window_ms: i64,
        now: i64,
    ) {
        if map.len() > 1024 {
            map.retain(|_, hits| hits.back().is_some_and(|t| *t > now - window_ms));
        }
        let hits = map.entry(key.to_string()).or_default();
        while hits.front().is_some_and(|t| *t <= now - window_ms) {
            hits.pop_front();
        }
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

// ── merchant portal (session + PIN) ──────────────────────────────────────────

/// Re-authenticate the portal caller with their PIN.
///
/// Stage 1 and 2 of the [`crate::auth`] split, without the money stage: nothing
/// under `/merchant/portal/*` moves a balance, so there is no transaction to
/// settle into and the counter is cleared on the spot.
async fn portal_pin(
    st: &AppState,
    acct: &AuthedAccount,
    pin: &str,
    policy: LockoutPolicy,
) -> Result<Option<Value>, ApiError> {
    let now = now_ms();
    if let Err(retry_after_ms) = st.pin_backoff.check(&acct.session_key, now) {
        return Ok(Some(
            json!({ "ok": false, "error": "too_many_attempts", "retry_after_ms": retry_after_ms }),
        ));
    }
    let id = acct.account_id.clone();
    let attempt = blocking(st.pool.clone(), move |conn| {
        auth::begin_pin_attempt(conn, &id, policy)
    })
    .await?;
    let (pin_hash, epoch) = match attempt {
        PinAttempt::Ready { pin_hash, epoch } => (pin_hash, epoch),
        PinAttempt::Locked { retry_after_ms } => {
            return Ok(Some(
                json!({ "ok": false, "error": "locked", "retry_after_ms": retry_after_ms }),
            ))
        }
        PinAttempt::NoPin => {
            return Ok(Some(json!({ "ok": false, "error": "invalid_pin" })));
        }
    };
    let pin = pin.to_string();
    let ok = tokio::task::spawn_blocking(move || auth::verify_pin_hash(&pin, &pin_hash)).await?;
    if !ok {
        let retry_after_ms = st.pin_backoff.record_failure(&acct.session_key, now);
        return Ok(Some(
            json!({ "ok": false, "error": "invalid_pin", "retry_after_ms": retry_after_ms }),
        ));
    }
    st.pin_backoff.clear(&acct.session_key);
    let id = acct.account_id.clone();
    blocking(st.pool.clone(), move |conn| {
        auth::clear_pin_failures(conn, &id, epoch).map_err(ApiError::from)
    })
    .await?;
    Ok(None)
}

#[derive(Deserialize)]
pub(crate) struct PortalRegisterReq {
    name: String,
    sub: Option<String>,
    glyph: Option<String>,
    pal: Option<String>,
    pin: String,
}

pub(crate) async fn portal_register(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<PortalRegisterReq>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let key = format!("mreg:{}", acct.account_id);
    // Attempts are counted here; the ten-minute allowance is not touched until a
    // shop actually exists (see the note on RL_REGISTER).
    if let Err(retry_after_ms) = st.rate.check(
        &format!("burst:{key}"),
        RL_REGISTER_BURST.0,
        RL_REGISTER_BURST.1,
        now_ms(),
    ) {
        return Ok(rate_limited(retry_after_ms));
    }
    if let Err(retry_after_ms) = st
        .rate
        .peek(&key, RL_REGISTER.0, RL_REGISTER.1, now_ms())
    {
        return Ok(rate_limited(retry_after_ms));
    }
    if let Some(refused) = portal_pin(&st, &acct, &req.pin, LockoutPolicy::Enforce).await? {
        return Ok(ok_json(refused));
    }
    let email_enabled = st.email_enabled();
    let (value, created) = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Where mail works, a shop that can take strangers' money must be
        // reachable afterwards. Where it does not (no identity token), the wallet
        // already runs handle+PIN only and this degrades with it rather than
        // making registration impossible.
        if email_enabled {
            let info = auth::account_full(&tx, &acct.account_id)?
                .ok_or_else(|| ApiError::unauthorized("account no longer exists"))?;
            if !info.email_verified {
                return Ok::<(Value, bool), ApiError>((
                    json!({ "ok": false, "error": "email_verification_required" }),
                    false,
                ));
            }
        }
        let out = register(
            &tx,
            &acct.account_id,
            &req.name,
            req.sub.as_deref(),
            req.glyph.as_deref(),
            req.pal.as_deref(),
        )?;
        let created = matches!(out, RegisterOutcome::Ok { .. });
        let v = match out {
            RegisterOutcome::Ok {
                merchant_id,
                name,
                api_key,
                api_key_prefix,
            } => json!({
                "ok": true, "merchant_id": merchant_id, "name": name,
                // Shown once. Nothing stores it, so nothing can show it again.
                "api_key": api_key, "api_key_prefix": api_key_prefix,
            }),
            RegisterOutcome::BadName(e) => {
                json!({ "ok": false, "error": "bad_name", "reason": e.code() })
            }
            RegisterOutcome::BadSub(e) => {
                json!({ "ok": false, "error": "bad_sub", "reason": e.code() })
            }
            RegisterOutcome::NameTaken => json!({ "ok": false, "error": "name_taken" }),
            RegisterOutcome::TooManyMerchants => {
                json!({ "ok": false, "error": "too_many_merchants", "limit": MAX_MERCHANTS_PER_ACCOUNT })
            }
        };
        tx.commit()?;
        Ok((v, created))
    })
    .await?;
    // Only now, with a shop committed, is the allowance spent.
    if created {
        st.rate.record(&key, RL_REGISTER.1, now_ms());
    }
    Ok(ok_json(value))
}

#[derive(Deserialize)]
pub(crate) struct PortalKeyReq {
    merchant_id: String,
    pin: String,
}

pub(crate) async fn portal_rotate_key(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<PortalKeyReq>,
) -> Result<Json<Value>, ApiError> {
    if let Some(refused) = portal_pin(&st, &acct, &req.pin, LockoutPolicy::Enforce).await? {
        return Ok(Json(refused));
    }
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let v = match rotate_key(&tx, &req.merchant_id, &acct.account_id)? {
            Some((api_key, api_key_prefix)) => {
                json!({ "ok": true, "api_key": api_key, "api_key_prefix": api_key_prefix })
            }
            None => json!({ "ok": false, "error": "unknown_merchant" }),
        };
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
pub(crate) struct PortalStatusReq {
    merchant_id: String,
    status: String,
    pin: String,
}

pub(crate) async fn portal_set_status(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<PortalStatusReq>,
) -> Result<Json<Value>, ApiError> {
    let status = match req.status.as_str() {
        STATUS_ACTIVE => STATUS_ACTIVE,
        STATUS_DISABLED => STATUS_DISABLED,
        _ => return Ok(Json(json!({ "ok": false, "error": "bad_status" }))),
    };
    // Stopping a shop is the one operation a lockout may not block. Somebody
    // whose API key is being abused, fumbling their PIN under pressure, must not
    // find that the switch which stops the bleeding is the thing they locked
    // themselves out of. The PIN is still required and the failure is still
    // recorded — and nothing here moves money.
    let policy = if status == STATUS_DISABLED {
        LockoutPolicy::Bypass
    } else {
        LockoutPolicy::Enforce
    };
    if let Some(refused) = portal_pin(&st, &acct, &req.pin, policy).await? {
        return Ok(Json(refused));
    }
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let v = if set_status(&tx, &req.merchant_id, &acct.account_id, status)? {
            json!({ "ok": true, "merchant_id": req.merchant_id, "status": status })
        } else {
            json!({ "ok": false, "error": "unknown_merchant" })
        };
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
pub(crate) struct PortalLimitsReq {
    merchant_id: String,
    pin: String,
    max_open_intents: Option<i64>,
    daily_issue_cap: Option<i64>,
}

pub(crate) async fn portal_set_limits(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<PortalLimitsReq>,
) -> Result<Json<Value>, ApiError> {
    if let Some(refused) = portal_pin(&st, &acct, &req.pin, LockoutPolicy::Enforce).await? {
        return Ok(Json(refused));
    }
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let v = if set_limits(
            &tx,
            &req.merchant_id,
            &acct.account_id,
            req.max_open_intents,
            req.daily_issue_cap,
        )? {
            let m = get(&tx, &req.merchant_id)?;
            json!({
                "ok": true,
                "max_open_intents": m.as_ref().map(|m| m.max_open_intents),
                "daily_issue_cap": m.map(|m| m.daily_issue_cap),
            })
        } else {
            json!({ "ok": false, "error": "unknown_merchant" })
        };
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
pub(crate) struct PortalCloseReq {
    merchant_id: String,
    pin: String,
}

/// Close a shop for good, giving its name and its owner's slot back.
///
/// Session + PIN like every other portal mutation. It cannot be reached with an
/// API key: a leaked key must not be able to retire the shop it belongs to.
pub(crate) async fn portal_close(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<PortalCloseReq>,
) -> Result<Json<Value>, ApiError> {
    if let Some(refused) = portal_pin(&st, &acct, &req.pin, LockoutPolicy::Enforce).await? {
        return Ok(Json(refused));
    }
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let v = match close(&tx, &req.merchant_id, &acct.account_id)? {
            CloseOutcome::Ok => {
                json!({ "ok": true, "merchant_id": req.merchant_id, "status": STATUS_DELETED })
            }
            CloseOutcome::NotFound => json!({ "ok": false, "error": "unknown_merchant" }),
            CloseOutcome::HasOpenIntents { count } => {
                json!({ "ok": false, "error": "open_intents", "count": count })
            }
            CloseOutcome::HasEscrowedFunds { count, total } => {
                json!({
                    "ok": false, "error": "escrowed_funds",
                    "count": count, "total_minor": total,
                })
            }
        };
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

pub(crate) async fn portal_list(
    State(st): State<AppState>,
    acct: AuthedAccount,
) -> Result<Json<Value>, ApiError> {
    let list = blocking(st.pool, move |conn| {
        list_for_owner(conn, &acct.account_id).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({ "ok": true, "merchants": list })))
}

// ── merchant API (Bearer moy_sk_…) ───────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct IntentCreateReq {
    idem_key: String,
    /// What to bill, in minor units (1/100 エメ) — `1250` is 12.50 エメ.
    ///
    /// Optional in the struct only so the old key below can be reported properly;
    /// [`amount_minor`](IntentCreateReq::amount_minor) refuses a request without
    /// it.
    amount_minor: Option<i64>,
    /// The pre-v8 field, which meant whole エメ. **Accepted by the parser purely
    /// in order to refuse it** — see [`amount_minor`](IntentCreateReq::amount_minor).
    amount: Option<i64>,
    description: String,
    order_ref: Option<String>,
    launch_app_id: Option<String>,
    /// Who the shop expects to pay. A hint, and a restriction: naming somebody
    /// stops anyone else from approving, but it never makes them pay.
    payer_hint_handle: Option<String>,
    expires_in_secs: Option<i64>,
}

impl IntentCreateReq {
    /// The amount to bill, or the refusal to send instead.
    ///
    /// **The rename from `amount` to `amount_minor` is the safety mechanism, not
    /// cosmetics.** Reusing `amount` with a new meaning would leave a wallet and a
    /// shop that disagree about the unit unable to notice: an integrator echoing
    /// the amount back compares its own number against itself, and one that
    /// derives the expected total from the same order row compares two values that
    /// came from the same side. Both agree while the customer is billed a hundred
    /// times too much (shop not upgraded) or a hundredth (wallet not upgraded),
    /// and the goods ship either way.
    ///
    /// A key that only the new unit uses makes the mismatch a `400` at order
    /// creation — before any money moves, in whichever direction the versions are
    /// skewed. That is worth more than the compatibility it costs.
    fn amount_minor(&self) -> Result<i64, (StatusCode, Json<Value>)> {
        match (self.amount_minor, self.amount) {
            // Whichever else is present: a caller still sending `amount` is
            // stating an amount in a unit this API no longer has.
            (_, Some(amount)) => Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": "unsupported_amount_unit",
                    "detail": format!(
                        "`amount` counted whole エメ and is no longer accepted; send \
                         `amount_minor` in 1/100 エメ instead (the {amount} you sent would be \
                         {amount}00 as amount_minor)"
                    ),
                })),
            )),
            (Some(minor), None) => Ok(minor),
            (None, None) => Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": "missing_amount_minor",
                    "detail": "`amount_minor` (1/100 エメ) is required",
                })),
            )),
        }
    }
}

pub(crate) async fn intent_create(
    State(st): State<AppState>,
    m: MerchantAuth,
    Json(req): Json<IntentCreateReq>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if req.idem_key.trim().is_empty() {
        return Err(ApiError::bad_request("idem_key required"));
    }
    // Before the rate limiter and before the idempotency replay: a caller on the
    // wrong side of the unit change must never reach a frozen success, and an
    // request this API cannot read has not used up anybody's issuance budget.
    let amount_minor = match req.amount_minor() {
        Ok(a) => a,
        Err(refused) => return Ok(refused),
    };
    if let Err(retry_after_ms) = st.rate.check(
        &format!("mint:{}", m.merchant.merchant_id),
        RL_INTENT_CREATE.0,
        RL_INTENT_CREATE.1,
        now_ms(),
    ) {
        return Ok(rate_limited(retry_after_ms));
    }
    let value = blocking(st.pool, move |conn| {
        let scope = payments::intent_scope(&m.merchant.merchant_id);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(prev) = db::idem_get(&tx, &req.idem_key, &scope)? {
            return Ok(replay(prev));
        }
        let payer_hint = match req.payer_hint_handle.as_deref().map(str::trim) {
            Some(h) if !h.is_empty() => match auth::lookup_handle(&tx, h)? {
                Some(a) => Some(a.account_id),
                // Refused rather than silently issued to nobody: a shop that
                // meant to address a customer should learn it got the handle
                // wrong before the customer is standing at the till.
                None => {
                    return Ok::<Value, ApiError>(
                        json!({ "ok": false, "error": "unknown_payer_hint" }),
                    )
                }
            },
            _ => None,
        };
        let out = payments::create(
            &tx,
            &m.merchant,
            &NewIntent {
                idem_key: &req.idem_key,
                amount: amount_minor,
                description: &req.description,
                order_ref: req.order_ref.as_deref(),
                launch_app_id: req.launch_app_id.as_deref(),
                payer_hint_account_id: payer_hint.as_deref(),
                expires_in_secs: req.expires_in_secs,
            },
        )?;
        let v = match out {
            CreateOutcome::Ok(i) => {
                // `amount_minor` on the way out as well as in: an integrator that
                // reads back `amount` gets nothing rather than a number in a unit
                // it would misread. The reply is frozen for replay, so v8 renames
                // the key inside the stored records too (schema_v8.sql §5).
                let v = json!({
                    "ok": true, "intent_id": i.intent_id, "state": i.state,
                    "amount_minor": i.amount, "expires_unix_ms": i.expires_unix_ms,
                });
                db::idem_put(&tx, &req.idem_key, &scope, &v.to_string())?;
                v
            }
            CreateOutcome::BadAmount => json!({ "ok": false, "error": "bad_amount" }),
            CreateOutcome::BadDescription(e) => {
                json!({ "ok": false, "error": "bad_description", "reason": e.code() })
            }
            CreateOutcome::BadOrderRef(e) => {
                json!({ "ok": false, "error": "bad_order_ref", "reason": e.code() })
            }
            CreateOutcome::BadTtl => json!({
                "ok": false, "error": "bad_expires_in_secs",
                "min": payments::MIN_TTL_SECS, "max": payments::MAX_TTL_SECS,
            }),
            CreateOutcome::Capped(IssueGuard::TooManyOpen { limit }) => {
                json!({ "ok": false, "error": "too_many_open_intents", "limit": limit })
            }
            CreateOutcome::Capped(IssueGuard::DailyCapExceeded { limit, issued }) => {
                json!({ "ok": false, "error": "daily_issue_cap", "limit": limit, "issued": issued })
            }
            CreateOutcome::Capped(IssueGuard::Ok) => {
                return Err(ApiError::internal("issuance guard reported Ok as a refusal"))
            }
        };
        tx.commit()?;
        Ok(v)
    })
    .await?;
    Ok(ok_json(value))
}

pub(crate) async fn intent_get(
    State(st): State<AppState>,
    m: MerchantAuth,
    Query(q): Query<IntentQuery>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if let Err(retry_after_ms) = st.rate.check(
        &format!("mget:{}", m.merchant.merchant_id),
        RL_INTENT_READ.0,
        RL_INTENT_READ.1,
        now_ms(),
    ) {
        return Ok(rate_limited(retry_after_ms));
    }
    let value = blocking(st.pool, move |conn| {
        match payments::get(conn, &q.intent_id)? {
            // Another merchant's intent reads as a missing one — the same
            // ownership discipline `op_status` uses, so this cannot be turned
            // into an oracle for other shops' order flow.
            Some(i) if i.merchant_id == m.merchant.merchant_id => Ok::<Value, ApiError>(
                json!({ "ok": true, "intent": payments::merchant_view(&m.merchant, &i, now_ms())? }),
            ),
            _ => Ok(json!({ "ok": false, "error": "unknown_intent" })),
        }
    })
    .await?;
    Ok(ok_json(value))
}

#[derive(Deserialize)]
pub(crate) struct IntentFulfillReq {
    intent_id: String,
    /// How much of the order was actually delivered, in minor units.
    ///
    /// **Optional in the struct so that ABSENT and `0` stay different things.**
    /// `0` is a legitimate report — "nothing could be delivered, return it all" —
    /// while a missing field is a caller that did not say. Declared as a bare
    /// `i64` this would be a `422` in the deserializer's words rather than a `400`
    /// in ours, and any later `#[serde(default)]` would quietly turn "did not say"
    /// into "refund everything".
    ///
    /// `_minor` is in the name for the reason `amount_minor` is: a number whose
    /// unit is carried only by convention is a number the next migration changes
    /// the meaning of without anybody noticing.
    fulfilled_amount_minor: Option<i64>,
    /// The shop's own explanation, for the wallet's log. Not stored — see
    /// [`payments::fulfill`].
    reason: Option<String>,
}

/// Report how much of a paid order was actually delivered.
///
/// **Moves no money.** [`payments::fulfill`] explains why an endpoint that takes
/// an amount from an API key holder does not breach the "a merchant credential
/// can move nothing" invariant. The payout happens later, on the release sweep,
/// once the gate has elapsed.
pub(crate) async fn intent_fulfill(
    State(st): State<AppState>,
    m: MerchantAuth,
    Json(req): Json<IntentFulfillReq>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if req.intent_id.trim().is_empty() {
        return Err(ApiError::bad_request("intent_id required"));
    }
    let Some(fulfilled) = req.fulfilled_amount_minor else {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": "missing_fulfilled_amount_minor",
                "detail": "`fulfilled_amount_minor` (1/100 エメ) is required; send 0 to report \
                           that nothing could be delivered",
            })),
        ));
    };
    let value = blocking(st.pool, move |conn| {
        let out = payments::fulfill(
            conn,
            &m.merchant.merchant_id,
            &req.intent_id,
            fulfilled,
            req.reason.as_deref(),
        )?;
        Ok::<(StatusCode, Value), ApiError>(match out {
            payments::FulfillOutcome::Ok {
                fulfilled_amount,
                refund_amount,
            } => (
                StatusCode::OK,
                json!({
                    "ok": true,
                    "intent_id": req.intent_id,
                    "state": "fulfilled",
                    "fulfilled_amount_minor": fulfilled_amount,
                    "refund_amount_minor": refund_amount,
                }),
            ),
            payments::FulfillOutcome::UnknownIntent => (
                StatusCode::OK,
                json!({ "ok": false, "error": "unknown_intent" }),
            ),
            // A conflict, not a success. A retrying integrator has to be able to
            // tell "your report was recorded" from "a report already existed",
            // because only the first decided what the customer gets back.
            payments::FulfillOutcome::AlreadyFulfilled { fulfilled_amount } => (
                StatusCode::CONFLICT,
                json!({
                    "ok": false, "error": "already_fulfilled",
                    "fulfilled_amount_minor": fulfilled_amount,
                }),
            ),
            payments::FulfillOutcome::NotHeld { stage } => (
                StatusCode::CONFLICT,
                json!({ "ok": false, "error": "not_held", "escrow_stage": stage }),
            ),
            payments::FulfillOutcome::AmountOutOfRange { amount } => (
                StatusCode::BAD_REQUEST,
                json!({
                    "ok": false, "error": "bad_fulfilled_amount",
                    "amount_minor": amount,
                    "detail": "fulfilled_amount_minor must be between 0 and the amount the \
                               customer approved",
                }),
            ),
        })
    })
    .await?;
    Ok((value.0, Json(value.1)))
}

#[derive(Deserialize)]
pub(crate) struct IntentCancelReq {
    intent_id: String,
}

pub(crate) async fn intent_cancel(
    State(st): State<AppState>,
    m: MerchantAuth,
    Json(req): Json<IntentCancelReq>,
) -> Result<Json<Value>, ApiError> {
    let value = blocking(st.pool, move |conn| {
        payments::cancel(conn, &req.intent_id, &m.merchant.merchant_id).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(value))
}


/// Too many calls. A `429` and not a `200 {ok:false}`: the wallet is refusing to
/// look at the request at all, which is an infrastructure answer — unlike
/// "insufficient" or "already_paid", which are facts about the request itself.
fn rate_limited(retry_after_ms: i64) -> (StatusCode, Json<Value>) {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "ok": false, "error": "rate_limited", "retry_after_ms": retry_after_ms })),
    )
}

/// The ordinary answer from a handler that can also rate-limit.
fn ok_json(v: Value) -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn intent_req(body: serde_json::Value) -> IntentCreateReq {
        serde_json::from_value(body).expect("the request parses")
    }

    /// The fail-closed half of the unit migration, from the wire in.
    ///
    /// A version skew between this wallet and a shop is invisible to every check
    /// either side already has — an echoed amount is compared against itself, and
    /// an expected total derived from the same order row agrees with itself. So
    /// the KEY carries the unit, and a request in the old dialect is refused
    /// before an intent exists rather than billed at 100× or 1/100.
    #[test]
    fn the_public_api_refuses_an_amount_in_the_old_unit() {
        let base = serde_json::json!({ "idem_key": "ord-1", "description": "りんご 1個" });

        let mut old = base.clone();
        old["amount"] = serde_json::json!(129);
        let (status, body) = intent_req(old).amount_minor().expect_err("`amount` was honoured");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0["error"], json!("unsupported_amount_unit"));

        // Sending both is the same refusal: a shop hedging its bets must not have
        // the wallet pick a unit for it.
        let mut both = base.clone();
        both["amount"] = serde_json::json!(129);
        both["amount_minor"] = serde_json::json!(12_900);
        let (status, body) = intent_req(both)
            .amount_minor()
            .expect_err("`amount` alongside `amount_minor` was tolerated");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0["error"], json!("unsupported_amount_unit"));

        // Neither is a request this API cannot read at all, and it says which key
        // it wants rather than defaulting to zero or to some other field.
        let (status, body) = intent_req(base.clone())
            .amount_minor()
            .expect_err("an amount-less request was accepted");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0["error"], json!("missing_amount_minor"));

        // The new dialect goes through untouched — no scaling on the way in, the
        // number IS the stored amount.
        let mut new = base;
        new["amount_minor"] = serde_json::json!(12_900);
        assert_eq!(intent_req(new).amount_minor().unwrap(), 12_900);
    }

    /// An absent `fulfilled_amount_minor` and a `0` are different statements, and
    /// the wire keeps them apart.
    ///
    /// `0` means "nothing could be delivered, refund it all" — a real report a
    /// shop makes. Absent means the caller did not say. Collapsing them (a bare
    /// `i64` with a serde default, say) would turn a malformed request into a full
    /// refund of somebody's order.
    #[test]
    fn a_missing_fulfilled_amount_is_not_a_report_of_zero() {
        let parse = |v: serde_json::Value| -> IntentFulfillReq {
            serde_json::from_value(v).expect("the request parses")
        };

        let absent = parse(serde_json::json!({ "intent_id": "pi_x" }));
        assert_eq!(absent.fulfilled_amount_minor, None);

        let zero = parse(serde_json::json!({
            "intent_id": "pi_x", "fulfilled_amount_minor": 0
        }));
        assert_eq!(zero.fulfilled_amount_minor, Some(0));

        let partial = parse(serde_json::json!({
            "intent_id": "pi_x", "fulfilled_amount_minor": 340_000,
            "reason": "2 of 3 lines undeliverable"
        }));
        assert_eq!(partial.fulfilled_amount_minor, Some(340_000));
        assert_eq!(partial.reason.as_deref(), Some("2 of 3 lines undeliverable"));

        // The old spelling is not quietly accepted: a caller sending `amount` or
        // `fulfilled_amount` has stated a number in a unit this API never names,
        // and it lands in the same place as sending nothing.
        for key in ["amount", "fulfilled_amount", "amount_minor"] {
            let req = parse(serde_json::json!({ "intent_id": "pi_x", key: 340_000 }));
            assert_eq!(req.fulfilled_amount_minor, None, "`{key}` was read as the amount");
        }
    }

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

    /// A real [`AppState`], so the registration handler can be driven exactly as
    /// axum drives it. Nothing here needs a tunnel: `portal_register` never asks
    /// `can_charge()`, and with no identity token the mailer reports disabled,
    /// which is the deployment shape that skips the email-verified requirement.
    fn app_state() -> (crate::api::AppState, AuthedAccount) {
        let pool = crate::db::open_memory().expect("in-memory pool");
        let conn = pool.get().unwrap();
        let hash = auth::hash_pin("1234").unwrap();
        auth::insert_account(&conn, "acct-m", "shopkeep", "shopkeep", "Shop", &hash, None).unwrap();
        drop(conn);
        let mc = crate::mc::McLink::new(mochi_hub_cs_sdk::CsHttpSender::default());
        let st = crate::api::AppState {
            pool: pool.clone(),
            charge: std::sync::Arc::new(crate::charge::ChargeCoordinator::new(
                pool.clone(),
                mc.clone(),
            )),
            mailer: crate::otp::Mailer::from_env(),
            attest: std::sync::Arc::new(crate::attest::AttestVerifier::new(mc)),
            challenges: std::sync::Arc::new(crate::attest::ChallengeStore::new()),
            char_sessions: std::sync::Arc::new(crate::attest::CharSessionStore::new()),
            rate: std::sync::Arc::new(RateLimiter::new()),
            pin_backoff: std::sync::Arc::new(crate::riskauth::PinBackoff::new()),
        };
        let acct = AuthedAccount {
            account_id: "acct-m".to_string(),
            phone_id: None,
            session_key: "sess-m".to_string(),
        };
        (st, acct)
    }

    async fn try_register(
        st: &crate::api::AppState,
        acct: &AuthedAccount,
        name: &str,
        pin: &str,
    ) -> Value {
        let (_, Json(v)) = portal_register(
            State(st.clone()),
            acct.clone(),
            Json(PortalRegisterReq {
                name: name.to_string(),
                sub: None,
                glyph: None,
                pal: None,
                pin: pin.to_string(),
            }),
        )
        .await
        .expect("the handler answers");
        v
    }

    /// A registration that creates no shop must not spend the shop-creation
    /// allowance. One mistyped name used to cost ten minutes.
    #[tokio::test]
    async fn a_failed_registration_does_not_block_the_next_attempt() {
        let (st, acct) = app_state();

        // Three ways to fail short of creating a row: a refused name, a name
        // that imitates another alphabet, and a wrong PIN.
        let v = try_register(&st, &acct, "MoyMoy 公式", "1234").await;
        assert_eq!(v["error"], json!("bad_name"), "{v}");
        assert_eq!(v["reason"], json!("reserved_name"), "{v}");
        let v = try_register(&st, &acct, "PiggleShoр", "1234").await; // Cyrillic er
        assert_eq!(v["reason"], json!("mixed_script"), "{v}");
        let v = try_register(&st, &acct, "Good Shop", "9999").await;
        assert_eq!(v["error"], json!("invalid_pin"), "{v}");

        // …and the honest attempt right afterwards still goes through.
        let v = try_register(&st, &acct, "Good Shop", "1234").await;
        assert_eq!(v["ok"], json!(true), "the allowance was spent on a failure: {v}");
        assert!(v["api_key"].as_str().unwrap().starts_with(API_KEY_PREFIX));
    }

    /// …but a registration that DID create a shop holds the allowance.
    #[tokio::test]
    async fn a_successful_registration_blocks_the_next_one() {
        let (st, acct) = app_state();
        let v = try_register(&st, &acct, "First Shop", "1234").await;
        assert_eq!(v["ok"], json!(true), "{v}");

        let (status, Json(v)) = portal_register(
            State(st.clone()),
            acct.clone(),
            Json(PortalRegisterReq {
                name: "Second Shop".to_string(),
                sub: None,
                glyph: None,
                pal: None,
                pin: "1234".to_string(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(v["error"], json!("rate_limited"), "{v}");
        assert!(v["retry_after_ms"].as_i64().unwrap() > 0);
        // Exactly one shop exists — the refusal created nothing.
        let n: i64 = st
            .pool
            .get()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM merchants", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn a_peek_asks_without_spending_and_record_spends() {
        let rl = RateLimiter::new();
        // Peeking never consumes, however often it is asked.
        for _ in 0..10 {
            assert!(rl.peek("k", 1, 1_000, 100).is_ok());
        }
        rl.record("k", 1_000, 100);
        assert!(rl.peek("k", 1, 1_000, 100).is_err());
        // …and the window still ages out on its own.
        assert!(rl.peek("k", 1, 1_000, 1_101).is_ok());
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
