//! Who a request acts as, and what it may see.
//!
//! Every tool call and REST request is turned into an [`Access`]: a
//! `knowell-auth` principal with the grants that bound it. Permissions are
//! enforced inside the engine — while pinning the view manifest (invisible
//! projects never enter a query), in graph expansion and context packing
//! (they only see pinned projects), for personal overlays (owner only) and
//! for memory (scope checks) — never by filtering answers afterwards.
//!
//! MCP is the agent interface: every MCP caller is mapped to an
//! [`Principal::Agent`] acting for its user, so it inherits only that user's
//! grants and can propose memory but never accept it.

use std::collections::BTreeMap;
use std::sync::Arc;

use knowell_auth::{
    Action, AgentSessionId, Grant, GrantSet, Principal, ProjectFilter, Resource, ResourceScope,
    Role, TokenScopes, UserId, authorize, authorize_token, visible_projects,
};
use knowell_core::{ContentHash, Name};
use knowell_knowledge::{Actor, ClientId, SessionId};
use knowell_mcp::{Caller, ToolError};
use uuid::Uuid;

/// The acting identity of one request and the grants that bound it.
#[derive(Debug, Clone)]
pub struct Access {
    principal: Principal,
    grants: Arc<GrantSet>,
    scopes: Option<TokenScopes>,
    display: String,
}

impl Access {
    /// An identity with its grants and no token scope restriction.
    pub fn new(principal: Principal, grants: Arc<GrantSet>) -> Self {
        let display = principal.to_string();
        Self {
            principal,
            grants,
            scopes: None,
            display,
        }
    }

    /// Restricts the identity to the scopes of the credential it presented.
    #[must_use]
    pub fn with_scopes(mut self, scopes: Option<TokenScopes>) -> Self {
        self.scopes = scopes;
        self
    }

    /// The acting principal.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The grants evaluated for this identity.
    pub fn grants(&self) -> &GrantSet {
        &self.grants
    }

    /// Projects this identity may read (search-time filter).
    pub fn visible(&self) -> ProjectFilter {
        visible_projects(&self.principal, &self.grants)
    }

    /// Whether `action` on `resource` is allowed (token scopes included).
    pub fn allows(&self, action: Action, resource: &Resource) -> bool {
        let decision = match &self.scopes {
            Some(scopes) => {
                authorize_token(&self.principal, scopes, action, resource, &self.grants)
            }
            None => authorize(&self.principal, action, resource, &self.grants),
        };
        decision.allowed
    }

    /// Whether the identity may read the code of `workspace/project`.
    pub fn reads_project(&self, workspace: &Name, project: &Name) -> bool {
        self.visible().allows(workspace, project)
            && self.allows(
                Action::ReadCode,
                &Resource::project(workspace.clone(), project.clone()),
            )
    }

    /// The user this identity acts for (`None` for service accounts).
    pub fn acting_user(&self) -> Option<UserId> {
        self.principal.acting_user()
    }

    /// Whether this identity is an agent.
    pub fn is_agent(&self) -> bool {
        self.principal.is_agent()
    }

    /// Stable text form, used to bind contexts to their owner.
    pub fn label(&self) -> &str {
        &self.display
    }

    /// The knowledge-model actor for memory writes and history entries.
    pub(crate) fn actor(&self) -> Result<Actor, ToolError> {
        match &self.principal {
            Principal::User(user) => knowell_knowledge::UserId::new(user.to_string())
                .map(Actor::Human)
                .map_err(|e| ToolError::internal(format!("user id: {e}"))),
            Principal::Agent {
                client, session, ..
            } => {
                let session = SessionId::new(session.to_string())
                    .map_err(|e| ToolError::internal(format!("session id: {e}")))?;
                let client = ClientId::new(client.as_str())
                    .map_err(|e| ToolError::internal(format!("client id: {e}")))?;
                Ok(Actor::Agent { session, client })
            }
            Principal::ServiceAccount(_) => Ok(Actor::System),
        }
    }
}

