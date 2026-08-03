//! MoyMoy's use of Hub-signed host attestation — how this backend learns WHICH
//! Minecraft character a wallet request may spend the emeralds of (MochiOS
//! DEV.md §7.3.10 G4).
//!
//! The wire schema, the Ed25519 verification, the pubkey cache and the challenge
//! store all live in `mochi-proto-attest`, shared with the Hub that issues the
//! assertions. What is left here is only what is MoyMoy's own decision: its
//! audience name, its two request bindings, which character an account most
//! recently confirmed, and how a refusal is reported to the app.
//!
//! ## What an assertion decides, and what it must never decide
//!
//! The wallet's account subject is, and stays, the `X-MoyMoy-Session` token
//! (handle + PIN — [`crate::auth`]). An assertion is NOT an alternative way to
//! authenticate; it answers a different question entirely: *given that this
//! request is already authenticated as MoyMoy account A, which in-world
//! character's inventory may it consume, and on which server does that character
//! live?* Consequently:
//!
//! * A charge is refused without a session, whatever the assertion says.
//! * `AttestedFacts::account_id` — the **Mochi** account that redeemed the
//!   credential — is deliberately never read for authorization (point 13 of the
//!   shared crate's checklist, decided here). It is a different namespace from a
//!   MoyMoy `account_id`, and no column correlates the two. Treating it as one
//!   would hand whoever controls a Mochi account a claim over a MoyMoy wallet it
//!   never proved anything about. It is logged, and nothing else.
//! * Nothing here creates a way to move value OUT of a wallet. `emerald_ops`
//!   only ever runs `direction = 'charge'` (emeralds → eme) and the mod's only
//!   verbs are `emerald.charge` / `inventory.query`. A server operator can, at
//!   worst, fund their OWN wallet from their OWN server's emeralds; they hold no
//!   operation over anybody else's MoyMoy balance, and this module must not
//!   become the place that changes.
//!
//! ## Who is trusted to attest, and where that is decided
//!
//! **On the Hub, keyed on `exsoft_id`** — `[attestation] trusted_exsoft_attesters`
//! — which is why the policy below selects [`AttesterTrust::AnyRoutable`]. That is
//! a deliberate delegation, not an absence of a check: the Hub refuses to SIGN for
//! an attester outside its allowlist, and MoyMoy keeps no second list because a
//! duplicated trust list is one that goes stale on one side, leaving the operator
//! running a policy they did not write. The shared crate spells out what the
//! delegation is worth on a Hub whose allowlist is unset (every enrolled connector
//! is trusted); MoyMoy is in the "subject means which character on which server"
//! case that variant is written for, not the "subject owns something" case.
//!
//! `attester_kinds` and `require_online_mode` are **not** a second trust boundary
//! either: `attester_kind` is the title the connector reported itself under and
//! `online_mode` is its own declaration, so an enrolled host can set either. They
//! stop a mix-up (an assertion from some future `ios` attester reaching logic
//! written for game hosts), not a dishonest attester.

use std::collections::HashMap;
use std::sync::Mutex;

use mochi_proto_attest::{
    request_hash, AttestError, AttesterTrust, ClaimsPolicy, ClaimsRejected, KeyInstall,
    PubkeyCache, VerifiedClaims, ATTEST_PUBKEY_PATH, DEFAULT_MIN_REFETCH_SECS,
};

pub use mochi_proto_attest::{now_unix_secs, AttestedFacts, ChallengeStore};

use crate::mc::McLink;

/// The audience a MoyMoy assertion must name. The app asks the OS for an
/// assertion "for moymoy"; anything minted for another backend is refused here
/// even though it is validly signed, so a backend that is legitimately handed one
/// cannot relay it at us.
pub const AUDIENCE: &str = "moymoy";

/// The game title MoyMoy deals with — both the directory zone that publishes the
/// signing key and the only `attester_kind` this backend acts on.
pub const ATTESTER_KIND: &str = "minecraft";

/// How long a confirmed character stays usable for inventory reads before the
/// user is asked to confirm again.
pub const CHAR_SESSION_TTL_MS: i64 = 10 * 60 * 1000;

/// Where this backend fetches the Hub's attestation public key: the `<title>`
/// directory zone, which the Hub terminates and serves without a forward-proxy
/// credential (the key is public by definition).
pub fn pubkey_url() -> String {
    format!("http://{ATTESTER_KIND}.auto.mnn{ATTEST_PUBKEY_PATH}")
}

