//! Deposit-notification delivery — the read side of `notification_outbox`.
//!
//! The write side is `wallet.rs`: the two credit primitives queue a row inside
//! the SAME transaction as the balance update, so a row exists exactly when a
//! deposit committed. This module drains those rows and posts an OS
//! notification to every device the credited account is logged in on
//! (`moymoy_sessions.mochi_account_id`, registered by `POST /wallet/link`).
//!
//! Delivery is BEST-EFFORT by design, and says so instead of hiding it: the
//! ledger is the record of the deposit, a notification is only a nudge (the
//! same posture as mnn-mail's `maybe_notify_new_mail`). Failures retry with
//! bounded backoff and are then dropped with a warning — they never block or
//! fail a wallet operation, which is also why this loop, not the money paths,
//! owns every HTTP call.

use std::time::Duration;

use rusqlite::Connection;
use serde_json::json;
use uuid::Uuid;

use crate::db::{now_ms, Pool};

/// The MochiOS notifications service. It runs in-band in the Hub process on
/// this same host — moymoy-cs is always launcher-spawned next to it, and every
/// transport default in this crate is loopback — so like the band's own
/// `BAND_NOTIFICATIONS_URL` this is a const, not config (approved decision: no
/// new env knob).
const NOTIFICATIONS_URL: &str = "http://127.0.0.1:7406";
/// Poll cadence. The outbox is normally empty and the due-scan is indexed, so
/// this is one cheap SELECT every 2s — and it is also what picks up rows
/// written by the admin CLI (a separate process this loop cannot be nudged by).
const POLL: Duration = Duration::from_secs(2);
/// Rows drained per pass; a burst beyond this waits for the next tick.
const BATCH: i64 = 32;
/// Failures a row survives before it is dropped.
const MAX_ATTEMPTS: i64 = 5;
/// First retry delay; doubles per failure (5s, 10s, 20s, 40s).
const BACKOFF_BASE_MS: i64 = 5_000;

/// The app the notification is attributed to and deep-links back into.
const APP_ID: &str = "com.mochi.moymoy";
const ACTION_URI: &str = "mochi-internal://com.mochi.moymoy/index.html";

/// One due outbox row, resolved and ready to deliver.
struct Job {
    outbox_id: String,
    attempts: i64,
    /// `@handle` of the credited account (display name when it has none) — the
    /// body names the account because one device can hold several.
    holder: String,
    label: String,
    amount: i64,
    /// Mochi accounts of every live linked session (deduped by the query).
    recipients: Vec<String>,
}

/// What happened to one outbox row this pass — all [`apply_outcomes`] needs.
/// Resolve failures and delivery failures both land here (`delivered: false`),
/// deliberately on the same path: a transient error recovers on a later
/// attempt, a permanent one (the account row is gone) ages out at
/// [`MAX_ATTEMPTS`], and neither can stall the rows behind it.
struct Outcome {
    outbox_id: String,
    attempts: i64,
    delivered: bool,
}

/// Spawn the delivery loop. Without a per-process identity token the loop
/// still runs but discards due rows: the outbox must not grow without bound
/// just because delivery is unconfigured (the same degrade posture as
/// [`crate::otp::Mailer::from_env`]).
pub fn spawn(pool: Pool) {
    let token = std::env::var("MOCHI_SVC_IDENTITY_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    let sender = match token {
        Some(token) => match reqwest::Client::builder().timeout(Duration::from_secs(10)).build() {
            Ok(client) => {
                tracing::info!("deposit notifications enabled (per-process identity)");
                Some((client, token))
            }
            Err(e) => {
                tracing::warn!(error = %e, "notification HTTP client failed to build — deposit notifications disabled");
                None
            }
        },
        None => {
            tracing::info!(
                "deposit notifications disabled (no per-process identity token) — outbox rows will be discarded"
            );
            None
        }
    };
    tokio::spawn(async move {
        loop {
            if let Err(e) = pass(&pool, sender.as_ref()).await {
                tracing::warn!(error = %e, "notification delivery pass failed");
            }
            tokio::time::sleep(POLL).await;
        }
    });
}

