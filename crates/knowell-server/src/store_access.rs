//! [`StoreTokenStore`]: API tokens and grants read from the database
//! (`knowell_store::identity`), for hubs and any server with users.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use knowell_auth::{
    AgentSessionId, Grant, GrantSet, Principal, ResourceScope, Role, ServiceAccountId, StoredToken,
    TokenHash, TokenId, TokenScope, TokenScopes, UserId,
};
use knowell_core::Name;
use knowell_store::identity::{
    self, GrantScope, NewApiToken, StoredApiToken, StoredGrant, TokenAgent,
};
use knowell_store::{
    ApiTokenId, ApiTokenScope, GrantRole, OrganizationId, PrincipalId, PrincipalKind, Store,
    StoreError, hierarchy,
};
use time::OffsetDateTime;
use tokio::sync::OnceCell;

use crate::access::{AccessError, BoxFuture, TokenStore};

/// Default minimum time between two `last_used_at` writes for one token.
pub const DEFAULT_TOKEN_TOUCH_INTERVAL: Duration = Duration::from_secs(60);

/// Tokens whose last write time is remembered in memory before old entries
/// are dropped (the database check still throttles after that).
const MAX_TRACKED_TOKENS: usize = 10_000;

/// A [`TokenStore`] over the database: tokens by prefix and grants of the
/// configured organization, read per request so revocations, disabled
/// principals and grant changes apply immediately.
///
/// - A disabled principal's tokens verify as revoked (from the disable
///   time) and it holds no grants.
/// - Successful uses update `last_used_at` at most once per
///   [`DEFAULT_TOKEN_TOUCH_INTERVAL`] (configurable) per token: an in-memory
///   check skips the query, and the update itself only writes when the
///   stored time is older, which also throttles several replicas.
/// - Overlay shares are not persisted yet; [`TokenStore::grants_for`]
///   returns none.
///
/// Rows that cannot be mapped to knowell-auth types are skipped with a
/// warning (they never authenticate anyone).
pub struct StoreTokenStore {
    store: Store,
    organization: Name,
    organization_id: OnceCell<OrganizationId>,
    touch_interval: Duration,
    touched: Mutex<HashMap<TokenId, Instant>>,
}

impl std::fmt::Debug for StoreTokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreTokenStore")
            .field("organization", &self.organization)
            .field("touch_interval", &self.touch_interval)
            .finish_non_exhaustive()
    }
}

impl StoreTokenStore {
    /// Tokens and grants of `organization` (looked up by name on first use;
    /// until it exists, no token authenticates).
    pub fn new(store: Store, organization: Name) -> Self {
        Self {
            store,
            organization,
            organization_id: OnceCell::new(),
            touch_interval: DEFAULT_TOKEN_TOUCH_INTERVAL,
            touched: Mutex::new(HashMap::new()),
        }
    }

    /// Changes the minimum time between two `last_used_at` writes of one
    /// token.
    pub fn with_touch_interval(mut self, interval: Duration) -> Self {
        self.touch_interval = interval;
        self
    }

    /// The organization's id, cached once it exists.
    async fn organization_id(&self) -> Result<Option<OrganizationId>, StoreError> {
        if let Some(id) = self.organization_id.get() {
            return Ok(Some(*id));
        }
        let mut conn = self.store.acquire().await?;
        let found = hierarchy::find_organization(&mut conn, &self.organization).await?;
        Ok(found.map(|org| {
            // A concurrent first lookup may have set it already; same value.
            let _ = self.organization_id.set(org.id);
            org.id
        }))
    }

