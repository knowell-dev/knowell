//! API tokens.
//!
//! # Format
//!
//! ```text
//! kn_<payload:52><checksum:8>          (63 characters)
//! ```
//!
//! * `payload`: 256 random bits from the OS, lowercase RFC 4648 base32
//!   without padding (52 characters, canonical form only).
//! * `checksum`: the first 5 bytes of
//!   `BLAKE3-derive-key("knowell api token checksum v1", payload bytes)`,
//!   base32 (8 characters). It detects typos and lets secret scanners
//!   recognise a leaked token with a negligible false-positive rate, using the
//!   `kn_` prefix plus the checksum; it is not a security boundary.
//!
//! # Storage
//!
//! The server keeps only a [`StoredToken`]: the keyed BLAKE3 hash of the
//! payload under a server-side [`Pepper`], and a lookup prefix (the first 8
//! payload characters, 40 bits). The prefix is not unique-by-construction;
//! storage returns every record with the prefix and [`verify`] picks the
//! right one by hash. A database leak alone does not reveal usable tokens, and
//! without the pepper the hashes cannot be brute-forced offline.
//!
//! # Narrowing
//!
//! Agent tokens are narrower by construction: they cannot carry the
//! [`TokenScope::Admin`] scope, must expire, and live at most
//! [`MAX_AGENT_TOKEN_LIFETIME`].

use std::fmt;

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::authz::{TokenScope, TokenScopes};
use crate::encoding::{base32_decode, base32_encode, ct_eq, random_bytes};
use crate::principal::Principal;

/// Text prefix of every token.
pub const TOKEN_PREFIX: &str = "kn_";
/// Longest lifetime an agent token may be issued for.
pub const MAX_AGENT_TOKEN_LIFETIME: Duration = Duration::hours(24);

const PAYLOAD_BYTES: usize = 32;
const PAYLOAD_CHARS: usize = 52;
const CHECK_BYTES: usize = 5;
const CHECK_CHARS: usize = 8;
const LOOKUP_CHARS: usize = 8;
const MIN_PEPPER_BYTES: usize = 16;

/// Errors from issuing or verifying tokens. Messages never contain token
/// text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// The operating system could not supply random bytes.
    #[error("operating system randomness is unavailable")]
    Entropy,
    /// The pepper is shorter than 16 bytes.
    #[error("token pepper must be at least 16 bytes")]
    PepperTooShort,
    /// The expiry is not in the future.
    #[error("token expiry must be in the future")]
    AlreadyExpired,
    /// Agent tokens must expire.
    #[error("agent tokens must have an expiry")]
    ExpiryRequired,
    /// Agent token lifetime exceeds [`MAX_AGENT_TOKEN_LIFETIME`].
    #[error("agent tokens may live at most 24 hours")]
    ExpiryTooLong,
    /// Agent tokens cannot carry the admin scope.
    #[error("agent tokens cannot carry the admin scope")]
    AgentAdminScope,
    /// The presented text is not a well-formed token.
    #[error("malformed token")]
    Malformed,
    /// The token does not match the stored record.
    #[error("invalid token")]
    Invalid,
    /// The token was revoked.
    #[error("token has been revoked")]
    Revoked,
    /// The token has expired.
    #[error("token has expired")]
    Expired,
}

/// Server-side secret mixed into token hashes (keyed BLAKE3). Supplied by the
/// caller from its secret store; never stored next to the hashes.
#[derive(Clone)]
pub struct Pepper([u8; 32]);

impl Pepper {
    /// Derives the hashing key from caller-supplied secret bytes.
    ///
    /// # Errors
    /// [`TokenError::PepperTooShort`] for fewer than 16 bytes.
    pub fn new(secret: &[u8]) -> Result<Self, TokenError> {
        if secret.len() < MIN_PEPPER_BYTES {
            return Err(TokenError::PepperTooShort);
        }
        let mut hasher = blake3::Hasher::new_derive_key("knowell api token pepper v1");
        hasher.update(secret);
        Ok(Self(*hasher.finalize().as_bytes()))
    }

