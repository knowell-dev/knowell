# knowell-pg-managed

The zero-setup database mode of Knowell: `know` installs and runs its **own**
PostgreSQL, bound to loopback, with a random password it keeps for you.

Knowell has a single storage backend, PostgreSQL + pgvector, provisioned in one of
three ways:

| Mode | Who runs PostgreSQL | This crate |
|---|---|---|
| **Managed** (default for one developer) | Knowell, from binaries it downloads | yes |
| Docker Compose | a `pgvector/pgvector` container | no |
| External server | you | no |

## Lifecycle

```rust
let pg = ManagedPostgres::new(ManagedConfig::new(knowell_home))?; // major 17 by default
pg.install().await?;          // download + SHA-256 verify, cached
pg.init_data_dir().await?;    // initdb, random password, loopback-only config
let port = pg.start().await?; // 127.0.0.1:<persisted free port>
pg.ensure_database("knowell").await?;
let url = pg.connection_url("knowell")?; // SecretString, never log it
pg.stop().await?;
```

`status()` reports `NotInstalled`, `Installed`, `Stopped`, `Running { pid, port }`
or `StalePostmasterPid { pid }`. `start()` refuses to start over a running server
(`AlreadyRunning`) or a crashed one's pid file (`StalePostmasterPid`);
`clear_stale_postmaster_pid()` removes the file only when no server is alive.

Other operations: `backup`, `restore`, `upgrade`, `install_extension_bundle`,
`extension_available`, `run_sql`, `remove_data_dir`.

## File layout

```text
<knowell_home>/pg/
  dist/<x.y.z>/          PostgreSQL binaries (bin/, lib/, share/), shared by all clusters
  password               superuser password (owner-only)
  tmp/                   short-lived PGPASSFILE / --pwfile files, deleted after use
  <major>/
    data/                the cluster; knowell.conf (rewritten at every start) is included
                         from postgresql.conf and holds listen_addresses and port
    state.json           {"schema":1,"port":54321}
    postgres.log         server log
```

## How the binaries are obtained

Binaries come from the `theseus-rs/postgresql-binaries` GitHub releases through
`postgresql_archive` (the crate `postgresql_embedded` is built on), with default
features: `theseus` (download at runtime, nothing bundled in the Rust binary) and
the platform TLS stack (`native-tls`: Schannel on Windows, Secure Transport on macOS,
OpenSSL on Linux). The archive's published SHA-256 is verified before unpacking, and
unpacking happens in a staging directory that is renamed into place.

`postgresql_embedded::PostgreSQL` itself is not used: its `Drop` stops a running
server (unsafe for a daemon that outlives a CLI call), it cannot install without
also running `initdb`, and it probes ports on `0.0.0.0`. The crate drives
`initdb`, `pg_ctl`, `psql`, `pg_dump`, `pg_restore` and `pg_upgrade` directly.

`pg_ctl start` has null standard streams; PostgreSQL writes to `postgres.log`.
On Windows, the small `windows-spawn` dependency restricts handle inheritance to
those three null-device handles. Redirected CLI pipes therefore close when `know`
exits, while PostgreSQL continues running. A failed start reports the exit code and
server log path. Cancelling or timing out the launcher kills it; successful startup
does not attach the server to a kill-on-drop job.

An already cached minor version is reused without network access. Resolving a new
download pages through the unauthenticated GitHub API (60 requests per hour per IP);
set `GITHUB_TOKEN` in the environment to raise the limit.

## Security notes

* **Loopback only.** `listen_addresses = '127.0.0.1'`; on Unix, Unix sockets are
  disabled too. `knowell.conf` is rewritten on every start. `initdb` uses
  `scram-sha-256` for all connections.
* **Password.** 32 alphanumeric characters from the OS CSPRNG (about 190 bits),
  generated at `init_data_dir()`. It is never placed on a command line, logged or
  shown by `Debug`; `connection_url()` returns a `SecretString`. Child processes get
  it through a temporary owner-only `PGPASSFILE` (and `initdb --pwfile`) that is
  deleted afterwards, and captured output is scrubbed of it.
