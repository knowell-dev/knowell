//! Roles, resource scopes and grants.

use std::fmt;

use knowell_core::Name;
use serde::{Deserialize, Serialize};

use crate::principal::{Principal, UserId};

/// A role, ordered by privilege: `Viewer < Member < Maintainer < Admin`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Reads code and memory.
    Viewer,
    /// Viewer, plus proposes memory and writes tasks.
    Member,
    /// Member, plus accepts memory and manages indexes.
    Maintainer,
    /// Maintainer, plus manages workspaces, providers, users and audit.
    Admin,
}

impl Role {
    /// Stable lowercase name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Member => "member",
            Self::Maintainer => "maintainer",
            Self::Admin => "admin",
        }
    }
}

/// Where a grant applies. A grant covers its scope and everything below it:
/// organization, then workspace, then project.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum ResourceScope {
    /// The whole organization (every workspace and project).
    Organization,
    /// One workspace and its projects.
    Workspace {
        /// Workspace name.
        workspace: Name,
    },
    /// One project.
    Project {
        /// Workspace containing the project.
        workspace: Name,
        /// Project name.
        project: Name,
    },
}

impl ResourceScope {
    /// Shorthand for [`ResourceScope::Workspace`].
    pub fn workspace(workspace: Name) -> Self {
        Self::Workspace { workspace }
    }

    /// Shorthand for [`ResourceScope::Project`].
    pub fn project(workspace: Name, project: Name) -> Self {
        Self::Project { workspace, project }
    }

    /// 0 for organization, 1 for workspace, 2 for project: higher is more
    /// specific and wins in [`crate::authorize`].
    pub fn specificity(&self) -> u8 {
        match self {
            Self::Organization => 0,
            Self::Workspace { .. } => 1,
            Self::Project { .. } => 2,
        }
    }

    /// True when a grant at this scope applies to `resource`. A narrower
    /// grant never covers a wider resource.
    pub fn covers(&self, resource: &Resource) -> bool {
        match (self, resource) {
            (Self::Organization, _) => true,
            (Self::Workspace { workspace: g }, Resource::Workspace { workspace: r }) => g == r,
            (Self::Workspace { workspace: g }, Resource::Project { workspace: r, .. }) => g == r,
            (
                Self::Project {
                    workspace: gw,
                    project: gp,
                },
                Resource::Project {
                    workspace: rw,
                    project: rp,
                },
            ) => gw == rw && gp == rp,
            _ => false,
        }
    }
}

/// The thing an action is performed on.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum Resource {
    /// The organization itself (users, tokens, audit).
    Organization,
    /// A workspace.
    Workspace {
        /// Workspace name.
        workspace: Name,
    },
    /// A project (code, memory, index).
    Project {
        /// Workspace containing the project.
        workspace: Name,
        /// Project name.
        project: Name,
    },
}

impl Resource {
    /// Shorthand for [`Resource::Workspace`].
    pub fn workspace(workspace: Name) -> Self {
        Self::Workspace { workspace }
    }

    /// Shorthand for [`Resource::Project`].
    pub fn project(workspace: Name, project: Name) -> Self {
        Self::Project { workspace, project }
    }
}

/// Stable text form: `org`, `workspace:<w>`, `project:<w>/<p>`.
impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Organization => f.write_str("org"),
            Self::Workspace { workspace } => write!(f, "workspace:{workspace}"),
            Self::Project { workspace, project } => write!(f, "project:{workspace}/{project}"),
        }
    }
}

/// Error building a [`Grant`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    /// Agents hold no grants of their own; they inherit their user's.
    #[error("agents cannot be granted roles directly; grant the user instead")]
    AgentGrantee,
}

/// A role given to a principal at a scope.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Grant {
    principal: Principal,
    role: Role,
    scope: ResourceScope,
}

impl Grant {
    /// Creates a grant for a user or service account.
    ///
    /// # Errors
    /// [`GrantError::AgentGrantee`] when `principal` is an agent.
    pub fn new(principal: Principal, role: Role, scope: ResourceScope) -> Result<Self, GrantError> {
        if principal.is_agent() {
            return Err(GrantError::AgentGrantee);
        }
        Ok(Self {
            principal,
            role,
            scope,
        })
    }

    /// The grantee (a user or service account).
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The granted role.
    pub fn role(&self) -> Role {
        self.role
    }