    fn hash(&self, payload: &[u8]) -> TokenHash {
        let mut hasher = blake3::Hasher::new_keyed(&self.0);
        hasher.update(b"knowell api token v1");
        hasher.update(payload);
        TokenHash(hasher.finalize())
    }
}

impl fmt::Debug for Pepper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pepper([redacted])")
    }
}

/// A freshly issued token, shown to its owner exactly once. `Debug` redacts
/// it; there is deliberately no `Display`, `Clone` or `Serialize`.
pub struct PlaintextToken(String);

impl PlaintextToken {
    /// The secret token text. Call this only to hand the token to its owner.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PlaintextToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PlaintextToken([redacted])")
    }
}

/// Keyed hash of a token payload. Equality is constant-time.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TokenHash(blake3::Hash);

impl TokenHash {
    /// Wraps a keyed hash read back from storage. The bytes are the output of
    /// [`TokenHash::as_bytes`]; nothing is hashed here.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(blake3::Hash::from_bytes(bytes))
    }

    /// The 32 hash bytes, for storage. Not secret on its own (it is a keyed
    /// hash), but never log it.
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenHash([redacted])")
    }
}

impl Serialize for TokenHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.to_hex().as_str())
    }
}

impl<'de> Deserialize<'de> for TokenHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        blake3::Hash::from_hex(&text)
            .map(Self)
            .map_err(|_| serde::de::Error::custom("invalid token hash"))
    }
}

/// Identifier of a stored token (safe to log and show).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenId(Uuid);

impl TokenId {
    /// Wraps an id read back from storage.
    pub fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// The underlying UUID.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.hyphenated())
    }
}

/// What the server persists for a token. Contains no plaintext.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StoredToken {
    /// Record identifier.
    pub id: TokenId,
    /// Lookup prefix: `kn_` plus the first 8 payload characters.
    pub prefix: String,
    /// Keyed hash of the payload.
    pub hash: TokenHash,
    /// Who the token authenticates as.
    pub principal: Principal,
    /// What the token may do at most.
    pub scopes: TokenScopes,
    /// Issue time.
    pub created_at: OffsetDateTime,
    /// Expiry; always set for agent tokens.
    pub expires_at: Option<OffsetDateTime>,
    /// Revocation time, if revoked.
    pub revoked_at: Option<OffsetDateTime>,
}

impl StoredToken {
    /// Revokes the token. Idempotent: the earliest revocation time is kept.
    pub fn revoke(&mut self, at: OffsetDateTime) {
        match self.revoked_at {
            Some(existing) if existing <= at => {}
            _ => self.revoked_at = Some(at),
        }
    }

    /// True when the token is revoked or expired at `now`.
    pub fn is_inactive(&self, now: OffsetDateTime) -> bool {
        self.revoked_at.is_some_and(|r| r <= now) || self.expires_at.is_some_and(|e| e <= now)
    }
}

/// A token that passed [`verify`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VerifiedToken {
    /// The stored record's id.
    pub token_id: TokenId,
    /// Who the token authenticates as.
    pub principal: Principal,
    /// What the token may do at most; pass to
    /// [`authorize_token`](crate::authorize_token).
    pub scopes: TokenScopes,
}

struct Parsed {
    payload: Vec<u8>,
    prefix: String,
}

fn checksum(payload: &[u8]) -> [u8; CHECK_BYTES] {
    let mut hasher = blake3::Hasher::new_derive_key("knowell api token checksum v1");
    hasher.update(payload);
    let digest = hasher.finalize();
    let mut out = [0u8; CHECK_BYTES];
    for (dst, src) in out.iter_mut().zip(digest.as_bytes()) {
        *dst = *src;
    }
    out
}

fn parse(text: &str) -> Option<Parsed> {
    let body = text.strip_prefix(TOKEN_PREFIX)?;
    if body.len() != PAYLOAD_CHARS + CHECK_CHARS || !body.is_ascii() {
        return None;
    }
    let (payload_text, check_text) = body.split_at_checked(PAYLOAD_CHARS)?;
    let payload = base32_decode(payload_text)?;
    let check = base32_decode(check_text)?;
    if payload.len() != PAYLOAD_BYTES || !ct_eq(&check, &checksum(&payload)) {
        return None;
    }
    let lookup = payload_text.get(..LOOKUP_CHARS)?;
    Some(Parsed {
        payload,
        prefix: format!("{TOKEN_PREFIX}{lookup}"),
    })
}

