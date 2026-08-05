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
/// Delivery failures a row survives before it is dropped.
const MAX_ATTEMPTS: i64 = 5;
/// First retry delay; doubles per failure (5s, 10s, 20s, 40s).
const BACKOFF_BASE_MS: i64 = 5_000;

/// The app the notification is attributed to and deep-links back into.
const APP_ID: &str = "com.mochi.moymoy";
const ACTION_URI: &str = "mochi-internal://com.mochi.moymoy/index.html";

/// One due outbox row, with everything delivery needs already resolved.
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
/// apply the outcomes (blocking hop).
async fn pass(pool: &Pool, sender: Option<&(reqwest::Client, String)>) -> anyhow::Result<()> {
    let jobs = {
        let pool = pool.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Job>> {
            let conn = pool.get()?;
            fetch_due(&conn, now_ms())
        })
        .await??
    };
    if jobs.is_empty() {
        return Ok(());
    }

    let Some((client, token)) = sender else {
        // Degrade mode: drop rather than accumulate. Debug, not warn — the
        // disabled state was announced once at startup.
        tracing::debug!(count = jobs.len(), "delivery disabled — discarding due notification rows");
        let ids: Vec<String> = jobs.into_iter().map(|j| j.outbox_id).collect();
        let pool = pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            delete_rows(&conn, &ids)
        })
        .await??;
        return Ok(());
    };

    let mut outcomes = Vec::with_capacity(jobs.len());
    for job in jobs {
        let delivered = deliver(client, token, &job).await;
        outcomes.push((job, delivered));
    }
    let pool = pool.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get()?;
        apply_outcomes(&conn, &outcomes, now_ms())
    })
    .await??;
    Ok(())
}

/// The due rows, oldest first, each with its recipients and holder resolved in
/// the same connection checkout.
fn fetch_due(conn: &Connection, now: i64) -> anyhow::Result<Vec<Job>> {
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
    rows.into_iter()
        .map(|(outbox_id, account_id, label, amount, attempts)| {
            Ok(Job {
                outbox_id,
                attempts,
                holder: holder_label(conn, &account_id)?,
                label,
                amount,
                recipients: recipients(conn, &account_id, now)?,
            })
        })
        .collect()
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
                "body": format!("{}: {} +{} エメ", job.holder, job.label, job.amount),
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
/// (the service is down or refusing us), not a swallowed error — hence the
/// warning, and hence no refund-like compensation: the ledger already holds
/// the deposit this row failed to announce.
fn apply_outcomes(conn: &Connection, outcomes: &[(Job, bool)], now: i64) -> anyhow::Result<()> {
    for (job, delivered) in outcomes {
        if *delivered {
            delete_rows(conn, std::slice::from_ref(&job.outbox_id))?;
            continue;
        }
        let attempts = job.attempts + 1;
        if attempts >= MAX_ATTEMPTS {
            tracing::warn!(outbox_id = %job.outbox_id, attempts, "notification undeliverable — dropping");
            delete_rows(conn, std::slice::from_ref(&job.outbox_id))?;
        } else {
            conn.execute(
                "UPDATE notification_outbox SET attempts = ?2, next_attempt_unix_ms = ?3 \
                 WHERE outbox_id = ?1",
                rusqlite::params![job.outbox_id, attempts, now + (BACKOFF_BASE_MS << (attempts - 1))],
            )?;
        }
    }
    Ok(())
}

fn delete_rows(conn: &Connection, ids: &[String]) -> anyhow::Result<()> {
    for id in ids {
        conn.execute("DELETE FROM notification_outbox WHERE outbox_id = ?1", [id])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::PooledConn;

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

        let jobs = fetch_due(&conn, NOW).unwrap();
        assert_eq!(
            jobs.iter().map(|j| j.outbox_id.as_str()).collect::<Vec<_>>(),
            vec!["o-early", "o-later"]
        );
        assert_eq!(jobs[0].holder, "@alice");
        assert_eq!(jobs[0].recipients, vec!["m-1"]);
        assert_eq!((jobs[0].label.as_str(), jobs[0].amount), ("Bob から受取", 40));
    }

    #[test]
    fn a_failure_backs_off_and_the_last_one_drops_the_row() {
        let conn = wallet();
        const NOW: i64 = 1_000;
        queue(&conn, "o-1", 1, 0);
        let job = |attempts| Job {
            outbox_id: "o-1".into(),
            attempts,
            holder: "@alice".into(),
            label: "x".into(),
            amount: 1,
            recipients: vec!["m-1".into()],
        };

        // First failure: still present, pushed into the future.
        apply_outcomes(&conn, &[(job(0), false)], NOW).unwrap();
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
        apply_outcomes(&conn, &[(job(MAX_ATTEMPTS - 1), false)], NOW).unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM notification_outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn a_delivered_row_is_deleted() {
        let conn = wallet();
        queue(&conn, "o-1", 1, 0);
        let job = Job {
            outbox_id: "o-1".into(),
            attempts: 0,
            holder: "@alice".into(),
            label: "x".into(),
            amount: 1,
            recipients: vec!["m-1".into()],
        };
        apply_outcomes(&conn, &[(job, true)], 1_000).unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM notification_outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }
}