/// The request binding for a charge: the idem_key and the amount, under a
/// charge-only domain.
///
/// The domain is what keeps this hash space disjoint from the session one, so an
/// assertion the user approved to confirm a character can never be spent as one
/// that authorizes a consume. The amount is in the binding because the consent
/// modal states it — an assertion approved for 100 エメ does not verify against a
/// request for 10000.
pub fn charge_request_hash(idem_key: &str, amount: i64) -> String {
    request_hash("moymoy.charge.v1", &[idem_key, &amount.to_string()])
}

/// The request binding for a character confirmation.
///
/// **Constant, with no parts.** A confirmation has no request content to bind to:
/// what makes it unreplayable is the challenge, which is inside the signed claims
/// and spent exactly once against the account and purpose it was issued for. The
/// binding's remaining job is domain separation, which the domain alone does.
///
/// (It cannot bind the challenge even in principle: the expected hash has to be
/// known before `VerifiedClaims::check` runs, and the challenge is one of the
/// fields that check exists to gate.)
pub fn session_request_hash() -> String {
    request_hash("moymoy.session.v1", &[])
}

/// Why an assertion was not acted on.
#[derive(Debug, thiserror::Error)]
pub enum AttestFailure {
    /// The signature, claims version or expiry check failed.
    #[error(transparent)]
    Verify(#[from] AttestError),
    /// Signature-valid, but it did not satisfy this backend's policy.
    #[error(transparent)]
    Rejected(#[from] ClaimsRejected),
    /// The Hub's public key could not be obtained, so nothing could be checked.
    /// Distinct from a refusal — this is OUR outage, not the caller's fault, and
    /// it is never resolved by skipping the check.
    #[error("attestation public key unavailable: {0}")]
    KeyUnavailable(String),
}

impl AttestFailure {
    /// The error code the client sees.
    ///
    /// Every refusal collapses to one code on purpose, following the shared
    /// crate's guidance on [`ClaimsRejected`]: answering differently per reason
    /// builds an oracle for this backend's own policy, telling a caller which
    /// check to work on next. The specific reason is logged in full — an operator
    /// loses nothing, and an honest user's remedy ("confirm again") is the same
    /// either way.
    ///
    /// `attest_unavailable` is the one distinction kept, because it is the one
    /// case that is not the caller's fault and that a retry actually fixes.
    pub fn code(&self) -> &'static str {
        match self {
            AttestFailure::KeyUnavailable(_) => "attest_unavailable",
            AttestFailure::Verify(_) | AttestFailure::Rejected(_) => "attest_invalid",
        }
    }
}

/// Verifies Hub-signed assertions, holding the public key it fetched over MNN.
///
/// The transport is here rather than in the shared crate because fetching is each
/// backend's own business — this one rides its own cs tunnel — and keeping HTTP
/// out of the shared crate is what lets every app backend depend on it.
pub struct AttestVerifier {
    mc: McLink,
    cache: tokio::sync::Mutex<PubkeyCache>,
}

impl AttestVerifier {
    pub fn new(mc: McLink) -> Self {
        AttestVerifier {
            mc,
            cache: tokio::sync::Mutex::new(PubkeyCache::new(DEFAULT_MIN_REFETCH_SECS)),
        }
    }

    /// Verify `assertion` and apply MoyMoy's policy in ONE call, returning the
    /// facts or a refusal.
    ///
    /// Deliberately not two calls. The shared crate makes `subject` unreachable
    /// until the policy has run precisely so a consumer cannot skip it; splitting
    /// this into `verify` + `check` at the handler level would hand that mistake
    /// back to every call site.
    ///
    /// The key is fetched **lazily**, on the first assertion that needs it —
    /// fetching at startup would always fail, because the cs tunnel this rides has
    /// not connected yet at that point. A bad signature is the only failure worth
    /// re-fetching on, and the re-check happens only after an actual rotation;
    /// `PubkeyCache` owns both rules. There is no "couldn't get the key, so allow
    /// it" branch.
    pub async fn verify(
        &self,
        assertion: &str,
        expected_request_hash: &str,
        now_unix: u64,
    ) -> Result<AttestedFacts, AttestFailure> {
        let mut cache = self.cache.lock().await;
        if cache.current().is_none() {
            self.fetch_into(&mut cache, now_unix).await?;
        }
        // Copied out so the cache can be borrowed mutably for a re-fetch below.
        let key = cache
            .current()
            .map(|(_, k)| k.to_vec())
            .expect("a key is installed or fetch_into returned Err");

        let err = match mochi_proto_attest::verify(assertion, &key, now_unix) {
            Ok(verified) => return self.apply_policy(verified, expected_request_hash),
            Err(e) => e,
        };
        if !cache.should_refetch(&err, now_unix) {
            return Err(err.into());
        }
        if self.fetch_into(&mut cache, now_unix).await? != KeyInstall::Rotated {
            // The same key came back, so the signature really is bad. Re-verifying
            // would only produce the same answer.
            return Err(err.into());
        }
        let key = cache
            .current()
            .map(|(_, k)| k.to_vec())
            .expect("just installed");
        let verified = mochi_proto_attest::verify(assertion, &key, now_unix)?;
        self.apply_policy(verified, expected_request_hash)
    }