/// True when `text` is exactly a well-formed Knowell token (prefix, canonical
/// payload and valid checksum). Used by scanners and by [`crate::RequestId`]
/// to keep tokens out of logs.
pub fn looks_like_token(text: &str) -> bool {
    parse(text).is_some()
}

/// The lookup prefix of a presented token, for finding candidate
/// [`StoredToken`]s. `None` for malformed text.
pub fn token_prefix(presented: &str) -> Option<String> {
    parse(presented).map(|p| p.prefix)
}

/// Issues a token for `principal`.
///
/// Returns the plaintext (hand it to the owner once; never store or log it)
/// and the [`StoredToken`] to persist.
///
/// # Errors
/// * [`TokenError::AlreadyExpired`] when `expires_at <= now`.
/// * For agent principals: [`TokenError::AgentAdminScope`],
///   [`TokenError::ExpiryRequired`], [`TokenError::ExpiryTooLong`].
/// * [`TokenError::Entropy`] when the OS cannot supply randomness.
pub fn issue_token(
    principal: &Principal,
    scopes: TokenScopes,
    expires_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
    pepper: &Pepper,
) -> Result<(PlaintextToken, StoredToken), TokenError> {
    if expires_at.is_some_and(|e| e <= now) {
        return Err(TokenError::AlreadyExpired);
    }
    if principal.is_agent() {
        if scopes.contains(TokenScope::Admin) {
            return Err(TokenError::AgentAdminScope);
        }
        let expiry = expires_at.ok_or(TokenError::ExpiryRequired)?;
        if expiry - now > MAX_AGENT_TOKEN_LIFETIME {
            return Err(TokenError::ExpiryTooLong);
        }
    }

    let payload: [u8; PAYLOAD_BYTES] = random_bytes().map_err(|_| TokenError::Entropy)?;
    let id_bytes: [u8; 16] = random_bytes().map_err(|_| TokenError::Entropy)?;
    let payload_text = base32_encode(&payload);
    let text = format!(
        "{TOKEN_PREFIX}{payload_text}{}",
        base32_encode(&checksum(&payload))
    );
    let lookup = payload_text.get(..LOOKUP_CHARS).unwrap_or_default();
    let stored = StoredToken {
        id: TokenId(uuid::Builder::from_random_bytes(id_bytes).into_uuid()),
        prefix: format!("{TOKEN_PREFIX}{lookup}"),
        hash: pepper.hash(&payload),
        principal: principal.clone(),
        scopes,
        created_at: now,
        expires_at,
        revoked_at: None,
    };
    Ok((PlaintextToken(text), stored))
}

