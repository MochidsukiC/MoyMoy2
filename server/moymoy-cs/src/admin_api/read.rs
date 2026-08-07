//! The reads themselves. Every function here is a pure `&Connection -> rows`
//! query with no authority of its own: nothing in this file writes, and nothing
//! in it moves money.
//!
//! ## Why these queries are written out rather than reused
//!
//! The portal reads (`merchant::list_for_owner`, `payments::sales_page`) all
//! carry `WHERE owner_account_id = ?` / `WHERE merchant_id = ?`, because in the
//! portal a shop that is not yours reads as a shop that does not exist. An
//! operator view is the opposite question — "every shop, including the ones
//! nobody is watching" — so reusing those would mean loosening a filter that is
//! load-bearing everywhere else it appears. They stay as they are and the
//! cross-cutting queries live here.
//!
//! ## Amounts
//!
//! Every amount is an integer in minor units (1/100 エメ) and reaches JSON as an
//! integer. Nothing here divides, and no amount is ever put through an `f64`
//! (DEV.md 金額の単位): a balance is a sum, and binary floating point cannot add
//! decimal fractions exactly.

use std::collections::HashMap;

use rusqlite::{params_from_iter, Connection, OptionalExtension, ToSql};
use serde::Serialize;

use crate::merchant::{self, STATUS_ACTIVE, STATUS_DELETED, STATUS_DISABLED};
use crate::payments::{self, STATE_PAID};
use crate::wallet;

/// The window the overview reports activity over.
///
/// A ROLLING 24 hours, not a calendar day, and the JSON says `last_24h_*` for
/// that reason. It is the same window `merchant::check_issuance` already means by
/// "daily", so the console and the issuance ceilings cannot disagree about which
/// payments are recent — and it needs no timezone, which this process does not
/// have an opinion about.
const WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// Default / maximum page sizes. Same shape as `payments::SALES_*`: a page an
/// operator asked for is bounded, and the response says when it was cut.
pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;

fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

// ── the derived values ───────────────────────────────────────────────────────
//
// These four are the whole reason this module exists, and they are DERIVED on
// every read rather than stored. DEV.md 加盟店売上ページ: "実在する金（エスクロー
// 口座）を追う2つ目の数字を作ると、いつか食い違う" — the escrow account holds one
// pot for every shop at once, so a per-merchant escrow column would be a second
// number claiming to track money that only one number actually tracks.
//
// Each is a single `GROUP BY merchant_id` pass rather than a query per shop, and
// each `COALESCE`s its sum: the v9 migration stamped `released_unix_ms` on the
// pre-v9 `paid` rows while leaving `fulfilled_amount` NULL, so a shop whose only
// settlements predate v9 sums nothing at all.

/// Money this shop has taken that MoyMoy is still holding: escrowed, not
/// released, and not parked. **Parked rows are excluded here on purpose** — they
/// are waiting for a person, not for the release sweep, and folding them in
/// would report money as "on its way" when the only thing that can move it is an
/// operator refund.
const SQL_PENDING: &str = "SELECT merchant_id, COALESCE(SUM(amount), 0) FROM payment_intents \
     WHERE escrowed_unix_ms IS NOT NULL AND released_unix_ms IS NULL \
       AND escrow_parked_unix_ms IS NULL \
     GROUP BY merchant_id";

/// The same set of rows, but the ones the no-report deadline set aside.
const SQL_PARKED: &str = "SELECT merchant_id, COALESCE(SUM(amount), 0) FROM payment_intents \
     WHERE escrowed_unix_ms IS NOT NULL AND released_unix_ms IS NULL \
       AND escrow_parked_unix_ms IS NOT NULL \
     GROUP BY merchant_id";

/// What escrow has actually paid out to this shop, ever. `fulfilled_amount` and
/// not `amount`: the shortfall on a partly-fulfilled order went back to the
/// buyer and was never the shop's.
const SQL_SETTLED: &str = "SELECT merchant_id, COALESCE(SUM(fulfilled_amount), 0) \
     FROM payment_intents WHERE released_unix_ms IS NOT NULL \
     GROUP BY merchant_id";

/// How many payments this shop has taken.
const SQL_PAID_COUNT: &str = "SELECT merchant_id, COUNT(*) FROM payment_intents \
     WHERE state = ?1 GROUP BY merchant_id";

/// Run one of the `GROUP BY merchant_id` aggregates above into a lookup.
fn by_merchant(
    conn: &Connection,
    sql: &str,
    args: &[&dyn ToSql],
) -> rusqlite::Result<HashMap<String, i64>> {
    let mut stmt = conn.prepare(sql)?;
    let mut out = HashMap::new();
    let mut rows = stmt.query(args)?;
    while let Some(r) = rows.next()? {
        out.insert(r.get::<_, String>(0)?, r.get::<_, i64>(1)?);
    }
    Ok(out)
}

// ── overview ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct MerchantCounts {
    pub active: i64,
    pub disabled: i64,
    pub deleted: i64,
    /// Every row, including any status the three above do not name — so a shop
    /// can never go missing from the total by having a status this build has not
    /// heard of.
    pub total: i64,
}

#[derive(Debug, Serialize)]
pub struct Overview {
    /// What the escrow account actually holds, read straight off the account row.
    /// This is the real money; every per-merchant figure below is derived.
    pub escrow_balance_minor: i64,
    /// So the console can point at the same row in `/admin/api/accounts` without
    /// having to know how the id is derived.
    pub escrow_account_id: String,
    pub merchants: MerchantCounts,
    /// Payments taken in the last [`WINDOW_MS`], keyed on `escrowed_unix_ms` —
    /// the instant the payer was debited, stamped in the same transaction as the
    /// transfer and never rewritten afterwards. Deliberately NOT
    /// `SUM(transactions.amount) WHERE kind = 'pay'`: `pay` also labels the
    /// escrow→merchant release, the escrow→payer shortfall and the operator
    /// refund, so summing it would count one purchase up to three times.
    pub last_24h_paid_total_minor: i64,
    pub last_24h_paid_count: i64,
    /// Intents a person has to decide about: escrowed, never released, and set
    /// aside by the no-report deadline. The ONLY source for this is
    /// `payment_intents` — `transactions` has no state column, because a ledger
    /// row is by definition a movement that already happened.
    pub parked_count: i64,
    pub parked_total_minor: i64,
    pub generated_unix_ms: i64,
}

