//! Authorization: actions, decisions and search-time project filters.
//!
//! Rules, in evaluation order (see [`authorize`]):
//!
//! 1. Deny by default: no covering grant means no access.
//! 2. An agent may only perform [`Action::agent_permitted`] actions, and only
//!    with its user's grants. It can never exceed its user.
//! 3. [`Action::ManageUsers`] applies to the organization resource only.
//! 4. The most specific covering scope decides the role (project over
//!    workspace over organization), even when that role is lower than a
//!    broader one: a narrower grant restricts as well as grants.
//! 5. The role must reach [`Action::min_role`].
//! 6. A user's uncommitted overlay is visible only to its owner (and agents
//!    acting for the owner), or to users it was explicitly shared with.
//!    Administrators do not see other users' overlays.

use std::collections::BTreeSet;
use std::fmt;

use knowell_core::Name;

use crate::grant::{GrantSet, Resource, ResourceScope, Role};
use crate::principal::{Principal, UserId};

/// Something a principal may try to do.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Action {
    /// Read indexed code and search it.
    ReadCode,
    /// Read memory records.
    ReadMemory,
    /// Propose a memory record (pending review).
    ProposeMemory,
    /// Accept or reject proposed memory.
    AcceptMemory,
    /// Create or update tasks and checkpoints.
    WriteTask,
    /// Start, reconfigure or reset indexes.
    ManageIndex,
    /// Change workspace and project settings.
    ManageWorkspace,
    /// Change model-provider settings and policy.
    ManageProviders,
    /// Manage users, roles and tokens (organization resource only).
    ManageUsers,
    /// Read the audit log.
    ReadAudit,
    /// Connect over MCP at all.
    UseMcp,
    /// Read the uncommitted working-tree overlay owned by this user.
    ReadUncommittedOverlay(UserId),
}

impl Action {
    /// Lowest role that may perform the action (before overlay and agent
    /// rules).
    pub fn min_role(&self) -> Role {
        match self {
            Self::ReadCode | Self::ReadMemory | Self::UseMcp | Self::ReadUncommittedOverlay(_) => {
                Role::Viewer
            }
            Self::ProposeMemory | Self::WriteTask => Role::Member,
            Self::AcceptMemory | Self::ManageIndex => Role::Maintainer,
            Self::ManageWorkspace | Self::ManageProviders | Self::ManageUsers | Self::ReadAudit => {
                Role::Admin
            }
        }
    }

    /// Whether an agent may ever perform the action, whatever its user's role.
    ///
    /// Agents read, propose and write tasks. Accepting memory (a human
    /// review step) and all administration stay with humans.
    pub fn agent_permitted(&self) -> bool {
        matches!(
            self,
            Self::ReadCode
                | Self::ReadMemory
                | Self::ProposeMemory
                | Self::WriteTask
                | Self::UseMcp
                | Self::ReadUncommittedOverlay(_)
        )
    }

    /// The token scope class this action needs (MCP read/write/admin
    /// separation).
    pub fn token_scope(&self) -> TokenScope {
        match self {
            Self::ReadCode | Self::ReadMemory | Self::UseMcp | Self::ReadUncommittedOverlay(_) => {
                TokenScope::Read
            }
            Self::ProposeMemory | Self::WriteTask => TokenScope::Write,
            Self::AcceptMemory
            | Self::ManageIndex
            | Self::ManageWorkspace
            | Self::ManageProviders
            | Self::ManageUsers
            | Self::ReadAudit => TokenScope::Admin,
        }
    }

    /// Stable snake_case code used in audit records.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ReadCode => "read_code",
            Self::ReadMemory => "read_memory",
            Self::ProposeMemory => "propose_memory",
            Self::AcceptMemory => "accept_memory",
            Self::WriteTask => "write_task",
            Self::ManageIndex => "manage_index",
            Self::ManageWorkspace => "manage_workspace",
            Self::ManageProviders => "manage_providers",
            Self::ManageUsers => "manage_users",
            Self::ReadAudit => "read_audit",
            Self::UseMcp => "use_mcp",
            Self::ReadUncommittedOverlay(_) => "read_uncommitted_overlay",
        }
    }
}

