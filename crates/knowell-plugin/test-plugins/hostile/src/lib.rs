//! A deliberately misbehaving Knowell plugin used by the host's isolation
//! tests. The analysed file's name selects the behaviour (`loop.toy` spins
//! forever, `memory.toy` allocates without bound, ...); see `analyze`.
//!
//! Nothing here is an example of how to write a plugin.

wit_bindgen::generate!({
    path: "../../wit",
    world: "plugin",
});

use std::hint::black_box;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use exports::knowell::plugin::analyzer::{
    self, Analysis, AnalyzeError, Contract, ContractKind, ContractRole, Edge, EdgeKind, Evidence,
    LineRange, Resolution, SourceFile,
};
use exports::knowell::plugin::metadata::{self, Capability, PluginInfo};
use knowell::plugin::log::{self, Level};
use knowell::plugin::project_files::{self, ReadError};

struct Hostile;

impl metadata::Guest for Hostile {
    fn info() -> PluginInfo {
        PluginInfo {
            name: "hostile".to_string(),
            version: "0.1.0".to_string(),
            languages: vec!["toy".to_string()],
            frameworks: Vec::new(),
            capabilities: vec![Capability::ProjectFiles],
        }
    }
}

const LINE_1: LineRange = LineRange { start: 1, end: 1 };

fn contract(key: impl Into<String>, range: LineRange) -> Contract {
    Contract {
        kind: ContractKind::Endpoint,
        key: key.into(),
        role: ContractRole::Producer,
        range,
    }
}

fn only(contracts: Vec<Contract>) -> Result<Analysis, AnalyzeError> {
    Ok(Analysis {
        contracts,
        edges: Vec::new(),
    })
}

fn read_error_name(error: ReadError) -> &'static str {
    match error {
        ReadError::Denied => "denied",
        ReadError::NotFound => "not-found",
        ReadError::TooLarge => "too-large",
        ReadError::NotText => "not-text",
        ReadError::BudgetExhausted => "budget-exhausted",
        ReadError::Unavailable => "unavailable",
    }
}

fn outcome<T, E: std::fmt::Debug>(result: Result<T, E>) -> String {
    match result {
        Ok(_) => "allowed".to_string(),
        Err(_) => "denied".to_string(),
    }
}

#[expect(unconditional_recursion, reason = "the stack overflow is the test")]
fn recurse(depth: u64) -> u64 {
    // `black_box` keeps the compiler from turning this into a loop.
    black_box(depth) + recurse(black_box(depth + 1))
}

