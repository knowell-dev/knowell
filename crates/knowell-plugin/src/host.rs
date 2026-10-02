//! The plugin host: one Wasmtime engine, its epoch thread, the shared linker
//! and the compiled-component caches.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use wasmtime::component::{Component, Linker};
use wasmtime::{Cache, CacheConfig, Engine, OptLevel};

use crate::config::{EPOCH_TICK, HostConfig};
use crate::error::PluginError;
use crate::grants::Grants;
use crate::manifest::{API_VERSION, Capability, Manifest, Sha256Digest};
use crate::plugin::{self, Plugin};
use crate::sanitize::sanitize;
use crate::state::{HostState, build_linker};

/// Compiled components kept in memory per host; beyond this the in-memory
/// map is cleared (the disk cache still avoids recompiling).
const MAX_MEMORY_CACHE_ENTRIES: usize = 64;

/// Where a component comes from.
#[derive(Debug, Clone, Copy)]
pub enum PluginSource<'a> {
    /// A `.wasm` component file; read up to the size limit.
    Path(&'a Path),
    /// Component bytes already in memory.
    Bytes(&'a [u8]),
}

impl<'a> From<&'a Path> for PluginSource<'a> {
    fn from(path: &'a Path) -> Self {
        Self::Path(path)
    }
}

impl<'a> From<&'a [u8]> for PluginSource<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        Self::Bytes(bytes)
    }
}

/// Counters of the compiled-component caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// Components compiled in this host's memory cache.
    pub memory_entries: usize,
    /// Loads served from the on-disk cache (no compilation).
    pub disk_hits: usize,
    /// Loads that compiled and wrote the on-disk cache.
    pub disk_misses: usize,
}

/// Runs analyzer plugins as sandboxed WebAssembly components.
///
/// Every call (`metadata` at load, each `analyze`) gets a fresh instance with
/// its own fuel, memory, table and time budget, a WASI context that grants
/// nothing, and only the capabilities the user granted. A plugin failing in
/// any way leaves the host and other plugins unaffected.
///
/// Cloning is cheap and shares the engine.
#[derive(Clone)]
pub struct PluginHost {
    inner: Arc<HostInner>,
}

pub(crate) struct HostInner {
    pub(crate) engine: Engine,
    pub(crate) config: HostConfig,
    pub(crate) linker: Linker<HostState>,
    compiled: Mutex<HashMap<Sha256Digest, Component>>,
    disk_cache: Option<(Cache, PathBuf)>,
    _ticker: EpochTicker,
}

impl PluginHost {
    /// Validates `config`, creates the engine (fuel metering and epoch
    /// interruption on) and starts the epoch thread that enforces timeouts.
    pub fn new(config: HostConfig) -> Result<Self, PluginError> {
        config.validate()?;
        let engine_error = |e: wasmtime::Error| PluginError::Engine(sanitize(&e.to_string(), 512));

        let disk_cache = match &config.cache_dir {
            Some(dir) => {
                let dir = dir.join(config.fingerprint());
                let mut cache_config = CacheConfig::new();
                cache_config.with_directory(&dir);
                Some((Cache::new(cache_config).map_err(engine_error)?, dir))
            }
            None => None,
        };

        let mut wasm = wasmtime::Config::new();
        wasm.wasm_component_model(true)
            .consume_fuel(true)
            .epoch_interruption(true)
            .cranelift_opt_level(OptLevel::Speed)
            .cache(disk_cache.as_ref().map(|(cache, _)| cache.clone()));
        let engine = Engine::new(&wasm).map_err(engine_error)?;
        let linker = build_linker(&engine).map_err(engine_error)?;
        let ticker = EpochTicker::start(engine.clone())?;
        Ok(Self {
            inner: Arc::new(HostInner {
                engine,
                config,
                linker,
                compiled: Mutex::new(HashMap::new()),
                disk_cache,
                _ticker: ticker,
            }),
        })
    }

    /// The host's configuration.
    pub fn config(&self) -> &HostConfig {
        &self.inner.config
    }

    /// The directory compiled components are cached in (the configured
    /// `cache_dir` plus a configuration fingerprint), if disk caching is on.
    pub fn cache_dir(&self) -> Option<&Path> {
        self.inner.disk_cache.as_ref().map(|(_, dir)| dir.as_path())
    }

