# knowell-auth

Identity and access for Knowell. Pure logic and crypto: no storage, no network, no
async. Callers load grants and token records and pass them in; Knowell enforces the
answers inside storage and search (search, graph expansion, context packing, memory).

## Authorization

`authorize(&Principal, Action, &Resource, &GrantSet) -> Decision`, plus
`authorize_token` (adds the API token's scope check) and `visible_projects` for
search-time filtering.

Rules, in order:

1. Deny by default.
2. An agent acts for a user: it inherits only that user's grants and may only do
   read, propose-memory, write-task and MCP actions (never accept memory or administer).
3. `ManageUsers` applies to the organization resource only.
4. The most specific covering scope decides the role (project > workspace >
   organization), even if lower than a broader grant. A narrower grant never covers a
   wider resource.
5. The role must reach the action's minimum: Viewer reads code/memory and uses MCP;
   Member proposes memory and writes tasks; Maintainer accepts memory and manages
   indexes; Admin manages workspaces, providers, users and reads audit.
6. `ReadUncommittedOverlay(owner)` additionally requires being that owner (or an agent
   acting for them) or an explicit `OverlayShare`; admins do not see others' overlays.

## Tokens

`kn_` + 52 chars base32 (256 random bits) + 8 chars checksum, lowercase, 63 chars. The
server stores `StoredToken`: keyed BLAKE3 hash under a caller-supplied `Pepper`, a
lookup prefix, principal, scopes, expiry, revocation. Verification is constant-time.
Agent tokens cannot carry the admin scope, must expire and live at most 24 hours.
Plaintext tokens, peppers, keys and session ids have redacted `Debug`.

## Panel

Session ids (256-bit), session-bound stateless CSRF tokens (`nonce.mac`, keyed BLAKE3,
header echo) and `OriginPolicy`, an exact `Host`/`Origin` allow-list (loopback
`127.0.0.1`, `localhost`, `[::1]` with the panel port) that defeats DNS rebinding.

## CI identity

`OidcVerifier` is the seam for JWT verification (implemented later). `CiPolicy` maps
verified `CiClaims` to `CiAction`s: exact issuer, configured repository only,
`pull_request_target` never trusted, refs matched exactly or by a branch/tag-namespace
prefix, optional workflow and environment constraints. Forks and `refs/pull/*` get
nothing.

## Audit

`AuditEvent::to_json_line()` emits a versioned, fixed-order JSON line. It holds only
ids, enum codes and a validated `RequestId` (token-shaped ids are rejected).

## Not in scope

JWT/JWKS verification, persistence, password or OIDC user login, rate limiting.