/// One drain pass: resolve due rows (blocking hop), fan out the HTTP posts,
/// apply the outcomes (blocking hop, one transaction).
async fn pass(pool: &Pool, sender: Option<&(reqwest::Client, String)>) -> anyhow::Result<()> {
    let (jobs, mut outcomes) = {
        let pool = pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            fetch_due(&conn, now_ms())
        })
        .await??
    };
    if jobs.is_empty() && outcomes.is_empty() {
        return Ok(());
    }

    let Some((client, token)) = sender else {
        // Degrade mode: drop rather than accumulate — resolved or not. Debug,
        // not warn: the disabled state was announced once at startup.
        let ids: Vec<String> = jobs
            .iter()
            .map(|j| j.outbox_id.clone())
            .chain(outcomes.iter().map(|o| o.outbox_id.clone()))
            .collect();
        tracing::debug!(count = ids.len(), "delivery disabled — discarding due notification rows");
        let pool = pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            delete_rows(&conn, &ids)
        })
        .await??;
        return Ok(());
    };

    for job in jobs {
        let delivered = deliver(client, token, &job).await;
        outcomes.push(Outcome {
            outbox_id: job.outbox_id,
            attempts: job.attempts,
            delivered,
        });
    }
    let pool = pool.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get()?;
        apply_outcomes(&conn, &outcomes, now_ms())
    })
    .await??;
    Ok(())
}

/// The due rows, oldest first: the ones that resolved (with recipients and
/// holder, in the same connection checkout) and, separately, the ones that did
/// NOT resolve — already shaped as failed [`Outcome`]s.
///
/// Resolution is isolated PER ROW on purpose: one row whose account read
/// errors (vanished row, transient I/O) must not fail the pass — that would
/// skip `apply_outcomes`, leave its `attempts` untouched, and let the head of
/// the queue jam every row behind it for ever. Failing soft here puts the row
/// on the same backoff-then-age-out path as a delivery failure.
fn fetch_due(conn: &Connection, now: i64) -> anyhow::Result<(Vec<Job>, Vec<Outcome>)> {
    let rows: Vec<(String, String, String, i64, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT outbox_id, account_id, label, amount, attempts \
             FROM notification_outbox WHERE next_attempt_unix_ms <= ?1 \
             ORDER BY created_unix_ms ASC LIMIT ?2",
        )?;
        let v = stmt
            .query_map(rusqlite::params![now, BATCH], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };
    let mut jobs = Vec::new();
    let mut failed = Vec::new();
    for (outbox_id, account_id, label, amount, attempts) in rows {
        match resolve(conn, &account_id, now) {
            Ok((holder, recipients)) => jobs.push(Job {
                outbox_id,
                attempts,
                holder,
                label,
                amount,
                recipients,
            }),
            Err(e) => {
                tracing::debug!(error = %e, outbox_id = %outbox_id,
                    "outbox row failed to resolve — scheduling it like a delivery failure");
                failed.push(Outcome {
                    outbox_id,
                    attempts,
                    delivered: false,
                });
            }
        }
    }
    Ok((jobs, failed))
}

/// Holder and recipients for one credited account — the per-row half of
/// [`fetch_due`], separated so its `?`s stop at the row boundary.
fn resolve(conn: &Connection, account_id: &str, now: i64) -> anyhow::Result<(String, Vec<String>)> {
    Ok((
        holder_label(conn, account_id)?,
        recipients(conn, account_id, now)?,
    ))
}

/// Every device (Mochi account) a live session of `account_id` has linked.
/// "Logged-in devices" is exactly this query: logout deleted its row, expiry
/// is filtered here, never-linked sessions are NULL, and DISTINCT collapses
/// re-logins from the same device.
fn recipients(conn: &Connection, account_id: &str, now: i64) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT mochi_account_id FROM moymoy_sessions \
         WHERE account_id = ?1 AND mochi_account_id IS NOT NULL AND expires_unix_ms > ?2",
    )?;
    let v = stmt
        .query_map(rusqlite::params![account_id, now], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(v)
}

