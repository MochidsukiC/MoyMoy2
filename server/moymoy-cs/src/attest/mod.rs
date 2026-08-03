//! MoyMoy's use of Hub-signed host attestation — how this backend learns WHICH
//! Minecraft character a wallet request may spend the emeralds of (MochiOS
//! DEV.md §7.3.10 G4).
//!
//! The cryptography and the claims shape are **not** here: they are in [`core`],
//! which knows nothing about wallets and is written to be lifted into the shared
//! `mochi-proto-attest` crate. This module is only the part that is MoyMoy's own
//! decision — its audience name, its two request bindings, its nonce store, and
//! its cache of "which character did this account confirm".
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
//! * [`core::AttestClaims::account_id`] — the **Mochi** account that redeemed the
//!   credential — is deliberately never read for authorization. It is a
//!   different namespace from a MoyMoy `account_id`, and no column correlates
//!   the two. Treating it as one would hand whoever controls a Mochi account a
//!   claim over a MoyMoy wallet it never proved anything about.
//! * Nothing here creates a way to move value OUT of a wallet. `emerald_ops`
//!   only ever runs `direction = 'charge'` (emeralds → eme) and the mod's only
//!   verbs are `emerald.charge` / `inventory.query`. A server operator can, at
//!   worst, fund their OWN wallet from their OWN server's emeralds; they hold no
//!   operation over anybody else's MoyMoy balance, and this module must not
//!   become the place that changes.
//!
//! ## Who is trusted to attest, and where that is decided
//!
//! **On the Hub, keyed on `exsoft_id`.** `[attestation] trusted_exsoft_attesters`
//! names the connectors whose word an operator is willing to treat as identity,
//! and the Hub refuses to SIGN for one outside it — so an assertion that reaches
//! this backend has already passed that gate. MoyMoy deliberately keeps no second
//! allowlist: a duplicated trust list is one that goes stale on one side, and the
//! operator would then be running a policy they did not write.
//!
//! What [`check_claims`] adds is **not** a second trust boundary. `attester_kind`
//! is the connector's self-reported game title and `online_mode` is its own
//! declaration — the mTLS certificate binds only `attester_id`, so a trusted
//! connector that chose to lie could set either. Requiring them catches an
//! attester of an unexpected type reaching logic written for Minecraft; it does
//! nothing against a dishonest one, and no code here should be written as though
//! it did. The one check that genuinely defends this process is the routable-label
//! check on `attester_id`, because that string becomes the URL the consume is
//! delivered to ([`crate::mc::direct_url`]).

pub mod core;

use std::collections::HashMap;
use std::sync::Mutex;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;

pub use self::core::{now_unix_secs, AttestClaims};
use self::core::{ClaimPolicy, PublicKey, VerifyError};
use crate::mc::McLink;

/// The audience a MoyMoy assertion must name. The app asks the OS for an
/// assertion "for moymoy"; anything minted for another backend is refused here
/// even though it is validly signed, so a backend that is legitimately handed one
/// cannot relay it at us.
pub const AUDIENCE: &str = "moymoy";

/// The game title MoyMoy deals with. Used both to address the key endpoint
/// ([`pubkey_url`]) and as the `attester_kind` guard — see the module docs for
/// why the latter is a guard and not a proof.
pub const ATTESTER_KIND: &str = "minecraft";

/// Floor on how often a bad signature may trigger a re-fetch of the public key.
/// Without it, a stream of junk assertions would be an amplification path into
/// the Hub: one MNN round-trip per bad token.
const PUBKEY_REFETCH_MIN_SECS: u64 = 60;

/// How long an issued challenge stays redeemable. Longer than the Hub's own
/// 120 s assertion TTL would be pointless; shorter would expire a user who is
/// still reading the consent modal.
pub const CHALLENGE_TTL_MS: i64 = 120_000;

/// Live challenges one account may hold. Bounds the store structurally rather
/// than trusting a sweep to keep up: an account that asks for challenges in a
/// loop evicts only its own.
pub const MAX_CHALLENGES_PER_ACCOUNT: usize = 8;

