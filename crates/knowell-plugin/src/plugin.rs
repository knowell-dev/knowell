//! A loaded plugin and the sandboxed execution of its exports.

use std::sync::Arc;

use wasmtime::component::{Component, ComponentExportIndex, InstancePre};
use wasmtime::{Store, Trap};

use crate::error::PluginError;
use crate::grants::ProjectFilesGrant;
use crate::host::HostInner;
use crate::manifest::{API_VERSION, ApiVersion, Capability, Manifest};
use crate::output::{AnalyzerOutput, PluginInfo, SourceFile, line_count};
use crate::sanitize::{sanitize, tail};
use crate::state::{Denied, HostState};
use crate::wire::{self, AnalyzeParams, AnalyzeResults, InfoResults, WireSourceFile};

const PACKAGE: &str = "knowell:plugin/";
const ANALYZER: &str = "analyzer";
const METADATA: &str = "metadata";
const LOG: &str = "log";
const PROJECT_FILES: &str = "project-files";
/// Longest fault description / stderr excerpt kept in an error, in bytes.
const MAX_FAULT_TEXT: usize = 512;

/// A loaded, verified plugin. Calls are independent: each runs in a fresh
/// sandboxed instance, so a failed call never affects the next one, and a
/// `Plugin` can be used from several threads at once.
pub struct Plugin {
    host: Arc<HostInner>,
    pre: InstancePre<HostState>,
    analyze: ComponentExportIndex,
    manifest: Manifest,
    info: PluginInfo,
    name: Arc<str>,
    files: Option<ProjectFilesGrant>,
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("manifest", &self.manifest)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// Exports resolved at load time.
struct Exports {
    info: ComponentExportIndex,
    analyze: ComponentExportIndex,
}

/// Checks imports and exports, links the component, then runs `metadata.info`
/// in the sandbox and validates it against the manifest.
pub(crate) fn instantiate(
    host: Arc<HostInner>,
    component: &Component,
    manifest: &Manifest,
    files: Option<ProjectFilesGrant>,
) -> Result<Plugin, PluginError> {
    let name: Arc<str> = Arc::from(manifest.name().as_str());
    let exports = inspect(&host, component, manifest)?;
    let pre = host
        .linker
        .instantiate_pre(component)
        .map_err(|e| PluginError::Link {
            plugin: name.to_string(),
            reason: sanitize(&format!("{e:#}"), MAX_FAULT_TEXT),
        })?;
    let mut sandbox = Sandbox::new(&host, &name, files.as_ref())?;
    let instance = pre
        .instantiate(&mut sandbox.store)
        .map_err(|e| sandbox.classify(e))?;
    let link_error = |e: wasmtime::Error| PluginError::Link {
        plugin: name.to_string(),
        reason: sanitize(&format!("{e:#}"), MAX_FAULT_TEXT),
    };
    // Type-check both exports against the host's view of the WIT now, so a
    // mismatched plugin is refused at load rather than on first use.
    instance
        .get_typed_func::<AnalyzeParams<'_>, AnalyzeResults>(&mut sandbox.store, exports.analyze)
        .map_err(link_error)?;
    let info_fn = instance
        .get_typed_func::<(), InfoResults>(&mut sandbox.store, exports.info)
        .map_err(link_error)?;
    let (raw,) = info_fn
        .call(&mut sandbox.store, ())
        .map_err(|e| sandbox.classify(e))?;
    let info = wire::validate_info(raw, manifest)?;
    sandbox.store.data().finish();
    drop(sandbox);
    tracing::debug!(
        plugin = %name,
        version = manifest.version(),
        languages = ?info.languages(),
        "loaded plugin"
    );
    Ok(Plugin {
        host,
        pre,
        analyze: exports.analyze,
        manifest: manifest.clone(),
        info,
        name,
        files,
    })
}

impl Plugin {
    /// The manifest the plugin was loaded with.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The plugin's validated self-description.
    pub fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// Whether the plugin declared `language`.
    pub fn supports_language(&self, language: &str) -> bool {
        self.info.languages.contains(language)
    }

