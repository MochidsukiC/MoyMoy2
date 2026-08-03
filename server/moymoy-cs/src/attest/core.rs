//! Hub-signed host attestation — **the verification core** (MochiOS DEV.md
//! §7.3.10 G4).
//!
//! ## This module knows nothing about MoyMoy
//!
//! Deliberately: the Hub's game-player attestation is a platform primitive that
//! any app backend can consume, not a MoyMoy feature. So nothing here mentions
//! wallets, and nothing here decides policy — no audience string, no attester
//! kind, no HTTP client, no application error type. It takes bytes and a public
//! key, and says what the Hub signed. Everything an individual backend has to
//! *decide* is a parameter ([`ClaimPolicy`]) or lives in its own module (see the
//! parent module for MoyMoy's).
//!
//! It is written to be lifted verbatim into the shared `mochi-proto-attest`
//! crate once that exists; until then it is a mirror of
//! `MochiOS2.0/hub/ipvm-router/src/attest.rs`, and [`CLAIMS_VERSION`] is what
//! makes the mirror safe — a Hub that changes the claims shape must bump it, and
//! this build then refuses the token rather than reading a shape it has not seen.
//!
//! ## Token shape
//!
//! `base64url(claims_json) + "." + base64url(ed25519_sig)` — deliberately not a
//! JWT, and no JWT library. The signature covers [`SIGNING_CONTEXT`] followed by
//! **exactly the bytes carried in the first segment**, so verification never
//! re-serializes the claims and there is no canonicalization gap to work in.
//!
//! ## What a verified assertion does and does not establish
//!
//! It establishes that **the Hub** signed these claims, unexpired, for this
//! audience and this request. It does NOT establish that the named attester is
//! honest: any enrolled connector can ask the Hub to attest a player present on
//! its own server, and [`AttestClaims::attester_kind`] in particular is the
//! connector's own self-reported title (the mTLS certificate binds only the
//! `server_id`). Which attesters an operator is willing to believe is a HUB
//! policy — `[attestation] trusted_exsoft_attesters`, keyed on `exsoft_id` — and
//! the Hub refuses to sign for one outside it. A consumer of this module must not
//! reconstruct that decision from the claims.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Prefixed to the claims bytes before signing, and never transmitted.
///
/// The Hub signs with ONE key, which also signs MDP capabilities. Without a
/// context, a signature produced for one protocol could in principle be
/// presented to the other's verifier. A verifier reconstructs the prefix, so it
/// costs nothing on the wire and cannot be stripped by a peer.
pub const SIGNING_CONTEXT: &[u8; 16] = b"mochi-attest-v1\0";

/// The claims-schema version this build understands. Checked before any field is
/// trusted.
pub const CLAIMS_VERSION: u32 = 1;

/// The signature algorithm the pubkey document must name.
pub const ALG: &str = "ed25519";

/// The facts the Hub asserts about one redemption.
///
/// Deserialize-only, and never re-serialized: the signature covers the received
/// bytes, so this struct is only ever a reader of them. Every field is required
/// on the wire — a claims blob missing one does not parse, which is the point.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AttestClaims {
    /// Claims-schema version.
    pub v: u32,
    /// What kind of host attested (e.g. `minecraft`) — the game title.
    ///
    /// **Self-reported by the connector, not proven.** The Hub copies it from
    /// the title the connector announced in its presence frames; the mTLS
    /// certificate binds only [`Self::attester_id`]. Useful to refuse an attester
    /// of an unexpected kind by accident; useless against one that lies.
    pub attester_kind: String,
    /// Which host attested — the `exsoft_id` the Hub read off that connector's
    /// mTLS certificate, never from a request. This is the one identity in the
    /// claims that the transport actually binds.
    pub attester_id: String,
    /// Who is attested: the attester's canonical player label (Minecraft = the
    /// player UUID).
    pub subject: String,
    /// The Mochi account that redeemed the credential, read from the caller the
    /// Hub gateway itself stamped.
    pub account_id: String,
    /// Whether the attesting server proved the player's identity upstream
    /// (Minecraft online-mode). Same caveat as [`Self::attester_kind`]: it comes
    /// from the attester, so it is a declaration, not a proof.
    pub online_mode: bool,
    /// Who the assertion was minted FOR, as the redeemer named them.
    pub audience: String,
    /// The audience's own anti-replay nonce, echoed into the signed claims.
    pub challenge: String,
    /// The redeemer's binding of this assertion to one request — opaque to the
    /// Hub, computed and checked by the app and its backend.
    pub request_hash: String,
    /// Issued-at, unix seconds.
    pub iat: u64,
    /// Expiry, unix seconds.
    pub exp: u64,
}