/// How long a confirmed character stays usable for inventory reads before the
/// user is asked to confirm again.
pub const CHAR_SESSION_TTL_MS: i64 = 10 * 60 * 1000;

/// Where this backend fetches the Hub's attestation public key.
pub fn pubkey_url() -> String {
    self::core::pubkey_url(ATTESTER_KIND)
}

/// Why an assertion could not be checked at all.
#[derive(Debug, thiserror::Error)]
pub enum AttestError {
    /// The signature, version or expiry check failed.
    #[error(transparent)]
    Verify(#[from] VerifyError),
    /// The Hub's public key could not be obtained, so nothing could be checked.
    /// Distinct from a refusal — this is OUR outage, not the caller's fault, and
    /// it is never resolved by skipping the check.
    #[error("attestation public key unavailable: {0}")]
    KeyUnavailable(String),
}

impl AttestError {
    /// The error code the client sees. The specific reason a token failed stays
    /// in the log: a caller learns "this did not verify", not which step it fell
    /// at. `attest_unavailable` is separated out because it is the one case a
    /// legitimate user should retry.
    pub fn code(&self) -> &'static str {
        match self {
            AttestError::KeyUnavailable(_) => "attest_unavailable",
            AttestError::Verify(_) => "attest_invalid",
        }
    }
}

/// The Hub public key, as last fetched.
struct KeyState {
    key: PublicKey,
    fetched_unix: u64,
}

/// Verifies Hub-signed assertions, holding the public key it fetched over MNN.
///
/// The transport half lives here rather than in [`core`] on purpose: fetching is
/// a consumer's business (this one rides its own cs tunnel), and keeping it out
/// of the core is what lets the core be a pure, socket-free library.
pub struct AttestVerifier {
    mc: McLink,
    key: tokio::sync::Mutex<Option<KeyState>>,
}

impl AttestVerifier {
    pub fn new(mc: McLink) -> Self {
        AttestVerifier {
            mc,
            key: tokio::sync::Mutex::new(None),
        }
    }

    /// Check `assertion` (signature → claims version → expiry).
    ///
    /// The key is fetched **lazily**, on the first assertion that needs it —
    /// fetching at startup would always fail, because the cs tunnel this rides
    /// has not connected yet at that point.
    ///
    /// A bad signature is the only thing that re-fetches, at most once per
    /// [`PUBKEY_REFETCH_MIN_SECS`], and the re-check happens only if the key
    /// actually changed — that is a key ROTATION, which a cached key would
    /// otherwise turn into a permanent outage. There is deliberately no
    /// "couldn't get the key, so allow it" branch: an unverifiable assertion is
    /// refused.
    pub async fn verify(
        &self,
        assertion: &str,
        now_unix: u64,
    ) -> Result<AttestClaims, AttestError> {
        let mut guard = self.key.lock().await;
        if guard.is_none() {
            *guard = Some(self.fetch_key(now_unix).await?);
        }
        let state = guard.as_ref().expect("just populated");
        let first = self::core::verify(assertion, &state.key.bytes, now_unix);
        let stale_enough = now_unix.saturating_sub(state.fetched_unix) >= PUBKEY_REFETCH_MIN_SECS;
        if !matches!(first, Err(VerifyError::BadSignature)) || !stale_enough {
            return first.map_err(AttestError::from);
        }

        let previous_key_id = state.key.key_id.clone();
        let refreshed = self.fetch_key(now_unix).await?;
        let rotated = refreshed.key.key_id != previous_key_id;
        *guard = Some(refreshed);
        if !rotated {
            // Same key ⇒ the signature is genuinely bad. Re-running the check
            // would only produce the same answer.
            return Err(AttestError::Verify(VerifyError::BadSignature));
        }
        let state = guard.as_ref().expect("just populated");
        tracing::info!(
            key_id = %state.key.key_id, previous = %previous_key_id,
            "attestation key rotated; re-checking the assertion against the new key"
        );
        self::core::verify(assertion, &state.key.bytes, now_unix).map_err(AttestError::from)
    }