    /// Cache counters since this host was created.
    pub fn cache_stats(&self) -> CacheStats {
        let memory_entries = self
            .inner
            .compiled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        let (disk_hits, disk_misses) =
            self.inner.disk_cache.as_ref().map_or((0, 0), |(cache, _)| {
                (cache.cache_hits(), cache.cache_misses())
            });
        CacheStats {
            memory_entries,
            disk_hits,
            disk_misses,
        }
    }

    /// Loads a plugin after checking, in order: the manifest's API version,
    /// that every requested capability is granted, the component size, its
    /// SHA-256 against the manifest, its imports and exports, and finally the
    /// plugin's own metadata (run sandboxed) against the manifest.
    pub fn load(
        &self,
        source: PluginSource<'_>,
        manifest: &Manifest,
        grants: &Grants,
    ) -> Result<Plugin, PluginError> {
        let name = manifest.name().as_str();
        if !manifest.api_version().is_compatible_with(API_VERSION) {
            return Err(PluginError::IncompatibleApi {
                plugin: name.to_string(),
                requested: manifest.api_version().to_string(),
                supported: API_VERSION.to_string(),
            });
        }
        for capability in manifest.capabilities() {
            if !grants.grants(*capability) {
                return Err(PluginError::CapabilityNotGranted {
                    plugin: name.to_string(),
                    capability: *capability,
                });
            }
        }

        let bytes = self.read_component(source, name)?;
        let actual = Sha256Digest::of(&bytes);
        if actual != manifest.sha256() {
            return Err(PluginError::HashMismatch {
                plugin: name.to_string(),
                expected: manifest.sha256().to_string(),
                actual: actual.to_string(),
            });
        }
        let component = self.compile(actual, &bytes, name)?;
        // Only requested capabilities reach the plugin, whatever was granted.
        let files = if manifest.requests(Capability::ProjectFiles) {
            grants.project_files().cloned()
        } else {
            None
        };
        plugin::instantiate(Arc::clone(&self.inner), &component, manifest, files)
    }

    fn read_component<'a>(
        &self,
        source: PluginSource<'a>,
        plugin: &str,
    ) -> Result<Cow<'a, [u8]>, PluginError> {
        let limit = self.inner.config.max_component_bytes;
        let too_large = || PluginError::ComponentTooLarge {
            plugin: plugin.to_string(),
            limit,
        };
        match source {
            PluginSource::Bytes(bytes) => {
                if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
                    return Err(too_large());
                }
                Ok(Cow::Borrowed(bytes))
            }
            PluginSource::Path(path) => {
                let read_error = |source| PluginError::Read {
                    path: path.to_path_buf(),
                    source,
                };
                let file = File::open(path).map_err(read_error)?;
                let mut bytes = Vec::new();
                file.take(limit.saturating_add(1))
                    .read_to_end(&mut bytes)
                    .map_err(read_error)?;
                if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
                    return Err(too_large());
                }
                Ok(Cow::Owned(bytes))
            }
        }
    }

    fn compile(
        &self,
        digest: Sha256Digest,
        bytes: &[u8],
        plugin: &str,
    ) -> Result<Component, PluginError> {
        let cached = self
            .inner
            .compiled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&digest)
            .cloned();
        if let Some(component) = cached {
            return Ok(component);
        }
        // Compile outside the lock; a concurrent load of the same plugin only
        // wastes work, it cannot produce a different result.
        let component = Component::from_binary(&self.inner.engine, bytes).map_err(|e| {
            PluginError::Compile {
                plugin: plugin.to_string(),
                reason: sanitize(&format!("{e:#}"), 512),
            }
        })?;
        let mut compiled = self
            .inner
            .compiled
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if compiled.len() >= MAX_MEMORY_CACHE_ENTRIES {
            compiled.clear();
        }
        compiled.insert(digest, component.clone());
        Ok(component)
    }
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHost")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

/// Advances the engine epoch every [`EPOCH_TICK`] so running calls hit their
/// deadline. Stops when dropped (with the last host or plugin).
struct EpochTicker {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: Engine) -> Result<Self, PluginError> {
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = thread::Builder::new()
            .name("knowell-plugin-epoch".to_string())
            .spawn(move || {
                while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(EPOCH_TICK) {
                    engine.increment_epoch();
                }
            })
            .map_err(|e| PluginError::Engine(format!("cannot start the epoch thread: {e}")))?;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        // Dropping the sender wakes the thread with `Disconnected`.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            // The thread only loops on a channel; it cannot panic.
            let _ = thread.join();
        }
    }
}