/// Why an assertion could not be verified at all — as opposed to verifying and
/// then failing policy, which is [`ClaimRejection`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// Not `<b64url>.<b64url>`, or a segment that is not valid base64url / JSON.
    #[error("malformed assertion")]
    Malformed,
    /// Claims carrying a schema version this build does not know.
    #[error("unsupported claims version {0}")]
    UnsupportedVersion(u32),
    /// The signature does not verify against the given public key.
    #[error("signature does not verify")]
    BadSignature,
    /// `exp` is in the past.
    #[error("assertion expired")]
    Expired,
}

/// Check `assertion` against `public_key` and `now_unix` (seconds).
///
/// Signature first, then version, then expiry: no field of the claims is
/// returned to a caller before the signature over them verified, so a forged
/// token is never a source of "facts" even on an error path.
pub fn verify(
    assertion: &str,
    public_key: &[u8],
    now_unix: u64,
) -> Result<AttestClaims, VerifyError> {
    let (claims_b64, sig_b64) = assertion.split_once('.').ok_or(VerifyError::Malformed)?;
    // Exactly two segments — `a.b.c` is a different token shape, not a longer
    // version of this one.
    if sig_b64.contains('.') {
        return Err(VerifyError::Malformed);
    }
    let json = URL_SAFE_NO_PAD
        .decode(claims_b64)
        .map_err(|_| VerifyError::Malformed)?;
    let sig = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| VerifyError::Malformed)?;

    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(&signing_bytes(&json), &sig)
        .map_err(|_| VerifyError::BadSignature)?;

    let claims: AttestClaims = serde_json::from_slice(&json).map_err(|_| VerifyError::Malformed)?;
    if claims.v != CLAIMS_VERSION {
        return Err(VerifyError::UnsupportedVersion(claims.v));
    }
    if claims.exp <= now_unix {
        return Err(VerifyError::Expired);
    }
    Ok(claims)
}

/// The bytes actually signed for `claims_json`: the context, then the exact
/// bytes the token carries. One function so issue and verify cannot drift.
fn signing_bytes(claims_json: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(SIGNING_CONTEXT.len() + claims_json.len());
    msg.extend_from_slice(SIGNING_CONTEXT);
    msg.extend_from_slice(claims_json);
    msg
}

/// What one consumer requires of a verified assertion beyond the signature.
///
/// A parameter rather than constants, because every one of these is the
/// *consumer's* decision: its own audience name, whether it deals with a
/// particular kind of host at all, and which request the assertion must be bound
/// to.
pub struct ClaimPolicy<'a> {
    /// The audience this consumer answers to. An assertion minted for anyone
    /// else is refused even though it is validly signed — otherwise a backend
    /// that was legitimately handed one could relay it.
    pub audience: &'a str,
    /// The [`AttestClaims::attester_kind`] to require, or `None` to accept any.
    ///
    /// A guard against an unexpected host type reaching logic written for
    /// another, NOT a security check — see the field's own docs for why a
    /// dishonest attester is not what this stops.
    pub require_attester_kind: Option<&'a str>,
    /// Whether to refuse an attester that declares it did not prove the player
    /// upstream. Same caveat: a declaration, not a proof.
    pub require_online_mode: bool,
    /// Whether [`AttestClaims::attester_id`] must be a routable single label.
    /// Required by any consumer that turns the id into a `.mnn` name.
    pub require_routable_attester_id: bool,
    /// The digest the assertion must be bound to.
    pub request_hash: &'a str,
}

