//! Isolation tests against real WebAssembly components.
//!
//! The components in `test-plugins/prebuilt/` are built from the sources next
//! to them (`python crates/knowell-plugin/test-plugins/build.py`):
//! `toy-endpoints` is a well-behaved plugin, `hostile` attacks the sandbox in
//! a different way for each input file name.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use knowell_core::{LineRange, RepoPath};
use knowell_plugin::{
    AnalyzerOutput, Capability, Contract, ContractKind, ContractRole, DeclineKind, Edge, EdgeKind,
    Evidence, Grants, HostConfig, Manifest, Plugin, PluginError, PluginHost, PluginSource,
    ProjectFilesGrant, Resolution, Sha256Digest, SourceFile,
};

const TOY: &[u8] = include_bytes!("../test-plugins/prebuilt/toy-endpoints.wasm");
const HOSTILE: &[u8] = include_bytes!("../test-plugins/prebuilt/hostile.wasm");

fn manifest_toml(name: &str, version: &str, api: &str, sha256: &str, caps: &[&str]) -> String {
    let caps: Vec<String> = caps.iter().map(|c| format!("\"{c}\"")).collect();
    format!(
        "name = \"{name}\"\nversion = \"{version}\"\napi-version = \"{api}\"\n\
         sha256 = \"{sha256}\"\ncapabilities = [{}]\n",
        caps.join(", ")
    )
}

fn manifest_for(name: &str, bytes: &[u8], caps: &[&str]) -> Manifest {
    let digest = Sha256Digest::of(bytes).to_string();
    Manifest::from_toml(&manifest_toml(name, "0.1.0", "0.1.0", &digest, caps)).unwrap()
}

fn host(config: HostConfig) -> PluginHost {
    PluginHost::new(config).unwrap()
}

fn toy(host: &PluginHost) -> Plugin {
    host.load(
        PluginSource::Bytes(TOY),
        &manifest_for("toy-endpoints", TOY, &[]),
        &Grants::none(),
    )
    .unwrap()
}

/// A project root with a few files, plus files *outside* it.
struct Project {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
}

fn project() -> Project {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("src/app.toy"), "route GET /a -> a\n").unwrap();
    fs::write(root.join(".git/config"), "[core]\n").unwrap();
    fs::write(
        root.join(".env"),
        "KNOWELL_CANARY_TOKEN=not-a-real-secret\n",
    )
    .unwrap();
    fs::write(root.join("large.txt"), "x".repeat(4096)).unwrap();
    fs::write(dir.path().join("outside.txt"), "outside the project").unwrap();
    Project { _dir: dir, root }
}

fn hostile_with(host: &PluginHost, project: &Project) -> Plugin {
    let grant = ProjectFilesGrant::new(&project.root, |path: &RepoPath| {
        !path.file_name().starts_with(".env")
    })
    .unwrap();
    host.load(
        PluginSource::Bytes(HOSTILE),
        &manifest_for("hostile", HOSTILE, &["project-files"]),
        &Grants::none().with_project_files(grant),
    )
    .unwrap()
}

fn hostile(host: &PluginHost) -> (Plugin, Project) {
    let project = project();
    (hostile_with(host, &project), project)
}

fn run(plugin: &Plugin, mode: &str, text: &str) -> Result<AnalyzerOutput, PluginError> {
    let path = RepoPath::new(format!("modes/{mode}.toy")).unwrap();
    plugin.analyze(&SourceFile {
        path: &path,
        language: "toy",
        text,
    })
}

fn keys(output: &AnalyzerOutput) -> Vec<&str> {
    output.contracts.iter().map(|c| c.key.as_str()).collect()
}

/// After any failure the same plugin and host must keep working.
fn assert_healthy(plugin: &Plugin) {
    let output = run(plugin, "ok", "line\n").unwrap();
    assert_eq!(keys(&output), ["GET /healthy"]);
}

fn line(n: u32) -> LineRange {
    LineRange::new(n, n).unwrap()
}

// ---- well-behaved plugin ---------------------------------------------------