    /// Analyses one file in a fresh sandboxed instance and validates the
    /// output: its size (copying out of the plugin is capped first, see
    /// [`HostConfig::max_output_bytes`](crate::HostConfig::max_output_bytes)),
    /// every key, line ranges against the file, and enum values.
    pub fn analyze(&self, file: &SourceFile<'_>) -> Result<AnalyzerOutput, PluginError> {
        let config = &self.host.config;
        if !self.supports_language(file.language) {
            return Err(PluginError::UnsupportedLanguage {
                plugin: self.name.to_string(),
                language: sanitize(file.language, 64),
            });
        }
        if u64::try_from(file.text.len()).unwrap_or(u64::MAX) > config.max_input_bytes {
            return Err(PluginError::InputTooLarge {
                limit: config.max_input_bytes,
            });
        }
        let lines = line_count(file.text);

        let mut sandbox = Sandbox::new(&self.host, &self.name, self.files.as_ref())?;
        let instance = self
            .pre
            .instantiate(&mut sandbox.store)
            .map_err(|e| sandbox.classify(e))?;
        let analyze = instance
            .get_typed_func::<AnalyzeParams<'_>, AnalyzeResults>(&mut sandbox.store, self.analyze)
            .map_err(|e| PluginError::Link {
                plugin: self.name.to_string(),
                reason: sanitize(&format!("{e:#}"), MAX_FAULT_TEXT),
            })?;
        let input = WireSourceFile {
            path: file.path.as_str(),
            language: file.language,
            text: file.text,
        };
        let (result,) = analyze
            .call(&mut sandbox.store, (input,))
            .map_err(|e| sandbox.classify(e))?;
        sandbox.store.data().finish();
        match result {
            Ok(raw) => wire::validate_analysis(raw, &self.name, config.max_output_bytes, lines),
            Err(raw) => Err(wire::decline(&raw, &self.name)),
        }
    }
}

/// One call's store, configured with every limit.
struct Sandbox<'h> {
    host: &'h HostInner,
    name: &'h str,
    store: Store<HostState>,
}

impl<'h> Sandbox<'h> {
    fn new(
        host: &'h HostInner,
        name: &'h Arc<str>,
        files: Option<&ProjectFilesGrant>,
    ) -> Result<Self, PluginError> {
        let config = &host.config;
        let state = HostState::new(config, Arc::clone(name), files);
        let mut store = Store::new(&host.engine, state);
        store.limiter(|state| &mut state.limiter);
        store.set_hostcall_fuel(wire::hostcall_fuel(config.max_output_bytes));
        store
            .set_fuel(config.fuel_per_call)
            .map_err(|e| PluginError::Engine(sanitize(&e.to_string(), MAX_FAULT_TEXT)))?;
        store.set_epoch_deadline(config.epoch_ticks());
        store.epoch_deadline_trap();
        Ok(Self { host, name, store })
    }

    /// Maps a failed instantiation or call to the limit or fault behind it.
    fn classify(&self, error: wasmtime::Error) -> PluginError {
        let plugin = self.name.to_string();
        let config = &self.host.config;
        let trap = error.downcast_ref::<Trap>().copied();
        match trap {
            Some(Trap::OutOfFuel) => {
                return PluginError::FuelExhausted {
                    plugin,
                    fuel: config.fuel_per_call,
                };
            }
            Some(Trap::Interrupt) => {
                return PluginError::Timeout {
                    plugin,
                    timeout_ms: config.timeout_ms(),
                };
            }
            _ => {}
        }
        let state = self.store.data();
        state.finish();
        if state.limiter.memory_exceeded {
            return PluginError::MemoryLimit {
                plugin,
                limit: config.max_memory_bytes,
            };
        }
        if state.limiter.table_exceeded {
            return PluginError::TableLimit {
                plugin,
                limit: config.max_table_elements,
            };
        }
        if trap == Some(Trap::StackOverflow) {
            return PluginError::StackOverflow { plugin };
        }
        if let Some(exit) = error.downcast_ref::<wasmtime_wasi::I32Exit>() {
            return PluginError::Exited {
                plugin,
                code: exit.0,
            };
        }
        if is_hostcall_fuel_exhausted(&error) {
            return PluginError::OutputTooLarge {
                plugin,
                limit: config.max_output_bytes,
            };
        }
        let reason = if let Some(denied) = error.downcast_ref::<Denied>() {
            denied.to_string()
        } else if let Some(trap) = trap {
            trap.to_string()
        } else {
            format!("{error:#}")
        };
        PluginError::Fault {
            plugin,
            reason: sanitize(&reason, MAX_FAULT_TEXT),
            stderr: tail(&state.stderr.contents(), MAX_FAULT_TEXT),
        }
    }
}