    /// Persists a token issued with `knowell_auth::issue_token` (never the
    /// plaintext). The principal (for an agent token: its user) must exist
    /// in the store. A record that is already revoked stays revoked.
    ///
    /// # Errors
    /// The store's errors: [`StoreError::NotFound`] for an unknown
    /// principal, [`StoreError::AlreadyExists`] for a known id or hash,
    /// [`StoreError::InvalidInput`] for a record breaking the token rules.
    pub async fn save_token(
        &self,
        token: &StoredToken,
        label: Option<&str>,
        created_by: Option<PrincipalId>,
    ) -> Result<StoredApiToken, StoreError> {
        let (principal, agent) = match &token.principal {
            Principal::User(user) => (PrincipalId(user.as_uuid()), None),
            Principal::ServiceAccount(account) => (PrincipalId(account.as_uuid()), None),
            Principal::Agent {
                on_behalf_of,
                client,
                session,
            } => (
                PrincipalId(on_behalf_of.as_uuid()),
                Some(TokenAgent {
                    client: client.clone(),
                    session: session.as_uuid(),
                }),
            ),
        };
        let key_hash = hash_bytes(&token.hash)
            .ok_or_else(|| StoreError::InvalidInput("the token hash is malformed".to_owned()))?;
        let new = NewApiToken {
            id: ApiTokenId(token.id.as_uuid()),
            principal,
            agent,
            prefix: token.prefix.clone(),
            key_hash,
            scopes: token.scopes.iter().map(store_scope).collect(),
            label: label.map(str::to_owned),
            created_by,
            created_at: token.created_at,
            expires_at: token.expires_at,
        };
        let mut conn = self.store.acquire().await?;
        let saved = identity::insert_api_token(&mut conn, &new).await?;
        match token.revoked_at {
            Some(at) => {
                identity::revoke_api_token(&mut conn, saved.id, at).await?;
                identity::get_api_token(&mut conn, saved.id)
                    .await?
                    .ok_or_else(|| StoreError::Corrupt("saved api token vanished".to_owned()))
            }
            None => Ok(saved),
        }
    }

    /// Revokes the token `id` at `at` (the earliest revocation time is
    /// kept). Returns whether it exists.
    ///
    /// # Errors
    /// The store's errors.
    pub async fn revoke_token(&self, id: TokenId, at: OffsetDateTime) -> Result<bool, StoreError> {
        let mut conn = self.store.acquire().await?;
        identity::revoke_api_token(&mut conn, ApiTokenId(id.as_uuid()), at).await
    }

    async fn load_tokens(&self, prefix: &str) -> Result<Vec<StoredToken>, StoreError> {
        let Some(org) = self.organization_id().await? else {
            return Ok(Vec::new());
        };
        let mut conn = self.store.acquire().await?;
        let rows = identity::tokens_with_prefix(&mut conn, org, prefix).await?;
        Ok(rows
            .iter()
            .filter_map(|row| match auth_token(row) {
                Ok(token) => Some(token),
                Err(reason) => {
                    tracing::warn!(token = %row.id, reason, "api token record skipped");
                    None
                }
            })
            .collect())
    }

    async fn load_grants(&self, principal: &Principal) -> Result<GrantSet, StoreError> {
        let mut set = GrantSet::new();
        let Some(org) = self.organization_id().await? else {
            return Ok(set);
        };
        let id = match principal {
            Principal::User(user) => user.as_uuid(),
            Principal::ServiceAccount(account) => account.as_uuid(),
            // Agents hold no grants of their own; they act with their user's.
            Principal::Agent { on_behalf_of, .. } => on_behalf_of.as_uuid(),
        };
        let mut conn = self.store.acquire().await?;
        for row in identity::grants_for_principal(&mut conn, PrincipalId(id)).await? {
            if row.organization != org {
                continue;
            }
            match auth_grant(&row) {
                Ok(grant) => set.add(grant),
                Err(reason) => tracing::warn!(grant = %row.id, reason, "grant record skipped"),
            }
        }
        Ok(set)
    }

    /// Whether `token`'s last use should be written now (and remember it).
    fn due(&self, token: TokenId) -> bool {
        let Ok(mut touched) = self.touched.lock() else {
            return true;
        };
        let now = Instant::now();
        if touched
            .get(&token)
            .is_some_and(|last| now.duration_since(*last) < self.touch_interval)
        {
            return false;
        }
        if touched.len() >= MAX_TRACKED_TOKENS {
            let interval = self.touch_interval;
            touched.retain(|_, last| now.duration_since(*last) < interval);
            if touched.len() >= MAX_TRACKED_TOKENS {
                touched.clear();
            }
        }
        touched.insert(token, now);
        true
    }
}

fn unavailable(err: &StoreError) -> AccessError {
    // Store errors never carry connection strings or secrets.
    tracing::warn!(error = %err, "identity store lookup failed");
    AccessError::Unavailable("the identity store failed; see the server log".to_owned())
}