#[test]
fn well_behaved_plugin_extracts_endpoints() {
    let host = host(HostConfig::default());
    let plugin = toy(&host);
    let path = RepoPath::new("web/routes.toy").unwrap();
    let text = "# users service\n\
                route GET /users/{id} -> getUser\n\
                \n\
                fetch post /orders from submitOrder  # client call\n\
                nonsense line\n";
    let file = SourceFile {
        path: &path,
        language: "toy",
        text,
    };
    let output = plugin.analyze(&file).unwrap();
    assert_eq!(
        output.contracts,
        [
            Contract {
                kind: ContractKind::Endpoint,
                key: "GET /users/{id}".to_string(),
                role: ContractRole::Producer,
                range: line(2),
            },
            Contract {
                kind: ContractKind::Endpoint,
                key: "POST /orders".to_string(),
                role: ContractRole::Consumer,
                range: line(4),
            },
        ]
    );
    assert_eq!(
        output.edges,
        [
            Edge {
                from: "getUser".to_string(),
                to: "GET /users/{id}".to_string(),
                kind: EdgeKind::Exposes,
                evidence: Evidence::Syntactic,
                resolution: Resolution::Resolved,
                range: line(2),
            },
            Edge {
                from: "submitOrder".to_string(),
                to: "POST /orders".to_string(),
                kind: EdgeKind::Consumes,
                evidence: Evidence::Heuristic,
                resolution: Resolution::Unresolved,
                range: line(4),
            },
        ]
    );
    // Fresh instance per call, frozen clocks, fixed randomness: deterministic.
    assert_eq!(plugin.analyze(&file).unwrap(), output);
    let empty = plugin
        .analyze(&SourceFile {
            path: &path,
            language: "toy",
            text: "",
        })
        .unwrap();
    assert_eq!(empty, AnalyzerOutput::default());
}

#[test]
fn metadata_is_validated_and_exposed() {
    let host = host(HostConfig::default());
    let plugin = toy(&host);
    let info = plugin.info();
    assert_eq!(info.name().as_str(), "toy-endpoints");
    assert_eq!(info.version(), "0.1.0");
    assert_eq!(info.languages().iter().collect::<Vec<_>>(), ["toy"]);
    assert_eq!(info.frameworks().iter().collect::<Vec<_>>(), ["toy-http"]);
    assert!(info.capabilities().is_empty());
    assert!(plugin.supports_language("toy"));
    assert!(!plugin.supports_language("rust"));
    assert_eq!(plugin.manifest().name().as_str(), "toy-endpoints");
}

#[test]
fn host_refuses_languages_the_plugin_did_not_declare() {
    let host = host(HostConfig::default());
    let plugin = toy(&host);
    let path = RepoPath::new("a.rs").unwrap();
    let err = plugin
        .analyze(&SourceFile {
            path: &path,
            language: "rust",
            text: "fn main() {}",
        })
        .unwrap_err();
    assert!(
        matches!(err, PluginError::UnsupportedLanguage { .. }),
        "{err}"
    );
}

#[test]
fn input_size_is_limited() {
    let host = host(HostConfig {
        max_input_bytes: 16,
        ..HostConfig::default()
    });
    let plugin = toy(&host);
    let path = RepoPath::new("a.toy").unwrap();
    let err = plugin
        .analyze(&SourceFile {
            path: &path,
            language: "toy",
            text: &"x".repeat(17),
        })
        .unwrap_err();
    assert!(
        matches!(err, PluginError::InputTooLarge { limit: 16 }),
        "{err}"
    );
}

#[test]
fn plugins_are_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Plugin>();
    assert_send_sync::<PluginHost>();

    let host = host(HostConfig::default());
    let plugin = Arc::new(toy(&host));
    let workers: Vec<_> = (0..4)
        .map(|i| {
            let plugin = Arc::clone(&plugin);
            thread::spawn(move || {
                let path = RepoPath::new(format!("w{i}.toy")).unwrap();
                let text = format!("route GET /w/{i} -> h{i}\n");
                plugin
                    .analyze(&SourceFile {
                        path: &path,
                        language: "toy",
                        text: &text,
                    })
                    .unwrap()
                    .contracts
            })
        })
        .collect();
    for (i, worker) in workers.into_iter().enumerate() {
        let contracts = worker.join().unwrap();
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].key, format!("GET /w/{i}"));
    }
}

// ---- load-time refusals ----------------------------------------------------

