//! Resource limits and settings of the plugin host.

use std::path::PathBuf;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::error::PluginError;
use crate::manifest::API_VERSION;

/// Granularity of the wall-clock limit: the epoch thread ticks this often.
pub(crate) const EPOCH_TICK: Duration = Duration::from_millis(10);

/// Largest linear memory a 32-bit WebAssembly instance can address.
const MAX_WASM32_MEMORY: u64 = 4 * 1024 * 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// Settings of a [`PluginHost`](crate::PluginHost). Every limit applies to one
/// call (`metadata` at load time, or one `analyze`), each of which runs in a
/// fresh instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    /// Fuel units per call. One unit is roughly one WebAssembly instruction;
    /// instantiation and copying values across the boundary consume fuel too.
    pub fuel_per_call: u64,
    /// Linear memory limit in bytes, summed over all memories of an instance.
    pub max_memory_bytes: u64,
    /// Table limit in elements, summed over all tables of an instance.
    pub max_table_elements: u64,
    /// Wall-clock limit per call (instantiation included), enforced by epoch
    /// interruption with a granularity of 10 ms.
    pub timeout: Duration,
    /// Output limit in bytes: the sum of all returned strings plus 32 bytes
    /// per contract or edge. Wasmtime refuses to copy more than twice this
    /// (at least 1 MiB) out of a plugin in one transfer, so the host never
    /// allocates much more than the limit; the exact limit is checked on the
    /// copied output.
    pub max_output_bytes: u64,
    /// Largest file text passed to a plugin, in bytes.
    pub max_input_bytes: u64,
    /// Largest component binary accepted, in bytes.
    pub max_component_bytes: u64,
    /// Log messages accepted per call; the rest are dropped and counted.
    pub max_log_messages: u32,
    /// Longest log message kept, in bytes; longer ones are truncated.
    pub max_log_message_bytes: u32,
    /// Largest file a plugin may read through `project-files`, in bytes.
    pub max_project_file_bytes: u64,
    /// `project-files` reads allowed per call.
    pub max_project_file_reads: u32,
    /// Absolute directory for the on-disk cache of compiled components, or
    /// `None` to compile every plugin on load. The host uses a subdirectory
    /// named after a fingerprint of this configuration and the host version,
    /// and Wasmtime adds its own version and compiler settings to every key.
    /// Anyone who can write here can run native code in the host process:
    /// use a directory only the current user can write.
    pub cache_dir: Option<PathBuf>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            fuel_per_call: 1_000_000_000,
            max_memory_bytes: 128 * MIB,
            max_table_elements: 100_000,
            timeout: Duration::from_secs(5),
            max_output_bytes: 4 * MIB,
            max_input_bytes: 4 * MIB,
            max_component_bytes: 32 * MIB,
            max_log_messages: 64,
            max_log_message_bytes: 1024,
            max_project_file_bytes: MIB,
            max_project_file_reads: 32,
            cache_dir: None,
        }
    }
}

impl HostConfig {
    /// Checks that every limit is non-zero and within a sane range.
    pub fn validate(&self) -> Result<(), PluginError> {
        fn check(ok: bool, message: &str) -> Result<(), PluginError> {
            if ok {
                Ok(())
            } else {
                Err(PluginError::InvalidConfig(message.to_string()))
            }
        }
        check(self.fuel_per_call > 0, "fuel_per_call must be positive")?;
        check(
            (64 * 1024..=MAX_WASM32_MEMORY).contains(&self.max_memory_bytes),
            "max_memory_bytes must be between 64 KiB (one page) and 4 GiB",
        )?;
        check(
            (1..=u64::from(u32::MAX)).contains(&self.max_table_elements),
            "max_table_elements must be between 1 and 2^32-1",
        )?;
        check(
            self.timeout >= Duration::from_millis(1) && self.timeout <= Duration::from_secs(3600),
            "timeout must be between 1 ms and 1 hour",
        )?;
        check(
            (1..=GIB).contains(&self.max_output_bytes),
            "max_output_bytes must be between 1 byte and 1 GiB",
        )?;
        check(
            (1..=GIB).contains(&self.max_input_bytes),
            "max_input_bytes must be between 1 byte and 1 GiB",
        )?;
        check(
            (1..=GIB).contains(&self.max_component_bytes),
            "max_component_bytes must be between 1 byte and 1 GiB",
        )?;
        check(
            self.max_log_message_bytes > 0,
            "max_log_message_bytes must be positive",
        )?;
        check(
            (1..=GIB).contains(&self.max_project_file_bytes),
            "max_project_file_bytes must be between 1 byte and 1 GiB",
        )?;
        if let Some(dir) = &self.cache_dir {
            check(dir.is_absolute(), "cache_dir must be an absolute path")?;
        }
        Ok(())
    }

