//! Per-call store state, resource limiter and the host side of the plugin's
//! imports (`log`, `project-files`, and a locked-down WASI preview 2).

use std::sync::Arc;
use std::time::Duration;

use wasmtime::component::{Linker, ResourceTable, WasmStr};
use wasmtime::{AsContext, Engine, ResourceLimiter, StoreContextMut};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{
    Deterministic, HostMonotonicClock, HostWallClock, WasiCtx, WasiCtxBuilder, WasiCtxView,
    WasiView,
};

use crate::config::HostConfig;
use crate::grants::{ProjectFilesGrant, ReadFailure};
use crate::manifest::API_VERSION;
use crate::sanitize::sanitize;
use crate::wire::{WireLogLevel, WireReadError};

/// Bytes of plugin stderr kept per call (for fault reports).
pub(crate) const STDERR_CAPACITY: usize = 64 * 1024;
/// Longest path a plugin may pass to `read-file`, in bytes.
const MAX_PATH_BYTES: usize = 1024;
/// Core instances, tables and memories one component instance may create.
/// Real plugins need a handful (main module plus a few adapter shims).
const MAX_INSTANCES: usize = 32;
const MAX_TABLES: usize = 32;
const MAX_MEMORIES: usize = 8;
/// Largest `wasi:random` request served, in bytes.
const MAX_RANDOM_BYTES: u64 = 64 * 1024;
/// Fixed "randomness": plugin output must not depend on a random seed, and a
/// plugin has no business doing cryptography.
const RANDOM_SEED: [u8; 8] = [0x6b, 0x6e, 0x6f, 0x77, 0x65, 0x6c, 0x6c, 0x21];

/// WASI package version whose clock functions are shadowed below.
const WASI_CLOCKS: &str = "wasi:clocks/monotonic-clock@0.2.12";

/// Store data of one plugin call.
pub(crate) struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    pub(crate) limiter: Limiter,
    log: LogBudget,
    files: Option<FileAccess>,
    plugin: Arc<str>,
    pub(crate) stderr: MemoryOutputPipe,
}

impl HostState {
    pub(crate) fn new(
        config: &HostConfig,
        plugin: Arc<str>,
        files: Option<&ProjectFilesGrant>,
    ) -> Self {
        let stderr = MemoryOutputPipe::new(STDERR_CAPACITY);
        Self {
            wasi: locked_down_wasi(stderr.clone()),
            table: ResourceTable::new(),
            limiter: Limiter::new(config),
            log: LogBudget {
                remaining: config.max_log_messages,
                max_bytes: usize::try_from(config.max_log_message_bytes).unwrap_or(usize::MAX),
                dropped: 0,
            },
            files: files.map(|grant| FileAccess {
                grant: grant.clone(),
                reads_left: config.max_project_file_reads,
                max_bytes: config.max_project_file_bytes,
            }),
            plugin,
            stderr,
        }
    }

