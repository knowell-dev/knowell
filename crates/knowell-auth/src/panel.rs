//! Web-panel security helpers: session ids, CSRF tokens and the
//! `Host` / `Origin` allow-list.
//!
//! # CSRF pattern
//!
//! Knowell uses a **session-bound, stateless synchronizer token**: the server
//! hands the page a token `nonce.mac`, where `mac` is keyed BLAKE3 over the
//! session id and the nonce under a server-side [`CsrfKey`]. The page echoes
//! it in a request header (`X-CSRF-Token`) on every state-changing request and
//! the server recomputes the MAC. Compared with plain double-submit cookies
//! there is nothing a network attacker or a sibling subdomain can plant (they
//! cannot forge a MAC), the token is useless under any other session, and the
//! server stores nothing. The comparison is constant-time. Tokens are valid for
//! the life of their session; rotate the session id on login and privilege
//! change.
//!
//! # DNS rebinding
//!
//! A rebinding page makes `evil.example` resolve to `127.0.0.1`; the browser
//! then sends requests to the panel with `Host: evil.example:<port>` and the
//! page's own `Origin`. [`OriginPolicy`] rejects any request whose `Host` is
//! not on the allow-list, which defeats rebinding regardless of what the name
//! resolves to, and requires a matching `Origin` on state-changing requests.

use std::collections::BTreeSet;
use std::fmt;

use crate::encoding::{base32_decode, base32_encode, ct_eq, random_bytes};

const SESSION_BYTES: usize = 32;
const NONCE_BYTES: usize = 16;
const MIN_KEY_BYTES: usize = 16;

/// Errors from panel-security helpers. Messages never echo request input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PanelError {
    /// The operating system could not supply random bytes.
    #[error("operating system randomness is unavailable")]
    Entropy,
    /// The CSRF key is shorter than 16 bytes.
    #[error("csrf key must be at least 16 bytes")]
    KeyTooShort,
    /// The text is not a session id.
    #[error("malformed session id")]
    MalformedSessionId,
    /// No CSRF token was presented.
    #[error("csrf token missing")]
    CsrfMissing,
    /// The CSRF token is malformed.
    #[error("csrf token malformed")]
    CsrfMalformed,
    /// The CSRF token does not match this session.
    #[error("csrf token invalid")]
    CsrfInvalid,
    /// The `Host` header is absent.
    #[error("host header missing")]
    MissingHost,
    /// The `Host` header is not on the allow-list.
    #[error("host not allowed")]
    HostNotAllowed,
    /// A state-changing request carries no `Origin` header.
    #[error("origin header missing")]
    MissingOrigin,
    /// The `Origin` header is not on the allow-list.
    #[error("origin not allowed")]
    OriginNotAllowed,
    /// An allow-list entry is not `host:port`.
    #[error("allowed host must look like `name:port` or `[v6]:port`")]
    InvalidAllowedHost,
}

/// A panel session identifier: 256 random bits, lowercase base32 (52
/// characters). `Debug` redacts it. Store only [`fingerprint`](Self::fingerprint)
/// server-side if the session table could leak.
#[derive(Clone, PartialEq, Eq)]
pub struct PanelSessionId(String);

impl PanelSessionId {
    /// Generates a fresh session id.
    ///
    /// # Errors
    /// [`PanelError::Entropy`] when the OS cannot supply randomness.
    pub fn generate() -> Result<Self, PanelError> {
        let bytes: [u8; SESSION_BYTES] = random_bytes().map_err(|_| PanelError::Entropy)?;
        Ok(Self(base32_encode(&bytes)))
    }

    /// Parses a session id presented in a cookie.
    ///
    /// # Errors
    /// [`PanelError::MalformedSessionId`] unless canonical 52-character base32.
    pub fn parse(text: &str) -> Result<Self, PanelError> {
        match base32_decode(text) {
            Some(bytes) if bytes.len() == SESSION_BYTES => Ok(Self(text.to_owned())),
            _ => Err(PanelError::MalformedSessionId),
        }
    }

