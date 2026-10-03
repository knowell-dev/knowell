# Releasing Knowell

Public releases start at **stable 1.0.0**. No 0.x release or 1.0.0 prerelease is
published or installed by the bootstrap scripts. A later preview requires published
stable 1.0.0. Publication-free development dry runs keep the actual workspace version;
`--allow-unreleased` is permitted only when generating and validating their assets.
Those artifacts are not eligible for native updates.

## Installation ownership

| Installation | Runtime and update owner |
|---|---|
| Direct bootstrap | Stable native launcher, immutable version directories, explicit `know update` |
| Homebrew, Scoop, winget, `.deb`, `.rpm`, Cargo | Package manager installs the `know` engine; update through that manager |
| npm / npx | JavaScript launcher pins the package's engine version; update the npm package |
| Container | Image contains the engine; pull an explicit image version/digest and recreate it |
| Source build / unmanaged archive | Operator-owned engine; no automatic adoption or overwrite |

Manager archives retain `<archive>/know[.exe]` as the engine. Official archives also
contain `know-launcher[.exe]` for bootstrap compatibility, but package templates select
the engine. Merely having a launcher in an archive does not create a direct-install receipt.
No native updater mutates a manager installation or changes an npm package's pin.

The default direct roots are `~/.local/share/knowell/install` and
`%LOCALAPPDATA%\Programs\knowell`. POSIX installs create a new
`~/.local/bin/know` symlink; Windows adds the dedicated root to the user's PATH.
`--install-dir` / `-InstallDir` selects a **new dedicated root**, never a shared bin
folder. Bootstrap refuses any existing root, command conflict, symlink or reparse-point
ancestor, and competing bootstrap lock. It does not adopt old unmanaged installations.
Choose a new root and change PATH explicitly to migrate an old installation.
POSIX publication also requires the destination parent to belong to the current user
and prohibit group/other writes. Choose a private 0700 subdirectory when installing
under a shared location such as `/tmp`; the installer never chmods a shared parent.

A direct root contains:

```text
know[.exe]                         stable launcher
install.json                      format_version=1, owner=direct, target, launcher_protocol=1
launcher.json                     independent launcher version, target, SHA-256 and size
current.json / previous.json      engine image records, format_version=1
versions/<version>/<target>/know[.exe]
versions/<version>/<target>/know-launcher[.exe]   retained prepared launcher
metadata/                         verified TUF history, separate from KNOWELL_HOME
runtime.lock / update.lock / launcher.lock
transaction.json                  explicit activation/recovery journal, when needed
```

Private POSIX directories use mode 0700 and bootstrap files inherit a restrictive umask.
Windows bootstrap replaces the root DACL with protected owner, SYSTEM and Administrators
access; subsequent runtime files inherit that protected ACL. Shared bin directories are
never chmodded. The runtime stays in its version directory while active processes run.
The updater does not erase retained engines automatically.

## Bootstrap trust and native update trust

Bootstrap downloads bounded digest-prefixed **raw** engine and launcher files, checks
both against `SHA256SUMS`, stages a complete private installation, then publishes its
receipts and initial pointer. It does not extract arbitrary archives or overwrite a
running executable. Initial HTTPS/checksum trust is an independent bootstrap boundary:
checksums from the same release do not authenticate a TUF root.

`--attestation require` / `-Attestation require` also requires authenticated `gh` and
verifies repository, `.github/workflows/release.yml`, and the exact source tag for both
components. `auto` checks provenance when authenticated `gh` is available; `skip` is
explicitly for controlled bootstrap verification. Ordinary native updates do not need
`gh`, a GitHub account, or a token.

Native updates require TUF signatures, expiry checks, monotonic metadata history,
root-threshold verification and exact signed component hashes and lengths. There is no
fabricated production trust root or inferred production metadata host. The owner must
provision an independently distributed public root and a signed repository first:

```sh
know update --configure-source --trust-root /trusted/root.json \
  --metadata-url https://updates.example.invalid/metadata/ \
  --targets-url https://github.com/knowell-dev/knowell/releases/download/
know update --plan
know update --prepare --version 1.1.0
know update --apply
know update --status
```

`example.invalid` is a placeholder, not a deployed service. Offline sources require
explicit `--offline` and two `file:///` URLs. Provisioning verifies the signed repository
before storing its public source; absence, expiry, malformed metadata or invalid
signatures fail closed. Source credentials and signing keys never belong in receipts.
No scheduled background updater or forced update is installed.