#[test]
fn wrong_manifest_hash_is_refused() {
    let host = host(HostConfig::default());
    let pinned_to_other = manifest_for("toy-endpoints", HOSTILE, &[]);
    let err = host
        .load(PluginSource::Bytes(TOY), &pinned_to_other, &Grants::none())
        .unwrap_err();
    match err {
        PluginError::HashMismatch {
            expected, actual, ..
        } => {
            assert_eq!(expected, Sha256Digest::of(HOSTILE).to_string());
            assert_eq!(actual, Sha256Digest::of(TOY).to_string());
        }
        other => panic!("unexpected: {other}"),
    }

    // One flipped byte is enough.
    let mut tampered = TOY.to_vec();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    let err = host
        .load(
            PluginSource::Bytes(&tampered),
            &manifest_for("toy-endpoints", TOY, &[]),
            &Grants::none(),
        )
        .unwrap_err();
    assert!(matches!(err, PluginError::HashMismatch { .. }), "{err}");
}

#[test]
fn incompatible_api_versions_are_refused() {
    let host = host(HostConfig::default());
    let digest = Sha256Digest::of(TOY).to_string();
    for api in ["0.2.0", "0.0.1", "1.0.0", "0.1.1"] {
        let manifest =
            Manifest::from_toml(&manifest_toml("toy-endpoints", "0.1.0", api, &digest, &[]))
                .unwrap();
        let err = host
            .load(PluginSource::Bytes(TOY), &manifest, &Grants::none())
            .unwrap_err();
        assert!(
            matches!(&err, PluginError::IncompatibleApi { requested, .. } if requested == api),
            "{api}: {err}"
        );
    }
}