/// Stable text form: the code, plus `:<owner>` for overlay reads.
impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadUncommittedOverlay(owner) => write!(f, "{}:{owner}", self.code()),
            other => f.write_str(other.code()),
        }
    }
}

/// Coarse permission class carried by API tokens, mirroring the MCP
/// read / write / admin tool separation.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TokenScope {
    /// Read-only actions.
    Read,
    /// Proposals and task writes.
    Write,
    /// Administration and review.
    Admin,
}

/// A non-empty set of [`TokenScope`]s.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct TokenScopes(BTreeSet<TokenScope>);

impl TokenScopes {
    /// Builds a set; `None` when `scopes` is empty (a token must allow
    /// something).
    pub fn new(scopes: impl IntoIterator<Item = TokenScope>) -> Option<Self> {
        let set: BTreeSet<TokenScope> = scopes.into_iter().collect();
        (!set.is_empty()).then_some(Self(set))
    }

    /// Read-only token.
    pub fn read_only() -> Self {
        Self(BTreeSet::from([TokenScope::Read]))
    }

    /// True when `scope` is included.
    pub fn contains(&self, scope: TokenScope) -> bool {
        self.0.contains(&scope)
    }

    /// The scopes in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = TokenScope> + '_ {
        self.0.iter().copied()
    }
}

/// Why a decision was made. [`DecisionReason::code`] is stable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecisionReason {
    /// Allowed by the governing grant.
    Granted,
    /// Allowed: the overlay belongs to the acting user.
    GrantedOverlayOwner,
    /// Allowed: the overlay owner shared it with the acting user.
    GrantedOverlayShared,
    /// No grant covers the resource.
    DeniedNoGrant,
    /// The governing role is below the action's minimum.
    DeniedInsufficientRole,
    /// Agents may never perform this action.
    DeniedAgentNotPermitted,
    /// The action applies to the organization resource only.
    DeniedOrganizationOnly,
    /// The overlay belongs to another user and is not shared.
    DeniedOverlayPrivate,
    /// The presented token does not carry the needed scope.
    DeniedTokenScope,
}

impl DecisionReason {
    /// Stable snake_case code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::GrantedOverlayOwner => "granted_overlay_owner",
            Self::GrantedOverlayShared => "granted_overlay_shared",
            Self::DeniedNoGrant => "denied_no_grant",
            Self::DeniedInsufficientRole => "denied_insufficient_role",
            Self::DeniedAgentNotPermitted => "denied_agent_not_permitted",
            Self::DeniedOrganizationOnly => "denied_organization_only",
            Self::DeniedOverlayPrivate => "denied_overlay_private",
            Self::DeniedTokenScope => "denied_token_scope",
        }
    }
}

/// Outcome of an authorization check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Decision {
    /// Whether the action may proceed.
    pub allowed: bool,
    /// Why.
    pub reason: DecisionReason,
}

impl Decision {
    fn allow(reason: DecisionReason) -> Self {
        Self {
            allowed: true,
            reason,
        }
    }

    fn deny(reason: DecisionReason) -> Self {
        Self {
            allowed: false,
            reason,
        }
    }
}

/// Decides whether `principal` may perform `action` on `resource`.
///
/// Pure and deterministic; see the module documentation for the rules.
pub fn authorize(
    principal: &Principal,
    action: Action,
    resource: &Resource,
    grants: &GrantSet,
) -> Decision {
    if principal.is_agent() && !action.agent_permitted() {
        return Decision::deny(DecisionReason::DeniedAgentNotPermitted);
    }
    if matches!(action, Action::ManageUsers) && !matches!(resource, Resource::Organization) {
        return Decision::deny(DecisionReason::DeniedOrganizationOnly);
    }
    let Some(role) = grants.effective_role(principal, resource) else {
        return Decision::deny(DecisionReason::DeniedNoGrant);
    };
    if role < action.min_role() {
        return Decision::deny(DecisionReason::DeniedInsufficientRole);
    }
    let Action::ReadUncommittedOverlay(owner) = action else {
        return Decision::allow(DecisionReason::Granted);
    };
    let Some(acting) = principal.acting_user() else {
        return Decision::deny(DecisionReason::DeniedOverlayPrivate);
    };
    if acting == owner {
        return Decision::allow(DecisionReason::GrantedOverlayOwner);
    }
    let shared = grants
        .overlay_shares()
        .iter()
        .any(|s| s.owner == owner && s.shared_with == acting && s.scope.covers(resource));
    if shared {
        Decision::allow(DecisionReason::GrantedOverlayShared)
    } else {
        Decision::deny(DecisionReason::DeniedOverlayPrivate)
    }
}