An explicit `--configure-source` may correct or relocate repository URLs only with the
exact original bootstrap root bytes. It preserves accepted root, metadata, and clock
history, and replaces the endpoint pin only after the new signed repository verifies.
Changing the bootstrap anchor or clearing trust state is not a source-repair procedure.

Activation requires all participating engines to close and database admission to enter
maintenance. Schema-changing activation requires explicit `--allow-migration` and a
fresh managed backup (`--backup PATH`) or the external-backup confirmation option.
External database operators must confirm direct/session connections and that every
client participates in admission or has stopped. A failed migration does not silently
activate the candidate. Config/schema/index/jobs/protocol/launcher compatibility comes
from signed explicit ranges, never SemVer assumptions. Release contract changes require
reviewing `dist/update-compatibility.json` together with actual store migrations.

Interrupted activation uses `know update --recover old` or `--recover new`; interrupted
trust-state publication uses `--recover-metadata`. Rollback is explicit and subject to
current signed revocation and data compatibility. Restoring data is a separate operator
decision; binary rollback does not silently restore a database backup.

Pre-migration backups are captured under exclusive admission and include the persistent
maintenance owner. Record the UUID with the protected backup. After a deliberate restore,
inspect `know maintain --status` using the configuration of the restored database and a
schema-compatible retained engine. Reconcile the software journal with `--recover old`
or `--recover new`, or explicitly finish manager-owned maintenance with the same
`know maintain --operation UUID`. Runtime admission remains closed until schema and
owner validation succeed; do not delete the intent table to bypass recovery.

Launcher replacement is separate from engine activation. Close every launcher first,
then invoke the retained **raw engine**:

```sh
<root>/versions/<active-version>/<target>/know update --launcher --install-root <root>
```

Use `know.exe` on Windows. This command verifies the active release's signed paired
launcher and its independent record. If launcher replacement was interrupted or `know`
is unavailable, invoke that retained raw engine with the explicit install root. Bootstrap
scripts cannot repair an existing root by overwriting it. Preserve the root and trust
history for recovery; never delete metadata to bypass rollback/expiry protections.

## Release assets and signed target contract

| Channel | Published content |
|---|---|
| GitHub Release | Six required engine/launcher pairs (Linux gnu, macOS, Windows; x64 and arm64), manager archives, per-platform compatibility manifests, Linux `.deb`/`.rpm` for both architectures, `SHA256SUMS`, CycloneDX SBOM and ten PostgreSQL 17/18 pgvector bundles |
| crates.io | Dependency-ordered crate publication; currently gated by workspace publish metadata |
| GHCR | Multi-architecture engine image built from release binaries, with provenance |
| npm | Package-pinned launcher, with provenance; stable `latest`, preview `next` |
| Homebrew / Scoop / winget | Stable engine manifests rendered from actual release checksums |
| PyPI | Disabled scaffold; no update/install support promised |

The two musl targets are experimental and optional. Stable releases alone change manager
manifests and container/npm `latest`; later previews use explicit version tags and npm
`next`. Package registries and winget review cannot become available atomically with a
GitHub release. A partially failed registry publication does not justify modifying
already published bytes or advertising an incomplete native release.

A logical TUF target is exactly
`v<version>/knowell-<version>-<target>-engine[.exe]` or `-launcher[.exe]`.
The physical GitHub release asset is `<sha256>.<basename>`, with no duplicate unprefixed
raw payload. The client maps Tough's consistent-snapshot nested request to
`v<version>/<sha256>.<basename>`; offline mirrors must preserve the same layout.

The engine target's signed `custom.knowell` object declares format 1, exact version,
platform and channel, component `engine`, all compatibility ranges, the paired launcher
{name, sha256, size}, and explicit `revoked`. The launcher's signed target descriptor has
empty `custom`; its exact identity is bound by the engine descriptor. Top-level targets
must have **no delegations** (absent or null, including no empty delegation object).
Unknown fields, duplicate JSON keys, unsupported ranges and missing pairs are rejected.
The per-platform `.update.json` manifests and `unsigned-update-targets.json` contain no
signatures and never authorize a native update.

Generate a new cumulative signing input from verified release assets:

```sh
python dist/update_assets.py assemble --version 1.1.0 --assets release \
  --previous-input /reviewed/previous-unsigned-update-targets.json --out signing-input
```

Omit `--previous-input` for stable 1.0.0. Historical entries and revocations are preserved;
reused target identities are refused. The cumulative input is operator-reviewed input,
not a trusted repository. Retain historical raw assets when verifying/signing it.