* **Password file permissions.**
  Unix: mode 0600 file in a 0700 directory; reading refuses a file with group or
  other access. Windows: `std` cannot edit ACLs, so the file is created empty,
  `icacls /inheritance:r /grant:r <user>:F` is applied, and only then is the secret
  written (failure aborts). Windows cannot verify the ACL on read, and the
  temporary credential files are protected the same way.
* **Storage seam.** `PasswordStore` (`get`/`set`) is the extension point; the
  file store is the only implementation today, an OS-keychain store is planned.
* Backups are written to `<dest>.partial` and renamed; 0600 on Unix.
* Identifiers (database and extension names) are limited to `[a-z_][a-z0-9_]{0,62}`
  before they reach SQL.
* Clusters use `--locale=C`, UTF-8 and data checksums, so they behave the same on
  every platform and can be upgraded between majors with `pg_upgrade`.

## Backup, restore, upgrade

* `backup(db, path)` runs `pg_dump -Fc`.
* `restore(path, db)` creates a **new** database (error if it exists), runs
  `pg_restore --exit-on-error`, and drops the database again on failure.
* `upgrade(new_major, extension_bundle)` needs a stopped cluster. It installs the new
  major (and the pgvector bundle for it, which `pg_upgrade` needs when databases use
  pgvector), runs `initdb` with the same password, then `pg_upgrade --check` and
  `pg_upgrade` in copy mode. The old data directory is never touched; call
  `remove_data_dir()` on the old handle once the new cluster is verified. On failure
  the new data directory is removed. If `pg_upgrade` is missing from a distribution
  the error is `PgUpgradeUnavailable`. Run `ANALYZE` after the first start of the
  upgraded cluster (`pg_upgrade` does not carry statistics over).

## pgvector bundle layout (for the release CI)

Knowell's release CI builds and attests pgvector per platform and PostgreSQL major
version against the headers of the distribution that `install()` downloads. The
bundle is a **flat directory**:

```text
<bundle>/
  vector.control        required; contains  default_version = 'X.Y.Z'
  vector--X.Y.Z.sql     required; the script for default_version
  vector--A--B.sql      optional upgrade scripts, any number
  vector.so             required on Linux
  vector.dylib          required on macOS (vector.so is also accepted)
  vector.dll            required on Windows
```

Other files (licence, manifest, `bitcode/`) are ignored. Symlinks among the files
above are rejected. `install_extension_bundle(dir)` validates the bundle, asks the
installed `pg_config` for `--pkglibdir` and `--sharedir`, and copies the library to
`pkglibdir` and the control and SQL files to `<sharedir>/extension`, each through a
temporary name and a rename. It returns the pgvector version. The server need not be
stopped; `extension_available("vector")` then returns that version, and
`CREATE EXTENSION vector` works. The bundle must match the platform and PostgreSQL
major of the install; there is no cross-checking beyond the file names, so the CI
attestation is what vouches for it.

## Tests

Unit tests run everywhere, including a synthetic three-process regression which
checks that the caller's stdout/stderr reach EOF while its server remains alive.
They also cover launcher timeout/cancellation, environment isolation and invalid
process input. The CLI's ignored `managed_init_backup_restore` test exercises real
`know init` with bounded piped output and checks that PostgreSQL is still running.
`tests/e2e.rs` downloads and runs a real PostgreSQL and is
`#[ignore]`d (nightly or manual):

```text
python scripts/buildlock.py cargo test -p knowell-pg-managed --test e2e -- --ignored --nocapture
```

## Known limits

* Windows: see the ACL note above. `psql` decodes its arguments with the ANSI code
  page, so non-ASCII SQL literals passed to `run_sql` must use `U&'\00f6'` escapes.
  `pg_upgrade` writes to a file because its temporary servers can inherit its
  output handles. `pg_ctl start` uses the restricted null-stream launch described
  above; it cannot inherit the CLI's output pipes.
* One process at a time should manage a given home; there is no cross-process lock.
