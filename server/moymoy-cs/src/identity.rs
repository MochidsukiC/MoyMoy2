//! Account read helpers + UUID/card-face utilities shared by the wallet & auth
//! layers.
//!
//! Since the v2 redesign an account is an **independent MoyMoy account**
//! (handle + PIN — created by [`crate::auth`]); `account_id` is a server-generated
//! UUID, no longer a Minecraft UUID.
//!
//! There is no account ↔ Minecraft-character mapping here any more. Schema v5
//! dropped `account_mc_links`: which character a request may spend is decided per
//! request by a Hub-signed assertion ([`crate::attest`]), not by a table this
//! module wrote from an earlier self-asserted gameUuid.

use rusqlite::{Connection, OptionalExtension};
use uuid::Uuid;

/// One account row as the wallet consumes it (balance + card face + labels).
#[derive(Debug, Clone)]
pub struct Account {
    pub account_id: String,
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub balance: i64,
    pub holder: String,
    pub card_number: String,
    pub card_expiry: String,
    pub is_merchant: bool,
}

impl Account {
    /// Best human label: friendly name, else `@handle`, else a generic.
    pub fn label(&self) -> String {
        self.display_name
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.handle.clone().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "プレイヤー".to_string())
    }
}

/// Canonicalize a UUID string to lowercase hyphenated form, or `None` if it is
/// not a UUID.
pub fn normalize_uuid(s: &str) -> Option<String> {
    Uuid::parse_str(s.trim())
        .ok()
        .map(|u| u.hyphenated().to_string())
}

/// The Minecraft *offline-mode* UUID for `name`
/// (`UUID.nameUUIDFromBytes("OfflinePlayer:" + name)` — an MD5 v3 name UUID).
/// Used to give demo merchants stable backing-account ids.
pub fn offline_uuid(name: &str) -> Uuid {
    let digest = md5::compute(format!("OfflinePlayer:{name}").as_bytes());
    let mut b = digest.0; // [u8; 16]
    b[6] = (b[6] & 0x0f) | 0x30; // version 3
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    Uuid::from_bytes(b)
}

/// A stable, cosmetic 16-digit "card number" (grouped in 4s) derived from the
/// account id — purely for the card face in the design, never an identifier.
pub fn card_number_for(account_id: &str) -> String {
    let digest = md5::compute(account_id.as_bytes());
    let mut n: u64 = 0;
    for b in &digest.0[..8] {
        n = n.wrapping_mul(131).wrapping_add(u64::from(*b));
    }
    let span = 5_000_000_000_000_000u64;
    let sixteen = 5_000_000_000_000_000u64 + (n % span);
    let s = format!("{sixteen:016}");
    format!("{} {} {} {}", &s[0..4], &s[4..8], &s[8..12], &s[12..16])
}

/// Map a DB row to an [`Account`].
fn row_to_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        account_id: row.get("account_id")?,
        handle: row.get("handle")?,
        display_name: row.get("display_name")?,
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
        "SELECT account_id, handle, display_name, balance, holder, card_number, card_expiry, is_merchant \
         FROM accounts WHERE account_id = ?1",
        [account_id],
        row_to_account,
    )
    .optional()
}