#[test]
fn requested_capabilities_must_be_granted() {
    let host = host(HostConfig::default());
    let err = host
        .load(
            PluginSource::Bytes(HOSTILE),
            &manifest_for("hostile", HOSTILE, &["project-files"]),
            &Grants::none(),
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            PluginError::CapabilityNotGranted {
                capability: Capability::ProjectFiles,
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn capability_imports_must_be_declared_in_the_manifest() {
    let host = host(HostConfig::default());
    let project = project();
    let grant = ProjectFilesGrant::new(&project.root, |_: &RepoPath| true).unwrap();
    // Granted by the user, but the manifest hides that the plugin uses it.
    let err = host
        .load(
            PluginSource::Bytes(HOSTILE),
            &manifest_for("hostile", HOSTILE, &[]),
            &Grants::none().with_project_files(grant),
        )
        .unwrap_err();
    assert!(
        matches!(err, PluginError::UndeclaredCapability { .. }),
        "{err}"
    );
}

#[test]
fn metadata_must_match_the_manifest() {
    let host = host(HostConfig::default());
    let err = host
        .load(
            PluginSource::Bytes(TOY),
            &manifest_for("impostor", TOY, &[]),
            &Grants::none(),
        )
        .unwrap_err();
    assert!(matches!(err, PluginError::MetadataMismatch { .. }), "{err}");

    let digest = Sha256Digest::of(TOY).to_string();
    let wrong_version = Manifest::from_toml(&manifest_toml(
        "toy-endpoints",
        "9.9.9",
        "0.1.0",
        &digest,
        &[],
    ))
    .unwrap();
    let err = host
        .load(PluginSource::Bytes(TOY), &wrong_version, &Grants::none())
        .unwrap_err();
    assert!(matches!(err, PluginError::MetadataMismatch { .. }), "{err}");
}

#[test]
fn non_components_fail_to_compile() {
    let host = host(HostConfig::default());
    let core_module: &[u8] = b"\0asm\x01\0\0\0";
    for bytes in [b"definitely not wasm".as_slice(), core_module, &TOY[..100]] {
        let err = host
            .load(
                PluginSource::Bytes(bytes),
                &manifest_for("toy-endpoints", bytes, &[]),
                &Grants::none(),
            )
            .unwrap_err();
        assert!(matches!(err, PluginError::Compile { .. }), "{err}");
    }
}

#[test]
fn component_size_is_limited() {
    let host = host(HostConfig {
        max_component_bytes: 1024,
        ..HostConfig::default()
    });
    let manifest = manifest_for("toy-endpoints", TOY, &[]);
    let err = host
        .load(PluginSource::Bytes(TOY), &manifest, &Grants::none())
        .unwrap_err();
    assert!(
        matches!(err, PluginError::ComponentTooLarge { limit: 1024, .. }),
        "{err}"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("toy.wasm");
    fs::write(&path, TOY).unwrap();
    let err = host
        .load(PluginSource::Path(&path), &manifest, &Grants::none())
        .unwrap_err();
    assert!(
        matches!(err, PluginError::ComponentTooLarge { .. }),
        "{err}"
    );
}

#[test]
fn loads_from_a_file() {
    let host = host(HostConfig::default());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("toy.wasm");
    fs::write(&path, TOY).unwrap();
    let manifest = manifest_for("toy-endpoints", TOY, &[]);
    let plugin = host
        .load(
            PluginSource::from(path.as_path()),
            &manifest,
            &Grants::none(),
        )
        .unwrap();
    assert_eq!(plugin.info().name().as_str(), "toy-endpoints");

    let err = host
        .load(
            PluginSource::Path(&dir.path().join("missing.wasm")),
            &manifest,
            &Grants::none(),
        )
        .unwrap_err();
    assert!(matches!(err, PluginError::Read { .. }), "{err}");
}

// ---- resource limits -------------------------------------------------------

#[test]
fn infinite_loop_runs_out_of_fuel() {
    let host = host(HostConfig {
        fuel_per_call: 50_000_000,
        timeout: Duration::from_secs(60),
        ..HostConfig::default()
    });
    let (plugin, _project) = hostile(&host);
    let err = run(&plugin, "loop", "x\n").unwrap_err();
    assert!(
        matches!(
            err,
            PluginError::FuelExhausted {
                fuel: 50_000_000,
                ..
            }
        ),
        "{err}"
    );
    assert_healthy(&plugin);
}

#[test]
fn infinite_loop_hits_the_wall_clock_limit() {
    let host = host(HostConfig {
        fuel_per_call: u64::MAX / 4,
        timeout: Duration::from_millis(200),
        ..HostConfig::default()
    });
    let (plugin, _project) = hostile(&host);
    let started = Instant::now();
    let err = run(&plugin, "loop", "x\n").unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        matches!(
            err,
            PluginError::Timeout {
                timeout_ms: 200,
                ..
            }
        ),
        "{err}"
    );
    assert!(elapsed >= Duration::from_millis(150), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    assert_healthy(&plugin);
}

#[test]
fn memory_hog_hits_the_memory_limit() {
    let host = host(HostConfig {
        max_memory_bytes: 32 * 1024 * 1024,
        ..HostConfig::default()
    });
    let (plugin, _project) = hostile(&host);
    for mode in ["memory", "big-alloc"] {
        let err = run(&plugin, mode, "x\n").unwrap_err();
        assert!(
            matches!(err, PluginError::MemoryLimit { limit, .. } if limit == 32 * 1024 * 1024),
            "{mode}: {err}"
        );
    }
    assert_healthy(&plugin);
}

#[test]
fn memory_limit_applies_at_instantiation() {
    // Smaller than the plugin's initial memory: refused while loading.
    let host = host(HostConfig {
        max_memory_bytes: 64 * 1024,
        ..HostConfig::default()
    });
    let err = host
        .load(
            PluginSource::Bytes(TOY),
            &manifest_for("toy-endpoints", TOY, &[]),
            &Grants::none(),
        )
        .unwrap_err();
    assert!(matches!(err, PluginError::MemoryLimit { .. }), "{err}");
}

#[test]
fn stack_overflow_is_contained() {
    let host = host(HostConfig::default());
    let (plugin, _project) = hostile(&host);
    let err = run(&plugin, "recurse", "x\n").unwrap_err();
    assert!(
        matches!(
            err,
            PluginError::StackOverflow { .. } | PluginError::Fault { .. }
        ),
        "{err}"
    );
    assert_healthy(&plugin);
}

#[test]
fn sleeping_cannot_block_the_host() {
    let host = host(HostConfig::default());
    let (plugin, _project) = hostile(&host);
    let started = Instant::now();
    let err = run(&plugin, "sleep", "x\n").unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    match err {
        PluginError::Fault { reason, .. } => assert!(reason.contains("sleep"), "{reason}"),
        other => panic!("unexpected: {other}"),
    }
    assert_healthy(&plugin);
}

#[test]
fn panics_and_exits_are_contained() {
    let host = host(HostConfig::default());
    let (plugin, _project) = hostile(&host);
    match run(&plugin, "panic", "x\n").unwrap_err() {
        PluginError::Fault { stderr, .. } => {
            let stderr = stderr.unwrap_or_default();
            assert!(stderr.contains("panicked on purpose"), "{stderr}");
        }
        other => panic!("unexpected: {other}"),
    }
    // The plugin calls `exit(3)`; WASI p2's `exit` only carries success or
    // failure, so any non-zero status arrives as 1.
    let err = run(&plugin, "exit", "x\n").unwrap_err();
    assert!(
        matches!(err, PluginError::Exited { code: 1, .. }),
        "{err:?}"
    );
    assert_healthy(&plugin);
}

#[test]
fn output_streams_and_logs_are_bounded() {
    let host = host(HostConfig::default());
    let (plugin, _project) = hostile(&host);
    // 1 MiB of stdout is discarded; 10 000 x 64 KiB log messages are capped.
    assert_eq!(
        keys(&run(&plugin, "stdout-spam", "x\n").unwrap()),
        ["GET /printed"]
    );
    assert_eq!(
        keys(&run(&plugin, "log-spam", "x\n").unwrap()),
        ["GET /logged"]
    );
    // stderr is captured up to 64 KiB; writing past it fails the plugin.
    let err = run(&plugin, "stderr-spam", "x\n").unwrap_err();
    assert!(matches!(err, PluginError::Fault { .. }), "{err}");
    assert_healthy(&plugin);
}

// ---- deny-by-default WASI --------------------------------------------------

#[test]
fn wasi_grants_no_ambient_authority() {
    let host = host(HostConfig::default());
    let (plugin, _project) = hostile(&host);
    let output = run(&plugin, "probe", "x\n").unwrap();
    assert_eq!(
        keys(&output),
        [
            "env-vars=0",
            "args=0",
            "fs-read-root=denied",
            "fs-read-file=denied",
            "fs-write=denied",
            "net-connect=denied",
            "net-listen=denied",
            "wall-clock=0",
        ]
    );
}

#[test]
fn project_files_cannot_escape_the_root() {
    let host = host(HostConfig {
        max_project_file_bytes: 1024,
        ..HostConfig::default()
    });
    let (plugin, _project) = hostile(&host);
    let paths = [
        "src/app.toy",
        "../outside.txt",
        "src/../../outside.txt",
        "/etc/passwd",
        "C:/Windows/win.ini",
        "src\\app.toy",
        ".env",
        ".git/config",
        "src/app.toy:stream",
        "missing.txt",
        "src",
        "large.txt",
    ];
    let output = run(&plugin, "read", &paths.join("\n")).unwrap();
    assert_eq!(
        keys(&output),
        [
            "src/app.toy => ok:18",
            "../outside.txt => denied",
            "src/../../outside.txt => denied",
            "/etc/passwd => denied",
            "C:/Windows/win.ini => denied",
            "src\\app.toy => denied",
            ".env => denied",
            ".git/config => denied",
            "src/app.toy:stream => denied",
            "missing.txt => not-found",
            "src => not-found",
            "large.txt => too-large",
        ]
    );
}

#[test]
fn project_file_reads_are_budgeted_per_call() {
    let host = host(HostConfig {
        max_project_file_reads: 2,
        ..HostConfig::default()
    });
    let (plugin, _project) = hostile(&host);
    let text = "src/app.toy\nsrc/app.toy\nsrc/app.toy\n";
    let output = run(&plugin, "read", text).unwrap();
    assert_eq!(
        keys(&output),
        [
            "src/app.toy => ok:18",
            "src/app.toy => ok:18",
            "src/app.toy => budget-exhausted",
        ]
    );
    // The budget is per call, not per plugin.
    let output = run(&plugin, "read", "src/app.toy\n").unwrap();
    assert_eq!(keys(&output), ["src/app.toy => ok:18"]);
}

// ---- untrusted output ------------------------------------------------------

#[test]
fn oversized_output_is_rejected() {
    let host_default = host(HostConfig::default());
    let (plugin, project) = hostile(&host_default);
    let err = run(&plugin, "huge-count", "x\n").unwrap_err();
    assert!(matches!(err, PluginError::OutputTooLarge { .. }), "{err}");
    // A 1 MiB key fits the default budget but not the key limit...
    let err = run(&plugin, "huge-key", "x\n").unwrap_err();
    assert!(matches!(err, PluginError::InvalidOutput { .. }), "{err}");
    assert_healthy(&plugin);

    // ...and a small output budget rejects it before it is copied.
    let small = host(HostConfig {
        max_output_bytes: 64 * 1024,
        ..HostConfig::default()
    });
    let plugin = hostile_with(&small, &project);
    let err = run(&plugin, "huge-key", "x\n").unwrap_err();
    assert!(
        matches!(err, PluginError::OutputTooLarge { limit: 65536, .. }),
        "{err}"
    );
    // 200 000 contracts need ~8 MB of host memory, far past the 1 MiB copy
    // budget this limit implies: Wasmtime refuses the copy itself, and the
    // refusal is still reported as oversized output.
    let err = run(&plugin, "huge-count", "x\n").unwrap_err();
    assert!(
        matches!(err, PluginError::OutputTooLarge { limit: 65536, .. }),
        "{err:?}"
    );
    assert_healthy(&plugin);
}

#[test]
fn malformed_output_is_rejected() {
    let host = host(HostConfig::default());
    let (plugin, _project) = hostile(&host);
    for mode in [
        "long-key",
        "bad-range",
        "zero-range",
        "reversed-range",
        "empty-key",
        "control-key",
        "bad-edge",
    ] {
        let err = run(&plugin, mode, "only one line\n").unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidOutput { .. }),
            "{mode}: {err}"
        );
    }
    assert_healthy(&plugin);
}

#[test]
fn declines_are_reported_sanitised() {
    let host = host(HostConfig::default());
    let (plugin, _project) = hostile(&host);
    match run(&plugin, "fail", "x\n").unwrap_err() {
        PluginError::Declined { kind, message, .. } => {
            assert_eq!(kind, DeclineKind::Failed);
            assert_eq!(message, "cannot parse toy file");
        }
        other => panic!("unexpected: {other}"),
    }
    match run(&plugin, "fail-long", "x\n").unwrap_err() {
        PluginError::Declined { message, .. } => {
            assert!(message.len() <= 256 + '…'.len_utf8(), "{}", message.len());
            assert!(message.starts_with("first line\u{FFFD} e"), "{message}");
            assert!(!message.chars().any(char::is_control));
        }
        other => panic!("unexpected: {other}"),
    }
    match run(&plugin, "no-such-mode", "x\n").unwrap_err() {
        PluginError::Declined { kind, .. } => assert_eq!(kind, DeclineKind::Unsupported),
        other => panic!("unexpected: {other}"),
    }
}

// ---- compiled-component cache ----------------------------------------------

#[test]
fn compiled_components_are_cached_on_disk_per_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = HostConfig {
        cache_dir: Some(dir.path().to_path_buf()),
        ..HostConfig::default()
    };

    let first = host(config.clone());
    toy(&first);
    toy(&first); // second load is served from memory
    let stats = first.cache_stats();
    assert_eq!(stats.memory_entries, 1);
    assert_eq!((stats.disk_hits, stats.disk_misses), (0, 1));
    let first_dir = first.cache_dir().unwrap().to_path_buf();
    assert!(first_dir.starts_with(dir.path()));
    assert!(dir_has_files(&first_dir));

    // A new host with the same configuration does not recompile.
    let second = host(config.clone());
    toy(&second);
    assert_eq!(second.cache_dir(), Some(first_dir.as_path()));
    let stats = second.cache_stats();
    assert_eq!((stats.disk_hits, stats.disk_misses), (1, 0));

    // Any configuration change gets a fresh cache.
    let changed = host(HostConfig {
        fuel_per_call: config.fuel_per_call + 1,
        ..config
    });
    toy(&changed);
    assert_ne!(changed.cache_dir(), Some(first_dir.as_path()));
    let stats = changed.cache_stats();
    assert_eq!((stats.disk_hits, stats.disk_misses), (0, 1));

    // Without a cache directory nothing touches the disk.
    let uncached = host(HostConfig::default());
    toy(&uncached);
    assert_eq!(uncached.cache_dir(), None);
    let stats = uncached.cache_stats();
    assert_eq!((stats.disk_hits, stats.disk_misses), (0, 0));
}

fn dir_has_files(dir: &Path) -> bool {
    fs::read_dir(dir).unwrap().any(|entry| {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            dir_has_files(&path)
        } else {
            true
        }
    })
}
