# knowell-server

The HTTP side of Knowell (axum 0.8): the versioned REST API under `/api/v1` with an SSE
progress stream, the embedded local panel, webhook receivers for GitHub / GitLab / Gitea,
and the mount point for MCP over Streamable HTTP at `/mcp`. One binary runs it in every
role; only `hub` may listen on a non-loopback address.

```rust,ignore
let listener = knowell_server::bind(&config).await?;            // validates, loopback rule
let state = AppState::builder(config)
    .with_store(store)                                            // store-backed routes
    .with_engine(engine)                                          // delegated routes
    .with_store_tokens().with_pepper(pepper)                      // tokens + grants in the database
    .with_store_audit()                                           // audit events → audit_log
    .with_webhook_secrets(secrets)                                // per-provider secrets
    .with_mcp(knowell_mcp::streamable_http_router(server, &opts)) // /mcp
    .build()?;
state.events().publish(scope, event);                             // indexer progress → SSE
knowell_server::serve(listener, build_router(state.clone()), shutdown).await?;
state.flush_audit().await;                                        // queued audit events reach the database
```

Tokens and grants come from `with_token_store(Arc<dyn TokenStore>)` (default: an empty
`MemoryTokenStore`, which denies everything) or `with_store_tokens()` (`StoreTokenStore`
over the store's `principal` / `access_grant` / `api_token` tables). Audit events go to
`with_audit(Arc<dyn AuditSink>)` (default: `TracingAuditSink`) or `with_store_audit()`
(`StoreAuditSink`, writing the store's append-only `audit_log`). The `with_store_*`
options need `with_store`, and `with_store_audit` needs `build` to run inside a tokio
runtime; otherwise `build` fails with `ServerError::Config`. The last of
`with_token_store` / `with_store_tokens` (and of `with_audit` / `with_store_audit`) wins.

Use `AuthenticatedCallers` as the MCP server's caller resolver. It accepts only
authenticated HTTP request extensions and preserves the typed principal and token
scopes. The CLI configures the same `server.token_pepper` secret reference for token
issuance and verification; a configured reference that cannot be resolved prevents
startup. Without a configured pepper, bearer authentication stays disabled.

Without a store, store-backed routes answer `503 store_unavailable`; without an engine,
delegated routes answer `503 engine_unavailable` with the configured reason
(`AppStateBuilder::engine_unavailable_reason`). Nothing is ever faked: fields the server
cannot know yet are `null`.

## Route table

Auth: **public** = no credentials (Host/Origin checks still apply); **auth** = bearer token
or session cookie; cookie-authenticated non-GET requests also need an allowed `Origin` and
`X-Knowell-CSRF`. "Org-wide" means `authorize(action, Organization)` must allow it.

| Method | Path | Auth / authorization | Backed by |
|---|---|---|---|
| GET | `/api/v1/health/live` | public | process |
| GET | `/api/v1/session` | public (issues a local session when allowed, else 401) | sessions |
| POST | `/api/v1/session/login` | public, `Origin` required; body `{token}` (user token) | token store |
| POST | `/api/v1/session/logout` | auth | sessions |
| GET | `/api/v1/health` | auth | store `check_server`, `job_counts`; engine `HealthDetail` |
| GET | `/api/v1/workspaces`, `/workspaces/{id}` | auth, `read_code` scope, filtered by visibility | store + `knowell.toml` |
| GET | `/api/v1/projects[?workspace=<id>]`, `/projects/{id}` | auth, `read_code` scope, visibility | store + `knowell.toml` |
| GET | `/api/v1/indexes` | auth, visibility; jobs only for org-wide `read_code` | store: `list_jobs` (this organization's and unscoped jobs) |
| POST | `/api/v1/indexes/reindex` | `manage_index` on the view's project (audited) | store: `enqueue_scoped` `view.reindex` (the view's workspace) |
| GET | `/api/v1/jobs[?state=&limit=]` | org-wide `read_code` | store: `list_jobs` (this organization's and unscoped jobs) |
| POST | `/api/v1/jobs/{id}/retry` | org-wide `manage_index` (audited) | store `requeue_dead` |
| GET | `/api/v1/events` | auth, `read` scope; events filtered by visibility | `EventBus` (SSE) |
| POST | `/api/v1/search` | `read_code` pre-check | engine |
| GET | `/api/v1/graph?mode=&parent=`, `/graph/insights` | `read_code` pre-check | engine |
| POST | `/api/v1/graph/trace`, `/graph/impact`, `/context` | `read_code` pre-check | engine |
| GET | `/api/v1/domains`, `/glossary`, `/rules`, `/profiles`, `/profiles/{id}/switch-estimate`, `/quality/reports` | `read_code` pre-check | engine |
| GET | `/api/v1/memory`, `/tasks` | `read_memory` pre-check | engine |
| POST | `/api/v1/memory/{id}/decision` | `accept_memory` pre-check (engine authorizes the record's scope and audits) | engine |
| POST | `/api/v1/profiles/switch` | org-wide `manage_providers` (audited) → 202 | engine |
| GET | `/api/v1/usage?days=1..365`, `/integrations` | org-wide `read_code` | engine |
| GET | `/api/v1/admin` | non-hub: `{available:false,…}`; hub: org-wide `manage_users` | engine |
| POST | `/api/v1/webhooks/{github,gitlab,gitea}` | signature (no session/token) | store: `enqueue_scoped` `source.refresh` (the organization) |
| any | `/mcp`, `/mcp/{*rest}` | auth + `use_mcp` pre-check | the MCP router |
| GET/HEAD | `/`, `/{file}` | public | embedded / directory panel |

The pre-check (`Caller::precheck`) applies what is known without the concrete resource:
the agent action ceiling, the token's scope and a non-empty visibility. The engine gets the
caller's principal, grants and visible projects (`EngineContext`) and filters inside search,
graph expansion, context packing and memory.

Job kinds and payloads (camelCase JSON) for the indexer:

- `view.reindex` (`JOB_KIND_VIEW_REINDEX`): `viewId`, `projectId`, `projectName`,
  `workspaceName`, `scope` (`changed` | `full`), `requestedBy`; priority 10; idempotency
  key `view.reindex:<principal>:<Idempotency-Key>` when the header is sent.
- `source.refresh` (`JOB_KIND_SOURCE_REFRESH`): `provider`, `deliveryId`, `repository`,
  `repositoryUrls` (credential-free), `ref`, `before`, `after`; priority 5; idempotency key
  `webhook:<provider>:<delivery id>`.

## Middleware order

Outermost first:

1. **Request id** — UUIDv7 per request in `X-Request-Id`; added as `requestId` to every
   problem body; bare error statuses (router 404/405) become problem+json.
2. **Security headers** — `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`,
   `X-Frame-Options: DENY`, COOP/CORP `same-origin`, CSP (panel files: the panel policy
   below; everything else `default-src 'none'; frame-ancestors 'none'`), `Cache-Control:
   no-store` unless set, HSTS when `public_base_url` is https.
3. **Panic catcher** — a panic below answers `500 internal_error` (payload not logged).
4. **Host / Origin** — `Host` must be on the allow-list (DNS-rebinding defence; a missing
   port means 80, or 443 when the public URL is https); an `Origin`, when present, must be
   allowed on every method. The literal `null` origin is refused.
5. **Per route group** — `/api/v1` and webhooks: request timeout (time to response headers;
   streams continue). `/mcp`: body limit. Protected routes: authentication (below).
6. **Handlers** — JSON body limit + `Content-Type: application/json` + validation, then
   authorization of the concrete action and audit.

## Security model

- **Binding.** Every role except `hub` must listen on loopback (same rule as
  `knowell-config`); `bind` and `AppStateBuilder::build` refuse otherwise. A hub on a
  non-loopback address needs `public_base_url` (or `extra_allowed_hosts`).
- **Host allow-list.** `127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>`, the listen IP
  when specific, the public URL's authority and `extra_allowed_hosts` (exact `host:port`
  matches via `knowell_auth::OriginPolicy`).
- **Panel sessions.** `GET /api/v1/session` returns `{user, role, csrfToken, expiresAt}`.
  On a non-hub server with `local_user` set, a request without a valid cookie gets a new
  session for that user. A hub never does: the panel signs in with
  `POST /api/v1/session/login {token}` (a user's API token; the session is bounded by the
  token's scopes). The cookie is `knowell_session` (`__Host-knowell_session` + `Secure`
  over https), `HttpOnly; SameSite=Strict; Path=/`. Sessions live in memory (idle 12 h,
  absolute 7 days, at most 1024, least recently used evicted).
- **CSRF.** Session-bound stateless tokens (`knowell_auth::CsrfToken`, keyed per process).
  Required in `X-Knowell-CSRF` on every cookie-authenticated non-GET/HEAD/OPTIONS request,
  together with an allowed `Origin`. Bearer requests are exempt (no ambient credential).
- **API tokens.** `Authorization: Bearer kn_…`, looked up by prefix in the `TokenStore`,
  verified in constant time with the server pepper. 401 codes: `invalid_token`,
  `token_expired`, `token_revoked`, `unauthenticated`. Without a pepper, bearer tokens are
  refused. A bearer header takes precedence over a cookie. After a successful verification
  the server calls `TokenStore::token_used`; `StoreTokenStore` records `last_used_at` at
  most once per `DEFAULT_TOKEN_TOUCH_INTERVAL` (60 s, `with_touch_interval`) per token.
- **Database identity (`StoreTokenStore`).** Tokens and grants of the configured
  organization are read per request (revocations and grant changes apply at once). Only
  the lookup prefix and the keyed hash are stored; several records may share a prefix and
  the matching one is found by hash. A disabled principal's tokens answer `token_revoked`
  and it holds no grants. Agent tokens belong to the user they act for and carry the agent
  client and session. `save_token` persists a record from `knowell_auth::issue_token`;
  `revoke_token` revokes one. Overlay shares are not persisted yet (none are returned).
- **Authorization.** `knowell_auth::authorize` / `authorize_token` per route; reads filter
  by `visible_projects`. Invisible resources answer 404, not 403.
- **Audit.** Every denial and every allowed state change becomes a `knowell_auth::AuditEvent`
  in the `AuditSink` (default: JSON lines on the `knowell::audit` tracing target).
  `StoreAuditSink` queues events (bounded, `DEFAULT_AUDIT_QUEUE`) for a background task
  that writes them to `audit_log` in batches, with the organization id, the actor's text
  form and acting principal, action, resource, decision and request id. Recording never
  blocks a request; when the queue is full or a write fails, the event is logged as its
  JSON line at `warn` on `knowell::audit` instead. `AppState::flush_audit` waits for the
  queue to drain.
- **Limits.** JSON bodies 1 MiB, webhooks 10 MiB, MCP 4 MiB (announced or streamed), request
  timeout 30 s; all in `Limits`.
- **Errors.** RFC 7807 `application/problem+json`: `type` (`urn:knowell:problem:<code>`),
  `title`, `status`, `detail`, plus `code`, `message` (= `detail`) and `requestId`, which is
  what the panel's `ApiError` reads. Messages are fixed text: no request input, token,
  secret or connection string is ever echoed (serde errors are reduced to line/column).

## Panel embedding

`build.rs` checks for `panel/dist/index.html`. If present, the crate is compiled with
`--cfg knowell_panel_dist` and `include_dir!` embeds `panel/dist` (this is what the release
workflow does after downloading the panel artifact). Otherwise it embeds
`panel-placeholder/`, one CSP-clean page that explains how to build the panel, so Rust-only
builds (CI, `cargo test`) never fail. Cargo cannot watch a directory that does not exist
without rebuilding every time, so after building the panel for the first time run
`cargo clean -p knowell-server` (or touch `build.rs`).

`PanelMode::Directory(path)` serves a directory at run time (panel development);
`PanelMode::Disabled` turns the panel off. `assets/*` (hashed by Vite) are served with
`Cache-Control: public, max-age=31536000, immutable`; everything else, including
`index.html`, with `no-cache`. MIME types come from `mime_guess` (text types with
`charset=utf-8`). Paths are percent-decoded strictly; `..`, `.`, empty segments, backslashes,
drive letters and control characters are refused, and directory mode re-checks the
canonical path stays inside the root. There is no SPA fallback (the panel uses hash
routing). Panel CSP:

```text
default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; connect-src 'self';
frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'
```

Panel development against a real engine: the Vite dev server proxies `/api` with its own
`Host` (`127.0.0.1:5173`), so add that to `extra_allowed_hosts`.

## Webhook setup

Configure one secret per provider (at least 16 bytes; resolve it from a secret reference
and pass it as `SecretString` in `WebhookSecrets`). A provider without a secret has no
endpoint (404).

| Provider | URL | Content type | Secret header | Delivery id |
|---|---|---|---|---|
| GitHub | `https://<hub>/api/v1/webhooks/github` | `application/json` | `X-Hub-Signature-256` (HMAC-SHA256) | `X-GitHub-Delivery` |
| GitLab | `https://<hub>/api/v1/webhooks/gitlab` | (JSON) | `X-Gitlab-Token` (compared in constant time) | `Idempotency-Key`, else `X-Gitlab-Event-UUID` |
| Gitea / Forgejo | `https://<hub>/api/v1/webhooks/gitea` | `application/json` | `X-Gitea-Signature` (HMAC-SHA256) | `X-Gitea-Delivery` |

Only push events are queued (GitHub `push`, GitLab `Push Hook` / `Tag Push Hook`, Gitea
`push`); GitHub `ping` and other events answer `200 {"accepted":false,…}`. A queued push
answers `202 {"accepted":true,"jobId":…,"created":…}`. The signature is verified before the
body is parsed; the payload is trusted only to name the repository (`owner/name`, clone
URLs without credentials, ref, before/after). The indexer maps it to registered sources
and re-reads git itself. Redeliveries with the same delivery id return the same job
(`created: false`) while the job record exists. The signature does not cover the delivery
id, so a replayed body with a new id can at most cause a redundant refresh.

## MCP mount

`with_mcp(router)` takes a router that serves `/mcp` itself (as
`knowell_mcp::streamable_http_router` builds it); requests keep their path. In front of it:
Host/Origin checks, the MCP body limit, authentication and the `use_mcp` pre-check. The
`Authenticated` caller is inserted into the request extensions so an MCP `CallerResolver`
can read it from `http::request::Parts`. The MCP transport's own host/origin options still
apply; keep them consistent with the server's allow-list (its defaults compare loopback
host names).

## Serving and shutdown

`serve(listener, router, shutdown)` uses `into_make_service_with_connect_info` (the MCP
`LocalOnly` resolver checks peer addresses). When `shutdown` resolves, it stops accepting,
ends SSE streams and waits up to `DEFAULT_SHUTDOWN_GRACE` (10 s, `serve_with_grace` to
change) for in-flight requests.

## Tests

```sh
python scripts/buildlock.py cargo test -p knowell-server
```

Unit tests sit next to the code (parsers, signatures, cookies, sessions, paths, config).
`tests/server/` drives the full router with `tower::ServiceExt::oneshot` (and a real TCP
listener for serving and shutdown); `tests/server/identity.rs` covers `StoreTokenStore`,
`StoreAuditSink` and tenant-scoped job listings against the database. Store-backed tests need `KNOWELL_TEST_DATABASE_URL`
exactly as `crates/knowell-store/README.md` describes; without it they print one
`skipping …` line and pass.

## Known gaps

- Job listings show this organization's jobs plus unscoped ones (jobs enqueued without a
  tenant, e.g. by producers that still call `jobs::enqueue`); `GET /api/v1/health` queue
  counts and the oldest queued age cover the whole queue.
- Overlay shares are not persisted, so `StoreTokenStore` grants no shared overlays.
- Sessions and CSRF keys are per process: several hub replicas need sticky sessions until
  a shared session store exists. No rate limiting yet.
- Not yet reported by the store (always `null`): workspace members and profile, project
  kind / languages / file count / worktrees / analysis level, view tiers and coverage,
  generation profile and chunk counts, profile migrations.
