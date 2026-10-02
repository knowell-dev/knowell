# Knowell panel

The local web panel of the Knowell engine: a Svelte 5 (runes) + TypeScript single-page app built
with Vite. It builds to static files in `panel/dist`; `knowell-server` serves them on
`127.0.0.1`. There is no Node process at runtime.

Screens: Overview, Workspaces, Projects, Indexes, Jobs, Search playground, Code graph, Domains and
glossary, Memory, Rules, Model profiles, Quality, Agents and usage, Integrations, Administration
(hub role only).

## Develop against the mock client

```sh
cd panel
npm ci
npm run dev          # http://127.0.0.1:5173, fictional e-commerce workspace
```

In dev the app uses `MockApiClient` (`src/lib/api/mock.ts`, data in `mockData.ts`); no engine is
needed. Switches (Vite env vars):

| Variable                       | Effect                                                                                                                  |
| ------------------------------ | ----------------------------------------------------------------------------------------------------------------------- |
| `VITE_API=http`                | Use the real `HttpApiClient` (`/api/v1/...`); the dev server proxies `/api` to `127.0.0.1:7420` (see `vite.config.ts`). |
| `VITE_API=mock`                | Force the mock, also in a production build (used by the e2e smoke test).                                                |
| `VITE_MOCK_ROLE=hub`           | Mock reports the `hub` role, so the Administration screen shows data.                                                   |
| `VITE_MOCK_SHAPE=server`       | Mock sends `null` wherever the real server cannot know a value yet (default `rich` fills everything for demos).         |
| `VITE_MOCK_LOGIN=1`            | Mock answers 401 until a token starting with `kn_` is submitted (sign-in form).                                         |
| `VITE_MOCK_ENGINE=unavailable` | Engine-delegated routes answer 503 `engine_unavailable` (explanatory empty states).                                     |

A normal `npm run build` contains no mock code or fixture data.

The mock accepts `MockOptions`: `latencyMs`, `role`, `empty` (to see empty states), `failures`
(per-method `ApiError`, to see error states), `shape` (`rich` or `server`), `orgWide` (false hides
jobs and dead letters like a caller without organization-wide read access), `engineUnavailable`
(a reason string) and `requireLogin`. Tests use these.

## Develop against the real server

```sh
# terminal 1: know serve on 127.0.0.1:7420 (see crates/knowell-server/README.md)
# terminal 2:
cd panel
VITE_API=http npm run dev       # http://127.0.0.1:5173, /api is proxied to 127.0.0.1:7420
```

`knowell-server` checks the `Host` header (DNS-rebinding defence) and the `Origin` header (CSRF
defence, required on login and on every cookie-authenticated mutation). A plain proxy would send
`Host: 127.0.0.1:5173` and `Origin: http://127.0.0.1:5173`, which the server rejects. Two ways to
deal with that; this repository uses the second:

1. Add `127.0.0.1:5173` to the server's `extra_allowed_hosts`. This widens the server allow-list for
   a development-only reason and has to be remembered on every dev machine.
2. Let the proxy rewrite the request (`vite.config.ts`): `changeOrigin: true` sets `Host` to the
   server's own host, and an `Origin` header equal to this dev server's own origin
   (`http://127.0.0.1:5173` or `http://localhost:5173`) is rewritten to the server's origin.
   Any other `Origin` is forwarded unchanged, so a request from another site is still refused by
   the server. The dev server itself only listens on `127.0.0.1` and has Vite's own host check,
   so the cross-site protection is not lost, it moves in front of the proxy for dev. The server
   needs no special configuration. `KNOWELL_API_TARGET` overrides the target
   (default `http://127.0.0.1:7420`).

The session cookie (`knowell_session`, `SameSite=Strict`, `HttpOnly`) works through the proxy
because the browser only ever talks to `127.0.0.1:5173`.

## Wire contract notes

