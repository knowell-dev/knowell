//! Identity and access for Knowell: pure logic and crypto, no storage.
//!
//! - [`Principal`], [`Role`], [`Grant`], [`GrantSet`]: who may do what, where.
//! - [`authorize`] / [`authorize_token`] and [`visible_projects`]: the single
//!   decision point, deny by default, most specific scope wins, agents never
//!   exceed their user, uncommitted overlays are private to their owner.
//! - [`issue_token`] / [`verify`]: `kn_` API tokens stored as keyed BLAKE3
//!   hashes.
//! - [`PanelSessionId`], [`CsrfToken`], [`OriginPolicy`]: web-panel security
//!   including DNS-rebinding defence.
//! - [`OidcVerifier`] and [`CiPolicy`]: CI identity seam and policy.
//! - [`AuditEvent`]: stable, secret-free audit records.
//!
//! The crate performs no I/O besides reading OS randomness. Callers load
//! grants and token records from storage and pass them in, and enforce the
//! results inside storage and search.

mod audit;
mod authz;
mod ci;
mod encoding;
mod grant;
mod panel;
mod principal;
mod token;

pub use audit::{AuditError, AuditEvent, RequestId};
pub use authz::{
    Action, Decision, DecisionReason, ProjectFilter, TokenScope, TokenScopes, authorize,
    authorize_token, visible_projects,
};
pub use ci::{
    CiAction, CiClaims, CiDecision, CiPolicy, CiPolicyError, CiReason, CiRule, OidcError,
    OidcVerifier,
};
pub use grant::{Grant, GrantError, GrantSet, OverlayShare, Resource, ResourceScope, Role};
pub use panel::{CsrfKey, CsrfToken, OriginPolicy, PanelError, PanelSessionId};
pub use principal::{AgentSessionId, Principal, ServiceAccountId, UserId};
pub use token::{
    MAX_AGENT_TOKEN_LIFETIME, Pepper, PlaintextToken, StoredToken, TOKEN_PREFIX, TokenError,
    TokenHash, TokenId, VerifiedToken, issue_token, looks_like_token, token_prefix, verify,
};
