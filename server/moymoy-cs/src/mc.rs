//! In-world MC link — **HTTP in MNN** to the emerald mod on the Minecraft server
//! the user's consent named (MochiOS DEV.md §7.3.10).
//!
//! Both directions ride the backend's OWN reverse cs tunnel — the same one that
//! makes `moymoy.cs.mnn` reachable. A request goes out as an ordinary
//! `POST http://moymoy.<attester_id>.mnn/` and the Hub relays it to that server's
//! connector, which hands it to the mod. **The mod's answer is the HTTP
//! response.**
//!
//! ## Why the address is direct, not auto-routed
//!
//! This used to be `moymoy.<mc-uuid>.minecraft.auto.mnn` — the Hub resolved the
//! character through the presence directory and picked the server. That made the
//! DESTINATION a live lookup, re-evaluated on every send: a consume the user
//! approved while on one server could be delivered to whichever server the
//! directory named by the time the request (or a reconciliation re-send hours
//! later) actually went out. Since G4 the destination comes from the same signed
//! assertion that authorized the consume, so the server the user consented to is
//! the server that is asked, and it stays that server across every retry (which
//! is why `emerald_ops.attester_id` is persisted).
//!
//! ## What this replaced
//!
//! This used to be a SECOND connection: an mTLS QUIC link to the command bus
//! (`:7421`) carrying a bespoke reliable-send protocol, with the mod's replies
//! arriving asynchronously on `run_inbound` and being matched back to their
//! request by a hand-rolled correlation table (`req_id` → oneshot) — and, because
//! a cs host is single-owner, that connection had to claim the SIBLING sub-host
//! `charge.moymoy` so it would not collide with the wallet's `wallet.moymoy`.
//! HTTP removes all of it: the response is the reply, so there is no inbound
//! plane, no correlation table, no second claim, and no client certificate. The
//! backend is reachable at plain `moymoy.cs.mnn` again.
//!
//! ## The verbs
//!
//! * `emerald.charge` — consume the character's emeralds (emeralds → eme).
//! * `emerald.withdraw` — grant emeralds to the character (eme → emeralds), the
//!   same exchange run backwards. Its ack reports `granted` rather than
//!   `settled`, so neither direction can read the other's settlement.
//! * `inventory.query` — read the character's chargeable inventory (no effect).
//!
//! ## What survives, and why
//!
//! * `op_id` — NOT a correlation id (HTTP correlates by connection) but the
//!   **idempotency key** the mod claims a completed op under, so a retried charge
//!   re-acks the same settled amount instead of consuming twice, and a retried
//!   withdrawal re-acks the same granted amount instead of paying out twice. It
//!   stays in the payload and in the `emerald_ops` ledger.
//! * `req_id` on an inventory query — the mod drops a query that carries an empty
//!   one, so it is still generated and sent; it is simply no longer *used* here.
//! * The ledger and its reconciliation pass — a request can still fail after the
//!   mod acted (timeout, mid-stream error), which is exactly what
//!   [`ChargeOutcome::Ambiguous`] marks and what reconciliation re-drives. The
//!   ledger orders the two directions oppositely (consume-first for a charge,
//!   debit-first for a withdrawal); `charge.rs` explains why.

use std::time::Duration;

use mochi_hub_cs_sdk::{CsHttpResponse, CsHttpSender, HttpSendError};
use serde_json::Value;
use uuid::Uuid;

use mochi_proto_attest::{public_key_from_b64url, ATTEST_ALG};

use crate::attest::pubkey_url as attest_pubkey_url;

/// Deadline for one charge/withdraw round-trip. Deliberately LONGER than the connector
/// sidecar's own 30 s mod deadline so the honest `504` it synthesises wins the
/// race: a local timeout would leave consumption unknowable, while the sidecar's
/// 504 at least bounds where the request got to.
const CHARGE_TIMEOUT: Duration = Duration::from_secs(35);