`src/lib/api/types.ts` mirrors `crates/knowell-server/src/wire.rs` (camelCase, RFC 3339 times,
milliseconds). A field the server cannot know yet is `null`; every screen shows "not reported
yet" for it and never prints `null`, `undefined` or `0` in its place.

- Errors are `application/problem+json`; the panel reads `code`, `message`, `requestId`.
- 503 `engine_unavailable` (also `store_unavailable`, `not_initialized`) is shown as an
  explanatory empty state containing the server's reason, not as an error.
- Role `hub`: the panel shows a sign-in form (`POST /session/login {token}`) when `GET /session`
  answers 401, and a Sign out button (`POST /session/logout`). The token is never stored. A 401 on
  any later request returns the user to the form.
- Reindex sends an `Idempotency-Key` header and reads `202 {jobId, created}`. Retry of a dead
  letter reads `204`, or `409 not_dead` / `404`. A profile switch reads `202` with the engine body.
- `GET /api/v1/events` (SSE): `job`, `generation`, `heartbeat` and `resync`. `resync`, and any
  reconnect of the stream, makes Indexes and Jobs refetch.

## Scripts

| Script             | Does                                                                     |
| ------------------ | ------------------------------------------------------------------------ |
| `npm run dev`      | Vite dev server                                                          |
| `npm run build`    | Production build to `dist/`                                              |
| `npm run check`    | `svelte-check` (TypeScript strict)                                       |
| `npm run lint`     | ESLint flat config (Svelte + TypeScript)                                 |
| `npm run test`     | Vitest + Testing Library (jsdom)                                         |
| `npm run test:e2e` | Playwright smoke test (run `npx playwright install chromium` once first) |
| `npm run format`   | Prettier                                                                 |

In this repository, run every npm command through the machine build lock, for example
`python scripts/buildlock.py npm --prefix panel run build`.

## Layout

```
src/lib/api/types.ts    all REST wire types (hand-mirrored from knowell-server wire.rs)
src/lib/api/client.ts   ApiClient interface, ApiError, HttpApiClient (fetch, CSRF header, SSE)
src/lib/api/mock.ts     MockApiClient + mockData.ts fixtures (+ mockServerShape.ts: null-heavy variant)
src/lib/components/     small accessible UI parts (DataState, Card, Badge, Meter, Tabs, ...)
src/lib/router.svelte.ts hash router (no server fallback needed)
src/routes/             one file per screen
src/app.css             design tokens (dark default, light via data-theme)
```

## How the Rust server embeds the build

1. CI or `cargo xtask` runs `npm ci && npm run build` in `panel/`.
2. `knowell-server` embeds `panel/dist` into the binary (for example with `rust-embed` or
   `include_dir`) and serves it at `/` with an `index.html` for `/`.
3. Routing is hash based (`#/projects?id=...`), so the server needs no SPA fallback and the panel
   can be mounted under any prefix (`base: './'`).
4. The API is expected at `/api/v1/...` on the same origin, with the session cookie and a CSRF
   token returned by `GET /api/v1/session`; the client sends it as `X-Knowell-CSRF` on
   mutations. Progress events use `GET /api/v1/events` (SSE).
5. Suggested response headers: `Content-Security-Policy: default-src 'self'; img-src 'self' data:;
style-src 'self'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'`. The panel
   uses no inline scripts, inline event handlers, `eval` or external resources; styles are in
   bundled CSS (dynamic widths are set through the CSSOM, which CSP allows).

## Security rules for contributors

- Never render a secret value. Credentials appear only as references (`env:NAME`); use
  `SecretRef`.
- Repository text (snippets, paths, memory bodies) is rendered as text only. `{@html}` is a lint
  error. Use `CodeBlock` for source.
- No external fonts, CDNs or scripts: the panel works offline.

## Known limits

- The code graph uses a simple SVG layered layout; sigma.js (WebGL) will replace it for large
  graphs. A CodeMirror code viewer is also planned.
- Freshness tiers are labelled T0 to T3 as in `docs/ARCHITECTURE.md` section 6.4.
