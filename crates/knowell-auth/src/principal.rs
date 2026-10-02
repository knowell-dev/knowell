//! Who is acting: users, service accounts and agents.

use std::fmt;

use knowell_core::Name;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Wraps an existing UUID.
            pub fn new(id: Uuid) -> Self {
                Self(id)
            }

            /// The underlying UUID.
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0.hyphenated())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0.hyphenated())
            }
        }
    };
}

uuid_id!(
    /// Identifier of a human user.
    UserId
);
uuid_id!(
    /// Identifier of a non-human service account (CI, automation).
    ServiceAccountId
);
uuid_id!(
    /// Identifier of one agent session (one MCP connection or run).
    AgentSessionId
);

/// An authenticated acting identity.
///
/// A [`Principal::Agent`] always acts on behalf of a user and never holds
/// grants of its own: its permissions are the user's, further capped by the
/// action ceiling in [`crate::Action::agent_permitted`].
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Principal {
    /// A human user.
    User(UserId),
    /// A service account.
    ServiceAccount(ServiceAccountId),
    /// A coding agent acting for `on_behalf_of`.
    Agent {
        /// The user whose permissions bound this agent.
        on_behalf_of: UserId,
        /// The agent client, e.g. `claude-code`.
        client: Name,
        /// The agent session.
        session: AgentSessionId,
    },
}

impl Principal {
    /// True for [`Principal::Agent`].
    pub fn is_agent(&self) -> bool {
        matches!(self, Self::Agent { .. })
    }

    /// The user this principal acts as: the user itself, or the agent's
    /// owner. Service accounts act as no user.
    pub fn acting_user(&self) -> Option<UserId> {
        match self {
            Self::User(id) => Some(*id),
            Self::Agent { on_behalf_of, .. } => Some(*on_behalf_of),
            Self::ServiceAccount(_) => None,
        }
    }

    /// True when a grant issued to `grantee` applies to this principal.
    /// Agents inherit their user's grants, nothing else.
    pub(crate) fn inherits_from(&self, grantee: &Principal) -> bool {
        match (grantee, self) {
            (Self::User(a), Self::User(b)) => a == b,
            (Self::ServiceAccount(a), Self::ServiceAccount(b)) => a == b,
            (Self::User(a), Self::Agent { on_behalf_of, .. }) => a == on_behalf_of,
            _ => false,
        }
    }
}

/// Stable text form used in audit records:
/// `user:<uuid>`, `service_account:<uuid>`, `agent:<user>:<client>:<session>`.
impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(id) => write!(f, "user:{id}"),
            Self::ServiceAccount(id) => write!(f, "service_account:{id}"),
            Self::Agent {
                on_behalf_of,
                client,
                session,
            } => write!(f, "agent:{on_behalf_of}:{client}:{session}"),
        }
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    pub(crate) fn user(n: u128) -> Principal {
        Principal::User(UserId::new(Uuid::from_u128(n)))
    }

    pub(crate) fn uid(n: u128) -> UserId {
        UserId::new(Uuid::from_u128(n))
    }

    pub(crate) fn agent_of(n: u128) -> Principal {
        Principal::Agent {
            on_behalf_of: uid(n),
            client: Name::new("test-agent").unwrap(),
            session: AgentSessionId::new(Uuid::from_u128(900 + n)),
        }
    }

    pub(crate) fn service(n: u128) -> Principal {
        Principal::ServiceAccount(ServiceAccountId::new(Uuid::from_u128(n)))
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;

    #[test]
    fn agent_inherits_only_from_its_user() {
        let grantee = user(1);
        assert!(agent_of(1).inherits_from(&grantee));
        assert!(!agent_of(2).inherits_from(&grantee));
        assert!(user(1).inherits_from(&grantee));
        assert!(!service(1).inherits_from(&grantee));
        assert!(!user(1).inherits_from(&service(1)));
    }

    #[test]
    fn acting_user() {
        assert_eq!(user(3).acting_user(), Some(uid(3)));
        assert_eq!(agent_of(3).acting_user(), Some(uid(3)));
        assert_eq!(service(3).acting_user(), None);
    }

    #[test]
    fn display_is_stable() {
        assert_eq!(
            user(1).to_string(),
            "user:00000000-0000-0000-0000-000000000001"
        );
        assert!(agent_of(1).to_string().starts_with("agent:00000000-"));
        assert!(agent_of(1).to_string().contains(":test-agent:"));
    }
}