/// Deadline for an inventory query. Short on purpose — a user is watching the
/// charge screen, and an unanswered query is a legitimate "not reachable"
/// outcome, not a failure to surface. (Nothing is consumed by a query, so unlike
/// a charge there is no ambiguity to protect against.)
const INVENTORY_TIMEOUT: Duration = Duration::from_secs(3);

/// Deadline for fetching the Hub's attestation public key. Short: it is a
/// Hub-terminated directory read with no game server in the path, and a slow one
/// stalls a user waiting on a charge. Failing means the assertion is refused as
/// `attest_unavailable`, which a retry resolves.
const PUBKEY_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of asking the mod to move emeralds — in either direction.
///
/// The variants exist to keep **whether the mod may have acted in-world**
/// unambiguous, because that is what decides whether the ledger may treat a
/// failure as harmless. Nothing here ever collapses "we don't know" into "it
/// didn't happen". Which way the uncertainty is dangerous depends on the
/// direction (an un-credited consume vs. an un-refunded payout), so the variants
/// stay direction-neutral and `charge.rs` decides what each one means.
pub enum ChargeOutcome {
    /// The mod answered. The value is its settlement ack (`{op_id, status,
    /// settled}` for a charge, `{op_id, status, granted}` for a withdrawal) —
    /// feed it to the matching settler in `charge.rs`.
    Acked(Value),
    /// The Hub had no live connector for that server (404/503). The address is a
    /// fixed server now, not a character lookup, so this means "that server is
    /// not connected", not "the player is offline". Either way the Hub decided it
    /// before dialing anyone, so nothing happened in-world.
    ServerUnreachable,
    /// Nothing left this process (the tunnel is down, or the request could not be
    /// built). Nothing happened in-world.
    NotSent,
    /// The request WAS on the wire when the exchange failed. Whether the mod acted
    /// is unknown — the op must stay non-terminal so reconciliation re-drives it
    /// and the mod's idempotent re-ack settles it.
    Ambiguous(String),
}

/// The MoyMoy backend's link to the in-world mod.
///
/// Cheap to clone and valid across tunnel reconnects — while the tunnel is down
/// every call reports [`ChargeOutcome::NotSent`] rather than buffering.
#[derive(Clone)]
pub struct McLink {
    sender: CsHttpSender,
}

impl McLink {
    /// Wrap the HTTP half of the backend's cs tunnel
    /// (the `CsHttpSender` published with the live tunnel). The handle is published with the live
    /// tunnel as soon as it connects, so it can be built before the tunnel is up.
    pub fn new(sender: CsHttpSender) -> Self {
        McLink { sender }
    }

    /// Is the tunnel live right now? This is what `can_charge` reports — a
    /// real-time liveness signal, not a static "was a credential configured".
    pub fn is_connected(&self) -> bool {
        self.sender.is_connected()
    }

    /// Ask the mod on `attester_id` to consume `amount` emeralds for `uuid`, and
    /// read its settlement ack off the response.
    ///
    /// `attester_id` is the server the user's signed assertion named — see the
    /// module docs for why the destination is carried rather than looked up.
    pub async fn send_charge(
        &self,
        attester_id: &str,
        uuid: &Uuid,
        op_id: &str,
        idem_key: &str,
        amount: i64,
    ) -> ChargeOutcome {
        let payload = serde_json::json!({
            "op_id": op_id,
            "idem_key": idem_key,
            "verb": "emerald.charge",
            "target_uuid": uuid.to_string(),
            "amount": amount,
        });
        self.send_op("charge", attester_id, op_id, &payload).await
    }

    /// Ask the mod on `attester_id` to GRANT `amount` emeralds to `uuid` — the
    /// other half of the wallet, and the exact mirror of [`send_charge`] on the
    /// wire (same address, same `op_id` idempotency, same ack-is-the-response).
    ///
    /// The ack's amount field is `granted`, not `settled`, on purpose: the two
    /// directions must not be able to read each other's acks. See
    /// `charge::settle_withdraw_ack`.
    pub async fn send_withdraw(
        &self,
        attester_id: &str,
        uuid: &Uuid,
        op_id: &str,
        idem_key: &str,
        amount: i64,
    ) -> ChargeOutcome {
        let payload = serde_json::json!({
            "op_id": op_id,
            "idem_key": idem_key,
            "verb": "emerald.withdraw",
            "target_uuid": uuid.to_string(),
            "amount": amount,
        });
        self.send_op("withdraw", attester_id, op_id, &payload).await
    }

