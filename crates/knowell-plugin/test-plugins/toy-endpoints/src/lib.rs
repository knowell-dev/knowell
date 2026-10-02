//! A well-behaved Knowell analyzer plugin for the `toy` route language:
//!
//! ```text
//! route GET /users/{id} -> getUser     # getUser serves the endpoint
//! fetch GET /users/{id} from loadUser  # loadUser calls the endpoint
//! ```
//!
//! Every recognised line yields one endpoint contract and one edge.

wit_bindgen::generate!({
    path: "../../wit",
    world: "plugin",
});

use exports::knowell::plugin::analyzer::{
    self, Analysis, AnalyzeError, Contract, ContractKind, ContractRole, Edge, EdgeKind, Evidence,
    LineRange, Resolution, SourceFile,
};
use exports::knowell::plugin::metadata::{self, PluginInfo};

struct Toy;

impl metadata::Guest for Toy {
    fn info() -> PluginInfo {
        PluginInfo {
            name: "toy-endpoints".to_string(),
            version: "0.1.0".to_string(),
            languages: vec!["toy".to_string()],
            frameworks: vec!["toy-http".to_string()],
            capabilities: Vec::new(),
        }
    }
}

impl analyzer::Guest for Toy {
    fn analyze(file: SourceFile) -> Result<Analysis, AnalyzeError> {
        if file.language != "toy" {
            return Err(AnalyzeError::Unsupported(format!(
                "language `{}` is not toy",
                file.language
            )));
        }
        let mut contracts = Vec::new();
        let mut edges = Vec::new();
        for (index, line) in file.text.lines().enumerate() {
            let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
            let range = LineRange {
                start: number,
                end: number,
            };
            let code = line.split('#').next().unwrap_or("").trim();
            let words: Vec<&str> = code.split_whitespace().collect();
            match words.as_slice() {
                ["route", method, path, "->", handler] => {
                    let key = format!("{} {}", method.to_ascii_uppercase(), path);
                    contracts.push(Contract {
                        kind: ContractKind::Endpoint,
                        key: key.clone(),
                        role: ContractRole::Producer,
                        range,
                    });
                    edges.push(Edge {
                        from_symbol: (*handler).to_string(),
                        to_symbol: key,
                        kind: EdgeKind::Exposes,
                        evidence: Evidence::Syntactic,
                        resolution: Resolution::Resolved,
                        range,
                    });
                }
                ["fetch", method, path, "from", caller] => {
                    let key = format!("{} {}", method.to_ascii_uppercase(), path);
                    contracts.push(Contract {
                        kind: ContractKind::Endpoint,
                        key: key.clone(),
                        role: ContractRole::Consumer,
                        range,
                    });
                    edges.push(Edge {
                        from_symbol: (*caller).to_string(),
                        to_symbol: key,
                        kind: EdgeKind::Consumes,
                        evidence: Evidence::Heuristic,
                        resolution: Resolution::Unresolved,
                        range,
                    });
                }
                _ => {}
            }
        }
        Ok(Analysis { contracts, edges })
    }
}

export!(Toy);