    /// Reports log messages dropped over the per-call budget.
    pub(crate) fn finish(&self) {
        if self.log.dropped > 0 {
            tracing::debug!(
                target: "knowell_plugin::guest",
                plugin = %self.plugin,
                dropped = self.log.dropped,
                "plugin log messages over the per-call limit were dropped"
            );
        }
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// A WASI context that grants nothing: no preopened directories, no
/// environment variables or arguments, closed stdin, discarded stdout, a
/// bounded stderr capture, no network of any kind, clocks frozen at the Unix
/// epoch and deterministic randomness.
fn locked_down_wasi(stderr: MemoryOutputPipe) -> WasiCtx {
    let mut builder = WasiCtxBuilder::new();
    builder
        .stderr(stderr)
        .allow_tcp(false)
        .allow_udp(false)
        .allow_ip_name_lookup(false)
        .wall_clock(FrozenClock)
        .monotonic_clock(FrozenClock)
        .secure_random(Deterministic::new(RANDOM_SEED.to_vec()))
        .insecure_random(Deterministic::new(RANDOM_SEED.to_vec()))
        .insecure_random_seed(u128::from_le_bytes([0x5a; 16]))
        .max_random_size(MAX_RANDOM_BYTES);
    builder.build()
}

/// A clock that never moves: plugins get no timing side channel and their
/// output cannot depend on the time.
struct FrozenClock;

impl HostWallClock for FrozenClock {
    fn resolution(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn now(&self) -> Duration {
        Duration::ZERO
    }
}

impl HostMonotonicClock for FrozenClock {
    fn resolution(&self) -> u64 {
        1_000_000_000
    }

    fn now(&self) -> u64 {
        0
    }
}

/// Enforces the memory and table limits, summed over all memories / tables
/// of an instance, and records whether a limit was hit so a resulting trap
/// can be reported as a limit violation.
pub(crate) struct Limiter {
    max_memory: usize,
    max_table: usize,
    memory_used: usize,
    table_used: usize,
    pub(crate) memory_exceeded: bool,
    pub(crate) table_exceeded: bool,
}

impl Limiter {
    fn new(config: &HostConfig) -> Self {
        Self {
            max_memory: usize::try_from(config.max_memory_bytes).unwrap_or(usize::MAX),
            max_table: usize::try_from(config.max_table_elements).unwrap_or(usize::MAX),
            memory_used: 0,
            table_used: 0,
            memory_exceeded: false,
            table_exceeded: false,
        }
    }
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let grown = self
            .memory_used
            .checked_add(desired.saturating_sub(current))
            .filter(|total| *total <= self.max_memory);
        match grown {
            Some(total) => {
                self.memory_used = total;
                Ok(true)
            }
            None => {
                // Refuse the growth (`memory.grow` returns -1) as the spec
                // allows; a guest that cannot cope aborts and the abort is
                // reported as a memory-limit violation.
                self.memory_exceeded = true;
                Ok(false)
            }
        }
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let grown = self
            .table_used
            .checked_add(desired.saturating_sub(current))
            .filter(|total| *total <= self.max_table);
        match grown {
            Some(total) => {
                self.table_used = total;
                Ok(true)
            }
            None => {
                self.table_exceeded = true;
                Ok(false)
            }
        }
    }

    fn instances(&self) -> usize {
        MAX_INSTANCES
    }

    fn tables(&self) -> usize {
        MAX_TABLES
    }

    fn memories(&self) -> usize {
        MAX_MEMORIES
    }
}

struct LogBudget {
    remaining: u32,
    max_bytes: usize,
    dropped: u32,
}

struct FileAccess {
    grant: ProjectFilesGrant,
    reads_left: u32,
    max_bytes: u64,
}

/// A host facility a plugin is not allowed to use; calling it traps.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Denied(pub(crate) &'static str);

/// Builds the linker shared by every plugin of a host: locked-down WASI p2,
/// `knowell:plugin/log` and `knowell:plugin/project-files`.
pub(crate) fn build_linker(engine: &Engine) -> wasmtime::Result<Linker<HostState>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

    // A plugin must not block the host thread: epoch interruption only fires
    // while WebAssembly runs, so a sleep inside WASI would escape the time
    // limit. Every pollable left (closed stdin, discarding or bounded
    // in-memory stdout/stderr) is always ready, so `poll` cannot block.
    linker.allow_shadowing(true);
    {
        let mut clock = linker.instance(WASI_CLOCKS)?;
        for name in ["subscribe-duration", "subscribe-instant"] {
            clock.func_new(name, |_store, _ty, _params, _results| {
                Err(wasmtime::Error::new(Denied(
                    "plugins cannot sleep or wait on timers",
                )))
            })?;
        }
    }
    linker.allow_shadowing(false);

    let log_interface = format!("knowell:plugin/log@{API_VERSION}");
    linker.instance(&log_interface)?.func_wrap(
        "log",
        |store, (level, message): (WireLogLevel, WasmStr)| {
            guest_log(store, level, &message);
            Ok(())
        },
    )?;

    let files_interface = format!("knowell:plugin/project-files@{API_VERSION}");
    linker
        .instance(&files_interface)?
        .func_wrap("read-file", |store, (path,): (WasmStr,)| {
            Ok((read_file(store, &path),))
        })?;
    Ok(linker)
}

fn guest_log(mut store: StoreContextMut<'_, HostState>, level: WireLogLevel, message: &WasmStr) {
    let state = store.data_mut();
    if state.log.remaining == 0 {
        state.log.dropped = state.log.dropped.saturating_add(1);
        return;
    }
    state.log.remaining -= 1;
    let max_bytes = state.log.max_bytes;
    let text = match message.to_str(store.as_context()) {
        Ok(text) => sanitize(&text, max_bytes),
        Err(_) => "<message is not valid text>".to_string(),
    };
    let plugin = &*store.data().plugin;
    const TARGET: &str = "knowell_plugin::guest";
    match level {
        WireLogLevel::Trace => tracing::trace!(target: TARGET, plugin, "{text}"),
        WireLogLevel::Debug => tracing::debug!(target: TARGET, plugin, "{text}"),
        WireLogLevel::Info => tracing::info!(target: TARGET, plugin, "{text}"),
        WireLogLevel::Warn => tracing::warn!(target: TARGET, plugin, "{text}"),
        // A plugin cannot raise host error-level events; those are reserved
        // for failures of the host itself.
        WireLogLevel::Error => {
            tracing::warn!(target: TARGET, plugin, guest_level = "error", "{text}");
        }
    }
}

fn read_file(
    mut store: StoreContextMut<'_, HostState>,
    path: &WasmStr,
) -> Result<String, WireReadError> {
    let requested = match path.to_str(store.as_context()) {
        Ok(text) if text.len() <= MAX_PATH_BYTES => text.into_owned(),
        _ => return Err(WireReadError::Denied),
    };
    let Some(files) = store.data_mut().files.as_mut() else {
        return Err(WireReadError::Denied);
    };
    if files.reads_left == 0 {
        return Err(WireReadError::BudgetExhausted);
    }
    files.reads_left -= 1;
    files
        .grant
        .read(&requested, files.max_bytes)
        .map_err(|failure| match failure {
            ReadFailure::Denied => WireReadError::Denied,
            ReadFailure::NotFound => WireReadError::NotFound,
            ReadFailure::TooLarge => WireReadError::TooLarge,
            ReadFailure::NotText => WireReadError::NotText,
            ReadFailure::Unavailable => WireReadError::Unavailable,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_sums_memories_and_tables() {
        let config = HostConfig {
            max_memory_bytes: 3 * 65536,
            max_table_elements: 10,
            ..HostConfig::default()
        };
        let mut limiter = Limiter::new(&config);
        assert!(limiter.memory_growing(0, 65536, None).unwrap());
        assert!(
            limiter.memory_growing(0, 2 * 65536, None).unwrap(),
            "second memory"
        );
        assert!(!limiter.memory_growing(2 * 65536, 3 * 65536, None).unwrap());
        assert!(limiter.memory_exceeded);
        assert!(limiter.table_growing(0, 10, None).unwrap());
        assert!(!limiter.table_exceeded);
        assert!(!limiter.table_growing(10, 11, None).unwrap());
        assert!(limiter.table_exceeded);
        assert!(
            !limiter.memory_growing(0, usize::MAX, None).unwrap(),
            "no overflow"
        );
    }
}