/// Whether Wasmtime refused to copy more data out of the plugin than the
/// store's hostcall fuel allows. Wasmtime's error type for this is private,
/// so its (version-pinned, test-covered) message is matched.
fn is_hostcall_fuel_exhausted(error: &wasmtime::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .to_string()
            .contains("fuel allocated for hostcalls has been exhausted")
    })
}

/// Checks the component's imports and exports against what the host offers
/// and what the manifest declares.
fn inspect(
    host: &HostInner,
    component: &Component,
    manifest: &Manifest,
) -> Result<Exports, PluginError> {
    let plugin = manifest.name().as_str();
    let ty = component.component_type();
    for (import, _) in ty.imports(&host.engine) {
        if import.starts_with("wasi:") {
            // Provided (locked down) by the linker, or refused by it below.
            continue;
        }
        let Some((interface, version)) = knowell_interface(import) else {
            return Err(PluginError::UnsupportedImport {
                plugin: plugin.to_string(),
                import: sanitize(import, 128),
            });
        };
        check_version(plugin, version)?;
        match interface {
            LOG => {}
            PROJECT_FILES => {
                if !manifest.requests(Capability::ProjectFiles) {
                    return Err(PluginError::UndeclaredCapability {
                        plugin: plugin.to_string(),
                        import: sanitize(import, 128),
                        capability: Capability::ProjectFiles,
                    });
                }
            }
            _ => {
                return Err(PluginError::UnsupportedImport {
                    plugin: plugin.to_string(),
                    import: sanitize(import, 128),
                });
            }
        }
    }

    let mut analyzer = None;
    let mut metadata = None;
    for (export, _) in ty.exports(&host.engine) {
        let Some((interface, version)) = knowell_interface(export) else {
            continue;
        };
        let slot = match interface {
            ANALYZER => &mut analyzer,
            METADATA => &mut metadata,
            _ => continue,
        };
        check_version(plugin, version)?;
        *slot = Some(export);
    }
    let missing = |export: &str| PluginError::MissingExport {
        plugin: plugin.to_string(),
        export: format!("{PACKAGE}{export}@{API_VERSION}"),
    };
    let analyzer = analyzer.ok_or_else(|| missing(ANALYZER))?;
    let metadata = metadata.ok_or_else(|| missing(METADATA))?;
    let function = |interface: &str, function: &str, label: &str| {
        component
            .get_export_index(None, interface)
            .and_then(|parent| component.get_export_index(Some(&parent), function))
            .ok_or_else(|| missing(label))
    };
    Ok(Exports {
        info: function(metadata, "info", "metadata#info")?,
        analyze: function(analyzer, "analyze", "analyzer#analyze")?,
    })
}

/// Splits `knowell:plugin/<interface>@<version>`.
fn knowell_interface(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix(PACKAGE)?.split_once('@')
}

fn check_version(plugin: &str, version: &str) -> Result<(), PluginError> {
    let compatible = version
        .parse::<ApiVersion>()
        .is_ok_and(|v| v.is_compatible_with(API_VERSION));
    if compatible {
        Ok(())
    } else {
        Err(PluginError::IncompatibleApi {
            plugin: plugin.to_string(),
            requested: sanitize(version, 32),
            supported: API_VERSION.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_interface_names() {
        assert_eq!(
            knowell_interface("knowell:plugin/analyzer@0.1.0"),
            Some(("analyzer", "0.1.0"))
        );
        assert_eq!(knowell_interface("knowell:plugin/analyzer"), None);
        assert_eq!(knowell_interface("wasi:cli/environment@0.2.0"), None);
        assert_eq!(knowell_interface("other:plugin/analyzer@0.1.0"), None);
    }

    #[test]
    fn versions() {
        assert!(check_version("p", "0.1.0").is_ok());
        assert!(matches!(
            check_version("p", "0.2.0"),
            Err(PluginError::IncompatibleApi { .. })
        ));
        assert!(check_version("p", "garbage").is_err());
    }
}