/// Resolves MCP callers to identities.
///
/// The default [`StaticAccess`] serves a standalone install (the local user)
/// and fixed subject tables (tests, simple hubs). A hub with a token store
/// plugs its own resolver in.
pub trait AccessResolver: Send + Sync + 'static {
    /// The identity of an MCP caller, or `PermissionDenied`.
    fn resolve(&self, caller: &Caller) -> Result<Access, ToolError>;
}

/// A fixed table of identities: the local user plus named subjects.
///
/// Every MCP caller becomes an agent acting for the matching user; the agent
/// session is derived from the client name so the same client keeps the same
/// author identity across calls.
#[derive(Debug, Clone)]
pub struct StaticAccess {
    local: Option<(UserId, Arc<GrantSet>)>,
    subjects: BTreeMap<String, (UserId, Arc<GrantSet>)>,
}

impl StaticAccess {
    /// No identity at all: every caller is denied.
    pub fn deny_all() -> Self {
        Self {
            local: None,
            subjects: BTreeMap::new(),
        }
    }

    /// The local user with an organization-wide admin grant (a standalone
    /// install where the machine owner sees everything).
    pub fn local_admin(user: UserId) -> Self {
        let mut grants = GrantSet::new();
        if let Ok(grant) = Grant::new(
            Principal::User(user),
            Role::Admin,
            ResourceScope::Organization,
        ) {
            grants.add(grant);
        }
        Self {
            local: Some((user, Arc::new(grants))),
            subjects: BTreeMap::new(),
        }
    }

    /// The local user with explicit grants.
    pub fn local_user(user: UserId, grants: GrantSet) -> Self {
        Self {
            local: Some((user, Arc::new(grants))),
            subjects: BTreeMap::new(),
        }
    }

    /// Adds an authenticated subject (as produced by a token or OIDC
    /// resolver) acting as `user` with `grants`.
    #[must_use]
    pub fn with_subject(
        mut self,
        subject: impl Into<String>,
        user: UserId,
        grants: GrantSet,
    ) -> Self {
        self.subjects
            .insert(subject.into(), (user, Arc::new(grants)));
        self
    }
}

impl AccessResolver for StaticAccess {
    fn resolve(&self, caller: &Caller) -> Result<Access, ToolError> {
        let found = match &caller.principal {
            knowell_mcp::Principal::LocalUser => self.local.as_ref(),
            knowell_mcp::Principal::Subject { id } => self.subjects.get(id),
        };
        let Some((user, grants)) = found else {
            return Err(ToolError::permission_denied(
                "this caller has no access to the engine",
            ));
        };
        Ok(Access::new(
            agent_principal(*user, caller)?,
            Arc::clone(grants),
        ))
    }
}

/// The agent principal of an MCP caller acting for `user`.
pub(crate) fn agent_principal(user: UserId, caller: &Caller) -> Result<Principal, ToolError> {
    let client_text = caller
        .client
        .as_ref()
        .map_or("mcp-client", |c| c.name.as_str());
    let client = client_name(client_text)?;
    let digest = ContentHash::of_parts([
        b"knowell.engine.agent-session.v1".as_slice(),
        user.as_uuid().as_bytes().as_slice(),
        client.as_str().as_bytes(),
    ]);
    let mut bytes = [0u8; 16];
    for (out, b) in bytes.iter_mut().zip(digest.as_bytes().iter()) {
        *out = *b;
    }
    Ok(Principal::Agent {
        on_behalf_of: user,
        client,
        session: AgentSessionId::new(Uuid::from_bytes(bytes)),
    })
}

