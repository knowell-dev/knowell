# Running a Knowell hub with Docker Compose

`compose.yml` runs a Knowell **hub** (shared server for a team) next to PostgreSQL 17 with
pgvector. Single developers do not need this: `know` runs standalone with a managed
PostgreSQL.

| Piece | Details |
|---|---|
| `hub` | `ghcr.io/knowell-dev/knowell` (or built from source with `--build`); non-root (uid 10001), read-only root filesystem, no capabilities; state in the `knowell-data` volume |
| `db` | `pgvector/pgvector:0.8.7-pg17`, pinned by digest; only reachable from the hub; data in the `pgdata` volume |
| Panel / API | Published on `127.0.0.1:7420` by default |

## Start

```sh
cd deploy
cp .env.example .env
printf '%s' "$(openssl rand -hex 24)" > secrets/postgres_password
printf 'postgres://knowell:%s@db:5432/knowell' "$(cat secrets/postgres_password)" > secrets/database_url
openssl rand -hex 32 > secrets/token_pepper
chmod 600 secrets/*
docker compose up -d --build        # --build: compile from source; omit once an image is published
docker compose ps                   # both services should become healthy
```

## Secrets

No secret value is written in any tracked file.

- `secrets/postgres_password` and `secrets/database_url` are Docker secrets (files, git-ignored).
  `config/hub.toml` refers to the URL as `file:/run/secrets/database_url`.
- `secrets/token_pepper` is the stable API-token pepper. The hub configuration references
  `file:/run/secrets/token_pepper`; back it up securely. Replacing it invalidates issued tokens.
- Provider API keys: add them to `.env` and reference them from the engine config as
  `env:NAME`, or add another file secret and reference it as `file:/run/secrets/<name>`.
  Never put a key into `compose.yml` or `config/hub.toml`.

## Users, tokens and login

Token commands are **local database administration**: run them on the installation that
owns the hub database configuration. They do not accept a remote administrator token.
For a non-Compose deployment, set `server.token_pepper = "env:KNOWELL_TOKEN_PEPPER"`
(or a `file:/path` reference) in the engine configuration and provision the same secret
to both `know token` and `know serve`. At least 16 bytes are required.

Bootstrap the first user with an explicit role; later issuance omits `--create-user`:

```sh
docker compose exec hub know token create --principal owner --create-user --role admin \
  --scopes read,write,admin --output /data/.knowell/owner.token
docker compose exec hub know token list --principal owner
```

`--organization` defaults to `local`, matching `know serve`. A new user's grant defaults
to that organization. Use `--grant-workspace NAME` and optionally `--grant-project NAME`
to limit it to existing resources. Existing users' grants are never changed by issuance.
Token scopes default to `read`; a write-capable agent token also needs `write`. The
credential expires after 720 hours by default (`--expires-hours 1..8760`). MCP user calls
act as agents and cannot accept memory even with an administrator credential.

Creation writes a new owner-only file; it refuses to replace any existing path and never
prints the token. Unix mode is 0600; Windows applies an owner ACL before writing bytes.
Transfer the file securely to the edge machine, then use its absolute path:

```sh
know login https://hub.example.com --token-ref file:/absolute/path/owner.token
```

Login checks authenticated hub health before storing the URL and secret reference. It
rejects redirects, invalid responses and remote HTTP; HTTP is allowed only on loopback.
`--skip-verify` explicitly saves an unverified configuration. Login alone does not implement
edge-to-hub indexing or two-machine task synchronization; those remain separate work.

Revoke by the identifier returned by creation or listing:

```sh
docker compose exec hub know token revoke TOKEN_UUID
```

Listing reveals only metadata. Token creation and revocation are audited transactionally
as local database-administrator operations. Revocation and stored grant changes apply to
later HTTP calls, including existing MCP contexts. Keep database administrator credentials
away from agents: they can use the local token commands to administer identities.

## Exposing the hub to a team

The panel binds to loopback on the host. To serve a team, put a TLS reverse proxy (Caddy,
nginx, Traefik) on the same host in front of `127.0.0.1:7420`. Only if a firewall or proxy
network protects the port, set `KNOWELL_PANEL_BIND=0.0.0.0` in `.env`.

## Backups and upgrades

- Back up the `pgdata` volume with `pg_dump` (`docker compose exec db pg_dump -U knowell knowell`)
  and the `knowell-data` volume.
- Upgrade Knowell: change `KNOWELL_IMAGE_TAG`, then `docker compose pull && docker compose up -d`.
- PostgreSQL major upgrades are a dump and restore; do not change the `db` image tag across
  majors on an existing volume.

## Image

`Dockerfile` has two final targets: `runtime` (default, builds `know` from source; used by
this compose file) and `runtime-prebuilt` (copies the release binaries; used by the release
workflow so the published image contains exactly the attested binary). Base images are pinned
by digest.

## Assumptions to confirm when `know serve` lands

The hub command line (`know serve --role hub`), the health check (`know doctor`) and the
container port (7420, the default `server.listen` port) follow the documented design;
adjust `command`, `healthcheck` and the port mapping if the implementation differs.
