//! Who is calling and what they may do: the [`TokenStore`] seam, the
//! in-memory [`MemoryTokenStore`], and [`Authenticated`], the identity the
//! authentication middleware attaches to every protected request.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use knowell_auth::{
    Action, Decision, DecisionReason, Grant, GrantSet, Principal, ProjectFilter, StoredToken,
    TokenId, TokenScopes,
};
use time::OffsetDateTime;

/// A boxed, `Send` future (object-safe async trait methods without extra
/// dependencies).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Errors from a [`TokenStore`]. Messages never contain token text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccessError {
    /// The backing storage cannot answer right now.
    #[error("the access store is unavailable: {0}")]
    Unavailable(String),
}

/// Storage of API tokens and of the grants principals act under.
///
/// The server never stores plaintext tokens: it looks up candidates by the
/// non-secret lookup prefix and verifies them with `knowell_auth::verify`.
/// Both lookups happen per request, so revocations and grant changes take
/// effect immediately. A hub uses [`crate::StoreTokenStore`] (its database);
/// tests and single-user setups use [`MemoryTokenStore`].
pub trait TokenStore: Send + Sync + 'static {
    /// Every stored token whose lookup prefix equals `prefix`
    /// (`kn_` + 8 characters), including revoked and expired ones.
    fn tokens_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> BoxFuture<'a, Result<Vec<StoredToken>, AccessError>>;

    /// The grants (and overlay shares) to evaluate `principal`'s requests
    /// against. Returning grants of other principals is harmless:
    /// `knowell_auth::authorize` applies only the applicable ones.
    fn grants_for<'a>(
        &'a self,
        principal: &'a Principal,
    ) -> BoxFuture<'a, Result<GrantSet, AccessError>>;

    /// Called after `token` authenticated a request at `at`, so a store can
    /// record the token's last use (throttled as it sees fit). It must not
    /// fail the request: implementations log their own errors. The default
    /// does nothing.
    fn token_used<'a>(&'a self, token: TokenId, at: OffsetDateTime) -> BoxFuture<'a, ()> {
        let _ = (token, at);
        Box::pin(std::future::ready(()))
    }
}

/// An in-memory [`TokenStore`]: tokens and grants live in the process.
#[derive(Debug, Default)]
pub struct MemoryTokenStore {
    tokens: RwLock<Vec<StoredToken>>,
    grants: RwLock<GrantSet>,
}

impl MemoryTokenStore {
    /// An empty store: no tokens, no grants (everything denied).
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a token record (as returned by `knowell_auth::issue_token`).
    ///
    /// # Errors
    /// [`AccessError::Unavailable`] if the lock was poisoned.
    pub fn insert_token(&self, token: StoredToken) -> Result<(), AccessError> {
        self.tokens.write().map_err(|_| poisoned())?.push(token);
        Ok(())
    }

    /// Revokes the token `id` at `at`. Returns whether it exists.
    ///
    /// # Errors
    /// [`AccessError::Unavailable`] if the lock was poisoned.
    pub fn revoke(&self, id: TokenId, at: OffsetDateTime) -> Result<bool, AccessError> {
        let mut tokens = self.tokens.write().map_err(|_| poisoned())?;
        let mut found = false;
        for token in tokens.iter_mut().filter(|t| t.id == id) {
            token.revoke(at);
            found = true;
        }
        Ok(found)
    }

    /// Adds a grant.
    ///
    /// # Errors
    /// [`AccessError::Unavailable`] if the lock was poisoned.
    pub fn add_grant(&self, grant: Grant) -> Result<(), AccessError> {
        self.grants.write().map_err(|_| poisoned())?.add(grant);
        Ok(())
    }

    /// Replaces every grant and overlay share.
    ///
    /// # Errors
    /// [`AccessError::Unavailable`] if the lock was poisoned.
    pub fn set_grants(&self, grants: GrantSet) -> Result<(), AccessError> {
        *self.grants.write().map_err(|_| poisoned())? = grants;
        Ok(())
    }
}

fn poisoned() -> AccessError {
    AccessError::Unavailable("in-memory token store lock poisoned".to_owned())
}

impl TokenStore for MemoryTokenStore {
    fn tokens_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> BoxFuture<'a, Result<Vec<StoredToken>, AccessError>> {
        let result = self.tokens.read().map_err(|_| poisoned()).map(|tokens| {
            tokens
                .iter()
                .filter(|t| t.prefix == prefix)
                .cloned()
                .collect()
        });
        Box::pin(std::future::ready(result))
    }

    fn grants_for<'a>(
        &'a self,
        _principal: &'a Principal,
    ) -> BoxFuture<'a, Result<GrantSet, AccessError>> {
        let result = self
            .grants
            .read()
            .map_err(|_| poisoned())
            .map(|g| g.clone());
        Box::pin(std::future::ready(result))
    }
}