impl TokenStore for StoreTokenStore {
    fn tokens_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> BoxFuture<'a, Result<Vec<StoredToken>, AccessError>> {
        Box::pin(async move { self.load_tokens(prefix).await.map_err(|e| unavailable(&e)) })
    }

    fn grants_for<'a>(
        &'a self,
        principal: &'a Principal,
    ) -> BoxFuture<'a, Result<GrantSet, AccessError>> {
        Box::pin(async move {
            self.load_grants(principal)
                .await
                .map_err(|e| unavailable(&e))
        })
    }

    fn token_used<'a>(&'a self, token: TokenId, at: OffsetDateTime) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if !self.due(token) {
                return;
            }
            let result = async {
                let mut conn = self.store.acquire().await?;
                identity::touch_api_token(
                    &mut conn,
                    ApiTokenId(token.as_uuid()),
                    at,
                    self.touch_interval,
                )
                .await
            }
            .await;
            if let Err(err) = result {
                tracing::warn!(error = %err, %token, "token last-use update failed");
            }
        })
    }
}

/// knowell-auth keeps `TokenId` and `TokenHash` opaque; their serde forms
/// (hyphenated UUID, 64 hex digits) are the stable way in and out.
fn auth_token(row: &StoredApiToken) -> Result<StoredToken, &'static str> {
    let id: TokenId = serde_json::from_value(serde_json::Value::String(row.id.to_string()))
        .map_err(|_| "token id")?;
    let hash: TokenHash = serde_json::from_value(serde_json::Value::String(hex(&row.key_hash)))
        .map_err(|_| "token hash")?;
    let principal = match (&row.agent, row.principal_kind) {
        (Some(agent), PrincipalKind::User) => Principal::Agent {
            on_behalf_of: UserId::new(row.principal.as_uuid()),
            client: agent.client.clone(),
            session: AgentSessionId::new(agent.session),
        },
        (Some(_), PrincipalKind::ServiceAccount) => return Err("agent of a service account"),
        (None, PrincipalKind::User) => Principal::User(UserId::new(row.principal.as_uuid())),
        (None, PrincipalKind::ServiceAccount) => {
            Principal::ServiceAccount(ServiceAccountId::new(row.principal.as_uuid()))
        }
    };
    let scopes = TokenScopes::new(row.scopes.iter().map(|s| match s {
        ApiTokenScope::Read => TokenScope::Read,
        ApiTokenScope::Write => TokenScope::Write,
        ApiTokenScope::Admin => TokenScope::Admin,
    }))
    .ok_or("no scopes")?;
    Ok(StoredToken {
        id,
        prefix: row.prefix.clone(),
        hash,
        principal,
        scopes,
        created_at: row.created_at,
        expires_at: row.expires_at,
        revoked_at: row.effective_revoked_at(),
    })
}

fn auth_grant(row: &StoredGrant) -> Result<Grant, &'static str> {
    let principal = match row.principal_kind {
        PrincipalKind::User => Principal::User(UserId::new(row.principal.as_uuid())),
        PrincipalKind::ServiceAccount => {
            Principal::ServiceAccount(ServiceAccountId::new(row.principal.as_uuid()))
        }
    };
    let role = match row.role {
        GrantRole::Viewer => Role::Viewer,
        GrantRole::Member => Role::Member,
        GrantRole::Maintainer => Role::Maintainer,
        GrantRole::Admin => Role::Admin,
    };
    let scope = match row.scope {
        GrantScope::Organization => ResourceScope::Organization,
        GrantScope::Workspace(_) => {
            ResourceScope::workspace(row.workspace_name.clone().ok_or("workspace name")?)
        }
        GrantScope::Project(_) => ResourceScope::project(
            row.workspace_name.clone().ok_or("workspace name")?,
            row.project_name.clone().ok_or("project name")?,
        ),
    };
    Grant::new(principal, role, scope).map_err(|_| "grantee")
}

fn store_scope(scope: TokenScope) -> ApiTokenScope {
    match scope {
        TokenScope::Read => ApiTokenScope::Read,
        TokenScope::Write => ApiTokenScope::Write,
        TokenScope::Admin => ApiTokenScope::Admin,
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        for nibble in [byte >> 4, byte & 0x0f] {
            if let Some(&digit) = DIGITS.get(usize::from(nibble)) {
                out.push(char::from(digit));
            }
        }
    }
    out
}