/// A valid [`Name`] for a self-reported client name: lowercased, other
/// characters replaced by `-`, cut to 64 bytes; `mcp-client` when nothing
/// usable remains.
fn client_name(raw: &str) -> Result<Name, ToolError> {
    let mut out = String::new();
    for c in raw.chars() {
        if out.len() >= Name::MAX_LEN {
            break;
        }
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') || out.ends_with('_') {
        out.pop();
    }
    Name::new(out)
        .or_else(|_| Name::new("mcp-client"))
        .map_err(|e| ToolError::internal(format!("client name: {e}")))
}

#[cfg(test)]
mod tests {
    use knowell_mcp::{ClientIdentity, TransportKind};

    use super::*;

    fn user(n: u128) -> UserId {
        UserId::new(Uuid::from_u128(n))
    }

    fn caller(client: Option<&str>) -> Caller {
        Caller {
            principal: knowell_mcp::Principal::LocalUser,
            transport: TransportKind::Stdio,
            client: client.map(|name| ClientIdentity {
                name: name.into(),
                version: "1".into(),
            }),
        }
    }

    #[test]
    fn mcp_callers_are_agents_for_their_user() {
        let access = StaticAccess::local_admin(user(1))
            .resolve(&caller(Some("Claude Code")))
            .unwrap();
        assert!(access.is_agent());
        assert_eq!(access.acting_user(), Some(user(1)));
        match access.principal() {
            Principal::Agent { client, .. } => assert_eq!(client.as_str(), "claude-code"),
            other => panic!("{other:?}"),
        }
        // Agents read but never accept memory, whatever the user's role.
        let ws = Name::new("shop").unwrap();
        assert!(access.allows(Action::ReadCode, &Resource::workspace(ws.clone())));
        assert!(access.allows(Action::ProposeMemory, &Resource::workspace(ws.clone())));
        assert!(!access.allows(Action::AcceptMemory, &Resource::workspace(ws)));
    }

    #[test]
    fn sessions_are_stable_per_client() {
        let resolver = StaticAccess::local_admin(user(1));
        let a = resolver.resolve(&caller(Some("codex"))).unwrap();
        let b = resolver.resolve(&caller(Some("codex"))).unwrap();
        let c = resolver.resolve(&caller(Some("cursor"))).unwrap();
        assert_eq!(a.principal(), b.principal());
        assert_ne!(a.principal(), c.principal());
    }

    #[test]
    fn unknown_subjects_are_denied() {
        let resolver = StaticAccess::deny_all();
        let error = resolver.resolve(&caller(None)).unwrap_err();
        assert_eq!(error.kind(), "permission_denied");
        let subject = Caller {
            principal: knowell_mcp::Principal::Subject { id: "x".into() },
            transport: TransportKind::StreamableHttp,
            client: None,
        };
        assert!(
            StaticAccess::local_admin(user(1))
                .resolve(&subject)
                .is_err()
        );
    }

    #[test]
    fn hostile_client_names_become_valid_names() {
        assert_eq!(client_name("  ").unwrap().as_str(), "mcp-client");
        assert_eq!(client_name("A/B c!").unwrap().as_str(), "a-b-c");
        assert_eq!(client_name(&"x".repeat(500)).unwrap().as_str().len(), 64);
        assert_eq!(client_name("ünï").unwrap().as_str(), "n");
    }

    #[test]
    fn project_visibility_follows_grants() {
        let mut grants = GrantSet::new();
        grants.add(
            Grant::new(
                Principal::User(user(2)),
                Role::Viewer,
                ResourceScope::project(Name::new("shop").unwrap(), Name::new("api").unwrap()),
            )
            .unwrap(),
        );
        let access = StaticAccess::deny_all()
            .with_subject("u2", user(2), grants)
            .resolve(&Caller {
                principal: knowell_mcp::Principal::Subject { id: "u2".into() },
                transport: TransportKind::StreamableHttp,
                client: None,
            })
            .unwrap();
        let shop = Name::new("shop").unwrap();
        assert!(access.reads_project(&shop, &Name::new("api").unwrap()));
        assert!(!access.reads_project(&shop, &Name::new("web").unwrap()));
    }
}
