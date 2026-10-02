//! Audit events with a stable, secret-free serialisation.

use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::authz::{Action, Decision};
use crate::grant::Resource;
use crate::principal::Principal;
use crate::token::looks_like_token;

const MAX_REQUEST_ID: usize = 128;

/// Errors building or serialising audit events.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuditError {
    /// The request id is empty, too long, uses characters outside
    /// `[A-Za-z0-9._:-]`, or looks like an API token.
    #[error("request id must be 1-128 characters of [A-Za-z0-9._:-] and must not be a token")]
    InvalidRequestId,
    /// The timestamp cannot be formatted (year out of range).
    #[error("audit timestamp out of range")]
    Timestamp,
}

/// A correlation id for one request. Restricted to a safe alphabet so it
/// cannot carry arbitrary (possibly secret) text into the audit log.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct RequestId(String);

impl RequestId {
    /// Validates `value`.
    ///
    /// # Errors
    /// [`AuditError::InvalidRequestId`] for text that is empty, over 128
    /// characters, outside `[A-Za-z0-9._:-]`, or a well-formed API token.
    pub fn new(value: impl Into<String>) -> Result<Self, AuditError> {
        let value = value.into();
        let ok = !value.is_empty()
            && value.len() <= MAX_REQUEST_ID
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".:_-".contains(&b))
            && !looks_like_token(&value);
        if ok {
            Ok(Self(value))
        } else {
            Err(AuditError::InvalidRequestId)
        }
    }

    /// The id text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One authorization outcome, ready to append to the audit log.
///
/// By construction it holds identifiers, enum codes and a validated request
/// id; it has no field that can carry a token or other secret.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AuditEvent {
    /// When the decision was made.
    pub at: OffsetDateTime,
    /// Who acted.
    pub actor: Principal,
    /// What they tried.
    pub action: Action,
    /// What they tried it on.
    pub resource: Resource,
    /// The outcome.
    pub decision: Decision,
    /// Correlation id of the request.
    pub request_id: RequestId,
}

impl AuditEvent {
    /// Serialises to one line of JSON with a fixed key order and format:
    ///
    /// ```text
    /// {"v":1,"at":"<RFC 3339 UTC, whole seconds>","actor":"…","action":"…",
    ///  "resource":"…","allowed":true,"reason":"…","request_id":"…"}
    /// ```
    ///
    /// The format is versioned by `"v"`; fields are only ever appended.
    ///
    /// # Errors
    /// [`AuditError::Timestamp`] when `at` cannot be formatted.
    pub fn to_json_line(&self) -> Result<String, AuditError> {
        let at = self
            .at
            .to_offset(UtcOffset::UTC)
            .replace_nanosecond(0)
            .map_err(|_| AuditError::Timestamp)?
            .format(&Rfc3339)
            .map_err(|_| AuditError::Timestamp)?;
        let mut out = String::with_capacity(256);
        out.push_str("{\"v\":1");
        push_field(&mut out, "at", &at);
        push_field(&mut out, "actor", &self.actor.to_string());
        push_field(&mut out, "action", &self.action.to_string());
        push_field(&mut out, "resource", &self.resource.to_string());
        out.push_str(",\"allowed\":");
        out.push_str(if self.decision.allowed {
            "true"
        } else {
            "false"
        });
        push_field(&mut out, "reason", self.decision.reason.code());
        push_field(&mut out, "request_id", self.request_id.as_str());
        out.push('}');
        Ok(out)
    }
}

fn push_field(out: &mut String, key: &str, value: &str) {
    out.push_str(",\"");
    out.push_str(key);
    out.push_str("\":\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::{DecisionReason, TokenScopes, authorize};
    use crate::grant::{Grant, GrantSet, ResourceScope, Role};
    use crate::principal::testutil::*;
    use crate::token::{Pepper, issue_token};
    use knowell_core::Name;
    use pretty_assertions::assert_eq;
    use time::macros::datetime;

    fn event(allowed: bool, reason: DecisionReason) -> AuditEvent {
        AuditEvent {
            at: datetime!(2026-10-02 12:34:56.789 +03:00),
            actor: user(1),
            action: Action::ReadCode,
            resource: Resource::project(Name::new("w").unwrap(), Name::new("p").unwrap()),
            decision: Decision { allowed, reason },
            request_id: RequestId::new("req-42").unwrap(),
        }
    }

    #[test]
    fn serialisation_is_stable() {
        assert_eq!(
            event(true, DecisionReason::Granted).to_json_line().unwrap(),
            "{\"v\":1,\"at\":\"2026-10-02T09:34:56Z\",\
             \"actor\":\"user:00000000-0000-0000-0000-000000000001\",\
             \"action\":\"read_code\",\"resource\":\"project:w/p\",\
             \"allowed\":true,\"reason\":\"granted\",\"request_id\":\"req-42\"}"
        );
        let denied = event(false, DecisionReason::DeniedNoGrant);
        assert!(denied.to_json_line().unwrap().contains("\"allowed\":false"));
    }

    #[test]
    fn overlay_and_agent_text() {
        let mut e = event(false, DecisionReason::DeniedOverlayPrivate);
        e.actor = agent_of(2);
        e.action = Action::ReadUncommittedOverlay(uid(1));
        let line = e.to_json_line().unwrap();
        assert!(
            line.contains("\"actor\":\"agent:00000000-0000-0000-0000-000000000002:test-agent:")
        );
        assert!(line.contains(
            "\"action\":\"read_uncommitted_overlay:00000000-0000-0000-0000-000000000001\""
        ));
    }

    #[test]
    fn request_id_validation() {
        for ok in ["a", "req-1", "01J:abc.def_ghi", &"x".repeat(128)] {
            assert!(RequestId::new(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "has space",
            "quo\"te",
            "new\nline",
            "é",
            &"x".repeat(129),
        ] {
            assert_eq!(
                RequestId::new(bad),
                Err(AuditError::InvalidRequestId),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn token_cannot_become_request_id_or_appear_in_audit() {
        let pepper = Pepper::new(b"fake-pepper-for-tests-0123456789").unwrap();
        let (plain, _) = issue_token(
            &user(1),
            TokenScopes::read_only(),
            None,
            datetime!(2026-10-02 12:00 UTC),
            &pepper,
        )
        .unwrap();
        assert_eq!(
            RequestId::new(plain.expose()),
            Err(AuditError::InvalidRequestId)
        );
        // A full authorize -> audit flow never mentions the token.
        let mut grants = GrantSet::new();
        grants.add(Grant::new(user(1), Role::Viewer, ResourceScope::Organization).unwrap());
        let resource = Resource::Organization;
        let decision = authorize(&user(1), Action::ReadCode, &resource, &grants);
        let e = AuditEvent {
            at: datetime!(2026-10-02 12:00 UTC),
            actor: user(1),
            action: Action::ReadCode,
            resource,
            decision,
            request_id: RequestId::new("r1").unwrap(),
        };
        let line = e.to_json_line().unwrap();
        let payload = &plain.expose()[3..55];
        assert!(!line.contains(payload) && !format!("{e:?}").contains(payload));
    }

    #[test]
    fn json_escaping() {
        let mut out = String::new();
        push_field(&mut out, "k", "a\"b\\c\n\u{1}");
        assert_eq!(out, ",\"k\":\"a\\\"b\\\\c\\u000a\\u0001\"");
    }
}