/// Like [`authorize`], additionally requiring that the presented API token
/// carries the scope the action needs.
pub fn authorize_token(
    principal: &Principal,
    scopes: &TokenScopes,
    action: Action,
    resource: &Resource,
    grants: &GrantSet,
) -> Decision {
    if !scopes.contains(action.token_scope()) {
        return Decision::deny(DecisionReason::DeniedTokenScope);
    }
    authorize(principal, action, resource, grants)
}

/// Which projects a principal may read, for filtering inside search, graph
/// expansion and context packing (never after the fact).
///
/// Every role includes read access, so a narrower grant can change *what* a
/// principal may do in a project but never hides it; visibility is therefore
/// the union of all applicable grants.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ProjectFilter {
    all: bool,
    workspaces: BTreeSet<Name>,
    projects: BTreeSet<(Name, Name)>,
}

impl ProjectFilter {
    /// True when `workspace/project` is visible.
    pub fn allows(&self, workspace: &Name, project: &Name) -> bool {
        self.all
            || self.workspaces.contains(workspace)
            || self
                .projects
                .contains(&(workspace.clone(), project.clone()))
    }

    /// True when every project is visible.
    pub fn is_all(&self) -> bool {
        self.all
    }

    /// True when nothing is visible.
    pub fn is_empty(&self) -> bool {
        !self.all && self.workspaces.is_empty() && self.projects.is_empty()
    }

    /// Workspaces visible in full (empty when [`is_all`](Self::is_all)).
    pub fn workspaces(&self) -> impl Iterator<Item = &Name> {
        self.workspaces.iter()
    }

    /// Individually visible projects not already covered by a workspace
    /// (empty when [`is_all`](Self::is_all)), as `(workspace, project)`.
    pub fn projects(&self) -> impl Iterator<Item = &(Name, Name)> {
        self.projects.iter()
    }
}

