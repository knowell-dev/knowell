//! Panel sessions: an in-memory store keyed by the session id's
//! fingerprint, plus cookie formatting and parsing.
//!
//! The cookie is `HttpOnly; SameSite=Strict; Path=/`, and `Secure` with the
//! `__Host-` name prefix when the server is reached over https (the prefix
//! makes browsers refuse cookies set by sibling hosts or for other paths).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use axum::http::header::COOKIE;
use knowell_auth::{PanelSessionId, Principal, TokenScopes};
use time::OffsetDateTime;

use crate::config::SessionSettings;

/// Cookie name over plain http (loopback panel).
pub(crate) const COOKIE_NAME: &str = "knowell_session";
/// Cookie name over https.
pub(crate) const SECURE_COOKIE_NAME: &str = "__Host-knowell_session";

/// What a live session grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionInfo {
    pub(crate) principal: Principal,
    pub(crate) scopes: Option<TokenScopes>,
    pub(crate) expires_at: OffsetDateTime,
}

#[derive(Debug)]
struct Record {
    info: SessionInfo,
    created: Instant,
    last_seen: Instant,
}

/// Sessions of this process.
#[derive(Debug)]
pub(crate) struct SessionStore {
    settings: SessionSettings,
    records: Mutex<HashMap<String, Record>>,
}

impl SessionStore {
    pub(crate) fn new(settings: SessionSettings) -> Self {
        Self {
            settings,
            records: Mutex::new(HashMap::new()),
        }
    }

    /// Creates a session; evicts expired sessions and, at capacity, the least
    /// recently used one. `None` when the OS has no randomness or the lock
    /// is poisoned.
    pub(crate) fn create(
        &self,
        principal: Principal,
        scopes: Option<TokenScopes>,
    ) -> Option<(PanelSessionId, SessionInfo)> {
        self.create_at(principal, scopes, Instant::now())
    }

    fn create_at(
        &self,
        principal: Principal,
        scopes: Option<TokenScopes>,
        now: Instant,
    ) -> Option<(PanelSessionId, SessionInfo)> {
        let id = PanelSessionId::generate().ok()?;
        let lifetime = time::Duration::try_from(self.settings.absolute_lifetime).ok()?;
        let info = SessionInfo {
            principal,
            scopes,
            expires_at: OffsetDateTime::now_utc().checked_add(lifetime)?,
        };
        let mut records = self.records.lock().ok()?;
        records.retain(|_, r| !self.expired(r, now));
        while records.len() >= self.settings.max_sessions {
            let oldest = records
                .iter()
                .min_by_key(|(_, r)| r.last_seen)
                .map(|(k, _)| k.clone())?;
            records.remove(&oldest);
        }
        records.insert(
            id.fingerprint(),
            Record {
                info: info.clone(),
                created: now,
                last_seen: now,
            },
        );
        Some((id, info))
    }

    /// The live session for `id`, refreshing its idle timer.
    pub(crate) fn get(&self, id: &PanelSessionId) -> Option<SessionInfo> {
        self.get_at(id, Instant::now())
    }

    fn get_at(&self, id: &PanelSessionId, now: Instant) -> Option<SessionInfo> {
        let mut records = self.records.lock().ok()?;
        let key = id.fingerprint();
        let record = records.get_mut(&key)?;
        if self.expired(record, now) {
            records.remove(&key);
            return None;
        }
        record.last_seen = now;
        Some(record.info.clone())
    }

    /// Ends a session. Returns whether it existed.
    pub(crate) fn remove(&self, id: &PanelSessionId) -> bool {
        self.records
            .lock()
            .map(|mut r| r.remove(&id.fingerprint()).is_some())
            .unwrap_or(false)
    }

    fn expired(&self, record: &Record, now: Instant) -> bool {
        let idle = now.saturating_duration_since(record.last_seen);
        let age = now.saturating_duration_since(record.created);
        idle >= self.settings.idle_timeout || age >= self.settings.absolute_lifetime
    }
}

/// The `Set-Cookie` value establishing `id`.
pub(crate) fn set_cookie(id: &PanelSessionId, secure: bool, max_age: Duration) -> String {
    let name = if secure {
        SECURE_COOKIE_NAME
    } else {
        COOKIE_NAME
    };
    let secure_attr = if secure { "; Secure" } else { "" };
    format!(
        "{name}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure_attr}",
        id.expose(),
        max_age.as_secs()
    )
}