    /// The cookie value. Expose only to set the cookie.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Hex BLAKE3 of the id, safe to use as a database key or log field.
    pub fn fingerprint(&self) -> String {
        blake3::hash(self.0.as_bytes()).to_hex().to_string()
    }

    /// Constant-time equality with another presented id.
    pub fn matches(&self, other: &PanelSessionId) -> bool {
        ct_eq(self.0.as_bytes(), other.0.as_bytes())
    }
}

impl fmt::Debug for PanelSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PanelSessionId([redacted])")
    }
}

/// Server-side key for CSRF MACs. `Debug` redacts it.
#[derive(Clone)]
pub struct CsrfKey([u8; 32]);

impl CsrfKey {
    /// Derives the MAC key from caller-supplied secret bytes (for the panel,
    /// a random value generated at start-up is enough).
    ///
    /// # Errors
    /// [`PanelError::KeyTooShort`] for fewer than 16 bytes.
    pub fn new(secret: &[u8]) -> Result<Self, PanelError> {
        if secret.len() < MIN_KEY_BYTES {
            return Err(PanelError::KeyTooShort);
        }
        let mut hasher = blake3::Hasher::new_derive_key("knowell panel csrf key v1");
        hasher.update(secret);
        Ok(Self(*hasher.finalize().as_bytes()))
    }

    /// A key from fresh OS randomness (for a per-process panel key).
    ///
    /// # Errors
    /// [`PanelError::Entropy`] when the OS cannot supply randomness.
    pub fn random() -> Result<Self, PanelError> {
        let bytes: [u8; 32] = random_bytes().map_err(|_| PanelError::Entropy)?;
        Self::new(&bytes)
    }

    fn mac(&self, session: &PanelSessionId, nonce: &[u8]) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new_keyed(&self.0);
        // Length-prefix the session so (session, nonce) splits are unambiguous.
        hasher.update(&(session.0.len() as u64).to_le_bytes());
        hasher.update(session.0.as_bytes());
        hasher.update(nonce);
        hasher.finalize()
    }
}

impl fmt::Debug for CsrfKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CsrfKey([redacted])")
    }
}

/// A CSRF token for one session: `<nonce base32>.<mac base32>`. `Debug`
/// redacts it.
pub struct CsrfToken(String);

impl CsrfToken {
    /// Generates a token bound to `session`.
    ///
    /// # Errors
    /// [`PanelError::Entropy`] when the OS cannot supply randomness.
    pub fn generate(key: &CsrfKey, session: &PanelSessionId) -> Result<Self, PanelError> {
        let nonce: [u8; NONCE_BYTES] = random_bytes().map_err(|_| PanelError::Entropy)?;
        let mac = key.mac(session, &nonce);
        Ok(Self(format!(
            "{}.{}",
            base32_encode(&nonce),
            base32_encode(mac.as_bytes())
        )))
    }

    /// The token text to embed in the page or return from the token endpoint.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Validates a presented token for `session`, in constant time.
    ///
    /// # Errors
    /// [`PanelError::CsrfMissing`] for `None` or empty,
    /// [`PanelError::CsrfMalformed`] for ill-formed text,
    /// [`PanelError::CsrfInvalid`] when the MAC does not match this session.
    pub fn validate(
        key: &CsrfKey,
        session: &PanelSessionId,
        presented: Option<&str>,
    ) -> Result<(), PanelError> {
        let presented = match presented {
            None | Some("") => return Err(PanelError::CsrfMissing),
            Some(text) => text,
        };
        let (nonce_text, mac_text) = presented.split_once('.').ok_or(PanelError::CsrfMalformed)?;
        let nonce = base32_decode(nonce_text)
            .filter(|n| n.len() == NONCE_BYTES)
            .ok_or(PanelError::CsrfMalformed)?;
        let mac = base32_decode(mac_text)
            .filter(|m| m.len() == 32)
            .ok_or(PanelError::CsrfMalformed)?;
        let expected = key.mac(session, &nonce);
        if ct_eq(expected.as_bytes(), &mac) {
            Ok(())
        } else {
            Err(PanelError::CsrfInvalid)
        }
    }
}

impl fmt::Debug for CsrfToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CsrfToken([redacted])")
    }
}

