//! Known SCIP indexers: project detection, command lines and PATH lookup.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

/// Languages with a known SCIP indexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    /// Rust (`rust-analyzer scip`).
    Rust,
    /// TypeScript and JavaScript (`scip-typescript`).
    TypeScript,
    /// Python (`scip-python`).
    Python,
    /// Go (`scip-go`).
    Go,
    /// Java, Kotlin and Scala through Maven or Gradle (`scip-java`).
    Java,
    /// C# / .NET (`scip-dotnet`).
    CSharp,
}

impl Language {
    /// Stable lowercase identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
            Self::CSharp => "csharp",
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A file in the project directory whose presence signals a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// A file with exactly this name (case-sensitive).
    File(&'static str),
    /// Any file with this extension, without the dot (case-insensitive).
    Extension(&'static str),
}

/// Builds the indexer arguments from the project directory and the absolute output path.
pub type ArgsFn = fn(project_dir: &Path, out_path: &Path) -> Vec<OsString>;

/// How to run one indexer.
#[derive(Debug, Clone, Copy)]
pub struct IndexerSpec {
    /// Stable identifier, equal to the tool name for the built-in specs.
    pub id: &'static str,
    /// The language the indexer covers.
    pub language: Language,
    /// Executable name looked up on `PATH`.
    pub tool: &'static str,
    /// Files in the project directory that signal this language; any one matches.
    pub markers: &'static [Marker],
    /// Shown to the user when the tool is missing.
    pub install_hint: &'static str,
    /// Builds the argument list; the process runs with the project directory as cwd.
    pub args: ArgsFn,
}

impl IndexerSpec {
    /// Whether `project_dir` contains one of the spec's markers (top level only).
    pub fn detects(&self, project_dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(project_dir) else {
            return false;
        };
        // Bound the directory walk: a hostile directory must not stall detection.
        for entry in entries.flatten().take(10_000) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let is_file = entry.file_type().map(|t| !t.is_dir()).unwrap_or(false);
            if !is_file {
                continue;
            }
            for marker in self.markers {
                let hit = match marker {
                    Marker::File(f) => name == *f,
                    Marker::Extension(ext) => name
                        .rsplit_once('.')
                        .is_some_and(|(stem, e)| !stem.is_empty() && e.eq_ignore_ascii_case(ext)),
                };
                if hit {
                    return true;
                }
            }
        }
        false
    }

    /// Resolves the tool on the current process `PATH`; `None` if it is not installed.
    pub fn locate(&self) -> Option<PathBuf> {
        find_on_path(self.tool)
    }
}

/// A detected project language and whether its indexer is installed.
#[derive(Debug, Clone)]
pub struct Detection {
    /// The matching indexer.
    pub spec: IndexerSpec,
    /// Where the tool was found; `None` means precise analysis is unavailable.
    pub tool_path: Option<PathBuf>,
}

/// The set of indexers Knowell knows how to drive.
#[derive(Debug, Clone)]
pub struct IndexerRegistry {
    specs: Vec<IndexerSpec>,
}

impl Default for IndexerRegistry {
    fn default() -> Self {
        Self {
            specs: BUILTIN.to_vec(),
        }
    }
}

impl IndexerRegistry {
    /// A registry with the built-in indexers.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with custom specs (for example in tests or site-specific tools).
    pub fn with_specs(specs: Vec<IndexerSpec>) -> Self {
        Self { specs }
    }

    /// All known specs, in detection order.
    pub fn specs(&self) -> &[IndexerSpec] {
        &self.specs
    }

    /// The spec for a language.
    pub fn for_language(&self, language: Language) -> Option<&IndexerSpec> {
        self.specs.iter().find(|s| s.language == language)
    }

    /// Specs whose markers are present in `project_dir`, in registry order.
    pub fn detect(&self, project_dir: &Path) -> Vec<&IndexerSpec> {
        self.specs
            .iter()
            .filter(|s| s.detects(project_dir))
            .collect()
    }