/// Why a verified assertion still did not satisfy a [`ClaimPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClaimRejection {
    #[error("assertion was minted for a different audience")]
    Audience,
    #[error("assertion is from an unexpected attester kind")]
    AttesterKind,
    #[error("attester declares it did not prove the player upstream")]
    OfflineAttester,
    #[error("attester_id is not a routable server label")]
    AttesterId,
    #[error("assertion is bound to a different request")]
    RequestHash,
}

impl ClaimRejection {
    /// A stable wire code for the refusal, so consumers on either side of an API
    /// spell the same failure the same way.
    pub fn code(self) -> &'static str {
        match self {
            ClaimRejection::Audience => "attest_audience",
            ClaimRejection::AttesterKind => "attest_attester_kind",
            ClaimRejection::OfflineAttester => "attest_offline_server",
            ClaimRejection::AttesterId => "attest_attester_id",
            ClaimRejection::RequestHash => "attest_request_hash",
        }
    }
}

/// Apply `policy` to already-verified `claims`.
///
/// Pure, and deliberately free of side effects: burning a single-use challenge
/// belongs to the caller, which can then order it as verify → policy → burn
/// rather than spending a nonce on a token that was never going to pass.
pub fn check_claims(claims: &AttestClaims, policy: &ClaimPolicy<'_>) -> Result<(), ClaimRejection> {
    if claims.audience != policy.audience {
        return Err(ClaimRejection::Audience);
    }
    if let Some(kind) = policy.require_attester_kind {
        if claims.attester_kind != kind {
            return Err(ClaimRejection::AttesterKind);
        }
    }
    if policy.require_online_mode && !claims.online_mode {
        return Err(ClaimRejection::OfflineAttester);
    }
    if policy.require_routable_attester_id && !is_routable_server_label(&claims.attester_id) {
        return Err(ClaimRejection::AttesterId);
    }
    if claims.request_hash != policy.request_hash {
        return Err(ClaimRejection::RequestHash);
    }
    Ok(())
}

/// A single DNS LDH label that can name a real server: 1..=63 of `a-z` / `0-9` /
/// `-`, no leading/trailing `-`, and not one of the Hub-reserved zone labels
/// (`auto` / `cs` / `usermail`), which never name a server.
///
/// Mirrors `MochiOS2.0/protocols/exsoft-connector/src/addr.rs`
/// (`ExSoftServerId::parse`). A consumer that builds `<something>.<id>.mnn` must
/// enforce this itself rather than inherit the assumption: a label carrying a dot
/// would address a different host, and a reserved one would address a Hub zone.
pub fn is_routable_server_label(label: &str) -> bool {
    let b = label.as_bytes();
    if b.is_empty() || b.len() > 63 {
        return false;
    }
    if b[0] == b'-' || b[b.len() - 1] == b'-' {
        return false;
    }
    if !b
        .iter()
        .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    {
        return false;
    }
    !matches!(label, "auto" | "cs" | "usermail")
}

// ── the public key ───────────────────────────────────────────────────────────

/// The Hub's attestation signing key, as published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    /// Raw Ed25519 public key bytes.
    pub bytes: Vec<u8>,
    /// The Hub's identifier for it. Comparing it across two fetches is how a
    /// consumer tells a key ROTATION from a genuinely bad signature.
    pub key_id: String,
}

/// Why a published key document could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PubkeyError {
    #[error("pubkey document is not JSON: {0}")]
    Malformed(String),
    #[error("unsupported signature algorithm {0:?}")]
    UnsupportedAlg(String),
    #[error("pubkey document has no {0}")]
    MissingField(&'static str),
    #[error("public_key is not base64url: {0}")]
    BadEncoding(String),
}

