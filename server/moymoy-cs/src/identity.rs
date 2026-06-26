//! Player identity & account provisioning.
//!
//! An account is keyed by the canonical Minecraft UUID. The app supplies it from
//! `mochi.os.gameUuid()`; when only a name is known (offline servers, name-only
//! transfers) we derive the Mojang *offline* UUID exactly as the server does
//! (`UUID.nameUUIDFromBytes("OfflinePlayer:" + name)`), so the two paths key the
//! same row.

use rusqlite::{Connection, OptionalExtension};
use uuid::Uuid;

use crate::db::now_ms;

/// One wallet account row (includes the cosmetic card face from the design).
#[derive(Debug, Clone)]
pub struct Account {
    pub account_id: String,
    pub mcid: Option<String>,
    pub balance: i64,
    pub holder: String,
    pub card_number: String,
    pub card_expiry: String,
    pub is_merchant: bool,
}

/// Canonicalize a UUID string to lowercase hyphenated form, or `None` if it is
/// not a UUID.
pub fn normalize_uuid(s: &str) -> Option<String> {
    Uuid::parse_str(s.trim()).ok().map(|u| u.hyphenated().to_string())
}

/// The Minecraft *offline-mode* UUID for `name`:
/// `UUID.nameUUIDFromBytes("OfflinePlayer:" + name)` — an MD5 (version-3) name
/// UUID. Lets a name-only request resolve to the same account a UUID request
/// would on an offline server.
pub fn offline_uuid(name: &str) -> Uuid {
    let digest = md5::compute(format!("OfflinePlayer:{name}").as_bytes());
    let mut b = digest.0; // [u8; 16]
    b[6] = (b[6] & 0x0f) | 0x30; // version 3
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    Uuid::from_bytes(b)
}

/// Resolve a request's identity to a canonical account_id: prefer a valid
/// `mc_uuid`, else derive the offline UUID from `mcid`. Returns `None` if neither
/// is usable.
pub fn resolve_account_id(mc_uuid: Option<&str>, mcid: Option<&str>) -> Option<String> {
    if let Some(u) = mc_uuid.and_then(normalize_uuid) {
        return Some(u);
    }
    mcid.map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .map(|n| offline_uuid(n).hyphenated().to_string())
}

/// A stable, cosmetic 16-digit "card number" (grouped in 4s) derived from the
/// account UUID — purely for the card face in the design, never an identifier.
pub fn card_number_for(account_id: &str) -> String {
    let digest = md5::compute(account_id.as_bytes());
    let mut n: u64 = 0;
    for b in &digest.0[..8] {
        n = n.wrapping_mul(131).wrapping_add(u64::from(*b));
    }
    // 16 digits in 5000_0000_0000_0000 ..= 9999_9999_9999_9999 (card-like leading digit).
    let span = 5_000_000_000_000_000u64;
    let sixteen = 5_000_000_000_000_000u64 + (n % span);
    let s = format!("{sixteen:016}");
    format!("{} {} {} {}", &s[0..4], &s[4..8], &s[8..12], &s[12..16])
}

/// Map a DB row to an [`Account`].
fn row_to_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        account_id: row.get("account_id")?,
        mcid: row.get("mcid")?,
        balance: row.get("balance")?,
        holder: row.get("holder")?,
        card_number: row.get("card_number")?,
        card_expiry: row.get("card_expiry")?,
        is_merchant: row.get::<_, i64>("is_merchant")? != 0,
    })
}

/// Fetch an account by id, if it exists.
pub fn get(conn: &Connection, account_id: &str) -> rusqlite::Result<Option<Account>> {
    conn.query_row(
        "SELECT account_id, mcid, balance, holder, card_number, card_expiry, is_merchant \
         FROM accounts WHERE account_id = ?1",
        [account_id],
        row_to_account,
    )
    .optional()
}

/// Get the account for `account_id`, creating it (zero balance, derived card
/// face) on first sight. Refreshes `mcid`/`holder` when a newer name is supplied.
/// Must be called inside the caller's transaction when atomicity with a balance
/// change is required.
pub fn get_or_create(
    conn: &Connection,
    account_id: &str,
    mcid: Option<&str>,
) -> rusqlite::Result<Account> {
    if let Some(mut acct) = get(conn, account_id)? {
        // Keep the display name fresh when the client reports a (newer) name.
        if let Some(name) = mcid.map(str::trim).filter(|n| !n.is_empty()) {
            if acct.mcid.as_deref() != Some(name) {
                let now = now_ms();
                conn.execute(
                    "UPDATE accounts SET mcid = ?2, holder = ?3, updated_unix_ms = ?4 \
                     WHERE account_id = ?1",
                    rusqlite::params![account_id, name, holder_from(name), now],
                )?;
                acct.mcid = Some(name.to_string());
                acct.holder = holder_from(name);
            }
        }
        return Ok(acct);
    }

    let now = now_ms();
    let holder = mcid.map(holder_from).unwrap_or_else(|| "PLAYER".to_string());
    let card = card_number_for(account_id);
    conn.execute(
        "INSERT INTO accounts \
           (account_id, mcid, balance, holder, card_number, card_expiry, is_merchant, created_unix_ms, updated_unix_ms) \
         VALUES (?1, ?2, 0, ?3, ?4, '07/29', 0, ?5, ?5)",
        rusqlite::params![account_id, mcid, holder, card, now],
    )?;
    Ok(Account {
        account_id: account_id.to_string(),
        mcid: mcid.map(str::to_string),
        balance: 0,
        holder,
        card_number: card,
        card_expiry: "07/29".to_string(),
        is_merchant: false,
    })
}

/// The card-face holder string for a player name (uppercased, ASCII-ish display).
fn holder_from(name: &str) -> String {
    name.to_uppercase()
}