/// Verifies a presented token against its stored record at time `now`.
///
/// The hash comparison is constant-time and runs before the revocation and
/// expiry checks, so those statuses are revealed only to a holder of the real
/// token.
///
/// # Errors
/// [`TokenError::Malformed`] for ill-formed text, [`TokenError::Invalid`] when
/// the prefix or hash differ, then [`TokenError::Revoked`] or
/// [`TokenError::Expired`].
pub fn verify(
    presented: &str,
    stored: &StoredToken,
    pepper: &Pepper,
    now: OffsetDateTime,
) -> Result<VerifiedToken, TokenError> {
    let parsed = parse(presented).ok_or(TokenError::Malformed)?;
    let hash = pepper.hash(&parsed.payload);
    let hash_ok = hash == stored.hash;
    let prefix_ok = ct_eq(parsed.prefix.as_bytes(), stored.prefix.as_bytes());
    if !(hash_ok && prefix_ok) {
        return Err(TokenError::Invalid);
    }
    if stored.revoked_at.is_some_and(|r| r <= now) {
        return Err(TokenError::Revoked);
    }
    if stored.expires_at.is_some_and(|e| e <= now) {
        return Err(TokenError::Expired);
    }
    Ok(VerifiedToken {
        token_id: stored.id,
        principal: stored.principal.clone(),
        scopes: stored.scopes.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::principal::testutil::*;
    use time::macros::datetime;

    fn pepper() -> Pepper {
        Pepper::new(b"fake-pepper-for-tests-0123456789").unwrap()
    }

    fn now() -> OffsetDateTime {
        datetime!(2026-10-02 12:00 UTC)
    }

    fn issue_user() -> (PlaintextToken, StoredToken) {
        issue_token(
            &user(1),
            TokenScopes::read_only(),
            Some(now() + Duration::days(30)),
            now(),
            &pepper(),
        )
        .unwrap()
    }

    #[test]
    fn format_is_as_documented() {
        let (plain, stored) = issue_user();
        let text = plain.expose();
        assert_eq!(text.len(), 63);
        assert!(text.starts_with("kn_"));
        assert!(
            text.bytes()
                .skip(3)
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
        );
        assert_eq!(stored.prefix, format!("kn_{}", &text[3..11]));
        assert!(looks_like_token(text));
        assert_eq!(token_prefix(text), Some(stored.prefix.clone()));
    }

    #[test]
    fn tokens_are_unique() {
        let (a, sa) = issue_user();
        let (b, sb) = issue_user();
        assert_ne!(a.expose(), b.expose());
        assert_ne!(sa.id, sb.id);
        assert_ne!(sa.hash, sb.hash);
    }

    #[test]
    fn verify_roundtrip() {
        let (plain, stored) = issue_user();
        let v = verify(plain.expose(), &stored, &pepper(), now()).unwrap();
        assert_eq!(v.principal, user(1));
        assert_eq!(v.token_id, stored.id);
        assert!(v.scopes.contains(TokenScope::Read));
    }

    #[test]
    fn stored_token_has_no_plaintext() {
        let (plain, stored) = issue_user();
        let payload = &plain.expose()[3..55];
        let dump = format!("{stored:?}");
        assert!(!dump.contains(payload));
        let hex = stored.hash.0.to_hex();
        assert!(!hex.contains(payload));
        assert!(!stored.prefix.contains(&plain.expose()[11..]));
    }

    #[test]
    fn wrong_pepper_fails() {
        let (plain, stored) = issue_user();
        let other = Pepper::new(b"another-fake-pepper-0123456789ab").unwrap();
        assert_eq!(
            verify(plain.expose(), &stored, &other, now()),
            Err(TokenError::Invalid)
        );
    }

    #[test]
    fn wrong_token_fails() {
        let (_, stored) = issue_user();
        let (other, _) = issue_user();
        assert_eq!(
            verify(other.expose(), &stored, &pepper(), now()),
            Err(TokenError::Invalid)
        );
    }

    #[test]
    fn tamper_detection() {
        let (plain, stored) = issue_user();
        let original = plain.expose().to_owned();
        // Flip every character in turn to another alphabet symbol.
        for i in 3..original.len() {
            let mut bytes = original.clone().into_bytes();
            bytes[i] = if bytes[i] == b'a' { b'b' } else { b'a' };
            let tampered = String::from_utf8(bytes).unwrap();
            let r = verify(&tampered, &stored, &pepper(), now());
            assert!(r.is_err(), "position {i} accepted");
        }
    }

    #[test]
    fn malformed_inputs() {
        let (plain, stored) = issue_user();
        let t = plain.expose().to_owned();
        let upper = t.to_uppercase();
        let long = format!("{t}a");
        let cases = [
            String::new(),
            "kn_".to_owned(),
            t[3..].to_owned(),
            t[..62].to_owned(),
            long,
            upper,
            format!("xx_{}", &t[3..]),
            format!("{t}\n"),
            format!(" {t}"),
            format!("kn_{}", "é".repeat(30)),
        ];
        for case in cases {
            assert_eq!(
                verify(&case, &stored, &pepper(), now()),
                Err(TokenError::Malformed),
                "{case:?}"
            );
            assert!(!looks_like_token(&case));
            assert_eq!(token_prefix(&case), None);
        }
    }

    #[test]
    fn expiry() {
        let (plain, stored) = issue_user();
        let at = stored.expires_at.unwrap();
        assert!(
            verify(
                plain.expose(),
                &stored,
                &pepper(),
                at - Duration::seconds(1)
            )
            .is_ok()
        );
        assert_eq!(
            verify(plain.expose(), &stored, &pepper(), at),
            Err(TokenError::Expired)
        );
        assert!(stored.is_inactive(at));
        assert!(!stored.is_inactive(now()));
    }

    #[test]
    fn revocation() {
        let (plain, mut stored) = issue_user();
        let t1 = now() + Duration::hours(1);
        stored.revoke(t1);
        stored.revoke(t1 + Duration::hours(5));
        assert_eq!(stored.revoked_at, Some(t1));
        assert!(
            verify(
                plain.expose(),
                &stored,
                &pepper(),
                t1 - Duration::seconds(1)
            )
            .is_ok()
        );
        assert_eq!(
            verify(plain.expose(), &stored, &pepper(), t1),
            Err(TokenError::Revoked)
        );
    }

    #[test]
    fn issue_rejects_past_expiry() {
        let r = issue_token(
            &user(1),
            TokenScopes::read_only(),
            Some(now()),
            now(),
            &pepper(),
        );
        assert!(matches!(r, Err(TokenError::AlreadyExpired)));
    }

    #[test]
    fn user_token_may_be_non_expiring_and_admin() {
        let scopes = TokenScopes::new([TokenScope::Read, TokenScope::Admin]).unwrap();
        let (plain, stored) = issue_token(&user(1), scopes, None, now(), &pepper()).unwrap();
        assert!(stored.expires_at.is_none());
        let far = now() + Duration::days(3650);
        assert!(verify(plain.expose(), &stored, &pepper(), far).is_ok());
    }

    #[test]
    fn agent_tokens_are_narrower_by_construction() {
        let agent = agent_of(1);
        let admin = TokenScopes::new([TokenScope::Admin]).unwrap();
        let ok_expiry = Some(now() + Duration::hours(1));
        assert!(matches!(
            issue_token(&agent, admin, ok_expiry, now(), &pepper()),
            Err(TokenError::AgentAdminScope)
        ));
        assert!(matches!(
            issue_token(&agent, TokenScopes::read_only(), None, now(), &pepper()),
            Err(TokenError::ExpiryRequired)
        ));
        let long = Some(now() + Duration::hours(24) + Duration::seconds(1));
        assert!(matches!(
            issue_token(&agent, TokenScopes::read_only(), long, now(), &pepper()),
            Err(TokenError::ExpiryTooLong)
        ));
        let rw = TokenScopes::new([TokenScope::Read, TokenScope::Write]).unwrap();
        let exactly = Some(now() + Duration::hours(24));
        assert!(issue_token(&agent, rw, exactly, now(), &pepper()).is_ok());
    }

    #[test]
    fn debug_is_redacted() {
        let (plain, stored) = issue_user();
        let text = plain.expose().to_owned();
        let payload = &text[3..55];
        for dump in [
            format!("{plain:?}"),
            format!("{:?}", pepper()),
            format!("{stored:?}"),
            format!("{:#?}", stored.hash),
        ] {
            assert!(!dump.contains(payload), "{dump}");
            assert!(!dump.contains(&text), "{dump}");
        }
        assert_eq!(format!("{plain:?}"), "PlaintextToken([redacted])");
    }

    #[test]
    fn pepper_must_be_long_enough() {
        assert!(matches!(
            Pepper::new(b"short"),
            Err(TokenError::PepperTooShort)
        ));
    }

    #[test]
    fn errors_do_not_echo_input() {
        let (plain, stored) = issue_user();
        let mut bad = plain.expose().to_owned();
        bad.push('!');
        let e = verify(&bad, &stored, &pepper(), now()).unwrap_err();
        assert!(!e.to_string().contains(&bad));
    }

    #[test]
    fn stored_token_serde_roundtrip() {
        let (_, stored) = issue_user();
        // Hash serialises as hex and parses back.
        let hex = stored.hash.0.to_hex().to_string();
        assert_eq!(hex.len(), 64);
        assert!(blake3::Hash::from_hex(&hex).is_ok());
    }
}