    async fn fetch_key(&self, now_unix: u64) -> Result<KeyState, AttestError> {
        let key = self
            .mc
            .fetch_attest_pubkey()
            .await
            .map_err(AttestError::KeyUnavailable)?;
        tracing::info!(key_id = %key.key_id, "fetched the hub attestation public key");
        Ok(KeyState {
            key,
            fetched_unix: now_unix,
        })
    }
}

/// MoyMoy's policy over a verified assertion.
///
/// Read the module docs before adding anything here: `attester_kind` and
/// `online_mode` are the attester's own declarations and stop only an accident,
/// not a hostile attester. Trust in the attester itself is the Hub's decision.
///
/// `Err` is the error code returned to the client.
pub fn check_claims(
    claims: &AttestClaims,
    expected_request_hash: &str,
) -> Result<(), &'static str> {
    let policy = ClaimPolicy {
        audience: AUDIENCE,
        require_attester_kind: Some(ATTESTER_KIND),
        require_online_mode: true,
        // Load-bearing: `attester_id` becomes `moymoy.<id>.mnn`, the address this
        // backend delivers a consume to.
        require_routable_attester_id: true,
        request_hash: expected_request_hash,
    };
    match self::core::check_claims(claims, &policy) {
        Ok(()) => Ok(()),
        Err(rejection) => {
            tracing::warn!(
                %rejection,
                attester_id = %claims.attester_id,
                attester_kind = %claims.attester_kind,
                audience = %claims.audience,
                "assertion refused by MoyMoy claim policy"
            );
            Err(rejection.code())
        }
    }
}

/// The request binding for a charge. The `moymoy.charge.v1` prefix keeps the two
/// request-hash spaces structurally disjoint, so an assertion approved for a
/// character confirmation can never be spent as one that authorizes a consume.
pub fn charge_request_hash(idem_key: &str, amount: i64) -> String {
    self::core::sha256_hex(&format!("moymoy.charge.v1\n{idem_key}\n{amount}"))
}

/// The request binding for a character confirmation (see
/// [`charge_request_hash`] for why the prefix is there).
pub fn session_request_hash(challenge: &str) -> String {
    self::core::sha256_hex(&format!("moymoy.session.v1\n{challenge}"))
}

// ── anti-replay nonces ───────────────────────────────────────────────────────

/// What an assertion is being asked to authorize. Carried in the challenge
/// binding AND in the request hash, so an assertion minted to confirm a character
/// can never be spent as one that authorizes a charge.
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

struct ChallengeRec {
    account_id: String,
    purpose: AttestPurpose,
    expires_ms: i64,
}

/// Single-use anti-replay nonces, bound to the account and purpose they were
/// issued for.
#[derive(Default)]
pub struct ChallengeStore {
    inner: Mutex<HashMap<String, ChallengeRec>>,
}