    /// Number of epoch ticks a call may run before it is interrupted. One
    /// extra tick covers the partially elapsed tick at the start of the call.
    pub(crate) fn epoch_ticks(&self) -> u64 {
        let tick = EPOCH_TICK.as_nanos().max(1);
        let ticks = self.timeout.as_nanos().div_ceil(tick);
        u64::try_from(ticks).unwrap_or(u64::MAX).saturating_add(1)
    }

    /// Timeout in whole milliseconds, for error messages.
    pub(crate) fn timeout_ms(&self) -> u64 {
        u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX)
    }

    /// Stable identifier of this configuration and host version. Compiled
    /// components are cached under it, so any change to the configuration or
    /// to the host invalidates the cache (Wasmtime additionally keys entries
    /// by its own version and compiler settings).
    pub(crate) fn fingerprint(&self) -> String {
        let canonical = format!(
            "knowell-plugin-cache/v1\n\
             host={host}\napi={API_VERSION}\nwasmtime=49\ntarget={arch}-{os}\n\
             fuel={fuel}\nmemory={memory}\ntable={table}\ntimeout_ns={timeout}\n\
             output={output}\ninput={input}\ncomponent={component}\n\
             log={log_count}x{log_bytes}\nfiles={file_reads}x{file_bytes}\n",
            host = env!("CARGO_PKG_VERSION"),
            arch = std::env::consts::ARCH,
            os = std::env::consts::OS,
            fuel = self.fuel_per_call,
            memory = self.max_memory_bytes,
            table = self.max_table_elements,
            timeout = self.timeout.as_nanos(),
            output = self.max_output_bytes,
            input = self.max_input_bytes,
            component = self.max_component_bytes,
            log_count = self.max_log_messages,
            log_bytes = self.max_log_message_bytes,
            file_reads = self.max_project_file_reads,
            file_bytes = self.max_project_file_bytes,
        );
        let digest = Sha256::digest(canonical.as_bytes());
        digest
            .iter()
            .take(16)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        HostConfig::default().validate().unwrap();
    }

    #[test]
    fn rejects_out_of_range_limits() {
        let base = HostConfig::default();
        let cases = [
            HostConfig {
                fuel_per_call: 0,
                ..base.clone()
            },
            HostConfig {
                max_memory_bytes: 1024,
                ..base.clone()
            },
            HostConfig {
                max_memory_bytes: MAX_WASM32_MEMORY + 1,
                ..base.clone()
            },
            HostConfig {
                max_table_elements: 0,
                ..base.clone()
            },
            HostConfig {
                timeout: Duration::ZERO,
                ..base.clone()
            },
            HostConfig {
                timeout: Duration::from_secs(7200),
                ..base.clone()
            },
            HostConfig {
                max_output_bytes: 0,
                ..base.clone()
            },
            HostConfig {
                max_input_bytes: 2 * GIB,
                ..base.clone()
            },
            HostConfig {
                max_component_bytes: 0,
                ..base.clone()
            },
            HostConfig {
                max_log_message_bytes: 0,
                ..base.clone()
            },
            HostConfig {
                max_project_file_bytes: 0,
                ..base.clone()
            },
            HostConfig {
                cache_dir: Some(PathBuf::from("relative/cache")),
                ..base.clone()
            },
        ];
        for case in cases {
            assert!(
                matches!(case.validate(), Err(PluginError::InvalidConfig(_))),
                "{case:?}"
            );
        }
    }

    #[test]
    fn epoch_ticks_round_up() {
        let mut config = HostConfig {
            timeout: Duration::from_millis(1),
            ..HostConfig::default()
        };
        assert_eq!(config.epoch_ticks(), 2);
        config.timeout = Duration::from_millis(100);
        assert_eq!(config.epoch_ticks(), 11);
        config.timeout = Duration::from_millis(101);
        assert_eq!(config.epoch_ticks(), 12);
        assert_eq!(config.timeout_ms(), 101);
    }

    #[test]
    fn fingerprint_tracks_every_limit_but_not_the_cache_dir() {
        let base = HostConfig::default();
        let fp = base.fingerprint();
        assert_eq!(fp.len(), 32);
        assert_eq!(fp, HostConfig::default().fingerprint(), "deterministic");
        let moved = HostConfig {
            cache_dir: Some(PathBuf::from("/elsewhere")),
            ..base.clone()
        };
        assert_eq!(moved.fingerprint(), fp);
        let changed = [
            HostConfig {
                fuel_per_call: 7,
                ..base.clone()
            },
            HostConfig {
                max_memory_bytes: 64 * MIB,
                ..base.clone()
            },
            HostConfig {
                timeout: Duration::from_secs(9),
                ..base.clone()
            },
            HostConfig {
                max_output_bytes: 1,
                ..base.clone()
            },
            HostConfig {
                max_project_file_reads: 1,
                ..base.clone()
            },
        ];
        for config in changed {
            assert_ne!(config.fingerprint(), fp, "{config:?}");
        }
    }
}