/// Where the Ed25519 public key for `title` is published: a directory-zone GET,
/// which the Hub serves without a forward-proxy credential — a backend has to be
/// able to read it before it trusts anything signed with it.
pub fn pubkey_url(title: &str) -> String {
    format!("http://{title}.auto.mnn/v1/attest/pubkey")
}

/// Parse the document [`pubkey_url`] serves.
///
/// Kept separate from fetching it so the transport is the consumer's business
/// (this module opens no sockets) and the parsing is testable without one. An
/// unexpected `alg` is an error rather than an attempt to read the bytes as
/// Ed25519 anyway: a Hub that rotated to another algorithm needs a build that
/// knows it.
pub fn parse_pubkey_document(json: &[u8]) -> Result<PublicKey, PubkeyError> {
    let v: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| PubkeyError::Malformed(e.to_string()))?;
    let alg = v
        .get("alg")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if alg != ALG {
        return Err(PubkeyError::UnsupportedAlg(alg.to_string()));
    }
    let encoded = v
        .get("public_key")
        .and_then(serde_json::Value::as_str)
        .ok_or(PubkeyError::MissingField("public_key"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|e| PubkeyError::BadEncoding(e.to_string()))?;
    let key_id = v
        .get("key_id")
        .and_then(serde_json::Value::as_str)
        .ok_or(PubkeyError::MissingField("key_id"))?
        .to_string();
    Ok(PublicKey { bytes, key_id })
}

// ── digests ──────────────────────────────────────────────────────────────────