pub fn overview(conn: &Connection, now: i64) -> rusqlite::Result<Overview> {
    let escrow_account_id = wallet::escrow_account_id();
    let escrow_balance_minor = wallet::balance(conn, escrow_account_id)?;

    let mut counts = MerchantCounts {
        active: 0,
        disabled: 0,
        deleted: 0,
        total: 0,
    };
    {
        let mut stmt = conn.prepare("SELECT status, COUNT(*) FROM merchants GROUP BY status")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let (status, n): (String, i64) = (r.get(0)?, r.get(1)?);
            match status.as_str() {
                STATUS_ACTIVE => counts.active = n,
                STATUS_DISABLED => counts.disabled = n,
                STATUS_DELETED => counts.deleted = n,
                _ => {}
            }
            counts.total += n;
        }
    }

    let (last_24h_paid_total_minor, last_24h_paid_count) = conn.query_row(
        "SELECT COALESCE(SUM(amount), 0), COUNT(*) FROM payment_intents \
         WHERE escrowed_unix_ms IS NOT NULL AND escrowed_unix_ms > ?1",
        [now - WINDOW_MS],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?;

    let (parked_count, parked_total_minor) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(amount), 0) FROM payment_intents \
         WHERE escrowed_unix_ms IS NOT NULL AND released_unix_ms IS NULL \
           AND escrow_parked_unix_ms IS NOT NULL",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?;

    Ok(Overview {
        escrow_balance_minor,
        escrow_account_id: escrow_account_id.to_string(),
        merchants: counts,
        last_24h_paid_total_minor,
        last_24h_paid_count,
        parked_count,
        parked_total_minor,
        generated_unix_ms: now,
    })
}

// ── merchants ────────────────────────────────────────────────────────────────

/// Every shop, with the three derived figures beside it.
///
/// No `escrow_minor` column exists on `merchants` and none is added: see the
/// comment above [`SQL_PENDING`].
#[derive(Debug, Serialize)]
pub struct MerchantAdminView {
    pub merchant_id: String,
    pub name: String,
    pub sub: Option<String>,
    /// `active` | `disabled` | `deleted` — the whole vocabulary. There is no
    /// "under review" state in this schema, so none is reported.
    pub status: String,
    pub listed: bool,
    pub account_id: String,
    /// The shop's own balance: released takings that have not been withdrawn.
    /// Separate from everything below, which is still escrow's.
    pub account_balance_minor: i64,
    pub owner_account_id: Option<String>,
    pub owner_handle: Option<String>,
    pub api_key_prefix: Option<String>,
    pub api_key_last_used_unix_ms: Option<i64>,
    pub max_open_intents: i64,
    pub daily_issue_cap: i64,
    /// Derived — escrowed, unreleased, unparked.
    pub pending_settlement_minor: i64,
    /// Derived — escrowed, unreleased, parked.
    pub parked_minor: i64,
    /// Derived — `SUM(fulfilled_amount)` over released intents.
    pub settled_total_minor: i64,
    /// Derived — `COUNT(*)` over `state = 'paid'`.
    pub paid_count: i64,
    pub created_unix_ms: i64,
}

/// **Every** merchant. No `owner_account_id` predicate — that is the difference
/// between this and `merchant::list_for_owner`, and it is the point.
pub fn merchants(conn: &Connection) -> rusqlite::Result<Vec<MerchantAdminView>> {
    let pending = by_merchant(conn, SQL_PENDING, &[])?;
    let parked = by_merchant(conn, SQL_PARKED, &[])?;
    let settled = by_merchant(conn, SQL_SETTLED, &[])?;
    let paid_count = by_merchant(conn, SQL_PAID_COUNT, &[&STATE_PAID])?;

    let mut stmt = conn.prepare(
        "SELECT m.merchant_id, m.name, m.sub, m.status, m.listed, m.account_id, \
                COALESCE(shop.balance, 0), m.owner_account_id, owner.handle, \
                m.api_key_prefix, m.api_key_last_used_unix_ms, \
                m.max_open_intents, m.daily_issue_cap, m.created_unix_ms \
         FROM merchants m \
         LEFT JOIN accounts shop  ON shop.account_id  = m.account_id \
         LEFT JOIN accounts owner ON owner.account_id = m.owner_account_id \
         ORDER BY m.created_unix_ms ASC, m.rowid ASC",
    )?;
    let mut out = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let merchant_id: String = r.get(0)?;
        out.push(MerchantAdminView {
            name: r.get(1)?,
            sub: r.get(2)?,
            status: r.get(3)?,
            listed: r.get::<_, i64>(4)? != 0,
            account_id: r.get(5)?,
            account_balance_minor: r.get(6)?,
            owner_account_id: r.get(7)?,
            owner_handle: r.get::<_, Option<String>>(8)?.filter(|h| !h.is_empty()),
            api_key_prefix: r.get(9)?,
            api_key_last_used_unix_ms: r.get(10)?,
            max_open_intents: r
                .get::<_, Option<i64>>(11)?
                .unwrap_or(merchant::DEFAULT_MAX_OPEN_INTENTS),
            daily_issue_cap: r
                .get::<_, Option<i64>>(12)?
                .unwrap_or(merchant::DEFAULT_DAILY_ISSUE_CAP),
            created_unix_ms: r.get(13)?,
            pending_settlement_minor: pending.get(&merchant_id).copied().unwrap_or(0),
            parked_minor: parked.get(&merchant_id).copied().unwrap_or(0),
            settled_total_minor: settled.get(&merchant_id).copied().unwrap_or(0),
            paid_count: paid_count.get(&merchant_id).copied().unwrap_or(0),
            merchant_id,
        });
    }
    Ok(out)
}