impl ChallengeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a 256-bit challenge bound to `(account_id, purpose)`.
    ///
    /// Expired entries are swept, and the account is held to
    /// [`MAX_CHALLENGES_PER_ACCOUNT`] live ones by evicting its own oldest — so
    /// one account cannot grow the map without bound, and cannot evict another's.
    pub fn issue(&self, account_id: &str, purpose: AttestPurpose, now_ms: i64) -> (String, i64) {
        let mut buf = [0u8; 32];
        OsRng.fill_bytes(&mut buf);
        let challenge = URL_SAFE_NO_PAD.encode(buf);
        let expires_ms = now_ms + CHALLENGE_TTL_MS;

        let mut map = self.lock();
        map.retain(|_, rec| rec.expires_ms > now_ms);
        let mut mine: Vec<(String, i64)> = map
            .iter()
            .filter(|(_, rec)| rec.account_id == account_id)
            .map(|(k, rec)| (k.clone(), rec.expires_ms))
            .collect();
        if mine.len() >= MAX_CHALLENGES_PER_ACCOUNT {
            mine.sort_by_key(|(_, exp)| *exp);
            for (key, _) in mine
                .iter()
                .take(mine.len() + 1 - MAX_CHALLENGES_PER_ACCOUNT)
            {
                map.remove(key);
            }
        }
        map.insert(
            challenge.clone(),
            ChallengeRec {
                account_id: account_id.to_string(),
                purpose,
                expires_ms,
            },
        );
        (challenge, expires_ms)
    }

    /// Redeem `challenge` for `(account_id, purpose)`. True only when it exists,
    /// has not expired, and was issued to exactly this account for exactly this
    /// purpose. The entry is removed either way, so a wrong guess burns it —
    /// single use is what makes a captured assertion unreplayable.
    pub fn consume(
        &self,
        challenge: &str,
        account_id: &str,
        purpose: AttestPurpose,
        now_ms: i64,
    ) -> bool {
        let mut map = self.lock();
        match map.remove(challenge) {
            Some(rec) => {
                rec.expires_ms > now_ms && rec.account_id == account_id && rec.purpose == purpose
            }
            None => false,
        }
    }

    /// The mutex is only ever held for map surgery (no user code, no awaits), so
    /// a poisoned lock means another thread panicked mid-surgery. Recovering the
    /// map is right here: the contents are short-lived nonces, and refusing to
    /// serve challenges forever would be a worse outcome than reusing a map whose
    /// worst case is a stale entry that still has to pass account+purpose+expiry.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ChallengeRec>> {
        self.inner.lock().unwrap_or_else(|e| {
            tracing::error!("ChallengeStore mutex was poisoned; recovering the map");
            e.into_inner()
        })
    }
}