/// `@handle`, or the display name for the handle-less (who cannot log in and so
/// have no linked devices anyway — this is completeness, not a real path).
fn holder_label(conn: &Connection, account_id: &str) -> anyhow::Result<String> {
    let (handle, display): (Option<String>, Option<String>) = conn.query_row(
        "SELECT handle, display_name FROM accounts WHERE account_id = ?1",
        [account_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(handle
        .filter(|h| !h.is_empty())
        .map(|h| format!("@{h}"))
        .unwrap_or_else(|| display.unwrap_or_default()))
}

/// POST one notification per linked device; `true` only when every recipient
/// accepted. A partial failure retries the WHOLE row later, which can repeat a
/// banner on devices that already got one — accepted, because per-recipient
/// delivery state is more machinery than a best-effort nudge deserves.
async fn deliver(client: &reqwest::Client, token: &str, job: &Job) -> bool {
    let mut all_ok = true;
    for recipient in &job.recipients {
        let body = json!({
            "account_id": recipient,
            "notification": {
                "id": Uuid::new_v4().to_string(),
                "app_id": APP_ID,
                "title": "入金",
                // `job.amount` is minor units, so it is rendered rather than
                // printed: the raw integer would announce every deposit at a
                // hundred times its value.
                "body": format!("{}: {} +{} エメ", job.holder, job.label,
                                crate::wallet::format_eme(job.amount)),
                "ts_unix_ms": now_ms(),
                "action_uri": ACTION_URI,
                "category": "wallet",
                "content_available": false,
            },
        });
        // `.json()` sets the Content-Type the service requires; the bearer is
        // this process's identity token and is never logged.
        let sent = client
            .post(format!("{NOTIFICATIONS_URL}/notifications"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await;
        match sent {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                tracing::debug!(status = %resp.status(), outbox_id = %job.outbox_id, "notification POST refused");
                all_ok = false;
            }
            Err(e) => {
                tracing::debug!(error = %e, outbox_id = %job.outbox_id, "notification POST failed");
                all_ok = false;
            }
        }
    }
    all_ok
}

/// Delete delivered rows; back off failed ones, dropping them at
/// [`MAX_ATTEMPTS`]. The drop is the designed end of the best-effort contract
/// (the service is down, refusing us, or the row cannot resolve any more), not
/// a swallowed error — hence the warning, and hence no refund-like
/// compensation: the ledger already holds the deposit this row failed to
/// announce.
///
/// The whole pass commits as ONE transaction: up to [`BATCH`] rows take the
/// write lock once, instead of each DELETE/UPDATE opening its own implicit
/// write transaction and elbowing the wallet's `BEGIN IMMEDIATE` up to 32
/// times per tick.
fn apply_outcomes(conn: &Connection, outcomes: &[Outcome], now: i64) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    for o in outcomes {
        if o.delivered {
            tx.execute("DELETE FROM notification_outbox WHERE outbox_id = ?1", [&o.outbox_id])?;
            continue;
        }
        let attempts = o.attempts + 1;
        if attempts >= MAX_ATTEMPTS {
            tracing::warn!(outbox_id = %o.outbox_id, attempts, "notification undeliverable — dropping");
            tx.execute("DELETE FROM notification_outbox WHERE outbox_id = ?1", [&o.outbox_id])?;
        } else {
            tx.execute(
                "UPDATE notification_outbox SET attempts = ?2, next_attempt_unix_ms = ?3 \
                 WHERE outbox_id = ?1",
                rusqlite::params![o.outbox_id, attempts, now + (BACKOFF_BASE_MS << (attempts - 1))],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Batch delete (degrade mode) — one transaction, same locking rationale as
/// [`apply_outcomes`].
fn delete_rows(conn: &Connection, ids: &[String]) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    for id in ids {
        tx.execute("DELETE FROM notification_outbox WHERE outbox_id = ?1", [id])?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::PooledConn;
    use rusqlite::OptionalExtension;

    /// A wallet with one account (`acct-a`, handle `alice`) on an in-memory DB.
    fn wallet() -> PooledConn {
        let pool = crate::db::open_memory().expect("in-memory pool");
        let conn = pool.get().expect("checkout");
        conn.execute(
            "INSERT INTO accounts (account_id, handle, handle_lower, display_name, balance, \
               created_unix_ms, updated_unix_ms) \
             VALUES ('acct-a', 'alice', 'alice', 'Alice', 0, 0, 0)",
            [],
        )
        .unwrap();
        conn
    }

    fn session(conn: &Connection, id: &str, mochi: Option<&str>, expires: i64) {
        conn.execute(
            "INSERT INTO moymoy_sessions \
               (session_id, account_id, token_hash, mochi_account_id, created_unix_ms, \
                last_seen_unix_ms, expires_unix_ms) \
             VALUES (?1, 'acct-a', ?1, ?2, 0, 0, ?3)",
            rusqlite::params![id, mochi, expires],
        )
        .unwrap();
    }

    fn queue(conn: &Connection, id: &str, created: i64, next_attempt: i64) {
        conn.execute(
            "INSERT INTO notification_outbox \
               (outbox_id, account_id, kind, label, amount, created_unix_ms, next_attempt_unix_ms) \
             VALUES (?1, 'acct-a', 'receive', 'Bob から受取', 40, ?2, ?3)",
            rusqlite::params![id, created, next_attempt],
        )
        .unwrap();
    }

    fn attempts_of(conn: &Connection, id: &str) -> Option<i64> {
        conn.query_row(
            "SELECT attempts FROM notification_outbox WHERE outbox_id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    #[test]
    fn recipients_are_live_linked_sessions_deduped_by_device() {
        let conn = wallet();
        const NOW: i64 = 1_000;
        session(&conn, "s-linked", Some("m-1"), NOW + 1); // counts
        session(&conn, "s-relogin", Some("m-1"), NOW + 1); // same device again ⇒ one entry
        session(&conn, "s-expired", Some("m-2"), NOW - 1); // logged out by time
        session(&conn, "s-unlinked", None, NOW + 1); // never registered a device

        assert_eq!(recipients(&conn, "acct-a", NOW).unwrap(), vec!["m-1"]);
    }

    #[test]
    fn fetch_due_takes_only_due_rows_oldest_first_with_the_holder_resolved() {
        let conn = wallet();
        const NOW: i64 = 1_000;
        session(&conn, "s1", Some("m-1"), NOW + 1);
        queue(&conn, "o-later", 2, NOW); // due, younger
        queue(&conn, "o-early", 1, NOW); // due, older
        queue(&conn, "o-backoff", 0, NOW + 1); // not due yet

        let (jobs, failed) = fetch_due(&conn, NOW).unwrap();
        assert!(failed.is_empty());
        assert_eq!(
            jobs.iter().map(|j| j.outbox_id.as_str()).collect::<Vec<_>>(),
            vec!["o-early", "o-later"]
        );
        assert_eq!(jobs[0].holder, "@alice");
        assert_eq!(jobs[0].recipients, vec!["m-1"]);
        assert_eq!((jobs[0].label.as_str(), jobs[0].amount), ("Bob から受取", 40));
    }

    /// M1: a row whose account row is gone resolves to a soft failure that ages
    /// out on the normal backoff path — it must not error the pass, and it must
    /// not stall the healthy rows behind it.
    #[test]
    fn a_row_whose_account_vanished_fails_soft_and_ages_out_without_stalling_the_queue() {
        let conn = wallet();
        const NOW: i64 = 1_000;
        session(&conn, "s1", Some("m-1"), NOW + 1);
        // Manufacture the orphan the FK would normally prevent — the case this
        // guards is a defensive one (nothing deletes accounts today).
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute(
            "INSERT INTO notification_outbox \
               (outbox_id, account_id, kind, label, amount, created_unix_ms) \
             VALUES ('o-ghost', 'acct-ghost', 'receive', 'x', 1, 1)",
            [],
        )
        .unwrap();
        queue(&conn, "o-ok", 2, 0);

        let (jobs, failed) = fetch_due(&conn, NOW).unwrap();
        // The healthy, younger row still resolves and would deliver…
        assert_eq!(
            jobs.iter().map(|j| j.outbox_id.as_str()).collect::<Vec<_>>(),
            vec!["o-ok"]
        );
        // …while the orphan is shaped exactly like a delivery failure.
        assert_eq!(failed.len(), 1);
        assert_eq!(
            (failed[0].outbox_id.as_str(), failed[0].delivered),
            ("o-ghost", false)
        );

        // Driven round the same backoff path, it consumes attempts and drains.
        let mut attempts = failed[0].attempts;
        let mut rounds = 0;
        loop {
            apply_outcomes(
                &conn,
                &[Outcome {
                    outbox_id: "o-ghost".into(),
                    attempts,
                    delivered: false,
                }],
                NOW,
            )
            .unwrap();
            match attempts_of(&conn, "o-ghost") {
                Some(a) => attempts = a,
                None => break,
            }
            rounds += 1;
            assert!(rounds <= MAX_ATTEMPTS, "the orphan never drained");
        }
        // The healthy row was never touched by any of it.
        assert_eq!(attempts_of(&conn, "o-ok"), Some(0));
    }

    #[test]
    fn a_failure_backs_off_and_the_last_one_drops_the_row() {
        let conn = wallet();
        const NOW: i64 = 1_000;
        queue(&conn, "o-1", 1, 0);
        let outcome = |attempts| Outcome {
            outbox_id: "o-1".into(),
            attempts,
            delivered: false,
        };

        // First failure: still present, pushed into the future.
        apply_outcomes(&conn, &[outcome(0)], NOW).unwrap();
        let (attempts, due): (i64, i64) = conn
            .query_row(
                "SELECT attempts, next_attempt_unix_ms FROM notification_outbox WHERE outbox_id = 'o-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(attempts, 1);
        assert_eq!(due, NOW + BACKOFF_BASE_MS);

        // Final failure: the row is gone (the ledger, not the outbox, is the record).
        apply_outcomes(&conn, &[outcome(MAX_ATTEMPTS - 1)], NOW).unwrap();
        assert_eq!(attempts_of(&conn, "o-1"), None);
    }

    #[test]
    fn a_delivered_row_is_deleted() {
        let conn = wallet();
        queue(&conn, "o-1", 1, 0);
        apply_outcomes(
            &conn,
            &[Outcome {
                outbox_id: "o-1".into(),
                attempts: 0,
                delivered: true,
            }],
            1_000,
        )
        .unwrap();
        assert_eq!(attempts_of(&conn, "o-1"), None);
    }
}