    /// Like [`IndexerRegistry::detect`], also resolving each tool on `PATH`.
    /// A detected language whose tool is missing is still returned (with
    /// `tool_path: None`) so the caller can report "precise analysis unavailable".
    pub fn detect_with_availability(&self, project_dir: &Path) -> Vec<Detection> {
        self.detect(project_dir)
            .into_iter()
            .map(|s| Detection {
                spec: *s,
                tool_path: s.locate(),
            })
            .collect()
    }
}

fn out_arg(out: &Path) -> OsString {
    out.as_os_str().to_owned()
}

fn args_rust(_project: &Path, out: &Path) -> Vec<OsString> {
    vec!["scip".into(), ".".into(), "--output".into(), out_arg(out)]
}

fn args_typescript(project: &Path, out: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["index".into(), "--output".into(), out_arg(out)];
    // Plain JavaScript projects have no tsconfig.json; let the indexer infer one.
    if !project.join("tsconfig.json").is_file() {
        args.push("--infer-tsconfig".into());
    }
    args
}

fn args_python(_project: &Path, out: &Path) -> Vec<OsString> {
    vec!["index".into(), ".".into(), "--output".into(), out_arg(out)]
}

fn args_go(_project: &Path, out: &Path) -> Vec<OsString> {
    vec!["--output".into(), out_arg(out)]
}

fn args_index_output(_project: &Path, out: &Path) -> Vec<OsString> {
    vec!["index".into(), "--output".into(), out_arg(out)]
}

/// The built-in indexers.
pub const BUILTIN: &[IndexerSpec] = &[
    IndexerSpec {
        id: "rust-analyzer",
        language: Language::Rust,
        tool: "rust-analyzer",
        markers: &[Marker::File("Cargo.toml")],
        install_hint: "install with `rustup component add rust-analyzer`",
        args: args_rust,
    },
    IndexerSpec {
        id: "scip-typescript",
        language: Language::TypeScript,
        tool: "scip-typescript",
        markers: &[Marker::File("tsconfig.json"), Marker::File("package.json")],
        install_hint: "install with `npm install -g @sourcegraph/scip-typescript`",
        args: args_typescript,
    },
    IndexerSpec {
        id: "scip-python",
        language: Language::Python,
        tool: "scip-python",
        markers: &[
            Marker::File("pyproject.toml"),
            Marker::File("setup.cfg"),
            Marker::File("setup.py"),
        ],
        install_hint: "install with `npm install -g @sourcegraph/scip-python`",
        args: args_python,
    },
    IndexerSpec {
        id: "scip-go",
        language: Language::Go,
        tool: "scip-go",
        markers: &[Marker::File("go.mod")],
        install_hint: "install with `go install github.com/scip-code/scip-go/cmd/scip-go@latest`",
        args: args_go,
    },
    IndexerSpec {
        id: "scip-java",
        language: Language::Java,
        tool: "scip-java",
        markers: &[
            Marker::File("pom.xml"),
            Marker::File("build.gradle"),
            Marker::File("build.gradle.kts"),
        ],
        install_hint: "install with `cs install scip-java` (Coursier) or see the scip-java docs",
        args: args_index_output,
    },
    IndexerSpec {
        id: "scip-dotnet",
        language: Language::CSharp,
        tool: "scip-dotnet",
        markers: &[
            Marker::Extension("csproj"),
            Marker::Extension("sln"),
            Marker::Extension("slnx"),
        ],
        install_hint: "install with `dotnet tool install --global scip-dotnet`",
        args: args_index_output,
    },
];

/// Looks `tool` up on the process `PATH` (and `PATHEXT` on Windows).
pub fn find_on_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let pathext = std::env::var_os("PATHEXT");
    which_in(tool, &path, pathext.as_deref())
}