impl analyzer::Guest for Hostile {
    fn analyze(file: SourceFile) -> Result<Analysis, AnalyzeError> {
        let mode = file.path.rsplit('/').next().unwrap_or("").to_string();
        match mode.as_str() {
            "ok.toy" => only(vec![contract("GET /healthy", LINE_1)]),
            "loop.toy" => {
                let mut counter = 0u64;
                loop {
                    counter = black_box(counter.wrapping_add(1));
                }
            }
            "memory.toy" => {
                let mut hoard: Vec<Vec<u8>> = Vec::new();
                loop {
                    let mut chunk = vec![0u8; 1 << 20];
                    if let Some(byte) = chunk.last_mut() {
                        *byte = 1;
                    }
                    hoard.push(black_box(chunk));
                }
            }
            "big-alloc.toy" => {
                let big: Vec<u8> = Vec::with_capacity(black_box(1usize << 30));
                only(vec![contract(format!("GET /{}", big.capacity()), LINE_1)])
            }
            "recurse.toy" => {
                let total = recurse(0);
                only(vec![contract(format!("GET /{total}"), LINE_1)])
            }
            "sleep.toy" => {
                std::thread::sleep(Duration::from_secs(30));
                only(vec![contract("GET /slept", LINE_1)])
            }
            "panic.toy" => panic!("hostile plugin panicked on purpose"),
            "exit.toy" => std::process::exit(3),
            "stdout-spam.toy" => {
                let line = "x".repeat(1023);
                for _ in 0..1024 {
                    println!("{line}");
                }
                only(vec![contract("GET /printed", LINE_1)])
            }
            "stderr-spam.toy" => {
                let line = "y".repeat(1023);
                for _ in 0..1024 {
                    eprintln!("{line}");
                }
                only(vec![contract("GET /printed", LINE_1)])
            }
            "log-spam.toy" => {
                let message = "z".repeat(64 * 1024);
                for _ in 0..10_000 {
                    log::log(Level::Error, &message);
                }
                log::log(Level::Info, "control\u{1b}[31m characters\nare escaped");
                only(vec![contract("GET /logged", LINE_1)])
            }
            "probe.toy" => {
                let clock = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs().to_string())
                    .unwrap_or_else(|_| "before-epoch".to_string());
                let observations = [
                    format!("env-vars={}", std::env::vars_os().count()),
                    format!("args={}", std::env::args_os().count()),
                    format!("fs-read-root={}", outcome(std::fs::read_dir("/"))),
                    format!("fs-read-file={}", outcome(std::fs::read("/etc/hosts"))),
                    format!("fs-write={}", outcome(std::fs::write("escape.txt", b"x"))),
                    format!(
                        "net-connect={}",
                        outcome(std::net::TcpStream::connect("127.0.0.1:80"))
                    ),
                    format!(
                        "net-listen={}",
                        outcome(std::net::TcpListener::bind("127.0.0.1:0"))
                    ),
                    format!("wall-clock={clock}"),
                ];
                only(
                    observations
                        .into_iter()
                        .map(|key| contract(key, LINE_1))
                        .collect(),
                )
            }
            "read.toy" => {
                let mut found = Vec::new();
                for (index, path) in file.text.lines().enumerate() {
                    let line = u32::try_from(index + 1).unwrap_or(u32::MAX);
                    let result = match project_files::read_file(path) {
                        Ok(text) => format!("ok:{}", text.len()),
                        Err(error) => read_error_name(error).to_string(),
                    };
                    found.push(contract(
                        format!("{path} => {result}"),
                        LineRange {
                            start: line,
                            end: line,
                        },
                    ));
                }
                only(found)
            }
            "huge-count.toy" => only((0..200_000).map(|_| contract("k", LINE_1)).collect()),
            "huge-key.toy" => only(vec![contract("k".repeat(1 << 20), LINE_1)]),
            "long-key.toy" => only(vec![contract("k".repeat(513), LINE_1)]),
            "bad-range.toy" => only(vec![contract(
                "GET /x",
                LineRange {
                    start: 1,
                    end: 1000,
                },
            )]),
            "zero-range.toy" => only(vec![contract("GET /x", LineRange { start: 0, end: 1 })]),
            "reversed-range.toy" => only(vec![contract("GET /x", LineRange { start: 2, end: 1 })]),
            "empty-key.toy" => only(vec![contract("", LINE_1)]),
            "control-key.toy" => only(vec![contract("GET /a\nb", LINE_1)]),
            "bad-edge.toy" => Ok(Analysis {
                contracts: Vec::new(),
                edges: vec![Edge {
                    from_symbol: String::new(),
                    to_symbol: "target".to_string(),
                    kind: EdgeKind::Calls,
                    evidence: Evidence::Syntactic,
                    resolution: Resolution::Resolved,
                    range: LINE_1,
                }],
            }),
            "fail.toy" => Err(AnalyzeError::Failed("cannot parse toy file".to_string())),
            "fail-long.toy" => Err(AnalyzeError::Failed(format!(
                "first line\u{7}\n{}",
                "e".repeat(1 << 20)
            ))),
            _ => Err(AnalyzeError::Unsupported(format!("unknown mode `{mode}`"))),
        }
    }
}

export!(Hostile);
