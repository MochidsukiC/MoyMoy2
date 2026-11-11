//! In-process wake-ups for a merchant long-polling one intent
//! (`GET /merchant/v1/intent/wait`).
//!
//! The database is the only source of truth for an intent's state. This module
//! carries no state of its own: it only says "this intent may have changed, read
//! it again". A notification is therefore allowed to be wrong (a handler notifies
//! whether or not its operation succeeded) — the waiter re-reads and goes back to
//! sleep if nothing changed — but it must never be MISSED, which is what the
//! register → re-read → wait order in `merchant::intent_wait` guarantees.
//!
//! Per-process on purpose, like [`crate::merchant::RateLimiter`]: there is one
//! moymoy-cs, and every writer of an intent's state runs inside it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::futures::Notified;
use tokio::sync::Notify;

/// Concurrent long-polls one shop may hold open. Each is a parked future, not a
/// thread or a database connection, so this bounds memory and file descriptors a
/// misbehaving integrator can pin, not CPU.
pub const MAX_WAITERS_PER_MERCHANT: usize = 16;

struct Entry {
    notify: Arc<Notify>,
    /// Live [`WaitGuard`]s on this intent. The entry is removed when it reaches
    /// zero, so the map holds exactly the intents somebody is waiting on and
    /// cannot grow with the number of intents ever polled.
    waiters: usize,
}

#[derive(Default)]
struct Inner {
    by_intent: HashMap<String, Entry>,
    /// Live [`WaitGuard`]s per merchant (the concurrency cap).
    by_merchant: HashMap<String, usize>,
}

#[derive(Default)]
pub struct IntentWatch {
    inner: Mutex<Inner>,
}

impl IntentWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a waiting slot for `merchant_id` on `intent_id`, or `None` when that
    /// shop already holds [`MAX_WAITERS_PER_MERCHANT`] of them.
    ///
    /// The slot is released when the returned guard is dropped — on a normal
    /// return, a panic, or the client disconnecting (axum drops the handler's
    /// future, and with it the guard). Nothing releases it any other way, so
    /// nothing can leak a slot or an entry.
    pub fn register(self: &Arc<Self>, merchant_id: &str, intent_id: &str) -> Option<WaitGuard> {
        let mut inner = self.lock();
        let held = inner.by_merchant.get(merchant_id).copied().unwrap_or(0);
        if held >= MAX_WAITERS_PER_MERCHANT {
            return None;
        }
        *inner
            .by_merchant
            .entry(merchant_id.to_string())
            .or_insert(0) += 1;
        let entry = inner
            .by_intent
            .entry(intent_id.to_string())
            .or_insert_with(|| Entry {
                notify: Arc::new(Notify::new()),
                waiters: 0,
            });
        entry.waiters += 1;
        let notify = entry.notify.clone();
        drop(inner);
        Some(WaitGuard {
            watch: self.clone(),
            merchant_id: merchant_id.to_string(),
            intent_id: intent_id.to_string(),
            notify,
        })
    }

    /// Wake everyone waiting on `intent_id`. Call it AFTER the transaction that
    /// may have changed the intent has committed (or failed): a waiter re-reads
    /// the row when woken, so it must be able to see the change.
    ///
    /// Never creates an entry. An intent nobody waits on needs no wake-up — a
    /// waiter that registers later reads the row after registering.
    pub fn notify(&self, intent_id: &str) {
        let notify = self
            .lock()
            .by_intent
            .get(intent_id)
            .map(|e| e.notify.clone());
        if let Some(n) = notify {
            // `notify_waiters`, not `notify_one`: several waiters can share an
            // intent, and `notify_one` would wake one and leave a stored permit
            // behind that a later, unrelated wait would consume as a false wake.
            n.notify_waiters();
        }
    }

    /// The map is held only for its own surgery, so a poisoned lock means another
    /// thread panicked mid-update of two counters. Refusing every long-poll for
    /// the rest of the process's life would be the worse outcome, and the guard's
    /// `Drop` must not panic on top of an unwinding thread.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| {
            tracing::error!("IntentWatch mutex was poisoned; recovering the counters");
            e.into_inner()
        })
    }
}

/// One live wait. Holds the shop's concurrency slot and the intent's entry.
pub struct WaitGuard {
    watch: Arc<IntentWatch>,
    merchant_id: String,
    intent_id: String,
    notify: Arc<Notify>,
}

impl WaitGuard {
    /// A future that completes on the next [`IntentWatch::notify`] for this
    /// intent. It is guaranteed to observe any `notify_waiters` issued after it is
    /// CREATED, even before it is first polled — so create it, then re-read the
    /// row, and no commit can fall between the two.
    pub fn notified(&self) -> Notified<'_> {
        self.notify.notified()
    }
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        let mut inner = self.watch.lock();
        if let Some(e) = inner.by_intent.get_mut(&self.intent_id) {
            e.waiters -= 1; // paired with the increment in `register`
            if e.waiters == 0 {
                inner.by_intent.remove(&self.intent_id);
            }
        }
        if let Some(n) = inner.by_merchant.get_mut(&self.merchant_id) {
            *n -= 1; // likewise
            if *n == 0 {
                inner.by_merchant.remove(&self.merchant_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    #[test]
    fn sixteen_waiters_are_allowed_per_merchant() {
        let watch = Arc::new(IntentWatch::new());
        let guards: Vec<_> = (0..16)
            .map(|n| watch.register("shop-a", &format!("intent-{n}")).unwrap())
            .collect();
        assert_eq!(guards.len(), 16);
        assert!(watch.register("shop-a", "overflow").is_none());
        assert!(watch.register("shop-b", "other-shop").is_some());
    }

    #[test]
    fn dropping_a_guard_reopens_one_slot() {
        let watch = Arc::new(IntentWatch::new());
        let mut guards: Vec<_> = (0..16)
            .map(|_| watch.register("shop", "intent").unwrap())
            .collect();
        assert!(watch.register("shop", "intent").is_none());
        drop(guards.pop().unwrap());
        let replacement = watch.register("shop", "replacement").unwrap();
        assert!(watch.register("shop", "overflow").is_none());
        drop(replacement);
    }

    #[tokio::test]
    async fn notification_completes_the_matching_waiter() {
        let watch = Arc::new(IntentWatch::new());
        let guard = watch.register("shop", "intent").unwrap();
        let notified = guard.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        watch.notify("intent");
        timeout(Duration::from_secs(1), notified).await.unwrap();
    }

    #[tokio::test]
    async fn notification_does_not_complete_another_intents_waiter() {
        let watch = Arc::new(IntentWatch::new());
        let guard = watch.register("shop", "intent").unwrap();
        let notified = guard.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        watch.notify("other-intent");
        assert!(timeout(Duration::from_millis(200), notified).await.is_err());
    }

    #[tokio::test]
    async fn one_notification_completes_both_waiters() {
        let watch = Arc::new(IntentWatch::new());
        let first = watch.register("shop", "intent").unwrap();
        let second = watch.register("shop", "intent").unwrap();
        let first_notified = first.notified();
        let second_notified = second.notified();
        tokio::pin!(first_notified, second_notified);
        first_notified.as_mut().enable();
        second_notified.as_mut().enable();
        watch.notify("intent");
        timeout(Duration::from_secs(1), async {
            tokio::join!(first_notified, second_notified);
        })
        .await
        .unwrap();
    }
}