    /// Where the grant applies.
    pub fn scope(&self) -> &ResourceScope {
        &self.scope
    }
}

/// An explicit share of one user's uncommitted overlay with another user.
///
/// The recipient still needs read access to the project; a share never
/// widens project permissions.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OverlayShare {
    /// The overlay owner.
    pub owner: UserId,
    /// The user the overlay is shared with.
    pub shared_with: UserId,
    /// The projects the share covers.
    pub scope: ResourceScope,
}

/// All grants and overlay shares known to the caller; storage loads these
/// (for a hub, per request) and passes them to the pure functions here.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct GrantSet {
    grants: Vec<Grant>,
    shares: Vec<OverlayShare>,
}

impl GrantSet {
    /// An empty set (everything denied).
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a grant.
    pub fn add(&mut self, grant: Grant) {
        self.grants.push(grant);
    }

    /// Adds an overlay share.
    pub fn add_overlay_share(&mut self, share: OverlayShare) {
        self.shares.push(share);
    }

    /// All grants.
    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }

    /// All overlay shares.
    pub fn overlay_shares(&self) -> &[OverlayShare] {
        &self.shares
    }

    /// Grants that apply to `principal` (an agent inherits its user's).
    pub(crate) fn applicable<'a>(
        &'a self,
        principal: &'a Principal,
    ) -> impl Iterator<Item = &'a Grant> + 'a {
        self.grants
            .iter()
            .filter(move |g| principal.inherits_from(&g.principal))
    }

    /// The role that governs `principal` on `resource`: the highest role among
    /// grants at the most specific covering scope. `None` when nothing covers.
    pub(crate) fn effective_role(
        &self,
        principal: &Principal,
        resource: &Resource,
    ) -> Option<Role> {
        let mut best: Option<(u8, Role)> = None;
        for grant in self.applicable(principal) {
            if !grant.scope.covers(resource) {
                continue;
            }
            let candidate = (grant.scope.specificity(), grant.role);
            best = Some(match best {
                Some(current) if current >= candidate => current,
                _ => candidate,
            });
        }
        best.map(|(_, role)| role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::principal::testutil::*;

    fn n(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    #[test]
    fn roles_are_ordered() {
        assert!(Role::Viewer < Role::Member);
        assert!(Role::Member < Role::Maintainer);
        assert!(Role::Maintainer < Role::Admin);
    }

    #[test]
    fn scope_coverage() {
        let org = ResourceScope::Organization;
        let ws = ResourceScope::workspace(n("w1"));
        let pr = ResourceScope::project(n("w1"), n("p1"));
        let r_org = Resource::Organization;
        let r_ws = Resource::workspace(n("w1"));
        let r_pr = Resource::project(n("w1"), n("p1"));
        let r_other = Resource::project(n("w1"), n("p2"));
        let r_other_ws = Resource::project(n("w2"), n("p1"));
        assert!(org.covers(&r_org) && org.covers(&r_ws) && org.covers(&r_pr));
        assert!(!ws.covers(&r_org) && ws.covers(&r_ws) && ws.covers(&r_pr));
        assert!(!ws.covers(&r_other_ws));
        assert!(!pr.covers(&r_org) && !pr.covers(&r_ws) && pr.covers(&r_pr));
        assert!(!pr.covers(&r_other));
        // Same project name in another workspace must not match.
        assert!(!pr.covers(&r_other_ws));
    }

    #[test]
    fn agent_cannot_hold_grants() {
        assert_eq!(
            Grant::new(agent_of(1), Role::Viewer, ResourceScope::Organization),
            Err(GrantError::AgentGrantee)
        );
    }

    #[test]
    fn most_specific_scope_wins_even_when_lower() {
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
        assert_eq!(set.effective_role(&user(1), &p), Some(Role::Viewer));
        let q = Resource::project(n("w"), n("q"));
        assert_eq!(set.effective_role(&user(1), &q), Some(Role::Admin));
    }

    #[test]
    fn highest_role_within_same_scope() {
        let mut set = GrantSet::new();
        set.add(Grant::new(user(1), Role::Member, ResourceScope::Organization).unwrap());
        set.add(Grant::new(user(1), Role::Maintainer, ResourceScope::Organization).unwrap());
        assert_eq!(
            set.effective_role(&user(1), &Resource::Organization),
            Some(Role::Maintainer)
        );
    }
}