/// The `Set-Cookie` value deleting the session cookie.
pub(crate) fn clear_cookie(secure: bool) -> String {
    let name = if secure {
        SECURE_COOKIE_NAME
    } else {
        COOKIE_NAME
    };
    let secure_attr = if secure { "; Secure" } else { "" };
    format!("{name}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure_attr}")
}

/// Every well-formed session id presented in `Cookie` headers under the
/// expected cookie name, in header order. Malformed values are skipped.
pub(crate) fn session_ids(headers: &HeaderMap, secure: bool) -> Vec<PanelSessionId> {
    let name = if secure {
        SECURE_COOKIE_NAME
    } else {
        COOKIE_NAME
    };
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k == name).then(|| PanelSessionId::parse(v.trim()).ok())?
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use knowell_auth::UserId;
    use uuid::Uuid;

    use super::*;

    fn user() -> Principal {
        Principal::User(UserId::new(Uuid::from_u128(7)))
    }

    fn store(idle: u64, absolute: u64, max: usize) -> SessionStore {
        SessionStore::new(SessionSettings {
            idle_timeout: Duration::from_secs(idle),
            absolute_lifetime: Duration::from_secs(absolute),
            max_sessions: max,
        })
    }

    #[test]
    fn create_get_remove() {
        let s = store(60, 600, 10);
        let (id, info) = s.create(user(), None).unwrap();
        assert_eq!(s.get(&id), Some(info));
        assert!(s.remove(&id));
        assert_eq!(s.get(&id), None);
        assert!(!s.remove(&id));
    }

    #[test]
    fn idle_and_absolute_expiry() {
        let s = store(60, 600, 10);
        let t0 = Instant::now();
        let (id, _) = s.create_at(user(), None, t0).unwrap();
        assert!(s.get_at(&id, t0 + Duration::from_secs(59)).is_some());
        // Touched at 59 s, so still alive at 118 s.
        assert!(s.get_at(&id, t0 + Duration::from_secs(118)).is_some());
        assert!(s.get_at(&id, t0 + Duration::from_secs(179)).is_none());

        let (id, _) = s.create_at(user(), None, t0).unwrap();
        let mut t = t0;
        for _ in 0..11 {
            t += Duration::from_secs(50);
            assert!(s.get_at(&id, t).is_some());
        }
        // 650 s after creation: past the absolute lifetime despite activity.
        assert!(s.get_at(&id, t0 + Duration::from_secs(650)).is_none());
    }

    #[test]
    fn capacity_evicts_least_recently_used() {
        let s = store(600, 6000, 2);
        let t0 = Instant::now();
        let (a, _) = s.create_at(user(), None, t0).unwrap();
        let (b, _) = s
            .create_at(user(), None, t0 + Duration::from_secs(1))
            .unwrap();
        assert!(s.get_at(&a, t0 + Duration::from_secs(2)).is_some());
        let (c, _) = s
            .create_at(user(), None, t0 + Duration::from_secs(3))
            .unwrap();
        assert!(s.get_at(&a, t0 + Duration::from_secs(4)).is_some());
        assert!(s.get_at(&b, t0 + Duration::from_secs(4)).is_none());
        assert!(s.get_at(&c, t0 + Duration::from_secs(4)).is_some());
    }

    #[test]
    fn cookie_format() {
        let id = PanelSessionId::generate().unwrap();
        let plain = set_cookie(&id, false, Duration::from_secs(60));
        assert_eq!(
            plain,
            format!(
                "knowell_session={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=60",
                id.expose()
            )
        );
        let secure = set_cookie(&id, true, Duration::from_secs(60));
        assert!(secure.starts_with("__Host-knowell_session="));
        assert!(secure.ends_with("; Secure"));
        assert!(clear_cookie(false).contains("Max-Age=0"));
    }

    #[test]
    fn cookie_parsing_is_strict() {
        let id = PanelSessionId::generate().unwrap();
        let mut headers = HeaderMap::new();
        let line = format!(
            "theme=dark; knowell_session=not-an-id; knowell_session={}; __Host-knowell_session={}",
            id.expose(),
            id.expose()
        );
        headers.insert(COOKIE, HeaderValue::from_str(&line).unwrap());
        let plain = session_ids(&headers, false);
        assert_eq!(plain.len(), 1);
        assert!(plain[0].matches(&id));
        let secure = session_ids(&headers, true);
        assert_eq!(secure.len(), 1);
        headers.insert(COOKIE, HeaderValue::from_static("knowell_session"));
        assert!(session_ids(&headers, false).is_empty());
        headers.insert(COOKIE, HeaderValue::from_static(";;=;knowell_session=;"));
        assert!(session_ids(&headers, false).is_empty());
    }
}