    /// One request/ack exchange with the mod, shared by both directions so a
    /// charge and a withdrawal classify their failures identically — the ledger's
    /// safety rests on that classification, and two copies of it would drift.
    /// `direction` is carried only so every line says which way the op ran.
    async fn send_op(
        &self,
        direction: &str,
        attester_id: &str,
        op_id: &str,
        payload: &Value,
    ) -> ChargeOutcome {
        let url = direct_url(attester_id);
        let resp = match self.post(&url, payload, CHARGE_TIMEOUT).await {
            Ok(r) => r,
            Err(e) => return charge_send_failure(&url, op_id, direction, e),
        };

        let body = String::from_utf8_lossy(&resp.body).into_owned();
        if !is_success(resp.status) {
            // 404 (nothing claims that server's zone) and 503 (the claim has no
            // live node) are both decided by the Hub BEFORE any server is dialed,
            // so they are the two statuses that provably changed nothing in-world.
            // Anything else got at least as far as a connector.
            if resp.status == 404 || resp.status == 503 {
                tracing::info!(%url, op_id, direction, status = resp.status, %body,
                    "emerald op not routable — that server's connector is not live (nothing happened in-world)");
                return ChargeOutcome::ServerUnreachable;
            }
            tracing::warn!(%url, op_id, direction, status = resp.status, %body,
                "emerald op failed past the Hub — its in-world effect is UNKNOWN; leaving the op for reconciliation");
            return ChargeOutcome::Ambiguous(format!("http_{}", resp.status));
        }

        // 2xx: the mod acks inline for every op it processes, so a missing or
        // unparseable ack is a broken contract — never settle on a guess.
        match serde_json::from_str::<Value>(&body) {
            Ok(v) if v.get("op_id").is_some() => ChargeOutcome::Acked(v),
            Ok(_) => {
                tracing::error!(%url, op_id, direction, %body,
                    "emerald op answered 2xx with no op_id in the ack — cannot settle; leaving the op for reconciliation");
                ChargeOutcome::Ambiguous("ack_without_op_id".to_string())
            }
            Err(e) => {
                tracing::error!(%url, op_id, direction, error = %e, %body,
                    "emerald op answered 2xx with a malformed ack — cannot settle; leaving the op for reconciliation");
                ChargeOutcome::Ambiguous("malformed_ack".to_string())
            }
        }
    }