// ── the confirmed character ──────────────────────────────────────────────────

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
/// Deliberately NOT persisted: it is a cache of a fact that decays (the player
/// moves servers, logs out), and a restart re-deriving it from a fresh consent is
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

    /// See [`ChallengeStore::lock`] — same reasoning, and the contents here are
    /// likewise a decaying cache rather than a security decision.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, CharSession>> {
        self.inner.lock().unwrap_or_else(|e| {
            tracing::error!("CharSessionStore mutex was poisoned; recovering the map");
            e.into_inner()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::core::test_signer::{claims_json, NOW};
    use super::*;

    #[test]
    fn request_hash_domain_separation() {
        // Pinned against the core's SHA-256 so this backend's spelling and the
        // app's cannot drift apart silently.
        assert_eq!(
            charge_request_hash("k1", 42),
            core::sha256_hex("moymoy.charge.v1\nk1\n42")
        );
        assert_eq!(
            session_request_hash("k1"),
            core::sha256_hex("moymoy.session.v1\nk1")
        );
        // The two spaces cannot collide: a charge assertion is not spendable as a
        // session one, whatever the inputs.
        assert_ne!(charge_request_hash("k1", 42), session_request_hash("k1"));
        assert_ne!(charge_request_hash("", 0), session_request_hash(""));
    }

    /// The core's fixture, re-addressed to THIS backend. The core deliberately
    /// carries no real consumer's audience, so naming ours is the glue's job.
    fn moymoy_claims(now: u64) -> serde_json::Value {
        let mut c = claims_json(now);
        c["audience"] = serde_json::json!(AUDIENCE);
        c
    }

    fn check(json: serde_json::Value) -> Result<(), &'static str> {
        let claims: AttestClaims = serde_json::from_value(json).unwrap();
        let hash = claims.request_hash.clone();
        check_claims(&claims, &hash)
    }

    #[test]
    fn moymoy_policy_refuses_another_audience_and_an_unexpected_attester() {
        assert_eq!(check(moymoy_claims(NOW)), Ok(()));

        // A validly signed assertion minted for another backend must not be
        // usable here, or a backend legitimately given one could relay it at us.
        let mut aud = moymoy_claims(NOW);
        aud["audience"] = serde_json::json!("piggleshop");
        assert_eq!(check(aud), Err("attest_audience"));

        // Accident prevention, NOT a security boundary — a trusted-but-dishonest
        // connector can set either of these freely (see the module docs). The
        // test pins the wiring, not a defence.
        let mut kind = moymoy_claims(NOW);
        kind["attester_kind"] = serde_json::json!("desktop");
        assert_eq!(check(kind), Err("attest_attester_kind"));

        let mut offline = moymoy_claims(NOW);
        offline["online_mode"] = serde_json::json!(false);
        assert_eq!(check(offline), Err("attest_offline_server"));
    }

    #[test]
    fn moymoy_policy_requires_a_routable_attester_id() {
        // This one IS load-bearing for this process: the id becomes the delivery
        // URL, and `cs` would address this very backend.
        for bad in ["mc.1", "cs", "auto", "usermail", "MC1", ""] {
            let mut c = moymoy_claims(NOW);
            c["attester_id"] = serde_json::json!(bad);
            assert_eq!(check(c), Err("attest_attester_id"), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_request_hash_mismatch_is_refused() {
        let claims: AttestClaims = serde_json::from_value(moymoy_claims(NOW)).unwrap();
        assert_eq!(
            check_claims(&claims, &charge_request_hash("k1", 42)),
            Err("attest_request_hash")
        );
    }

    #[test]
    fn a_challenge_is_single_use_and_bound_to_its_account_and_purpose() {
        let store = ChallengeStore::new();
        let now = 1_000;

        let (c, exp) = store.issue("acct-a", AttestPurpose::Charge, now);
        assert_eq!(exp, now + CHALLENGE_TTL_MS);
        assert!(store.consume(&c, "acct-a", AttestPurpose::Charge, now));
        assert!(
            !store.consume(&c, "acct-a", AttestPurpose::Charge, now),
            "a challenge must not be redeemable twice"
        );

        // Another account's guess burns the entry and still fails.
        let (c, _) = store.issue("acct-a", AttestPurpose::Charge, now);
        assert!(!store.consume(&c, "acct-b", AttestPurpose::Charge, now));
        assert!(!store.consume(&c, "acct-a", AttestPurpose::Charge, now));

        // A session challenge cannot authorize a charge.
        let (c, _) = store.issue("acct-a", AttestPurpose::Session, now);
        assert!(!store.consume(&c, "acct-a", AttestPurpose::Charge, now));

        // And an expired one is refused at `expires_ms` exactly.
        let (c, _) = store.issue("acct-a", AttestPurpose::Charge, now);
        assert!(!store.consume(&c, "acct-a", AttestPurpose::Charge, now + CHALLENGE_TTL_MS));
    }

    #[test]
    fn challenge_store_is_bounded_per_account() {
        let store = ChallengeStore::new();
        let now = 1_000;
        let issued: Vec<String> = (0..MAX_CHALLENGES_PER_ACCOUNT * 3)
            .map(|i| {
                store
                    .issue("acct-a", AttestPurpose::Charge, now + i as i64)
                    .0
            })
            .collect();
        assert_eq!(store.lock().len(), MAX_CHALLENGES_PER_ACCOUNT);
        // The newest survive; the evicted oldest are simply gone (not usable).
        let (kept, evicted) = issued.split_at(issued.len() - MAX_CHALLENGES_PER_ACCOUNT);
        for c in evicted {
            assert!(store.consume(c, "acct-a", AttestPurpose::Charge, now));
        }
        for c in kept {
            assert!(!store.consume(c, "acct-a", AttestPurpose::Charge, now));
        }
        // Another account is unaffected by the flood.
        let (mine, _) = store.issue("acct-b", AttestPurpose::Session, now);
        assert!(store.consume(&mine, "acct-b", AttestPurpose::Session, now));
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
    fn the_pubkey_url_names_the_minecraft_directory_zone() {
        assert_eq!(pubkey_url(), "http://minecraft.auto.mnn/v1/attest/pubkey");
    }
}