/// Looks `tool` up in an explicit `PATH`-style list. `pathext` is the
/// `;`-separated extension list used on Windows; it is ignored elsewhere.
/// Names containing a path separator are never resolved.
pub fn which_in(tool: &str, path_var: &OsStr, pathext: Option<&OsStr>) -> Option<PathBuf> {
    if tool.is_empty() || tool.contains(['/', '\\']) || tool.contains('\0') {
        return None;
    }
    let mut names: Vec<String> = vec![tool.to_owned()];
    if cfg!(windows) {
        let ext_list = pathext
            .and_then(|e| e.to_str())
            .unwrap_or(".COM;.EXE;.BAT;.CMD");
        let has_ext = Path::new(tool).extension().is_some();
        if !has_ext {
            names = ext_list
                .split(';')
                .filter(|e| !e.is_empty())
                .map(|e| format!("{tool}{e}"))
                .collect();
        }
    }
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in &names {
            let candidate = dir.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn ids(reg: &IndexerRegistry, dir: &Path) -> Vec<&'static str> {
        reg.detect(dir).into_iter().map(|s| s.id).collect()
    }

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"").unwrap();
    }

    #[test]
    fn detects_each_language() {
        let reg = IndexerRegistry::new();
        let cases: &[(&str, &str)] = &[
            ("Cargo.toml", "rust-analyzer"),
            ("tsconfig.json", "scip-typescript"),
            ("package.json", "scip-typescript"),
            ("pyproject.toml", "scip-python"),
            ("setup.cfg", "scip-python"),
            ("go.mod", "scip-go"),
            ("pom.xml", "scip-java"),
            ("build.gradle", "scip-java"),
            ("build.gradle.kts", "scip-java"),
            ("App.csproj", "scip-dotnet"),
            ("All.SLN", "scip-dotnet"),
        ];
        for (file, want) in cases {
            let dir = tempfile::tempdir().unwrap();
            touch(dir.path(), file);
            assert_eq!(ids(&reg, dir.path()), vec![*want], "marker {file}");
        }
    }

    #[test]
    fn polyglot_project_detects_all_in_registry_order() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "go.mod");
        touch(dir.path(), "Cargo.toml");
        touch(dir.path(), "package.json");
        assert_eq!(
            ids(&IndexerRegistry::new(), dir.path()),
            vec!["rust-analyzer", "scip-typescript", "scip-go"]
        );
    }

    #[test]
    fn empty_missing_and_directory_markers_do_not_match() {
        let reg = IndexerRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        assert!(reg.detect(dir.path()).is_empty());
        std::fs::create_dir(dir.path().join("Cargo.toml")).unwrap();
        assert!(reg.detect(dir.path()).is_empty());
        assert!(reg.detect(&dir.path().join("nope")).is_empty());
        touch(dir.path(), ".csproj");
        assert!(reg.detect(dir.path()).is_empty());
    }

    #[test]
    fn typescript_args_infer_tsconfig_only_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("index.scip");
        let spec = BUILTIN.iter().find(|s| s.id == "scip-typescript").unwrap();
        let args = (spec.args)(dir.path(), &out);
        assert!(args.iter().any(|a| a == "--infer-tsconfig"));
        touch(dir.path(), "tsconfig.json");
        let args = (spec.args)(dir.path(), &out);
        assert!(!args.iter().any(|a| a == "--infer-tsconfig"));
        assert!(args.iter().any(|a| a == out.as_os_str()));
    }

    #[test]
    fn availability_reports_missing_tool() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Cargo.toml");
        let reg = IndexerRegistry::with_specs(vec![IndexerSpec {
            tool: "knowell-no-such-indexer-xyz",
            ..BUILTIN[0]
        }]);
        let found = reg.detect_with_availability(dir.path());
        assert_eq!(found.len(), 1);
        assert!(found[0].tool_path.is_none());
    }

    #[test]
    fn which_in_finds_files_in_given_path() {
        let dir = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) {
            "mytool.exe"
        } else {
            "mytool"
        };
        let file = dir.path().join(name);
        std::fs::write(&file, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([dir.path()]).unwrap();
        let found = which_in("mytool", &path, Some(OsStr::new(".EXE"))).unwrap();
        assert_eq!(
            found.to_string_lossy().to_lowercase(),
            file.to_string_lossy().to_lowercase()
        );
        assert!(which_in("other", &path, None).is_none());
        assert!(which_in("../mytool", &path, None).is_none());
        assert!(which_in("", &path, None).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn which_in_ignores_non_executable_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plain"), b"").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert!(which_in("plain", &path, None).is_none());
    }
}