    /// Round-trip the character's chargeable inventory through the mod on
    /// `attester_id`.
    ///
    /// Returns `Some((online, emeralds, blocks))` when the mod answered —
    /// `online` distinguishes "this character is a live player there" (real
    /// counts) from "the mod replied but that UUID is nobody on this server"
    /// (`online=false`, so the counts are 0 because the character wasn't found,
    /// NOT because they own none). `None` means no round-trip completed at all
    /// (tunnel down, not routable, or no answer within the deadline). Callers
    /// MUST keep these three states distinct rather than collapsing them into
    /// "0 emeralds".
    pub async fn query_inventory(
        &self,
        attester_id: &str,
        uuid: &Uuid,
    ) -> Option<(bool, i64, i64)> {
        let payload = serde_json::json!({
            // The mod silently drops a query with an empty req_id, so it is still
            // sent — but nothing here matches on it any more: the answer arrives
            // as this request's own response.
            "req_id": Uuid::new_v4().to_string(),
            "verb": "inventory.query",
            "target_uuid": uuid.to_string(),
        });
        let url = direct_url(attester_id);
        let resp = match self.post(&url, &payload, INVENTORY_TIMEOUT).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(%url, error = %e,
                    "inventory.query failed — that server's connector is down, it doesn't host moymoy, or our tunnel is down");
                return None;
            }
        };
        let body = String::from_utf8_lossy(&resp.body).into_owned();
        if !is_success(resp.status) {
            tracing::info!(%url, status = resp.status, %body, "inventory.query not answered");
            return None;
        }
        let v: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%url, error = %e, %body, "inventory.query: malformed reply");
                return None;
            }
        };
        // The mod always sends `online`; absence ⇒ assume found (lenient).
        let online = v.get("online").and_then(Value::as_bool).unwrap_or(true);
        let emeralds = v.get("emeralds").and_then(Value::as_i64).unwrap_or(0);
        let blocks = v.get("blocks").and_then(Value::as_i64).unwrap_or(0);
        tracing::info!(uuid = %uuid, online, emeralds, blocks, "inventory.query reply");
        Some((online, emeralds, blocks))
    }

    /// Fetch the Hub's raw Ed25519 attestation public key from the directory
    /// zone.
    ///
    /// The transport half only — which is the reason `mochi-proto-attest` is
    /// sans-io. The one field that must not be read loosely goes through the
    /// shared [`public_key_from_b64url`], which accepts only base64url decoding to
    /// a 32-byte key. The `key_id` the endpoint also publishes is deliberately
    /// ignored: `PubkeyCache` derives it from these bytes, so a server-supplied
    /// label cannot disagree with the key it labels. Every failure is an `Err`
    /// carrying why; `crate::attest` turns that into `attest_unavailable` and
    /// refuses the assertion. There is no "assume the key" branch.
    pub async fn fetch_attest_pubkey(&self) -> Result<Vec<u8>, String> {
        let url = attest_pubkey_url();
        let resp = self
            .get(&url, PUBKEY_TIMEOUT)
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        let body = String::from_utf8_lossy(&resp.body).into_owned();
        if !is_success(resp.status) {
            return Err(format!("GET {url}: HTTP {} ({body})", resp.status));
        }
        let doc: Value =
            serde_json::from_str(&body).map_err(|e| format!("{url}: malformed reply: {e}"))?;
        let alg = doc.get("alg").and_then(Value::as_str).unwrap_or_default();
        if alg != ATTEST_ALG {
            // Refuse rather than try the bytes as Ed25519 anyway: a hub that
            // rotated to another algorithm needs a build that knows it.
            return Err(format!("{url}: unsupported signature algorithm {alg:?}"));
        }
        doc.get("public_key")
            .and_then(Value::as_str)
            .and_then(public_key_from_b64url)
            .ok_or_else(|| format!("{url}: reply has no usable ed25519 public_key"))
    }

    /// POST `payload` as JSON to `url` with a deadline. A timeout is reported as
    /// [`HttpSendError::Http`] so the caller classifies it the same way as any
    /// other mid-exchange failure (the request was already on the wire).
    async fn post(
        &self,
        url: &str,
        payload: &Value,
        deadline: Duration,
    ) -> Result<CsHttpResponse, HttpSendError> {
        let headers = vec![("content-type".to_string(), "application/json".to_string())];
        let body = payload.to_string().into_bytes();
        match tokio::time::timeout(deadline, self.sender.request("POST", url, &headers, body)).await
        {
            Ok(result) => result,
            Err(_elapsed) => Err(HttpSendError::Http(format!(
                "no answer within {}s",
                deadline.as_secs()
            ))),
        }
    }

    /// GET `url` with a deadline (bodyless request, same tunnel as [`post`]).
    async fn get(&self, url: &str, deadline: Duration) -> Result<CsHttpResponse, HttpSendError> {
        match tokio::time::timeout(deadline, self.sender.request("GET", url, &[], Vec::new())).await
        {
            Ok(result) => result,
            Err(_elapsed) => Err(HttpSendError::Http(format!(
                "no answer within {}s",
                deadline.as_secs()
            ))),
        }
    }
}

