# knowell-plugin

Sandboxed host for Knowell **analyzer plugins**: third-party code, compiled to
WebAssembly components, that extracts cross-project contracts (endpoints,
topics, tables, ...) and relations from source files when a declarative rule
pack is not enough.

Plugins are **untrusted**. They run in [Wasmtime](https://wasmtime.dev) with
a versioned interface, explicit capabilities, per-call resource limits, and a
WASI context that grants nothing; everything they return is validated before
Knowell uses it.

## WIT contract

The contract is the WIT package `knowell:plugin@0.1.0` in
[`wit/plugin.wit`](wit/plugin.wit) (also exported as `knowell_plugin::WIT`).
A plugin targets the `plugin` world:

```wit
world plugin {
    import log;            // bounded diagnostics
    import project-files;  // optional, capability-gated, read-only
    export metadata;       // who the plugin is, what it needs
    export analyzer;       // one file in, contracts and edges out
}
```

| Interface | Function | Purpose |
|---|---|---|
| `metadata` | `info() -> plugin-info` | Name, version, `languages`, `frameworks`, `capabilities`. Called once at load; name and version must equal the manifest, capabilities must be a subset of it. |
| `analyzer` | `analyze(file: source-file) -> result<analysis, analyze-error>` | `source-file` is `{path, language, text}`; `analysis` is `{contracts, edges}`. |
| `log` | `log(level, message)` | Diagnostics, routed to `tracing` (target `knowell_plugin::guest`). |
| `project-files` | `read-file(path) -> result<string, read-error>` | Read a UTF-8 file by project-relative path. Only with the `project-files` capability. |

Output items:

- **contract** — `kind` (`endpoint`, `topic`, `rpc`, `table`, `env-name`,
  `i18n-key`, `package`), `key` (normalised, e.g. `GET /users/{id}`), `role`
  (`producer` / `consumer`), `range` (1-based inclusive lines).
- **edge** — `from-symbol`, `to-symbol` (symbol or contract keys), `kind`
  (`calls`, `references`, `implements`, `imports`, `defines`, `tests`,
  `produces`, `consumes`, `reads`, `writes`, `exposes`, `depends-on`),
  `evidence` (`syntactic` / `heuristic` — plugins cannot claim semantic
  resolution), `resolution` (`resolved` / `ambiguous` / `unresolved`), `range`.

Host-side names (`ContractKind::as_str`, ...) match `knowell-graph`.

### Versioning

The WIT package version is the plugin API version. A plugin built against API
`A` runs on a host implementing `H` when they share the major version, the
minor version too while the major is 0, and `A <= H` (a plugin may not need
functions the host lacks). The check is applied to the manifest's
`api-version` and to every `knowell:plugin/*` interface the component imports
or exports.

## Manifest

Every plugin ships with a TOML manifest; unknown keys are rejected.

```toml
name = "toy-endpoints"       # lowercase slug, 1-64 chars [a-z0-9_-]
version = "0.1.0"            # the plugin's semver version
api-version = "0.1.0"        # knowell:plugin version it was built against
sha256 = "495cd092...7569e"  # SHA-256 of the .wasm file (64 hex digits)
capabilities = []            # e.g. ["project-files"]
```

`PluginHost::load` refuses the plugin, in this order, if: the API version is
incompatible; a requested capability is not granted by the user; the component
is larger than the limit; its SHA-256 differs from `sha256`; it imports
anything other than WASI p2, `log`, or `project-files` (the latter only if the
manifest requests it); it lacks the exports or their types differ from the
host's; or its `metadata.info` (run sandboxed) disagrees with the manifest.

## Capability model

Deny by default. The manifest *requests*, the user *grants* (`Grants`), and a
plugin receives only capabilities that are both requested and granted.

| Facility | What a plugin gets |
|---|---|
| File system (WASI) | Nothing: no preopened directories. |
| Network (WASI sockets) | Nothing: TCP, UDP and name lookup disabled, every address denied. |
| Environment, arguments, cwd | Empty. |
| stdin / stdout / stderr | Closed / discarded / captured (64 KiB) for fault reports. |
| Clocks | Frozen at the Unix epoch; sleeping and timer subscriptions trap. |
| Randomness | Deterministic fixed sequence (output must not depend on it). |
| `project-files` capability | Read-only UTF-8 files under one project root, see below. |

`ProjectFilesGrant::new(root, filter)` grants reads under `root`. For every
read the host requires a valid relative path (no absolute paths, drive
prefixes, `..`, backslashes, `:` streams, components ending in `.` or space),
resolves symlinks and requires the target to stay inside the root, refuses
`.git` and `.knowell`, asks `filter` about both the requested and the resolved
path, and enforces a per-file size limit and a per-call read count. Pass
Knowell's sensitive-file policy as `filter`, for example:

```rust
let policy = knowell_secrets::ExclusionPolicy::builtin();
let grant = ProjectFilesGrant::new(project_root, move |path| policy.check(path).is_none())?;
```

## Limits

All limits are per call (`metadata` at load, or one `analyze`), and every call
runs in a fresh instance, so a failure never leaks into the next call.

| `HostConfig` field | Default | Enforcement |
|---|---|---|
| `fuel_per_call` | 1 000 000 000 | Wasmtime fuel (~instructions) → `FuelExhausted` |
| `timeout` | 5 s | Epoch interruption, 10 ms ticks → `Timeout` |
| `max_memory_bytes` | 128 MiB | Sum over all linear memories → `MemoryLimit` |
| `max_table_elements` | 100 000 | Sum over all tables → `TableLimit` |
| `max_output_bytes` | 4 MiB | Strings + 32 B per item → `OutputTooLarge` (copying out is capped at 2× this, min 1 MiB) |
| `max_input_bytes` | 4 MiB | File text size → `InputTooLarge` |
| `max_component_bytes` | 32 MiB | `.wasm` size → `ComponentTooLarge` |
| `max_log_messages` / `max_log_message_bytes` | 64 / 1 KiB | Excess dropped and counted; text sanitised |
| `max_project_file_reads` / `max_project_file_bytes` | 32 / 1 MiB | `budget-exhausted` / `too-large` to the plugin |
| `cache_dir` | none | On-disk cache of compiled components |

Output validation: keys 1-512 bytes, not blank, no control characters; ranges
within the file's line count; enum values checked by the component ABI.
Plugin-supplied text in errors and logs is sanitised (line breaks flattened,
control and bidi-override characters replaced) and truncated.

### Compiled-component cache

Compiled components are kept in memory per host (keyed by SHA-256) and, with
`cache_dir` set, on disk through Wasmtime's cache under
`<cache_dir>/<fingerprint>/`. The fingerprint covers every `HostConfig` limit
and the host and API versions; Wasmtime additionally keys entries by its own
version, compiler settings and the component bytes. `PluginHost::cache_stats`
reports hits and misses.

## Using the host

```rust
use knowell_core::RepoPath;
use knowell_plugin::{Grants, HostConfig, Manifest, PluginHost, PluginSource, SourceFile};

let host = PluginHost::new(HostConfig::default())?;
let manifest = Manifest::from_toml(&std::fs::read_to_string("toy-endpoints.toml")?)?;
let plugin = host.load(PluginSource::Path("toy-endpoints.wasm".as_ref()), &manifest, &Grants::none())?;

let path = RepoPath::new("web/routes.toy")?;
let output = plugin.analyze(&SourceFile {
    path: &path,
    language: "toy",
    text: "route GET /users/{id} -> getUser\n",
})?;
```

`Plugin` is `Send + Sync`; calls from several threads run in separate
instances.

## Building a plugin (Rust)

1. Install the target: `rustup target add wasm32-wasip2`.
2. Create a library: `cargo new --lib my-plugin` and set
   ```toml
   [lib]
   crate-type = ["cdylib"]

   [dependencies]
   wit-bindgen = { version = "0.57", default-features = false, features = ["macros", "realloc", "std"] }

   [profile.release]
   opt-level = "s"
   lto = true
   strip = true
   ```
3. Copy `wit/plugin.wit` into `my-plugin/wit/` (or print it from
   `knowell_plugin::WIT`).
4. Implement the two exports:
   ```rust
   wit_bindgen::generate!({ path: "wit", world: "plugin" });

   use exports::knowell::plugin::analyzer::{self, Analysis, AnalyzeError, SourceFile};
   use exports::knowell::plugin::metadata::{self, PluginInfo};

   struct MyPlugin;

   impl metadata::Guest for MyPlugin {
       fn info() -> PluginInfo {
           PluginInfo {
               name: "my-plugin".into(),
               version: "0.1.0".into(),
               languages: vec!["typescript".into()],
               frameworks: vec!["express".into()],
               capabilities: vec![],
           }
       }
   }

   impl analyzer::Guest for MyPlugin {
       fn analyze(file: SourceFile) -> Result<Analysis, AnalyzeError> {
           // Parse `file.text`; return contracts and edges with 1-based line ranges.
           Ok(Analysis { contracts: vec![], edges: vec![] })
       }
   }

   export!(MyPlugin);
   ```
   Only call `knowell::plugin::log::log` or
   `knowell::plugin::project_files::read_file` if you need them; importing
   `project-files` requires the capability in the manifest.
5. Build: `cargo build --release --target wasm32-wasip2`; the component is
   `target/wasm32-wasip2/release/my_plugin.wasm`.
6. Hash it (`sha256sum my_plugin.wasm`, or `Get-FileHash` on Windows) and
   write the manifest with that `sha256`, the same `name`/`version` as
   `info()`, `api-version = "0.1.0"`, and the capabilities you use.
7. The user installs the `.wasm` and manifest and grants any capabilities in
   their configuration. Every rebuild changes the hash, so the manifest must be
   updated with it.

Tips: keep `analyze` pure (it gets a fresh instance per file and frozen
clocks); return `analyze-error.unsupported` for files you do not handle;
panics are reported with the tail of stderr, so `eprintln!` helps while
developing, but stay under 64 KiB of stderr per call; `std::process::exit`
fails the call.

## Security notes

- **Runtime updates.** Wasmtime and WASI require at least 49.0.2; the lockfile
  includes the fixes for RUSTSEC-2026-0321 through RUSTSEC-2026-0327.
  Dependency advisories are checked by `cargo deny` in CI.
- **Trust boundary.** The component is untrusted; the manifest pins its exact
  bytes, so a swapped binary is refused. The manifest itself is trusted as
  much as the user who installs it.
- **Isolation.** Wasmtime memory safety plus per-call fresh instances; no
  WASI ambient authority; host functions are bounded (log budget, read budget,
  random size). Sleeping is trapped because epoch interruption cannot stop a
  host call; all remaining WASI pollables are always ready.
- **Resource exhaustion.** Fuel, epoch deadline, memory/table limits, a
  hostcall-fuel cap on bytes copied out of the plugin, and output validation.
  Compilation itself is bounded only by `max_component_bytes`.
- **Determinism.** Frozen clocks and fixed randomness make output a function
  of the input (and of `project-files` contents), and remove timing side
  channels inside the sandbox.
- **Compiled cache.** Cached native code is loaded without re-validation:
  anyone who can write `cache_dir` can run code in the host. Use a directory
  only the current user can write.
- **project-files races.** Paths are resolved and checked, then opened; a
  local attacker who can swap a checked file for a symlink in between could
  redirect a read. Plugins themselves cannot write files.
- **Output is data.** Validated output can still be wrong or adversarial
  (misleading keys); it carries `syntactic`/`heuristic` evidence only and must
  be treated as untrusted data downstream.

## Test plugins

`test-plugins/` is a standalone `wasm32-wasip2` workspace (not part of the
Knowell workspace) with `toy-endpoints` (well-behaved) and `hostile` (one
attack per input file name: infinite loop, memory hog, stack overflow, sleep,
panic, exit, stdout/stderr/log spam, WASI probing, path escapes, oversized and
malformed output). The built components are committed in
`test-plugins/prebuilt/` (61 KB and 175 KB). Rebuild them with:

```sh
python crates/knowell-plugin/test-plugins/build.py
```

The script builds through `scripts/buildlock.py` into the shared `target/`
directory and prints each component's size and SHA-256. Tests compute hashes at
run time, so rebuilding needs no test changes.