/// SHA-256 as lower-case hex — the one spelling, so two sides of a
/// `request_hash` cannot drift onto different encodings.
pub fn sha256_hex(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Wall clock in unix **seconds** — the unit `iat` / `exp` are in.
pub fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod test_signer {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair as _};

    /// A throwaway signer on the same `ring` primitives the Hub signs with, so
    /// these tests exercise the real signature path rather than a stand-in.
    pub(crate) struct TestSigner {
        keypair: Ed25519KeyPair,
    }

    impl TestSigner {
        pub(crate) fn new() -> Self {
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("ed25519 keygen");
            TestSigner {
                keypair: Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
                    .expect("generated pkcs8 parses"),
            }
        }
        pub(crate) fn public_key(&self) -> Vec<u8> {
            self.keypair.public_key().as_ref().to_vec()
        }
        pub(crate) fn sign(&self, msg: &[u8]) -> Vec<u8> {
            self.keypair.sign(msg).as_ref().to_vec()
        }
        /// Mint a token the way the Hub does.
        pub(crate) fn issue(&self, claims: &serde_json::Value) -> String {
            let json = serde_json::to_vec(claims).unwrap();
            format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(&json),
                URL_SAFE_NO_PAD.encode(self.sign(&signing_bytes(&json)))
            )
        }
    }

    pub(crate) const NOW: u64 = 1_700_000_000;

    /// The audience in [`claims_json`]. A placeholder on purpose — no real
    /// consumer's name belongs in this module, including in its fixtures.
    pub(crate) const TEST_AUDIENCE: &str = "test-audience";

    /// A well-formed claims blob, as the Hub would emit it.
    pub(crate) fn claims_json(now: u64) -> serde_json::Value {
        serde_json::json!({
            "v": CLAIMS_VERSION,
            "attester_kind": "minecraft",
            "attester_id": "mc1",
            "subject": "550e8400-e29b-41d4-a716-446655440000",
            "account_id": "8f14e45f-ceea-467a-9d24-8bd8e2a5f2ab",
            "online_mode": true,
            "audience": TEST_AUDIENCE,
            "challenge": "nonce-1",
            "request_hash": "sha256:deadbeef",
            "iat": now,
            "exp": now + 120,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test_signer::*;
    use super::*;

    #[test]
    fn signing_context_is_pinned() {
        // The domain separation, pinned: the SAME hub key also signs MDP
        // capabilities, so a signature over the bare claims (no context) must not
        // verify here.
        let signer = TestSigner::new();
        let c = claims_json(NOW);
        assert!(verify(&signer.issue(&c), &signer.public_key(), NOW + 1).is_ok());

        let json = serde_json::to_vec(&c).unwrap();
        let bare = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&json),
            URL_SAFE_NO_PAD.encode(signer.sign(&json))
        );
        assert_eq!(
            verify(&bare, &signer.public_key(), NOW + 1),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn an_unknown_claims_version_is_refused_even_when_signed() {
        // A future Hub's assertion, correctly signed: the version gate must still
        // refuse it rather than read a shape it does not know.
        let signer = TestSigner::new();
        let mut c = claims_json(NOW);
        c["v"] = serde_json::json!(CLAIMS_VERSION + 1);
        assert_eq!(
            verify(&signer.issue(&c), &signer.public_key(), NOW + 1),
            Err(VerifyError::UnsupportedVersion(CLAIMS_VERSION + 1))
        );
    }

    #[test]
    fn an_expired_assertion_is_refused() {
        let signer = TestSigner::new();
        let token = signer.issue(&claims_json(NOW));
        let pk = signer.public_key();
        assert!(verify(&token, &pk, NOW + 119).is_ok());
        // `exp` is the first instant it is no longer valid.
        assert_eq!(verify(&token, &pk, NOW + 120), Err(VerifyError::Expired));
    }

    #[test]
    fn a_tampered_claims_segment_is_refused() {
        let signer = TestSigner::new();
        let token = signer.issue(&claims_json(NOW));
        let (_, sig_b64) = token.split_once('.').unwrap();

        // Rewrite the subject to another player and re-encode — exactly what the
        // signature exists to stop, and why the signed bytes are the transmitted
        // bytes rather than a re-serialization.
        let mut forged = claims_json(NOW);
        forged["subject"] = serde_json::json!("00000000-0000-0000-0000-0000000000ff");
        let forged_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        assert_eq!(
            verify(
                &format!("{forged_b64}.{sig_b64}"),
                &signer.public_key(),
                NOW + 1
            ),
            Err(VerifyError::BadSignature)
        );

        // A valid assertion from a DIFFERENT hub key likewise.
        let other = TestSigner::new();
        assert_eq!(
            verify(&token, &other.public_key(), NOW + 1),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn a_malformed_assertion_is_refused_without_panicking() {
        let signer = TestSigner::new();
        let pk = signer.public_key();
        for bad in ["", ".", "notbase64!.notbase64!", "aGVsbG8", "a.b.c"] {
            assert!(
                matches!(
                    verify(bad, &pk, NOW),
                    Err(VerifyError::Malformed) | Err(VerifyError::BadSignature)
                ),
                "{bad:?} was accepted"
            );
        }
    }

    fn policy(request_hash: &str) -> ClaimPolicy<'_> {
        ClaimPolicy {
            audience: TEST_AUDIENCE,
            require_attester_kind: Some("minecraft"),
            require_online_mode: true,
            require_routable_attester_id: true,
            request_hash,
        }
    }

    fn check(json: serde_json::Value) -> Result<(), ClaimRejection> {
        let claims: AttestClaims = serde_json::from_value(json).unwrap();
        let hash = claims.request_hash.clone();
        check_claims(&claims, &policy(&hash))
    }

    #[test]
    fn a_policy_admits_exactly_what_it_names() {
        assert_eq!(check(claims_json(NOW)), Ok(()));

        let mut aud = claims_json(NOW);
        aud["audience"] = serde_json::json!("piggleshop");
        assert_eq!(check(aud), Err(ClaimRejection::Audience));

        let mut kind = claims_json(NOW);
        kind["attester_kind"] = serde_json::json!("desktop");
        assert_eq!(check(kind), Err(ClaimRejection::AttesterKind));

        let mut offline = claims_json(NOW);
        offline["online_mode"] = serde_json::json!(false);
        assert_eq!(check(offline), Err(ClaimRejection::OfflineAttester));

        let claims: AttestClaims = serde_json::from_value(claims_json(NOW)).unwrap();
        assert_eq!(
            check_claims(&claims, &policy("some-other-digest")),
            Err(ClaimRejection::RequestHash)
        );
    }

    #[test]
    fn an_optional_policy_field_is_genuinely_optional() {
        // A consumer that does not care about the host kind must be able to say
        // so, rather than be forced to name one.
        let claims: AttestClaims = serde_json::from_value(claims_json(NOW)).unwrap();
        let hash = claims.request_hash.clone();
        let lax = ClaimPolicy {
            audience: TEST_AUDIENCE,
            require_attester_kind: None,
            require_online_mode: false,
            require_routable_attester_id: false,
            request_hash: &hash,
        };
        assert_eq!(check_claims(&claims, &lax), Ok(()));
    }

    #[test]
    fn an_unroutable_attester_id_is_refused() {
        // A consumer builds `<host>.<attester_id>.mnn` from this, so anything
        // that is not a single routable label must not reach its URL builder —
        // `cs` in particular names the Hub's own backend zone.
        for bad in [
            "", "mc 1", "mc.1", "MC1", "-mc", "mc-", "auto", "cs", "usermail",
        ] {
            assert!(!is_routable_server_label(bad), "{bad:?} was accepted");
            let mut c = claims_json(NOW);
            c["attester_id"] = serde_json::json!(bad);
            assert_eq!(check(c), Err(ClaimRejection::AttesterId), "{bad:?}");
        }
        for good in ["mc1", "a", "my-server-2"] {
            assert!(is_routable_server_label(good), "{good:?} was refused");
            let mut c = claims_json(NOW);
            c["attester_id"] = serde_json::json!(good);
            assert_eq!(check(c), Ok(()), "{good:?} was refused");
        }
    }

    #[test]
    fn the_sha256_spelling_is_pinned() {
        // A known vector, so this spelling and a consumer's (in another language)
        // cannot drift apart silently.
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex("").len(), 64);
    }

    #[test]
    fn the_pubkey_url_is_the_directory_zone() {
        // 3 labels ⇒ the `<title>.auto.mnn` DIRECTORY zone (Hub-terminated), not
        // an auto-routed command to some server.
        assert_eq!(
            pubkey_url("minecraft"),
            "http://minecraft.auto.mnn/v1/attest/pubkey"
        );
    }

    #[test]
    fn a_pubkey_document_round_trips_and_fails_closed() {
        let signer = TestSigner::new();
        let raw = signer.public_key();
        let doc = serde_json::json!({
            "alg": "ed25519",
            "public_key": URL_SAFE_NO_PAD.encode(&raw),
            "key_id": "abc123",
        })
        .to_string();
        let parsed = parse_pubkey_document(doc.as_bytes()).unwrap();
        assert_eq!(parsed.bytes, raw);
        assert_eq!(parsed.key_id, "abc123");

        // Every malformed shape is an error, never a key we then "try anyway".
        assert!(matches!(
            parse_pubkey_document(b"not json"),
            Err(PubkeyError::Malformed(_))
        ));
        let wrong_alg =
            serde_json::json!({ "alg": "rsa", "public_key": "AA", "key_id": "k" }).to_string();
        assert_eq!(
            parse_pubkey_document(wrong_alg.as_bytes()),
            Err(PubkeyError::UnsupportedAlg("rsa".to_string()))
        );
        let no_key = serde_json::json!({ "alg": "ed25519", "key_id": "k" }).to_string();
        assert_eq!(
            parse_pubkey_document(no_key.as_bytes()),
            Err(PubkeyError::MissingField("public_key"))
        );
        let no_id = serde_json::json!({ "alg": "ed25519", "public_key": "AA" }).to_string();
        assert_eq!(
            parse_pubkey_document(no_id.as_bytes()),
            Err(PubkeyError::MissingField("key_id"))
        );
        let bad_b64 =
            serde_json::json!({ "alg": "ed25519", "public_key": "!!!", "key_id": "k" }).to_string();
        assert!(matches!(
            parse_pubkey_document(bad_b64.as_bytes()),
            Err(PubkeyError::BadEncoding(_))
        ));
    }
}