// ── ledger ───────────────────────────────────────────────────────────────────

/// The five kinds `transactions.kind` actually holds. There is no sixth, and
/// there is no state column: a ledger row is a movement that already happened.
pub const LEDGER_KINDS: [&str; 5] = ["pay", "send", "receive", "charge", "withdraw"];

#[derive(Debug, Serialize)]
pub struct LedgerRow {
    pub id: String,
    pub account_id: String,
    /// NULL for the non-login rows (the escrow account, the pre-v6 demo shops).
    pub account_handle: Option<String>,
    pub account_display_name: Option<String>,
    pub kind: String,
    pub label: String,
    pub counterparty_id: Option<String>,
    pub counterparty_name: Option<String>,
    /// Signed, from `account_id`'s side: negative is money leaving.
    pub amount_minor: i64,
    pub balance_after_minor: i64,
    pub memo: Option<String>,
    pub ts_unix_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct LedgerPage {
    pub rows: Vec<LedgerRow>,
    /// Feed back as `before` for the next page; `null` at the end. Opaque on
    /// purpose — see [`Cursor`].
    pub next_before: Option<String>,
    pub truncated: bool,
}

/// A page boundary as `<ts_unix_ms>:<rowid>`.
///
/// The timestamp alone is not enough to page with, and the failure is silent
/// either way: `ts < before` drops every row that shares the boundary
/// millisecond with the last one on the page, and `ts <= before` repeats them
/// forever. `transactions` is append-only, so `rowid` is a stable tiebreak, and
/// the pair matches the `… DESC, rowid DESC` ordering DEV.md already settled on
/// for the sales page for exactly this reason.
///
/// The API emits it and the client hands it back untouched, so there is still
/// one cursor parameter and clients never have to know how it is put together.
#[derive(Debug, Clone, Copy)]
pub struct Cursor {
    pub ts_unix_ms: i64,
    pub rowid: i64,
}

impl Cursor {
    pub fn encode(&self) -> String {
        format!("{}:{}", self.ts_unix_ms, self.rowid)
    }