/// Allow-list for the `Host` and `Origin` headers.
///
/// Entries are exact `host:port` strings compared case-insensitively; there
/// are no wildcards, suffix matches or default-port elision, so
/// `127.0.0.1:8080.evil.com`, `localhost.:8080` and `localhost:8080@evil.com`
/// are all rejected.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OriginPolicy {
    hosts: BTreeSet<String>,
    allow_https: bool,
}

impl OriginPolicy {
    /// The default for the loopback-bound panel: `127.0.0.1:<port>`,
    /// `localhost:<port>` and `[::1]:<port>`, over `http` only.
    pub fn loopback(port: u16) -> Self {
        Self {
            hosts: BTreeSet::from([
                format!("127.0.0.1:{port}"),
                format!("localhost:{port}"),
                format!("[::1]:{port}"),
            ]),
            allow_https: false,
        }
    }

    /// Additionally allows an explicitly configured `host:port` (for a hub
    /// behind a named host).
    ///
    /// # Errors
    /// [`PanelError::InvalidAllowedHost`] unless the entry is a plain
    /// `name:port` or `[v6]:port` with a non-zero port and no wildcard.
    pub fn with_host(mut self, host_port: &str) -> Result<Self, PanelError> {
        let lower = host_port.to_ascii_lowercase();
        let valid_chars = !lower.is_empty()
            && lower
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-.:[]".contains(&b));
        let port_ok = lower.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p != 0)
        });
        if !(valid_chars && port_ok) {
            return Err(PanelError::InvalidAllowedHost);
        }
        self.hosts.insert(lower);
        Ok(self)
    }

    /// Also accepts `https://` origins (when the panel is served over TLS).
    pub fn with_https(mut self) -> Self {
        self.allow_https = true;
        self
    }

    /// Checks the `Host` header.
    ///
    /// # Errors
    /// [`PanelError::MissingHost`] or [`PanelError::HostNotAllowed`].
    pub fn check_host(&self, host: Option<&str>) -> Result<(), PanelError> {
        let host = host.ok_or(PanelError::MissingHost)?;
        if self.hosts.contains(&host.to_ascii_lowercase()) {
            Ok(())
        } else {
            Err(PanelError::HostNotAllowed)
        }
    }

    /// Checks the `Origin` header. An absent header passes unless
    /// `require_origin`; the literal `null` origin never passes.
    ///
    /// # Errors
    /// [`PanelError::MissingOrigin`] or [`PanelError::OriginNotAllowed`].
    pub fn check_origin(
        &self,
        origin: Option<&str>,
        require_origin: bool,
    ) -> Result<(), PanelError> {
        let Some(origin) = origin else {
            return if require_origin {
                Err(PanelError::MissingOrigin)
            } else {
                Ok(())
            };
        };
        let lower = origin.to_ascii_lowercase();
        let authority = lower.strip_prefix("http://").or_else(|| {
            self.allow_https
                .then(|| lower.strip_prefix("https://"))
                .flatten()
        });
        match authority {
            Some(a) if self.hosts.contains(a) => Ok(()),
            _ => Err(PanelError::OriginNotAllowed),
        }
    }

    /// Full request check: `Host` must always be allowed; `Origin` must be
    /// allowed when present and is mandatory when `state_changing`.
    ///
    /// # Errors
    /// The first failing check's error.
    pub fn check_request(
        &self,
        host: Option<&str>,
        origin: Option<&str>,
        state_changing: bool,
    ) -> Result<(), PanelError> {
        self.check_host(host)?;
        self.check_origin(origin, state_changing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> CsrfKey {
        CsrfKey::new(b"fake-csrf-secret-0123456789").unwrap()
    }

    #[test]
    fn session_ids() {
        let a = PanelSessionId::generate().unwrap();
        let b = PanelSessionId::generate().unwrap();
        assert_eq!(a.expose().len(), 52);
        assert!(!a.matches(&b));
        assert!(a.matches(&PanelSessionId::parse(a.expose()).unwrap()));
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint().len(), 64);
        assert!(!a.fingerprint().contains(a.expose()));
        assert_eq!(format!("{a:?}"), "PanelSessionId([redacted])");
        for bad in [
            "",
            "abc",
            &a.expose().to_uppercase(),
            &format!("{}a", a.expose()),
        ] {
            assert_eq!(
                PanelSessionId::parse(bad),
                Err(PanelError::MalformedSessionId)
            );
        }
    }

    #[test]
    fn csrf_roundtrip() {
        let s = PanelSessionId::generate().unwrap();
        let t = CsrfToken::generate(&key(), &s).unwrap();
        assert_eq!(CsrfToken::validate(&key(), &s, Some(t.expose())), Ok(()));
        let t2 = CsrfToken::generate(&key(), &s).unwrap();
        assert_ne!(t.expose(), t2.expose());
        assert_eq!(CsrfToken::validate(&key(), &s, Some(t2.expose())), Ok(()));
    }

    #[test]
    fn csrf_is_bound_to_session_and_key() {
        let s1 = PanelSessionId::generate().unwrap();
        let s2 = PanelSessionId::generate().unwrap();
        let t = CsrfToken::generate(&key(), &s1).unwrap();
        assert_eq!(
            CsrfToken::validate(&key(), &s2, Some(t.expose())),
            Err(PanelError::CsrfInvalid)
        );
        let other = CsrfKey::new(b"a-different-fake-secret-0123").unwrap();
        assert_eq!(
            CsrfToken::validate(&other, &s1, Some(t.expose())),
            Err(PanelError::CsrfInvalid)
        );
    }

    #[test]
    fn csrf_missing_malformed_tampered() {
        let s = PanelSessionId::generate().unwrap();
        let t = CsrfToken::generate(&key(), &s).unwrap();
        assert_eq!(
            CsrfToken::validate(&key(), &s, None),
            Err(PanelError::CsrfMissing)
        );
        assert_eq!(
            CsrfToken::validate(&key(), &s, Some("")),
            Err(PanelError::CsrfMissing)
        );
        for bad in ["nodot", ".", "a.b", "AAAA.BBBB", "..", "é.é"] {
            assert_eq!(
                CsrfToken::validate(&key(), &s, Some(bad)),
                Err(PanelError::CsrfMalformed),
                "{bad}"
            );
        }
        // Swap the nonce: the MAC no longer matches.
        let other = CsrfToken::generate(&key(), &s).unwrap();
        let (n1, _) = t.expose().split_once('.').unwrap();
        let (_, m2) = other.expose().split_once('.').unwrap();
        let spliced = format!("{n1}.{m2}");
        assert_eq!(
            CsrfToken::validate(&key(), &s, Some(&spliced)),
            Err(PanelError::CsrfInvalid)
        );
        // Flip each MAC character.
        let (n, m) = t.expose().split_once('.').unwrap();
        for i in 0..m.len() {
            let mut bytes = m.as_bytes().to_vec();
            bytes[i] = if bytes[i] == b'a' { b'b' } else { b'a' };
            let tampered = format!("{n}.{}", String::from_utf8(bytes).unwrap());
            assert!(CsrfToken::validate(&key(), &s, Some(&tampered)).is_err());
        }
    }

    #[test]
    fn csrf_debug_and_key_redacted() {
        let s = PanelSessionId::generate().unwrap();
        let t = CsrfToken::generate(&key(), &s).unwrap();
        assert_eq!(format!("{t:?}"), "CsrfToken([redacted])");
        assert_eq!(format!("{:?}", key()), "CsrfKey([redacted])");
        assert!(matches!(
            CsrfKey::new(b"short"),
            Err(PanelError::KeyTooShort)
        ));
        assert!(CsrfKey::random().is_ok());
    }

    fn policy() -> OriginPolicy {
        OriginPolicy::loopback(8731)
    }

    #[test]
    fn loopback_hosts_accepted() {
        for host in [
            "127.0.0.1:8731",
            "localhost:8731",
            "[::1]:8731",
            "LOCALHOST:8731",
        ] {
            assert_eq!(policy().check_host(Some(host)), Ok(()), "{host}");
        }
    }

    #[test]
    fn rebinding_attempts_rejected() {
        // evil.example resolves to 127.0.0.1, so the TCP connection succeeds,
        // but the browser still sends the attacker's name in Host.
        let p = policy();
        for host in [
            "evil.com",
            "evil.com:8731",
            "evil.example:8731",
            "127.0.0.1.evil.com:8731",
            "127.0.0.1:8731.evil.com",
            "localhost.:8731",
            "localhost:8731@evil.com",
            "evil.com@localhost:8731",
            "localhost",
            "127.0.0.1",
            "127.0.0.1:80",
            "localhost:8732",
            "0.0.0.0:8731",
            "[::ffff:127.0.0.1]:8731",
            "127.0.0.2:8731",
            " localhost:8731",
            "localhost:8731 ",
            "",
        ] {
            assert_eq!(
                p.check_host(Some(host)),
                Err(PanelError::HostNotAllowed),
                "{host:?}"
            );
        }
        assert_eq!(p.check_host(None), Err(PanelError::MissingHost));
        // Even with a same-looking Origin the foreign Host fails first.
        assert_eq!(
            p.check_request(Some("evil.com:8731"), Some("http://evil.com:8731"), true),
            Err(PanelError::HostNotAllowed)
        );
        assert_eq!(
            p.check_request(Some("evil.com:8731"), Some("http://127.0.0.1:8731"), true),
            Err(PanelError::HostNotAllowed)
        );
    }

    #[test]
    fn origin_checks() {
        let p = policy();
        for ok in [
            "http://127.0.0.1:8731",
            "http://localhost:8731",
            "http://[::1]:8731",
        ] {
            assert_eq!(p.check_origin(Some(ok), true), Ok(()), "{ok}");
        }
        for bad in [
            "null",
            "http://evil.com",
            "http://evil.com:8731",
            "https://localhost:8731",
            "http://localhost:8731/",
            "http://localhost:8731/path",
            "http://localhost:8731.evil.com",
            "http://user@localhost:8731",
            "ftp://localhost:8731",
            "localhost:8731",
            "",
        ] {
            assert_eq!(
                p.check_origin(Some(bad), true),
                Err(PanelError::OriginNotAllowed),
                "{bad}"
            );
        }
        assert_eq!(p.check_origin(None, true), Err(PanelError::MissingOrigin));
        assert_eq!(p.check_origin(None, false), Ok(()));
        // A present-but-bad Origin fails even on safe requests.
        assert!(p.check_origin(Some("http://evil.com"), false).is_err());
    }

    #[test]
    fn request_check() {
        let p = policy();
        assert_eq!(p.check_request(Some("localhost:8731"), None, false), Ok(()));
        assert_eq!(
            p.check_request(Some("localhost:8731"), None, true),
            Err(PanelError::MissingOrigin)
        );
        assert_eq!(
            p.check_request(Some("localhost:8731"), Some("http://localhost:8731"), true),
            Ok(())
        );
        assert_eq!(
            p.check_request(Some("localhost:8731"), Some("http://evil.com"), true),
            Err(PanelError::OriginNotAllowed)
        );
    }

    #[test]
    fn configured_hosts_and_https() {
        let p = policy().with_host("Knowell.Example.com:8443").unwrap();
        assert_eq!(p.check_host(Some("knowell.example.com:8443")), Ok(()));
        assert!(
            p.check_origin(Some("https://knowell.example.com:8443"), true)
                .is_err()
        );
        let p = p.with_https();
        assert_eq!(
            p.check_origin(Some("https://knowell.example.com:8443"), true),
            Ok(())
        );
        assert_eq!(p.check_origin(Some("https://localhost:8731"), true), Ok(()));
        for bad in [
            "*:80",
            "example.com",
            "example.com:0",
            ":80",
            "a b:80",
            "ex/ample.com:80",
            "example.com:99999",
            "",
        ] {
            assert_eq!(
                policy().with_host(bad),
                Err(PanelError::InvalidAllowedHost),
                "{bad}"
            );
        }
    }

    #[test]
    fn errors_do_not_echo_input() {
        let e = policy()
            .check_host(Some("secret-looking.evil.com"))
            .unwrap_err();
        assert!(!e.to_string().contains("evil"));
    }
}