/// The 32 raw bytes of a token hash, via its hex serde form.
fn hash_bytes(hash: &TokenHash) -> Option<[u8; 32]> {
    let text = match serde_json::to_value(hash).ok()? {
        serde_json::Value::String(text) => text,
        _ => return None,
    };
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    if pairs.len() != 32 || !rest.is_empty() {
        return None;
    }
    let mut out = [0u8; 32];
    for (slot, [hi, lo]) in out.iter_mut().zip(pairs) {
        *slot = (hex_value(*hi)? << 4) | hex_value(*lo)?;
    }
    Some(out)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use knowell_auth::{Pepper, issue_token};
    use uuid::Uuid;

    use super::*;

    fn pepper() -> Pepper {
        Pepper::new(b"fake-pepper-for-server-unit-tests").unwrap()
    }

    fn row_for(
        token: &StoredToken,
        kind: PrincipalKind,
        agent: Option<TokenAgent>,
    ) -> StoredApiToken {
        StoredApiToken {
            id: ApiTokenId(token.id.as_uuid()),
            organization: OrganizationId(Uuid::from_u128(9)),
            principal: PrincipalId(Uuid::from_u128(1)),
            principal_kind: kind,
            principal_disabled_at: None,
            agent,
            prefix: token.prefix.clone(),
            key_hash: hash_bytes(&token.hash).unwrap(),
            scopes: token.scopes.iter().map(store_scope).collect(),
            label: None,
            created_by: None,
            created_at: token.created_at,
            expires_at: token.expires_at,
            revoked_at: token.revoked_at,
            last_used_at: None,
        }
    }

    #[test]
    fn tokens_round_trip_through_rows() {
        let now = OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
        let user = Principal::User(UserId::new(Uuid::from_u128(1)));
        let (plain, issued) = issue_token(
            &user,
            TokenScopes::new([TokenScope::Read, TokenScope::Write]).unwrap(),
            None,
            now,
            &pepper(),
        )
        .unwrap();
        let back = auth_token(&row_for(&issued, PrincipalKind::User, None)).unwrap();
        assert_eq!(back, issued);
        // The mapped record still verifies the plaintext.
        assert!(knowell_auth::verify(plain.expose(), &back, &pepper(), now).is_ok());
        assert_eq!(hex(&[0x0f, 0xa0]), "0fa0");
    }

    #[test]
    fn agent_tokens_map_to_agent_principals() {
        let now = OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
        let agent = Principal::Agent {
            on_behalf_of: UserId::new(Uuid::from_u128(1)),
            client: Name::new("test-agent").unwrap(),
            session: AgentSessionId::new(Uuid::from_u128(5)),
        };
        let (_, issued) = issue_token(
            &agent,
            TokenScopes::read_only(),
            Some(now + time::Duration::hours(1)),
            now,
            &pepper(),
        )
        .unwrap();
        let link = TokenAgent {
            client: Name::new("test-agent").unwrap(),
            session: Uuid::from_u128(5),
        };
        let back = auth_token(&row_for(&issued, PrincipalKind::User, Some(link.clone()))).unwrap();
        assert_eq!(back.principal, agent);
        assert!(auth_token(&row_for(&issued, PrincipalKind::ServiceAccount, Some(link))).is_err());
    }

    #[test]
    fn disabled_principals_read_as_revoked() {
        let now = OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
        let user = Principal::User(UserId::new(Uuid::from_u128(1)));
        let (plain, issued) =
            issue_token(&user, TokenScopes::read_only(), None, now, &pepper()).unwrap();
        let mut row = row_for(&issued, PrincipalKind::User, None);
        row.principal_disabled_at = Some(now);
        let back = auth_token(&row).unwrap();
        assert_eq!(
            knowell_auth::verify(plain.expose(), &back, &pepper(), now),
            Err(knowell_auth::TokenError::Revoked)
        );
    }

    #[test]
    fn grants_need_their_scope_names() {
        let row = StoredGrant {
            id: knowell_store::GrantId(Uuid::from_u128(3)),
            organization: OrganizationId(Uuid::from_u128(9)),
            principal: PrincipalId(Uuid::from_u128(1)),
            principal_kind: PrincipalKind::ServiceAccount,
            role: GrantRole::Maintainer,
            scope: GrantScope::Project(knowell_store::ProjectId(Uuid::from_u128(4))),
            workspace_name: Some(Name::new("main").unwrap()),
            project_name: Some(Name::new("api").unwrap()),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        let grant = auth_grant(&row).unwrap();
        assert_eq!(grant.role(), Role::Maintainer);
        assert_eq!(
            grant.principal(),
            &Principal::ServiceAccount(ServiceAccountId::new(Uuid::from_u128(1)))
        );
        assert_eq!(
            grant.scope(),
            &ResourceScope::project(Name::new("main").unwrap(), Name::new("api").unwrap())
        );
        let missing = StoredGrant {
            project_name: None,
            ..row
        };
        assert!(auth_grant(&missing).is_err());
    }
}