    /// `None` for anything that is not a cursor this API emitted. A malformed
    /// cursor is refused by the handler rather than quietly treated as "start
    /// from the top", which would loop a paging client back to page one.
    pub fn parse(s: &str) -> Option<Cursor> {
        let (ts, rowid) = s.split_once(':')?;
        Some(Cursor {
            ts_unix_ms: ts.parse().ok()?,
            rowid: rowid.parse().ok()?,
        })
    }
}

/// Wrap `q` as a `%…%` LIKE pattern with the wildcards in it neutralised.
///
/// Without this a search for `_` or `%` matches every row, which on an operator
/// console reads as "the filter did nothing" rather than as a mistake. `\` is
/// the `ESCAPE` character and so escapes itself.
fn like_pattern(q: &str) -> String {
    let mut out = String::with_capacity(q.len() + 2);
    out.push('%');
    for ch in q.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

/// Every account's rows in one stream, newest first.
///
/// `kind`: one of [`LEDGER_KINDS`], or `all`. **With no `kind` at all, `receive`
/// is folded out** — every transfer writes one, so leaving them in shows each
/// send and each payment twice, once from each side. `wallet::history` makes the
/// same choice for the same reason; `kind=receive` and `kind=all` still reach
/// them.
///
/// **The `LIMIT` bounds the answer, not the work.** `idx_tx_account_time` leads
/// on `account_id`, and this query has no account to lead with, so
/// `EXPLAIN QUERY PLAN` is `SCAN t` + `USE TEMP B-TREE FOR ORDER BY`: every row
/// in `transactions` is read and sorted on each call, page 1 and page 40 alike.
/// That is affordable for an operator opening a console and is not affordable
/// for a UI polling it; an index on `(ts_unix_ms DESC)` is what would fix it,
/// and adding one is a schema migration rather than a change here.
pub fn ledger(
    conn: &Connection,
    q: Option<&str>,
    kind: Option<&str>,
    limit: i64,
    before: Option<Cursor>,
) -> rusqlite::Result<LedgerPage> {
    let limit = clamp_limit(Some(limit));
    let mut sql = String::from(
        "SELECT t.rowid, t.id, t.account_id, a.handle, a.display_name, t.kind, t.label, \
                t.counterparty_id, t.counterparty_name, t.amount, t.balance_after, t.memo, \
                t.ts_unix_ms \
         FROM transactions t LEFT JOIN accounts a ON a.account_id = t.account_id WHERE 1 = 1",
    );
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();

    match kind {
        Some("all") => {}
        Some(k) => {
            sql.push_str(" AND t.kind = ?");
            args.push(Box::new(k.to_string()));
        }
        None => sql.push_str(" AND t.kind <> 'receive'"),
    }

    if let Some(c) = before {
        sql.push_str(" AND (t.ts_unix_ms < ? OR (t.ts_unix_ms = ? AND t.rowid < ?))");
        args.push(Box::new(c.ts_unix_ms));
        args.push(Box::new(c.ts_unix_ms));
        args.push(Box::new(c.rowid));
    }

    if let Some(needle) = q {
        // The columns an operator has to search by: the ledger id they were given,
        // either side of the movement, and the human-readable text.
        const COLS: [&str; 7] = [
            "t.id",
            "t.account_id",
            "t.label",
            "t.counterparty_id",
            "t.counterparty_name",
            "t.memo",
            "a.handle",
        ];
        let pattern = like_pattern(needle);
        let clause = COLS
            .iter()
            .map(|c| format!("{c} LIKE ? ESCAPE '\\'"))
            .collect::<Vec<_>>()
            .join(" OR ");
        sql.push_str(&format!(" AND ({clause})"));
        for _ in COLS {
            args.push(Box::new(pattern.clone()));
        }
    }

    // One past the page, so `truncated` states a fact rather than a guess.
    sql.push_str(" ORDER BY t.ts_unix_ms DESC, t.rowid DESC LIMIT ?");
    args.push(Box::new(limit + 1));

    let mut stmt = conn.prepare(&sql)?;
    let mut cursors: Vec<Cursor> = Vec::new();
    let mut rows = Vec::new();
    let mut q_rows = stmt.query(params_from_iter(args.iter()))?;
    while let Some(r) = q_rows.next()? {
        cursors.push(Cursor {
            rowid: r.get(0)?,
            ts_unix_ms: r.get(12)?,
        });
        rows.push(LedgerRow {
            id: r.get(1)?,
            account_id: r.get(2)?,
            account_handle: r.get::<_, Option<String>>(3)?.filter(|h| !h.is_empty()),
            account_display_name: r.get::<_, Option<String>>(4)?.filter(|h| !h.is_empty()),
            kind: r.get(5)?,
            label: r.get(6)?,
            counterparty_id: r.get(7)?,
            counterparty_name: r.get(8)?,
            amount_minor: r.get(9)?,
            balance_after_minor: r.get(10)?,
            memo: r.get(11)?,
            ts_unix_ms: r.get(12)?,
        });
    }

    let truncated = rows.len() as i64 > limit;
    if truncated {
        rows.truncate(limit as usize);
        cursors.truncate(limit as usize);
    }
    let next_before = if truncated {
        cursors.last().map(Cursor::encode)
    } else {
        None
    };
    Ok(LedgerPage {
        rows,
        next_before,
        truncated,
    })
}

// ── intents ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct IntentAdminView {
    pub intent_id: String,
    pub merchant_id: String,
    pub merchant_name: Option<String>,
    pub amount_minor: i64,
    pub description: String,
    pub order_ref: Option<String>,
    /// As stored. `effective_state` is what to show — an intent whose deadline
    /// passed is already unpayable whether or not the 30-second sweep has
    /// rewritten the row yet (`Intent::effective_state`).
    pub state: String,
    pub effective_state: String,
    /// Straight from [`payments::escrow_stage`] — this module does not
    /// re-implement the classification, it calls it.
    pub stage: &'static str,
    pub payer_account_id: Option<String>,
    pub payer_handle: Option<String>,
    pub escrowed_unix_ms: Option<i64>,
    pub release_due_unix_ms: Option<i64>,
    pub escrow_deadline_unix_ms: Option<i64>,
    pub fulfilled_unix_ms: Option<i64>,
    pub fulfilled_amount_minor: Option<i64>,
    pub fulfil_reason: Option<String>,
    pub parked_unix_ms: Option<i64>,
    pub released_unix_ms: Option<i64>,
    pub refunded_unix_ms: Option<i64>,
    pub created_unix_ms: i64,
    pub expires_unix_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct IntentPage {
    pub intents: Vec<IntentAdminView>,
    pub truncated: bool,
}

/// The stages [`payments::escrow_stage`] can return.
pub const STAGES: [&str; 5] = ["none", "held", "fulfilled", "parked", "released"];

/// An index-friendly SUPERSET of the rows that can be in `stage`.
///
/// A superset and not an exact match, deliberately: the classification lives in
/// `payments::escrow_stage` and every row this returns is put through it below.
/// Writing the precedence out in SQL as well would be a second copy of the rule,
/// free to drift from the first — and the one that decides what a shop is told
/// would not be the one deciding what the operator is shown.
fn stage_prefilter(stage: &str) -> &'static str {
    match stage {
        "none" => " AND i.escrowed_unix_ms IS NULL",
        "held" | "fulfilled" | "parked" => {
            " AND i.escrowed_unix_ms IS NOT NULL AND i.released_unix_ms IS NULL"
        }
        "released" => " AND i.released_unix_ms IS NOT NULL",
        _ => "",
    }
}

/// Intents across every shop, newest first, optionally narrowed to one stage.
pub fn intents(
    conn: &Connection,
    stage: Option<&str>,
    limit: i64,
    now: i64,
) -> rusqlite::Result<IntentPage> {
    let limit = clamp_limit(Some(limit));
    let sql = format!(
        "SELECT i.intent_id, m.name, p.handle FROM payment_intents i \
         LEFT JOIN merchants m ON m.merchant_id = i.merchant_id \
         LEFT JOIN accounts  p ON p.account_id  = i.payer_account_id \
         WHERE 1 = 1{} ORDER BY i.created_unix_ms DESC, i.rowid DESC",
        stage.map(stage_prefilter).unwrap_or("")
    );

    // No `LIMIT` in the statement. The page is cut after the stage filter, so a
    // SQL limit would cut BEFORE it and could hand back a short page while
    // claiming there is more.
    //
    // Nothing is saved by that beyond the per-row `payments::get` below, and it
    // is worth being accurate about which: `EXPLAIN QUERY PLAN` on this
    // statement is `SCAN i` + `USE TEMP B-TREE FOR ORDER BY`. No index leads on
    // `created_unix_ms` (`idx_intents_merchant_time` leads on `merchant_id`,
    // `idx_intents_state_expiry` on `state`), so SQLite reads the table and
    // sorts it before yielding a single row — the early break stops the point
    // lookups, not the scan. `stage=parked` gets no help either: the one partial
    // index that mentions parking, `idx_intents_release_due`, is defined
    // `WHERE … escrow_parked_unix_ms IS NULL` and so excludes exactly the rows
    // that query wants. See [`ledger`] for the same shape, and DEV.md for the
    // index this would need before anything polls it.
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    let mut truncated = false;
    while let Some(r) = rows.next()? {
        let intent_id: String = r.get(0)?;
        let merchant_name: Option<String> = r.get(1)?;
        let payer_handle: Option<String> = r.get::<_, Option<String>>(2)?.filter(|h| !h.is_empty());
        // A point lookup per row so the row → `Intent` mapping stays the one in
        // `payments`. At `MAX_LIMIT` rows on an operator endpoint that is cheap,
        // and it is cheaper than a second copy of the column list here that could
        // fall out of step with the escrow fields it feeds.
        let Some(intent) = payments::get(conn, &intent_id)? else {
            continue;
        };
        let computed = payments::escrow_stage(&intent);
        if let Some(want) = stage {
            if computed != want {
                continue;
            }
        }
        if out.len() as i64 >= limit {
            truncated = true;
            break;
        }
        out.push(IntentAdminView {
            merchant_name,
            payer_handle,
            stage: computed,
            effective_state: intent.effective_state(now).to_string(),
            intent_id: intent.intent_id,
            merchant_id: intent.merchant_id,
            amount_minor: intent.amount,
            description: intent.description,
            order_ref: intent.order_ref,
            state: intent.state,
            payer_account_id: intent.payer_account_id,
            escrowed_unix_ms: intent.escrowed_unix_ms,
            release_due_unix_ms: intent.release_due_unix_ms,
            escrow_deadline_unix_ms: intent.escrow_deadline_unix_ms,
            fulfilled_unix_ms: intent.fulfilled_unix_ms,
            fulfilled_amount_minor: intent.fulfilled_amount,
            fulfil_reason: intent.fulfil_reason,
            parked_unix_ms: intent.escrow_parked_unix_ms,
            released_unix_ms: intent.released_unix_ms,
            refunded_unix_ms: intent.refunded_unix_ms,
            created_unix_ms: intent.created_unix_ms,
            expires_unix_ms: intent.expires_unix_ms,
        });
    }
    Ok(IntentPage {
        intents: out,
        truncated,
    })
}

// ── accounts ─────────────────────────────────────────────────────────────────

/// The cosmetic card face, the same three fields the app's own card shows.
#[derive(Debug, Serialize)]
pub struct CardFace {
    pub holder: String,
    pub number: String,
    pub expiry: String,
}

/// A wallet account as an operator sees it.
///
/// There is no `frozen` field and no held/attached balance, because there is no
/// such column and no such mechanism: a console that displayed one would be
/// showing a state the wallet cannot be in.
#[derive(Debug, Serialize)]
pub struct AccountAdminView {
    pub account_id: String,
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub mcid: Option<String>,
    pub balance_minor: i64,
    pub is_merchant: bool,
    pub card: CardFace,
    pub created_unix_ms: i64,
    pub updated_unix_ms: i64,
}

/// Every wallet account, biggest balance first.
///
/// The escrow account is in here like any other row rather than filtered out:
/// it holds real money, and an account view that hides the account holding the
/// most of it is worse than one that shows it. `Overview::escrow_account_id`
/// says which row it is.
///
/// Columns are listed one by one and never `SELECT *`. `pin_hash`,
/// `failed_pin_attempts`, `email`, `email_lower` and `email_verified_unix_ms`
/// live in this table too, and none of them is something an operator console
/// needs in order to answer "whose money is where".
pub fn accounts(
    conn: &Connection,
    q: Option<&str>,
    limit: i64,
) -> rusqlite::Result<(Vec<AccountAdminView>, bool)> {
    let limit = clamp_limit(Some(limit));
    let mut sql = String::from(
        "SELECT account_id, handle, display_name, mcid, balance, is_merchant, \
                holder, card_number, card_expiry, created_unix_ms, updated_unix_ms \
         FROM accounts WHERE 1 = 1",
    );
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some(needle) = q {
        let pattern = like_pattern(needle);
        sql.push_str(
            " AND (account_id LIKE ? ESCAPE '\\' OR handle LIKE ? ESCAPE '\\' \
                   OR display_name LIKE ? ESCAPE '\\' OR mcid LIKE ? ESCAPE '\\')",
        );
        for _ in 0..4 {
            args.push(Box::new(pattern.clone()));
        }
    }
    sql.push_str(" ORDER BY balance DESC, account_id ASC LIMIT ?");
    args.push(Box::new(limit + 1));

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push(AccountAdminView {
            account_id: r.get(0)?,
            handle: r.get::<_, Option<String>>(1)?.filter(|h| !h.is_empty()),
            display_name: r.get::<_, Option<String>>(2)?.filter(|h| !h.is_empty()),
            mcid: r.get(3)?,
            balance_minor: r.get(4)?,
            is_merchant: r.get::<_, i64>(5)? != 0,
            card: CardFace {
                holder: r.get(6)?,
                number: r.get(7)?,
                expiry: r.get(8)?,
            },
            created_unix_ms: r.get(9)?,
            updated_unix_ms: r.get(10)?,
        });
    }
    let truncated = out.len() as i64 > limit;
    if truncated {
        out.truncate(limit as usize);
    }
    Ok((out, truncated))
}

