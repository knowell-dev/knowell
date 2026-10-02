# deploy/secrets

Local secret files read by `compose.yml` as Docker secrets. Everything here except this
file is git-ignored. Create (values are yours; never commit them):

- `postgres_password`: the PostgreSQL password (no trailing newline needed).
- `database_url`: `postgres://knowell:<that password>@db:5432/knowell` (percent-encode special
  characters in the password; a hex password avoids the problem).

Restrict access: `chmod 600 deploy/secrets/*`.
