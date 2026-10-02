use std::fmt;
use std::path::PathBuf;

use crate::manifest::Capability;

/// Errors of the plugin host.
///
/// Plugins are untrusted: every piece of text in an error that came from a
/// plugin (decline messages, stderr, trap descriptions) is sanitised — control
/// and bidirectional-override characters replaced, length capped — before it
/// is stored here. Errors never contain the analysed file's contents.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PluginError {
    /// A [`HostConfig`](crate::HostConfig) value is out of range.
    #[error("invalid plugin host configuration: {0}")]
    InvalidConfig(String),
    /// The WebAssembly engine, its cache or its epoch thread could not be set up.
    #[error("cannot set up the plugin engine: {0}")]
    Engine(String),
    /// A capability grant could not be created (for example, the project root
    /// does not exist).
    #[error("invalid capability grant: {0}")]
    InvalidGrant(String),
    /// The manifest is not valid TOML or violates the manifest rules.
    #[error("invalid plugin manifest: {0}")]
    InvalidManifest(String),
    /// The plugin targets a plugin API version this host cannot run.
    #[error(
        "plugin `{plugin}` targets plugin api {requested}, but this host supports {supported}; \
         rebuild the plugin against knowell:plugin@{supported}"
    )]
    IncompatibleApi {
        /// Plugin name from the manifest.
        plugin: String,
        /// Version the plugin asked for (manifest or component interface).
        requested: String,
        /// Version this host implements.
        supported: String,
    },
    /// The component's SHA-256 differs from the one pinned in the manifest.
    #[error(
        "plugin `{plugin}` refused: component sha256 is {actual}, but the manifest pins {expected}"
    )]
    HashMismatch {
        /// Plugin name from the manifest.
        plugin: String,
        /// Digest pinned in the manifest.
        expected: String,
        /// Digest of the bytes that were offered.
        actual: String,
    },
    /// The manifest requests a capability the user has not granted.
    #[error(
        "plugin `{plugin}` requests capability `{capability}`, which is not granted; grant it in \
         the plugin configuration or remove the plugin"
    )]
    CapabilityNotGranted {
        /// Plugin name from the manifest.
        plugin: String,
        /// The missing grant.
        capability: Capability,
    },
    /// The component imports a capability interface its manifest does not request.
    #[error(
        "plugin `{plugin}` imports `{import}` but its manifest does not request capability \
         `{capability}`"
    )]
    UndeclaredCapability {
        /// Plugin name from the manifest.
        plugin: String,
        /// The import name.
        import: String,
        /// The capability the import belongs to.
        capability: Capability,
    },
    /// The component imports something the host does not provide.
    #[error("plugin `{plugin}` imports `{import}`, which the plugin host does not provide")]
    UnsupportedImport {
        /// Plugin name from the manifest.
        plugin: String,
        /// The import name (sanitised).
        import: String,
    },
    /// The component lacks a required export.
    #[error("plugin `{plugin}` does not export `{export}`")]
    MissingExport {
        /// Plugin name from the manifest.
        plugin: String,
        /// The missing export.
        export: String,
    },
    /// The component file could not be read.
    #[error("cannot read plugin component `{path}`: {source}")]
    Read {
        /// File that was read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The component is larger than [`HostConfig::max_component_bytes`](crate::HostConfig::max_component_bytes).
    #[error("plugin `{plugin}` component is larger than the {limit}-byte limit")]
    ComponentTooLarge {
        /// Plugin name from the manifest.
        plugin: String,
        /// The configured limit in bytes.
        limit: u64,
    },
    /// The bytes are not a valid WebAssembly component.
    #[error("plugin `{plugin}` does not compile: {reason}")]
    Compile {
        /// Plugin name from the manifest.
        plugin: String,
        /// Compiler message.
        reason: String,
    },
    /// The component's imports or exports do not type-check against the host.
    #[error("plugin `{plugin}` cannot be linked: {reason}")]
    Link {
        /// Plugin name from the manifest.
        plugin: String,
        /// Linker message (sanitised).
        reason: String,
    },
    /// The plugin's self-description is malformed.
    #[error("plugin `{plugin}` reported invalid metadata: {reason}")]
    InvalidMetadata {
        /// Plugin name from the manifest.
        plugin: String,
        /// What is wrong.
        reason: String,
    },
    /// The plugin's self-description contradicts its manifest.
    #[error("plugin `{plugin}` metadata does not match its manifest: {reason}")]
    MetadataMismatch {
        /// Plugin name from the manifest.
        plugin: String,
        /// What differs.
        reason: String,
    },
    /// The file's language is not one the plugin declared.
    #[error("plugin `{plugin}` does not analyse language `{language}`")]
    UnsupportedLanguage {
        /// Plugin name from the manifest.
        plugin: String,
        /// Requested language (sanitised).
        language: String,
    },
    /// The file is larger than [`HostConfig::max_input_bytes`](crate::HostConfig::max_input_bytes).
    #[error("input file is larger than the {limit}-byte plugin input limit")]
    InputTooLarge {
        /// The configured limit in bytes.
        limit: u64,
    },
    /// The call used up its fuel (instruction budget).
    #[error("plugin `{plugin}` ran out of fuel ({fuel} units per call)")]
    FuelExhausted {
        /// Plugin name from the manifest.
        plugin: String,
        /// The configured fuel per call.
        fuel: u64,
    },
    /// The call ran past its wall-clock limit.
    #[error("plugin `{plugin}` exceeded its {timeout_ms} ms time limit")]
    Timeout {
        /// Plugin name from the manifest.
        plugin: String,
        /// The configured limit in milliseconds.
        timeout_ms: u64,
    },
    /// The plugin tried to grow linear memory past the limit.
    #[error("plugin `{plugin}` exceeded its {limit}-byte memory limit")]
    MemoryLimit {
        /// Plugin name from the manifest.
        plugin: String,
        /// The configured limit in bytes.
        limit: u64,
    },
    /// The plugin tried to grow its tables past the limit.
    #[error("plugin `{plugin}` exceeded its {limit}-element table limit")]
    TableLimit {
        /// Plugin name from the manifest.
        plugin: String,
        /// The configured limit in elements.
        limit: u64,
    },
    /// The plugin exhausted the WebAssembly call stack.
    #[error("plugin `{plugin}` exhausted its call stack")]
    StackOverflow {
        /// Plugin name from the manifest.
        plugin: String,
    },
    /// The plugin called `exit` instead of returning.
    #[error("plugin `{plugin}` exited with status {code} instead of returning")]
    Exited {
        /// Plugin name from the manifest.
        plugin: String,
        /// Exit status passed by the plugin.
        code: i32,
    },
    /// The plugin trapped, used a denied facility, or violated the component ABI.
    #[error("plugin `{plugin}` failed: {reason}{}", StderrSuffix(.stderr.as_deref()))]
    Fault {
        /// Plugin name from the manifest.
        plugin: String,
        /// Trap or violation description (sanitised).
        reason: String,
        /// Tail of what the plugin wrote to stderr during the call (sanitised).
        stderr: Option<String>,
    },
    /// The plugin returned its own `analyze-error`.
    #[error("plugin `{plugin}` declined the file ({kind}): {message}")]
    Declined {
        /// Plugin name from the manifest.
        plugin: String,
        /// Which `analyze-error` case.
        kind: DeclineKind,
        /// The plugin's message (sanitised, truncated).
        message: String,
    },
    /// The output exceeded [`HostConfig::max_output_bytes`](crate::HostConfig::max_output_bytes).
    #[error("plugin `{plugin}` returned more than {limit} bytes of output")]
    OutputTooLarge {
        /// Plugin name from the manifest.
        plugin: String,
        /// The configured limit in bytes.
        limit: u64,
    },
    /// The output violates the contract (empty keys, ranges outside the file, ...).
    #[error("plugin `{plugin}` returned invalid output: {reason}")]
    InvalidOutput {
        /// Plugin name from the manifest.
        plugin: String,
        /// The first violation found.
        reason: String,
    },
}

/// Which case of the WIT `analyze-error` variant a plugin returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeclineKind {
    /// The file uses a dialect or version the plugin does not handle.
    Unsupported,
    /// The plugin failed to analyse the file.
    Failed,
}

impl fmt::Display for DeclineKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "unsupported",
            Self::Failed => "failed",
        })
    }
}

struct StderrSuffix<'a>(Option<&'a str>);

impl fmt::Display for StderrSuffix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(text) => write!(f, " (plugin stderr: {text})"),
            None => Ok(()),
        }
    }
}