/// Look up a single intent the operator named, for the console's detail view.
pub fn intent_by_id(
    conn: &Connection,
    intent_id: &str,
    now: i64,
) -> rusqlite::Result<Option<IntentAdminView>> {
    let Some(intent) = payments::get(conn, intent_id)? else {
        return Ok(None);
    };
    let merchant_name = conn
        .query_row(
            "SELECT name FROM merchants WHERE merchant_id = ?1",
            [&intent.merchant_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let payer_handle = match &intent.payer_account_id {
        Some(id) => conn
            .query_row(
                "SELECT handle FROM accounts WHERE account_id = ?1",
                [id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .filter(|h| !h.is_empty()),
        None => None,
    };
    Ok(Some(IntentAdminView {
        merchant_name,
        payer_handle,
        stage: payments::escrow_stage(&intent),
        effective_state: intent.effective_state(now).to_string(),
        intent_id: intent.intent_id,
        merchant_id: intent.merchant_id,
        amount_minor: intent.amount,
        description: intent.description,
        order_ref: intent.order_ref,
        state: intent.state,
        payer_account_id: intent.payer_account_id,
        escrowed_unix_ms: intent.escrowed_unix_ms,
        release_due_unix_ms: intent.release_due_unix_ms,
        escrow_deadline_unix_ms: intent.escrow_deadline_unix_ms,
        fulfilled_unix_ms: intent.fulfilled_unix_ms,
        fulfilled_amount_minor: intent.fulfilled_amount,
        fulfil_reason: intent.fulfil_reason,
        parked_unix_ms: intent.escrow_parked_unix_ms,
        released_unix_ms: intent.released_unix_ms,
        refunded_unix_ms: intent.refunded_unix_ms,
        created_unix_ms: intent.created_unix_ms,
        expires_unix_ms: intent.expires_unix_ms,
    }))
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Pool;
    use rusqlite::params;

    const NOW: i64 = 1_700_000_000_000;

    /// Two shops owned by two DIFFERENT accounts, so any query that quietly
    /// scopes itself to one owner shows up as a missing shop rather than as a
    /// number that is merely wrong.
    ///
    /// Rows are inserted directly rather than driven through `payments::create`
    /// / `approve`: the expectations below are hand-computed, and they are only
    /// worth something if the inputs are stated here in full.
    fn fixture() -> Pool {
        let pool = crate::db::open_memory().expect("in-memory pool");
        let conn = pool.get().expect("checkout");
        wallet::seed_escrow_account(&conn).expect("escrow account");

        for (id, handle, balance) in [
            ("acct-a", "alice", 10_000_i64),
            ("acct-b", "bob", 500),
            ("acct-shop1", "shopone", 250),
            ("acct-shop2", "shoptwo", 0),
        ] {
            conn.execute(
                "INSERT INTO accounts (account_id, handle, handle_lower, display_name, balance, \
                   holder, card_number, card_expiry, is_merchant, created_unix_ms, updated_unix_ms) \
                 VALUES (?1, ?2, ?2, ?2, ?3, 'HOLDER', '0000', '07/29', 0, ?4, ?4)",
                params![id, handle, balance, NOW],
            )
            .unwrap();
        }
        // Two owners, one shop each.
        for (mid, acct, owner, name) in [
            ("m-one", "acct-shop1", "acct-a", "Shop One"),
            ("m-two", "acct-shop2", "acct-b", "Shop Two"),
        ] {
            conn.execute(
                "INSERT INTO merchants (merchant_id, account_id, owner_account_id, name, status, \
                   listed, created_unix_ms, updated_unix_ms) \
                 VALUES (?1, ?2, ?3, ?4, 'active', 1, ?5, ?5)",
                params![mid, acct, owner, name, NOW],
            )
            .unwrap();
        }
        drop(conn);
        pool
    }

    /// Insert an intent with the escrow columns stated explicitly.
    #[allow(clippy::too_many_arguments)]
    fn intent(
        conn: &Connection,
        id: &str,
        merchant_id: &str,
        amount: i64,
        state: &str,
        escrowed: Option<i64>,
        parked: Option<i64>,
        released: Option<i64>,
        fulfilled: Option<i64>,
        fulfilled_amount: Option<i64>,
    ) {
        conn.execute(
            "INSERT INTO payment_intents (intent_id, merchant_id, amount, description, state, \
               payer_account_id, idem_key, created_unix_ms, updated_unix_ms, expires_unix_ms, \
               escrowed_unix_ms, escrow_parked_unix_ms, released_unix_ms, fulfilled_unix_ms, \
               fulfilled_amount) \
             VALUES (?1, ?2, ?3, 'test', ?4, 'acct-a', ?1, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id,
                merchant_id,
                amount,
                state,
                NOW,
                NOW + 600_000,
                escrowed,
                parked,
                released,
                fulfilled,
                fulfilled_amount
            ],
        )
        .unwrap();
    }

    /// The world every derived-value assertion below is computed against.
    ///
    /// Shop One: 1200 + 3400 held, 5000 parked, one released order that was
    /// fulfilled for 900 of its 1000, and one declined intent that never moved.
    /// Shop Two: 700 held only.
    fn seed_intents(conn: &Connection) {
        // held (escrowed, not parked, not released) → 精算待ち
        intent(conn, "pi-1", "m-one", 1_200, STATE_PAID, Some(NOW), None, None, None, None);
        intent(conn, "pi-2", "m-one", 3_400, STATE_PAID, Some(NOW), None, None, None, None);
        // parked → NOT 精算待ち
        intent(
            conn, "pi-3", "m-one", 5_000, STATE_PAID, Some(NOW), Some(NOW + 1), None, None, None,
        );
        // released, fulfilled 900 of 1000 → 累計精算
        intent(
            conn,
            "pi-4",
            "m-one",
            1_000,
            STATE_PAID,
            Some(NOW),
            None,
            Some(NOW + 2),
            Some(NOW + 1),
            Some(900),
        );
        // never paid → in none of the three
        intent(conn, "pi-5", "m-one", 9_999, "declined", None, None, None, None, None);
        // fulfilled but not yet released → still 精算待ち, stage `fulfilled`
        intent(
            conn, "pi-6", "m-two", 700, STATE_PAID, Some(NOW), None, None, Some(NOW + 1), Some(700),
        );
    }

    #[test]
    fn derived_values_match_hand_computed_totals() {
        let pool = fixture();
        let conn = pool.get().unwrap();
        seed_intents(&conn);

        let all = merchants(&conn).unwrap();
        let one = all.iter().find(|m| m.merchant_id == "m-one").unwrap();
        let two = all.iter().find(|m| m.merchant_id == "m-two").unwrap();

        // 精算待ち: pi-1 + pi-2 only. pi-3 is parked, pi-4 is released, pi-5 was
        // never escrowed.
        assert_eq!(one.pending_settlement_minor, 1_200 + 3_400);
        // 保留: pi-3 alone, and it is NOT in the figure above.
        assert_eq!(one.parked_minor, 5_000);
        // 累計精算: pi-4's fulfilled_amount, not its amount.
        assert_eq!(one.settled_total_minor, 900);
        // 取引件数: every `paid` intent — pi-1..pi-4. pi-5 is `declined`.
        assert_eq!(one.paid_count, 4);

        // A fulfilled-but-unreleased order is still money MoyMoy holds.
        assert_eq!(two.pending_settlement_minor, 700);
        assert_eq!(two.parked_minor, 0);
        assert_eq!(two.settled_total_minor, 0);
        assert_eq!(two.paid_count, 1);
    }

    /// The specific confusion the money is at risk from: a parked intent counted
    /// as pending would tell an operator the release sweep is going to pay it
    /// out, when nothing will.
    #[test]
    fn parked_never_counts_as_pending_settlement() {
        let pool = fixture();
        let conn = pool.get().unwrap();
        // One shop, one intent, and it is parked.
        intent(
            &conn, "pi-p", "m-one", 4_242, STATE_PAID, Some(NOW), Some(NOW + 1), None, None, None,
        );
        let one = merchants(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.merchant_id == "m-one")
            .unwrap();
        assert_eq!(one.pending_settlement_minor, 0);
        assert_eq!(one.parked_minor, 4_242);

        // And the overview's 要対応 counts it from `payment_intents`, which is
        // the only place that knows — `transactions` has no state to read.
        let ov = overview(&conn, NOW + 10).unwrap();
        assert_eq!(ov.parked_count, 1);
        assert_eq!(ov.parked_total_minor, 4_242);
    }

    #[test]
    fn every_escrow_stage_is_classified_from_payments() {
        let pool = fixture();
        let conn = pool.get().unwrap();
        seed_intents(&conn);

        let stage_of = |id: &str| {
            intent_by_id(&conn, id, NOW + 10)
                .unwrap()
                .expect("intent")
                .stage
        };
        assert_eq!(stage_of("pi-1"), "held");
        assert_eq!(stage_of("pi-3"), "parked");
        assert_eq!(stage_of("pi-4"), "released");
        assert_eq!(stage_of("pi-5"), "none");
        assert_eq!(stage_of("pi-6"), "fulfilled");

        // And the filter returns exactly those rows, for every stage there is.
        for stage in STAGES {
            let page = intents(&conn, Some(stage), MAX_LIMIT, NOW + 10).unwrap();
            assert!(
                page.intents.iter().all(|i| i.stage == stage),
                "stage={stage} leaked a row of another stage"
            );
        }
        let parked = intents(&conn, Some("parked"), MAX_LIMIT, NOW + 10).unwrap();
        assert_eq!(parked.intents.len(), 1);
        assert_eq!(parked.intents[0].intent_id, "pi-3");
        // Every intent is reachable with no filter at all.
        assert_eq!(intents(&conn, None, MAX_LIMIT, NOW + 10).unwrap().intents.len(), 6);
    }

    /// The cross-cutting queries must NOT be scoped the way the portal's are.
    #[test]
    fn cross_cutting_reads_are_not_scoped_to_one_owner() {
        let pool = fixture();
        let conn = pool.get().unwrap();
        seed_intents(&conn);

        // Both shops, owned by different accounts, in one list.
        let all = merchants(&conn).unwrap();
        assert_eq!(all.len(), 2);
        let owners: Vec<_> = all.iter().filter_map(|m| m.owner_account_id.clone()).collect();
        assert!(owners.contains(&"acct-a".to_string()));
        assert!(owners.contains(&"acct-b".to_string()));

        // Compare against the owner-scoped portal read, which sees one each.
        assert_eq!(merchant::list_for_owner(&conn, "acct-a").unwrap().len(), 1);
        assert_eq!(merchant::list_for_owner(&conn, "acct-b").unwrap().len(), 1);

        // Intents from both shops, in one page.
        let page = intents(&conn, None, MAX_LIMIT, NOW + 10).unwrap();
        assert!(page.intents.iter().any(|i| i.merchant_id == "m-one"));
        assert!(page.intents.iter().any(|i| i.merchant_id == "m-two"));

        // And every account, including the escrow account.
        let (accts, _) = accounts(&conn, None, MAX_LIMIT).unwrap();
        assert_eq!(accts.len(), 5);
        assert!(accts
            .iter()
            .any(|a| a.account_id == wallet::escrow_account_id()));
    }

    /// `receive` is written on the far side of every transfer, so leaving it in
    /// by default would show each movement twice.
    #[test]
    fn receive_is_folded_out_unless_asked_for() {
        let pool = fixture();
        let mut conn = pool.get().unwrap();
        {
            let tx = conn.transaction().unwrap();
            let out = wallet::transfer(&tx, "acct-a", "acct-b", 250, "send", "@bob へ送金").unwrap();
            assert!(matches!(out, wallet::TxResult::Ok { .. }), "{out:?}");
            tx.commit().unwrap();
        }
        // One transfer wrote two rows: the sender's `send` and the receiver's
        // `receive`.
        let raw: i64 = conn
            .query_row("SELECT COUNT(*) FROM transactions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, 2);

        let default = ledger(&conn, None, None, MAX_LIMIT, None).unwrap();
        assert_eq!(default.rows.len(), 1);
        assert_eq!(default.rows[0].kind, "send");

        // `all` and an explicit `kind=receive` still reach it.
        assert_eq!(ledger(&conn, None, Some("all"), MAX_LIMIT, None).unwrap().rows.len(), 2);
        let only = ledger(&conn, None, Some("receive"), MAX_LIMIT, None).unwrap();
        assert_eq!(only.rows.len(), 1);
        assert_eq!(only.rows[0].account_id, "acct-b");
    }

    /// Paging has to survive several rows sharing a millisecond, which a
    /// timestamp-only cursor cannot.
    #[test]
    fn cursor_pages_through_a_shared_millisecond() {
        let pool = fixture();
        let conn = pool.get().unwrap();
        for i in 0..5 {
            conn.execute(
                "INSERT INTO transactions (id, account_id, kind, label, amount, balance_after, \
                   ts_unix_ms) VALUES (?1, 'acct-a', 'send', 'x', -1, 0, ?2)",
                params![format!("tx-{i}"), NOW],
            )
            .unwrap();
        }
        let mut seen = Vec::new();
        let mut before = None;
        loop {
            let page = ledger(&conn, None, None, 2, before).unwrap();
            seen.extend(page.rows.iter().map(|r| r.id.clone()));
            match page.next_before {
                Some(c) => before = Cursor::parse(&c),
                None => break,
            }
            assert!(seen.len() <= 5, "cursor is repeating rows: {seen:?}");
        }
        seen.sort();
        assert_eq!(seen, vec!["tx-0", "tx-1", "tx-2", "tx-3", "tx-4"]);
    }

    #[test]
    fn overview_reports_escrow_and_the_rolling_window() {
        let pool = fixture();
        let conn = pool.get().unwrap();
        seed_intents(&conn);
        conn.execute(
            "UPDATE accounts SET balance = 6600 WHERE account_id = ?1",
            [wallet::escrow_account_id()],
        )
        .unwrap();
        // Older than the window: must not be counted.
        intent(
            &conn,
            "pi-old",
            "m-one",
            8_888,
            STATE_PAID,
            Some(NOW - WINDOW_MS - 1),
            None,
            None,
            None,
            None,
        );

        let ov = overview(&conn, NOW).unwrap();
        assert_eq!(ov.escrow_balance_minor, 6_600);
        // Pinned to the literal, not to `wallet::escrow_account_id()` — comparing
        // the field against the function it is set from would assert nothing.
        // This id IS the account: it is derived from the constant
        // `"moymoy:escrow"`, so a change to that derivation silently points the
        // console at a different, empty account. The plan document quotes this
        // same value as the console's `ESCROW_ID`.
        assert_eq!(
            ov.escrow_account_id,
            "2da695f8-34e5-342a-83db-797835052201"
        );
        assert_eq!(ov.merchants.active, 2);
        assert_eq!(ov.merchants.total, 2);
        // pi-1..pi-4 and pi-6 were escrowed inside the window; pi-5 never was and
        // pi-old is too old.
        assert_eq!(ov.last_24h_paid_count, 5);
        assert_eq!(
            ov.last_24h_paid_total_minor,
            1_200 + 3_400 + 5_000 + 1_000 + 700
        );
    }

    /// A `%` typed into the search box is a `%`, not "match everything".
    #[test]
    fn like_wildcards_in_a_query_are_escaped() {
        let pool = fixture();
        let conn = pool.get().unwrap();
        for (id, label) in [("tx-a", "100% wool"), ("tx-b", "plain")] {
            conn.execute(
                "INSERT INTO transactions (id, account_id, kind, label, amount, balance_after, \
                   ts_unix_ms) VALUES (?1, 'acct-a', 'send', ?2, -1, 0, ?3)",
                params![id, label, NOW],
            )
            .unwrap();
        }
        let hit = ledger(&conn, Some("100%"), None, MAX_LIMIT, None).unwrap();
        assert_eq!(hit.rows.len(), 1);
        assert_eq!(hit.rows[0].id, "tx-a");

        // A bare `%` is the character, not the wildcard: it finds the row that
        // literally contains one and leaves the other alone. Unescaped it would
        // match BOTH rows, which is the failure this is here to catch — the
        // filter appearing to work while doing nothing.
        let bare = ledger(&conn, Some("%"), None, MAX_LIMIT, None).unwrap();
        assert_eq!(bare.rows.len(), 1, "unescaped `%` matched every row");
        assert_eq!(bare.rows[0].id, "tx-a");
        // `_` is the single-character wildcard, and no label here contains one.
        assert_eq!(ledger(&conn, Some("_"), None, MAX_LIMIT, None).unwrap().rows.len(), 0);
    }

    /// The account view must not carry the columns that sit next to it in the
    /// same table.
    #[test]
    fn account_view_omits_credentials() {
        let pool = fixture();
        let conn = pool.get().unwrap();
        let (accts, _) = accounts(&conn, Some("alice"), MAX_LIMIT).unwrap();
        assert_eq!(accts.len(), 1);
        let json = serde_json::to_value(&accts[0]).unwrap();
        for leaked in ["pin_hash", "email", "email_lower", "failed_pin_attempts"] {
            assert!(json.get(leaked).is_none(), "{leaked} reached the JSON");
        }
        assert_eq!(json["balance_minor"], 10_000);
    }
}