/// The projects `principal` may read, derived from `grants`.
///
/// Agents get their user's visibility. Deny by default: no grants yields an
/// empty filter.
pub fn visible_projects(principal: &Principal, grants: &GrantSet) -> ProjectFilter {
    let mut filter = ProjectFilter::default();
    for grant in grants.applicable(principal) {
        match grant.scope() {
            ResourceScope::Organization => {
                return ProjectFilter {
                    all: true,
                    ..ProjectFilter::default()
                };
            }
            ResourceScope::Workspace { workspace } => {
                filter.workspaces.insert(workspace.clone());
            }
            ResourceScope::Project { workspace, project } => {
                filter.projects.insert((workspace.clone(), project.clone()));
            }
        }
    }
    let workspaces = &filter.workspaces;
    filter.projects.retain(|(w, _)| !workspaces.contains(w));
    filter
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grant::{Grant, OverlayShare};
    use crate::principal::testutil::*;
    use pretty_assertions::assert_eq;

    fn n(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    const ALL_ROLES: [Role; 4] = [Role::Viewer, Role::Member, Role::Maintainer, Role::Admin];

    fn all_actions() -> Vec<Action> {
        vec![
            Action::ReadCode,
            Action::ReadMemory,
            Action::ProposeMemory,
            Action::AcceptMemory,
            Action::WriteTask,
            Action::ManageIndex,
            Action::ManageWorkspace,
            Action::ManageProviders,
            Action::ManageUsers,
            Action::ReadAudit,
            Action::UseMcp,
            Action::ReadUncommittedOverlay(uid(1)),
        ]
    }

    fn set_with(scope: ResourceScope, role: Role) -> GrantSet {
        let mut set = GrantSet::new();
        set.add(Grant::new(user(1), role, scope).unwrap());
        set
    }

    #[test]
    fn deny_by_default() {
        let set = GrantSet::new();
        for action in all_actions() {
            let d = authorize(&user(1), action, &Resource::Organization, &set);
            assert_eq!(d.reason, DecisionReason::DeniedNoGrant, "{action}");
            assert!(!d.allowed);
        }
    }

    #[test]
    fn matrix_organization_grant_on_project_resource() {
        let set_for = |role| set_with(ResourceScope::Organization, role);
        let project = Resource::project(n("w"), n("p"));
        for role in ALL_ROLES {
            let set = set_for(role);
            for action in all_actions() {
                let d = authorize(&user(1), action, &project, &set);
                let expected = match action {
                    Action::ManageUsers => false, // organization resource only
                    other => role >= other.min_role(),
                };
                assert_eq!(d.allowed, expected, "{role:?} {action}");
            }
        }
    }

    #[test]
    fn matrix_manage_users_on_organization_resource() {
        for role in ALL_ROLES {
            let set = set_with(ResourceScope::Organization, role);
            let d = authorize(&user(1), Action::ManageUsers, &Resource::Organization, &set);
            assert_eq!(d.allowed, role == Role::Admin, "{role:?}");
        }
        let set = set_with(ResourceScope::workspace(n("w")), Role::Admin);
        let d = authorize(
            &user(1),
            Action::ManageUsers,
            &Resource::workspace(n("w")),
            &set,
        );
        assert_eq!(d.reason, DecisionReason::DeniedOrganizationOnly);
    }

    #[test]
    fn matrix_scope_coverage() {
        let project = Resource::project(n("w"), n("p"));
        let other_project = Resource::project(n("w"), n("q"));
        let other_ws = Resource::project(n("x"), n("p"));
        let workspace = Resource::workspace(n("w"));
        for role in ALL_ROLES {
            for action in all_actions() {
                if matches!(action, Action::ManageUsers) {
                    continue;
                }
                let ok = role >= action.min_role();
                let ws_set = set_with(ResourceScope::workspace(n("w")), role);
                assert_eq!(authorize(&user(1), action, &project, &ws_set).allowed, ok);
                assert_eq!(authorize(&user(1), action, &workspace, &ws_set).allowed, ok);
                assert!(!authorize(&user(1), action, &other_ws, &ws_set).allowed);
                assert!(!authorize(&user(1), action, &Resource::Organization, &ws_set).allowed);

                let pr_set = set_with(ResourceScope::project(n("w"), n("p")), role);
                assert_eq!(authorize(&user(1), action, &project, &pr_set).allowed, ok);
                assert!(!authorize(&user(1), action, &other_project, &pr_set).allowed);
                assert!(!authorize(&user(1), action, &workspace, &pr_set).allowed);
            }
        }
    }

    #[test]
    fn narrower_grant_restricts_broader_one() {
        let mut set = GrantSet::new();
        set.add(Grant::new(user(1), Role::Admin, ResourceScope::Organization).unwrap());
        set.add(
            Grant::new(
                user(1),
                Role::Viewer,
                ResourceScope::project(n("w"), n("p")),
            )
            .unwrap(),
        );
        let p = Resource::project(n("w"), n("p"));
        assert!(authorize(&user(1), Action::ReadCode, &p, &set).allowed);
        assert_eq!(
            authorize(&user(1), Action::ManageIndex, &p, &set).reason,
            DecisionReason::DeniedInsufficientRole
        );
        let q = Resource::project(n("w"), n("q"));
        assert!(authorize(&user(1), Action::ManageIndex, &q, &set).allowed);
    }

    #[test]
    fn narrower_grant_can_raise() {
        let mut set = GrantSet::new();
        set.add(Grant::new(user(1), Role::Viewer, ResourceScope::Organization).unwrap());
        set.add(
            Grant::new(
                user(1),
                Role::Maintainer,
                ResourceScope::project(n("w"), n("p")),
            )
            .unwrap(),
        );
        let p = Resource::project(n("w"), n("p"));
        assert!(authorize(&user(1), Action::ManageIndex, &p, &set).allowed);
    }

    #[test]
    fn other_principals_grants_do_not_apply() {
        let set = set_with(ResourceScope::Organization, Role::Admin);
        let d = authorize(&user(2), Action::ReadCode, &Resource::Organization, &set);
        assert_eq!(d.reason, DecisionReason::DeniedNoGrant);
        let d = authorize(&service(1), Action::ReadCode, &Resource::Organization, &set);
        assert_eq!(d.reason, DecisionReason::DeniedNoGrant);
    }

    #[test]
    fn service_account_grants() {
        let mut set = GrantSet::new();
        set.add(Grant::new(service(7), Role::Maintainer, ResourceScope::Organization).unwrap());
        let p = Resource::project(n("w"), n("p"));
        assert!(authorize(&service(7), Action::ManageIndex, &p, &set).allowed);
        assert!(!authorize(&service(7), Action::ManageProviders, &p, &set).allowed);
    }

    #[test]
    fn agent_never_exceeds_user() {
        let project = Resource::project(n("w"), n("p"));
        for role in ALL_ROLES {
            let set = set_with(ResourceScope::Organization, role);
            for action in all_actions() {
                let user_d = authorize(&user(1), action, &project, &set);
                let agent_d = authorize(&agent_of(1), action, &project, &set);
                assert!(
                    !agent_d.allowed || user_d.allowed,
                    "agent exceeded user: {role:?} {action}"
                );
                if !action.agent_permitted() {
                    assert_eq!(agent_d.reason, DecisionReason::DeniedAgentNotPermitted);
                }
            }
        }
    }

    #[test]
    fn agent_gets_nothing_without_user_grants() {
        let set = set_with(ResourceScope::Organization, Role::Admin);
        let d = authorize(
            &agent_of(2),
            Action::ReadCode,
            &Resource::Organization,
            &set,
        );
        assert_eq!(d.reason, DecisionReason::DeniedNoGrant);
    }

    #[test]
    fn agent_cannot_accept_memory_or_administer_even_for_admin_user() {
        let set = set_with(ResourceScope::Organization, Role::Admin);
        let p = Resource::project(n("w"), n("p"));
        for action in [
            Action::AcceptMemory,
            Action::ManageIndex,
            Action::ManageWorkspace,
            Action::ManageProviders,
            Action::ReadAudit,
        ] {
            assert!(
                !authorize(&agent_of(1), action, &p, &set).allowed,
                "{action}"
            );
        }
        assert!(authorize(&agent_of(1), Action::ProposeMemory, &p, &set).allowed);
    }

    #[test]
    fn overlay_visible_only_to_owner() {
        let mut set = GrantSet::new();
        set.add(Grant::new(user(1), Role::Viewer, ResourceScope::Organization).unwrap());
        set.add(Grant::new(user(2), Role::Admin, ResourceScope::Organization).unwrap());
        set.add(Grant::new(service(3), Role::Admin, ResourceScope::Organization).unwrap());
        let p = Resource::project(n("w"), n("p"));
        let act = Action::ReadUncommittedOverlay(uid(1));
        let own = authorize(&user(1), act, &p, &set);
        assert_eq!(own.reason, DecisionReason::GrantedOverlayOwner);
        assert!(authorize(&agent_of(1), act, &p, &set).allowed);
        // Admin and service accounts do not see someone else's overlay.
        let admin = authorize(&user(2), act, &p, &set);
        assert_eq!(admin.reason, DecisionReason::DeniedOverlayPrivate);
        assert!(!authorize(&agent_of(2), act, &p, &set).allowed);
        assert!(!authorize(&service(3), act, &p, &set).allowed);
    }

    #[test]
    fn overlay_explicit_share() {
        let mut set = GrantSet::new();
        set.add(Grant::new(user(1), Role::Viewer, ResourceScope::Organization).unwrap());
        set.add(Grant::new(user(2), Role::Viewer, ResourceScope::Organization).unwrap());
        set.add(Grant::new(user(3), Role::Viewer, ResourceScope::Organization).unwrap());
        set.add_overlay_share(OverlayShare {
            owner: uid(1),
            shared_with: uid(2),
            scope: ResourceScope::project(n("w"), n("p")),
        });
        let act = Action::ReadUncommittedOverlay(uid(1));
        let shared = Resource::project(n("w"), n("p"));
        let unshared = Resource::project(n("w"), n("q"));
        assert_eq!(
            authorize(&user(2), act, &shared, &set).reason,
            DecisionReason::GrantedOverlayShared
        );
        assert!(authorize(&agent_of(2), act, &shared, &set).allowed);
        assert!(!authorize(&user(2), act, &unshared, &set).allowed);
        assert!(!authorize(&user(3), act, &shared, &set).allowed);
        // A share is one-directional.
        let reverse = Action::ReadUncommittedOverlay(uid(2));
        assert!(!authorize(&user(1), reverse, &shared, &set).allowed);
    }

    #[test]
    fn overlay_share_does_not_widen_project_access() {
        let mut set = GrantSet::new();
        set.add(Grant::new(user(1), Role::Viewer, ResourceScope::Organization).unwrap());
        set.add_overlay_share(OverlayShare {
            owner: uid(1),
            shared_with: uid(2),
            scope: ResourceScope::Organization,
        });
        let p = Resource::project(n("w"), n("p"));
        let d = authorize(&user(2), Action::ReadUncommittedOverlay(uid(1)), &p, &set);
        assert_eq!(d.reason, DecisionReason::DeniedNoGrant);
    }

    #[test]
    fn owner_overlay_still_needs_project_access() {
        let set = GrantSet::new();
        let p = Resource::project(n("w"), n("p"));
        let d = authorize(&user(1), Action::ReadUncommittedOverlay(uid(1)), &p, &set);
        assert_eq!(d.reason, DecisionReason::DeniedNoGrant);
    }

    #[test]
    fn token_scope_narrows() {
        let set = set_with(ResourceScope::Organization, Role::Admin);
        let scopes = TokenScopes::read_only();
        let p = Resource::project(n("w"), n("p"));
        assert!(authorize_token(&user(1), &scopes, Action::ReadCode, &p, &set).allowed);
        let d = authorize_token(&user(1), &scopes, Action::ProposeMemory, &p, &set);
        assert_eq!(d.reason, DecisionReason::DeniedTokenScope);
        assert!(TokenScopes::new([]).is_none());
    }

    #[test]
    fn visible_projects_filters() {
        let mut set = GrantSet::new();
        set.add(Grant::new(user(1), Role::Viewer, ResourceScope::workspace(n("w1"))).unwrap());
        set.add(
            Grant::new(
                user(1),
                Role::Member,
                ResourceScope::project(n("w2"), n("p")),
            )
            .unwrap(),
        );
        set.add(
            Grant::new(
                user(1),
                Role::Member,
                ResourceScope::project(n("w1"), n("z")),
            )
            .unwrap(),
        );
        let f = visible_projects(&user(1), &set);
        assert!(!f.is_all() && !f.is_empty());
        assert!(f.allows(&n("w1"), &n("anything")));
        assert!(f.allows(&n("w2"), &n("p")));
        assert!(!f.allows(&n("w2"), &n("q")));
        assert!(!f.allows(&n("w3"), &n("p")));
        // The project grant inside an already visible workspace is folded in.
        assert_eq!(f.projects().count(), 1);
        assert_eq!(visible_projects(&agent_of(1), &set), f);
        assert!(visible_projects(&user(2), &set).is_empty());
        assert!(visible_projects(&agent_of(2), &set).is_empty());
    }

    #[test]
    fn visible_projects_organization_is_all() {
        let set = set_with(ResourceScope::Organization, Role::Viewer);
        let f = visible_projects(&user(1), &set);
        assert!(f.is_all() && f.allows(&n("a"), &n("b")));
    }

    #[test]
    fn action_text_is_stable() {
        assert_eq!(Action::ReadCode.to_string(), "read_code");
        assert_eq!(
            Action::ReadUncommittedOverlay(uid(1)).to_string(),
            "read_uncommitted_overlay:00000000-0000-0000-0000-000000000001"
        );
        assert_eq!(DecisionReason::DeniedNoGrant.code(), "denied_no_grant");
    }

    #[test]
    fn token_scope_classes_are_consistent_with_roles() {
        // Admin-class actions all need Maintainer or above; reads need Viewer.
        for action in all_actions() {
            match action.token_scope() {
                TokenScope::Read => assert_eq!(action.min_role(), Role::Viewer),
                TokenScope::Write => assert_eq!(action.min_role(), Role::Member),
                TokenScope::Admin => assert!(action.min_role() >= Role::Maintainer),
            }
        }
    }
}