## Offline signing and publication

`knowell-update-publisher` uses the approved Tough implementation rather than custom
signature code. It takes only explicit public-root, key-file and target-input paths,
positive metadata sequence and role-specific expirations; output must be a fresh local
directory. It validates public versions, complete platform pairs, explicit contracts,
all raw bytes and stable-first preview policy, signs roles, then loads the emitted
repository through the native verifier before publishing the local staging directory.
It does not upload assets or create real keys.
Ed25519/ECDSA signing keys are explicit local PKCS#8 DER files; encrypted keys and
remote/HSM key sources are not supported by this publisher.
The output directory contains metadata files only. Historical raw assets stay in the
flat cumulative asset directory; the signer uses and removes a private versioned mirror
to verify actual target downloads. Every retained version must keep all six platform
pairs. Role expiration limits are 365 days for targets, 30 for snapshot and seven for
timestamp, in nested order and no later than the trusted root's expiry.

```sh
python scripts/buildlock.py cargo run --locked -p knowell-update \
  --bin knowell-update-publisher -- \
  --root /trusted/root.json --targets signing-input/unsigned-update-targets.json \
  --key /offline/targets-key.der --key /offline/snapshot-key.der \
  --key /offline/timestamp-key.der --sequence 2 \
  --targets-expires 2026-11-01T00:00:00Z \
  --snapshot-expires 2026-10-20T00:00:00Z \
  --timestamp-expires 2026-10-10T00:00:00Z \
  --assets /reviewed/all-release-assets --out /fresh/signed-repository
```

Dates and paths are examples: choose future UTC expirations at the actual signing time.
The online production publisher may hold separate targets/snapshot/timestamp keys;
offline root keys must remain outside CI. Recommended initial policy: root 2-of-3 offline
custodians, each online role with separate key identity, root lifetime one year, targets
30 days, snapshot seven days, timestamp 24 hours, renewed before expiry even without a
new binary. Operational monitoring/renewal and the production host remain owner setup
prerequisites, not something a green development dry run proves.

Root provisioning and rotations may use official **tuftool 0.17.0**, matching Tough 0.24:

```sh
python scripts/buildlock.py cargo install --locked --version 0.17.0 tuftool
# On the owner's offline signing machine, using already provisioned distinct keys:
tuftool root init /offline/root.json
tuftool root expire /offline/root.json 'in 365 days'
tuftool root set-threshold /offline/root.json root 2
tuftool root set-threshold /offline/root.json targets 1
tuftool root set-threshold /offline/root.json snapshot 1
tuftool root set-threshold /offline/root.json timestamp 1
tuftool root add-key /offline/root.json -k /offline/root-a.der --role root
tuftool root add-key /offline/root.json -k /offline/root-b.der --role root
tuftool root add-key /offline/root.json -k /offline/root-c.der --role root
tuftool root add-key /offline/root.json -k /offline/targets-key.der --role targets
tuftool root add-key /offline/root.json -k /offline/snapshot-key.der --role snapshot
tuftool root add-key /offline/root.json -k /offline/timestamp-key.der --role timestamp
tuftool root sign /offline/root.json -k /offline/root-a.der -k /offline/root-b.der
```