    fn apply_policy(
        &self,
        verified: VerifiedClaims,
        expected_request_hash: &str,
    ) -> Result<AttestedFacts, AttestFailure> {
        // Captured before `check` consumes it. `VerifiedClaims`'s own Debug omits
        // subject / account_id, which is exactly right for a refusal log: it
        // identifies the token, not the person it names but that we have not yet
        // agreed to believe.
        let token = format!("{verified:?}");
        verified
            .check(&ClaimsPolicy {
                audience: AUDIENCE,
                attester_kinds: &[ATTESTER_KIND],
                request_hash: expected_request_hash,
                require_online_mode: true,
                // Trust is the Hub's decision — see the module docs.
                trusted_attesters: AttesterTrust::AnyRoutable,
            })
            .map_err(|rejection| {
                // Full detail here, one code to the caller (see `AttestFailure::code`).
                tracing::warn!(%rejection, %token, "assertion refused by MoyMoy claim policy");
                rejection.into()
            })
    }

    async fn fetch_into(
        &self,
        cache: &mut PubkeyCache,
        now_unix: u64,
    ) -> Result<KeyInstall, AttestFailure> {
        let key = self
            .mc
            .fetch_attest_pubkey()
            .await
            .map_err(AttestFailure::KeyUnavailable)?;
        let install = cache.install(key, now_unix);
        tracing::info!(
            key_id = cache.current().map(|(id, _)| id).unwrap_or(""),
            ?install,
            "fetched the hub attestation public key"
        );
        Ok(install)
    }
}

/// What an attestation is being asked to authorize.
///
/// The purpose is carried in TWO places, and both are load-bearing: the challenge
/// store binds it (so a nonce issued for one cannot be spent on the other), and
/// the request-hash domain separates it (so the signed binding of one does not
/// verify as the other).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttestPurpose {
    /// Authorize one specific `(idem_key, amount)` consume.
    Charge,
    /// Confirm which character this account is playing, for inventory reads.
    Session,
}

impl AttestPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            AttestPurpose::Charge => "charge",
            AttestPurpose::Session => "session",
        }
    }

    /// Parse a client-supplied purpose. An unknown string is `None` — never a
    /// default (fail closed).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "charge" => Some(AttestPurpose::Charge),
            "session" => Some(AttestPurpose::Session),
            _ => None,
        }
    }
}

/// A character this account confirmed, and the server it was confirmed on.
#[derive(Clone, Debug)]
pub struct CharSession {
    pub mc_uuid: String,
    pub attester_id: String,
    pub expires_ms: i64,
}

/// The confirmed character per MoyMoy account, so browsing the charge screen does
/// not re-prompt for consent on every inventory read.
///
/// MoyMoy's own, not the shared crate's: caching a confirmation is this app's UX
/// decision (it shows an inventory preview before the amount entry), and a backend
/// laid out differently would not want it at all.
///
/// Deliberately NOT persisted: it caches a fact that decays (the player moves
/// servers, logs out), and a restart re-deriving it from a fresh consent is
/// correct. Nothing here authorizes a consume — a charge always carries its own
/// assertion.
#[derive(Default)]
pub struct CharSessionStore {
    inner: Mutex<HashMap<String, CharSession>>,
}