/// The direct address of the mod on the server the assertion named:
/// `<identifier>.<server_id>.mnn` (3 labels, right-anchored). The identifier
/// `moymoy` is the mod's serve key; the path is `/` because the mod dispatches on
/// the payload's `verb`, not on the URL.
///
/// `attester_id` reaches here only out of an `AttestedFacts`, i.e. signed claims
/// that `ClaimsPolicy` already parsed through `ExSoftServerId` — a single
/// routable label, never one of the Hub-reserved zones. So it cannot introduce a
/// second host into the name, and `cs` in particular (which would address this
/// very backend) cannot get this far.
fn direct_url(attester_id: &str) -> String {
    format!("http://moymoy.{attester_id}.mnn/")
}

fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Classify a failure to complete the exchange, keeping "nothing was written" and
/// "it was on the wire" apart — the distinction the ledger depends on. Shared by
/// both directions (the classification is about the transport, not the verb);
/// `direction` is logged so an operator can see which way the op ran.
fn charge_send_failure(
    url: &str,
    op_id: &str,
    direction: &str,
    err: HttpSendError,
) -> ChargeOutcome {
    match err {
        // Nothing reached the Hub: no tunnel, or the stream never opened.
        HttpSendError::NotConnected => {
            tracing::warn!(%url, op_id, direction, "emerald op NOT sent — the cs tunnel is down; reconciliation will retry");
            ChargeOutcome::NotSent
        }
        HttpSendError::Open(m) => {
            tracing::warn!(%url, op_id, direction, error = %m, "emerald op NOT sent — could not open the tunnel stream");
            ChargeOutcome::NotSent
        }
        // The request was rejected before a stream existed, so nothing happened
        // in-world — but this is a bug in how we built it, not a transient.
        e @ (HttpSendError::BadUrl(_) | HttpSendError::BadRequest(_)) => {
            tracing::error!(%url, op_id, direction, error = %e, "emerald op request is malformed — NOT sent (this is a backend bug)");
            ChargeOutcome::NotSent
        }
        // Everything else (and every future variant of this #[non_exhaustive]
        // enum) happened with the request already on the wire. Defaulting to
        // ambiguous is the asset-safe direction in BOTH directions: a charge is
        // retried until the mod re-acks `duplicate`, and a withdrawal is never
        // refunded on a guess that the payout did not land.
        other => {
            tracing::warn!(%url, op_id, direction, error = %other,
                "emerald op exchange failed mid-flight — its in-world effect is UNKNOWN; leaving the op for reconciliation");
            ChargeOutcome::Ambiguous(other.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_direct_url_is_the_three_label_direct_form() {
        // `<serve_key>.<server_id>.mnn` — the Hub reads the middle label as the
        // connector zone. A fourth label (or a reserved one) would route
        // somewhere else entirely, which is why the claims policy pins the shape
        // before an id can reach here.
        assert_eq!(direct_url("mc1"), "http://moymoy.mc1.mnn/");
        assert_eq!(direct_url("my-server-2"), "http://moymoy.my-server-2.mnn/");
    }

    #[test]
    fn a_down_tunnel_is_not_sent_never_ambiguous() {
        // Nothing was written, so the op stays 'pending': a dead-letter can safely
        // fail a charge (no consumed emeralds to write off) and safely refund a
        // withdrawal (no emeralds were granted).
        for direction in ["charge", "withdraw"] {
            assert!(matches!(
                charge_send_failure("u", "op", direction, HttpSendError::NotConnected),
                ChargeOutcome::NotSent
            ));
            assert!(matches!(
                charge_send_failure("u", "op", direction, HttpSendError::Open("refused".into())),
                ChargeOutcome::NotSent
            ));
        }
    }

    #[test]
    fn a_mid_flight_failure_is_ambiguous() {
        // The request reached the Hub; the mod may have consumed (or granted). The
        // op must stay non-terminal so a late/duplicate ack can still settle it —
        // and so a withdrawal is never refunded while a payout may have landed.
        for direction in ["charge", "withdraw"] {
            assert!(matches!(
                charge_send_failure("u", "op", direction, HttpSendError::Http("reset".into())),
                ChargeOutcome::Ambiguous(_)
            ));
            assert!(matches!(
                charge_send_failure("u", "op", direction, HttpSendError::Body("eof".into())),
                ChargeOutcome::Ambiguous(_)
            ));
        }
    }
}
