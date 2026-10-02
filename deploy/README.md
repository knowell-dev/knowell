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
chmod 600 secrets/*
docker compose up -d --build        # --build: compile from source; omit once an image is published
docker compose ps                   # both services should become healthy
```

## Secrets

No secret value is written in any tracked file.

- `secrets/postgres_password` and `secrets/database_url` are Docker secrets (files, git-ignored).
  `config/hub.toml` refers to the URL as `file:/run/secrets/database_url`.
- Provider API keys: add them to `.env` and reference them from the engine config as
  `env:NAME`, or add another file secret and reference it as `file:/run/secrets/<name>`.
  Never put a key into `compose.yml` or `config/hub.toml`.

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