impl CharSessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&self, account_id: &str, mc_uuid: &str, attester_id: &str, now_ms: i64) {
        self.lock().insert(
            account_id.to_string(),
            CharSession {
                mc_uuid: mc_uuid.to_string(),
                attester_id: attester_id.to_string(),
                expires_ms: now_ms + CHAR_SESSION_TTL_MS,
            },
        );
    }

    pub fn get(&self, account_id: &str, now_ms: i64) -> Option<CharSession> {
        let mut map = self.lock();
        map.retain(|_, s| s.expires_ms > now_ms);
        map.get(account_id).cloned()
    }

    pub fn invalidate(&self, account_id: &str) {
        self.lock().remove(account_id);
    }

    /// The mutex is only ever held for map surgery (no user code, no awaits), so
    /// a poisoned lock means another thread panicked mid-surgery. Recovering the
    /// map is right here: the contents are a decaying cache, and refusing to serve
    /// the charge screen forever would be a worse outcome than reusing a map whose
    /// entries still expire on their own.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, CharSession>> {
        self.inner.lock().unwrap_or_else(|e| {
            tracing::error!("CharSessionStore mutex was poisoned; recovering the map");
            e.into_inner()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mochi_proto_attest::{
        issue, AssertionSigner as _, AttestClaims, SeededSigner, ATTEST_CLAIMS_VERSION,
    };

    const NOW: u64 = 1_700_000_000;
    const SEED: [u8; 32] = [7u8; 32];

    fn signer() -> SeededSigner {
        SeededSigner::from_seed(&SEED)
    }

    /// Claims as the Hub would mint them for a MoyMoy charge.
    fn charge_claims(idem_key: &str, amount: i64) -> AttestClaims {
        AttestClaims {
            v: ATTEST_CLAIMS_VERSION,
            attester_kind: ATTESTER_KIND.to_string(),
            attester_id: "mc1".to_string(),
            subject: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            account_id: "8f14e45f-ceea-467a-9d24-8bd8e2a5f2ab".to_string(),
            online_mode: true,
            audience: AUDIENCE.to_string(),
            challenge: "nonce-1".to_string(),
            request_hash: charge_request_hash(idem_key, amount),
            iat: NOW,
            exp: NOW + 120,
        }
    }

    /// The policy exactly as `AttestVerifier::apply_policy` builds it, so these
    /// tests exercise the real thing rather than a restatement of it.
    fn check(claims: &AttestClaims, expected_hash: &str) -> Result<AttestedFacts, ClaimsRejected> {
        let signer = signer();
        let token = issue(&signer, claims).unwrap();
        mochi_proto_attest::verify(&token, &signer.public_key_bytes(), NOW + 1)
            .expect("the fixture is validly signed and unexpired")
            .check(&ClaimsPolicy {
                audience: AUDIENCE,
                attester_kinds: &[ATTESTER_KIND],
                request_hash: expected_hash,
                require_online_mode: true,
                trusted_attesters: AttesterTrust::AnyRoutable,
            })
    }

    /// `check`, for the cases that must be refused. `AttestedFacts` is
    /// deliberately not comparable (it is a fact, not a value), so a rejection is
    /// asserted on its own.
    fn reject(claims: &AttestClaims, expected_hash: &str) -> ClaimsRejected {
        check(claims, expected_hash).expect_err("this fixture must be refused")
    }

    #[test]
    fn the_accept_path_yields_the_character_and_its_server() {
        // The path that decides whether a charge goes through, tested offline
        // thanks to the shared crate's dev-signer.
        let claims = charge_claims("idem-1", 250);
        let facts = check(&claims, &charge_request_hash("idem-1", 250)).expect("accepted");
        assert_eq!(facts.subject(), claims.subject);
        assert_eq!(facts.attester_id(), "mc1");
        assert_eq!(facts.challenge(), "nonce-1");
    }

    #[test]
    fn an_assertion_for_another_amount_or_key_does_not_verify() {
        // The binding that stops a captured assertion being spent on a different
        // charge. Both fields are in the hash, so moving either breaks it.
        let claims = charge_claims("idem-1", 250);
        for (key, amount) in [("idem-1", 251), ("idem-2", 250), ("idem-2", 251)] {
            assert_eq!(
                reject(&claims, &charge_request_hash(key, amount)),
                ClaimsRejected::RequestHashMismatch,
                "({key}, {amount}) was accepted"
            );
        }
    }

    #[test]
    fn a_session_assertion_cannot_authorize_a_charge() {
        // Domain separation, end to end: the user approved a character check, and
        // that approval must not settle a payment.
        let mut claims = charge_claims("idem-1", 250);
        claims.request_hash = session_request_hash();
        assert_eq!(
            reject(&claims, &charge_request_hash("idem-1", 250)),
            ClaimsRejected::RequestHashMismatch
        );
        // …and the reverse: a charge assertion is not a character confirmation.
        let charge = charge_claims("idem-1", 250);
        assert_eq!(
            reject(&charge, &session_request_hash()),
            ClaimsRejected::RequestHashMismatch
        );
        assert_ne!(charge_request_hash("idem-1", 250), session_request_hash());
    }

    #[test]
    fn another_backends_assertion_is_refused() {
        let mut claims = charge_claims("idem-1", 250);
        claims.audience = "piggleshop".to_string();
        assert_eq!(
            reject(&claims, &charge_request_hash("idem-1", 250)),
            ClaimsRejected::AudienceMismatch
        );
    }

    #[test]
    fn an_unexpected_attester_kind_or_an_offline_host_is_refused() {
        // Mix-up prevention, NOT a trust boundary — an enrolled host sets both of
        // these itself (see the module docs). The test pins the wiring.
        let mut kind = charge_claims("idem-1", 250);
        kind.attester_kind = "ios".to_string();
        assert_eq!(
            reject(&kind, &charge_request_hash("idem-1", 250)),
            ClaimsRejected::AttesterKindNotAllowed
        );

        let mut offline = charge_claims("idem-1", 250);
        offline.online_mode = false;
        assert_eq!(
            reject(&offline, &charge_request_hash("idem-1", 250)),
            ClaimsRejected::OnlineModeMissing
        );
    }

    #[test]
    fn an_attester_id_that_cannot_name_a_server_is_refused() {
        // Load-bearing for THIS backend specifically: the id becomes
        // `moymoy.<id>.mnn`, the address a consume is delivered to. `cs` would
        // address this very backend.
        for bad in ["mc.1", "cs", "auto", "usermail", "MC1", "", "-mc"] {
            let mut claims = charge_claims("idem-1", 250);
            claims.attester_id = bad.to_string();
            assert_eq!(
                reject(&claims, &charge_request_hash("idem-1", 250)),
                ClaimsRejected::AttesterIdMalformed,
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn a_failure_reports_one_code_but_separates_our_own_outage() {
        // A refusal never tells the caller which check it fell at (that would be
        // an oracle for the policy); an outage on our side is separated because it
        // is the one a retry fixes.
        assert_eq!(
            AttestFailure::from(ClaimsRejected::AudienceMismatch).code(),
            "attest_invalid"
        );
        assert_eq!(
            AttestFailure::from(ClaimsRejected::RequestHashMismatch).code(),
            "attest_invalid"
        );
        assert_eq!(
            AttestFailure::from(AttestError::BadSignature).code(),
            "attest_invalid"
        );
        assert_eq!(
            AttestFailure::KeyUnavailable("tunnel down".into()).code(),
            "attest_unavailable"
        );
    }

    #[test]
    fn purpose_parsing_is_fail_closed() {
        assert_eq!(AttestPurpose::parse("charge"), Some(AttestPurpose::Charge));
        assert_eq!(
            AttestPurpose::parse("session"),
            Some(AttestPurpose::Session)
        );
        for bad in ["", "Charge", "sessions", "admin"] {
            assert_eq!(AttestPurpose::parse(bad), None, "{bad:?} was accepted");
        }
        assert_eq!(AttestPurpose::Charge.as_str(), "charge");
        assert_eq!(AttestPurpose::Session.as_str(), "session");
    }

    #[test]
    fn char_session_expires_and_invalidates() {
        let store = CharSessionStore::new();
        let now = 5_000;
        store.put("acct-a", "uuid-1", "mc1", now);
        let s = store.get("acct-a", now).expect("just stored");
        assert_eq!(s.mc_uuid, "uuid-1");
        assert_eq!(s.attester_id, "mc1");

        assert!(store.get("acct-a", now + CHAR_SESSION_TTL_MS).is_none());

        store.put("acct-a", "uuid-1", "mc1", now);
        store.invalidate("acct-a");
        assert!(store.get("acct-a", now).is_none());
    }

    #[test]
    fn the_pubkey_url_names_the_minecraft_directory_zone() {
        // 3 labels ⇒ the `<title>.auto.mnn` DIRECTORY zone (Hub-terminated), not
        // an auto-routed command to some server.
        assert_eq!(pubkey_url(), "http://minecraft.auto.mnn/v1/attest/pubkey");
    }
}