Confirm `consistent_snapshot=true` and public key/threshold/version fields before root
signing. Do not reuse the README's demonstration single key across every role.
The native signer preserves strict nested target names and Knowell custom contracts;
plain `tuftool create --add-targets` alone does not preserve this contract.
The command/API basis is [official tuftool documentation](https://github.com/awslabs/tough/blob/tuftool-v0.17.0/tuftool/README.md)
and its [pinned create implementation](https://github.com/awslabs/tough/blob/tuftool-v0.17.0/tuftool/src/create.rs).

For root rotation, increment the root version by one, add/remove public role keys using
the offline ceremony, and obtain both the previous root threshold and new root threshold
signatures. Keep every sequential `<n>.root.json` permanently available so long-offline
clients can traverse rotations. Publish the new root before metadata signed with its new
online role keys. Changing only a root download URL is not a trust-reset mechanism.
After editing a separately saved next root, sign with the new quorum and cross-sign with
the old quorum (never use `--ignore-threshold`):

```sh
tuftool root bump-version /offline/root-next.json
tuftool root sign /offline/root-next.json -k /offline/new-root-a.der -k /offline/new-root-b.der
tuftool root sign /offline/root-next.json --cross-sign /offline/root-previous.json \
  -k /offline/old-root-a.der -k /offline/old-root-b.der
```

For a withdrawn release, preserve its engine descriptor and set signed `revoked=true`,
issue a new metadata sequence, and verify blocked plan/download/rollback behavior.
Do not replace immutable target bytes under an old identity or remove revocation history.

Publication order is mandatory:

1. Build all required platform pairs and archives, validate explicit compatibility and
   actual digests, and review the complete cumulative signing input.
2. Upload all immutable assets to the draft release and attest every checksum subject.
   Publish the release only after required registry jobs succeed. Download the public
   files again, verify every checksum and exact repository/workflow/source-ref provenance.
3. Run the offline/native signer and verify its signed staging repository. Check it with
   a provisioned native client on every required OS before advertising its channel.
4. Publish sequential root metadata and immutable versioned targets/snapshot metadata.
   Ensure all referenced raw assets are publicly accessible with exact lengths and hashes.
5. Commit the new trusted **timestamp last** as the channel's publication point. Use one
   conditional/atomic object replacement and serialized metadata sequencing; readers must
   never receive a timestamp referencing unavailable objects.
   Keep metadata sequence strictly increasing in durable operator state; a fresh output
   directory is not evidence that a chosen sequence exceeds the production channel.
6. Check fresh and long-offline clients, expiry renewal, revocation and interrupted metadata
   recovery. Preserve earlier root documents, signed metadata and immutable artifacts.

`release.yml` publishes GitHub/package-manager assets and retains an unsigned signing
input artifact. It does **not** advertise or deploy a TUF channel automatically. Production
root custody, hosting, online signer credentials, timestamp renewal and the final static
metadata deployment must be provisioned and approved by the owner separately. Enable
GitHub immutable releases before production publication; published assets are not clobbered.

## CI and local validation

The workflow pins Rust 1.96.0, builds engines plus launchers for six required and two
optional targets, packages raw targets, checks complete compatibility descriptors and
checks the engine's stateless identity/schema handshake before copying raw artifacts,
then attests all release checksum subjects. It runs native bootstrap/launcher smoke tests for
public versions on each build runner, including Windows installer fixture tests.
Development 0.x dry runs skip installing ineligible binaries while keeping offline
synthetic installer and updater tests. Assembly checks two `.deb` and two `.rpm` files,
required archives and all six raw pairs; unsigned signing input is a separate CI artifact.
Public release verification downloads and hashes every file and verifies build identity
before downstream npm/Homebrew/Scoop/winget jobs.

A green dry run still does not prove remote TUF deployment, production signing custody,
`.crate` contents while `publish=false`, or an arm64 container run. Attestations and
production provenance verification are absent in publication-free dry runs. Synthetic
fixtures cover tamper, malformed manifests, conflicting ownership, duplicate components,
interrupted/concurrent cache publication, bounded downloads, cache revalidation and
Windows protected ACLs. Native POSIX symlinks are verified on Linux; compatibility shells
that emulate links fail explicitly before publishing a command.

Local commands (one build/test command at a time):

```sh
python -m unittest discover -s dist -v
python scripts/buildlock.py npm --prefix wrappers/npm test
python scripts/buildlock.py cargo test -p knowell-update --locked
python scripts/buildlock.py cargo test -p knowell-launcher --locked
```

All cargo/npm builds and tests, including release jobs, go through `scripts/buildlock.py`.

## Release environment and owner checklist

Environment `release` requires owner approval and `v*` branch/tag restrictions. Secret
names only: `CARGO_REGISTRY_TOKEN`, `NPM_TOKEN`, `RELEASE_APP_PRIVATE_KEY`, `WINGET_PAT`.
`RELEASE_APP_CLIENT_ID` is a variable. GitHub/GHCR jobs use scoped job tokens and
attestations use OIDC; pull-request jobs receive none of these release secrets.

Before stable 1.0: finish roadmap/security/changelog criteria; lift `publish=false` only
when every crate's publication metadata and dependency versions are ready; run the full
dry run; provision release channels, protected root custody and signed TUF hosting; verify
bootstrap trust distribution and recovery on Linux, macOS, Windows x64/arm64. The owner
creates the matching version tag on the default branch and approves protected publishing.
Crates and GHCR cannot be rolled back atomically with other registries; rerun only safe,
idempotent failed publication steps and never rewrite an already public release.
After first crate publication, configure trusted publishing before removing the initial
registry token. The `knowell-action` tag, docs hosting and PyPI remain separate decisions.