/// How a request was authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMethod {
    /// A panel session cookie (CSRF protection applies).
    Session {
        /// BLAKE3 fingerprint of the session id (safe to log).
        fingerprint: String,
    },
    /// An `Authorization: Bearer kn_…` API token.
    Token {
        /// The verified token's id.
        token_id: TokenId,
    },
}

/// The authenticated caller of a protected request, available to handlers
/// and, under `/mcp`, in the request extensions for the MCP caller resolver.
#[derive(Debug, Clone)]
pub struct Authenticated {
    /// Who is acting.
    pub principal: Principal,
    /// Upper bound of what the credential may do; `None` for a local panel
    /// session (bounded only by the principal's grants).
    pub scopes: Option<TokenScopes>,
    /// The grants evaluated for this request.
    pub grants: Arc<GrantSet>,
    /// Projects the principal may read (search-time filter).
    pub visible: ProjectFilter,
    /// How the request was authenticated.
    pub method: AuthMethod,
}

impl Authenticated {
    /// Decides `action` on `resource`, applying the credential's scopes.
    pub fn decide(&self, action: Action, resource: &knowell_auth::Resource) -> Decision {
        match &self.scopes {
            Some(scopes) => knowell_auth::authorize_token(
                &self.principal,
                scopes,
                action,
                resource,
                &self.grants,
            ),
            None => knowell_auth::authorize(&self.principal, action, resource, &self.grants),
        }
    }

    /// The resource-independent part of authorization: the agent action
    /// ceiling and the credential's scope. Used where the resource is
    /// resolved later (by the engine).
    pub fn precheck(&self, action: Action) -> Result<(), DecisionReason> {
        if self.principal.is_agent() && !action.agent_permitted() {
            return Err(DecisionReason::DeniedAgentNotPermitted);
        }
        if let Some(scopes) = &self.scopes
            && !scopes.contains(action.token_scope())
        {
            return Err(DecisionReason::DeniedTokenScope);
        }
        Ok(())
    }

    /// True when every project of the organization is visible.
    pub fn sees_everything(&self) -> bool {
        self.visible.is_all()
    }

    /// True when the workspace or any of its projects is visible.
    pub fn sees_workspace(&self, workspace: &knowell_core::Name) -> bool {
        self.visible.is_all()
            || self.visible.workspaces().any(|w| w == workspace)
            || self.visible.projects().any(|(w, _)| w == workspace)
    }
}

#[cfg(test)]
mod tests {
    use knowell_auth::{
        Pepper, Resource, ResourceScope, Role, UserId, issue_token, visible_projects,
    };
    use knowell_core::Name;
    use uuid::Uuid;

    use super::*;

    fn user(n: u128) -> Principal {
        Principal::User(UserId::new(Uuid::from_u128(n)))
    }

    fn n(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    #[tokio::test]
    async fn memory_store_finds_by_prefix_and_revokes() {
        let store = MemoryTokenStore::new();
        let pepper = Pepper::new(b"fake-pepper-for-server-tests-01").unwrap();
        let now = OffsetDateTime::now_utc();
        let (plain, stored) =
            issue_token(&user(1), TokenScopes::read_only(), None, now, &pepper).unwrap();
        let prefix = knowell_auth::token_prefix(plain.expose()).unwrap();
        store.insert_token(stored.clone()).unwrap();
        let found = store.tokens_with_prefix(&prefix).await.unwrap();
        assert_eq!(found, vec![stored.clone()]);
        assert!(
            store
                .tokens_with_prefix("kn_aaaaaaaa")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(store.revoke(stored.id, now).unwrap());
        let found = store.tokens_with_prefix(&prefix).await.unwrap();
        assert!(found[0].revoked_at.is_some());
    }

    #[test]
    fn decide_applies_scopes_and_precheck() {
        let mut grants = GrantSet::new();
        grants.add(Grant::new(user(1), Role::Admin, ResourceScope::Organization).unwrap());
        let auth = Authenticated {
            principal: user(1),
            scopes: Some(TokenScopes::read_only()),
            visible: visible_projects(&user(1), &grants),
            grants: Arc::new(grants),
            method: AuthMethod::Session {
                fingerprint: "f".into(),
            },
        };
        let project = Resource::project(n("w"), n("p"));
        assert!(auth.decide(Action::ReadCode, &project).allowed);
        let denied = auth.decide(Action::ManageIndex, &project);
        assert_eq!(denied.reason, DecisionReason::DeniedTokenScope);
        assert_eq!(
            auth.precheck(Action::AcceptMemory),
            Err(DecisionReason::DeniedTokenScope)
        );
        assert!(auth.sees_everything());
        assert!(auth.sees_workspace(&n("anything")));

        let unscoped = Authenticated {
            scopes: None,
            ..auth.clone()
        };
        assert!(unscoped.decide(Action::ManageIndex, &project).allowed);
        assert_eq!(unscoped.precheck(Action::ManageIndex), Ok(()));
    }
}
