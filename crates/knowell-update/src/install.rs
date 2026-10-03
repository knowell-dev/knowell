//! Owned installation records, runtime admission and recoverable activation.
//!
//! Locks belong to the canonical installation, independent of KNOWELL_HOME.
//! A pending transaction never expires or silently selects another engine.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

/// Current installation/launcher protocol, starting with the first stable release.
pub const FORMAT_VERSION: u32 = 1;
/// Maximum size of a local JSON control record, in bytes.
const MAX_RECORD_BYTES: u64 = 64 * 1024;
const MAX_SOURCE_BYTES: u64 = 5 * 1024 * 1024;

/// Explicitly provisioned public update source; contains no credentials or signing keys.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSource {
    /// Source record format, currently 1.
    pub format_version: u32,
    /// Exact original public trust-anchor bytes, preserving its datastore pin.
    pub trusted_root: Vec<u8>,
    /// Credential-free metadata base URL.
    pub metadata_url: String,
    /// Credential-free target base URL.
    pub targets_url: String,
    /// Explicit file-repository opt-in.
    pub offline: bool,
}

impl std::fmt::Debug for UpdateSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpdateSource")
            .field("format_version", &self.format_version)
            .field("trust_configured", &!self.trusted_root.is_empty())
            .field("offline", &self.offline)
            .finish_non_exhaustive()
    }
}

impl UpdateSource {
    /// Checks public-source bounds and rejects credential-bearing URLs before persistence.
    /// Signature verification remains mandatory before provisioning this source.
    pub fn validate(&self) -> Result<()> {
        if self.format_version != FORMAT_VERSION
            || self.trusted_root.is_empty()
            || self.trusted_root.len() > 1024 * 1024
        {
            return Err(Error::Source);
        }
        for value in [&self.metadata_url, &self.targets_url] {
            if value.len() > 2048 {
                return Err(Error::Source);
            }
            let url = url::Url::parse(value).map_err(|_| Error::Source)?;
            if url.scheme() != if self.offline { "file" } else { "https" }
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || !url.path().ends_with('/')
                || (self.offline && url.host_str().is_some())
                || (!self.offline && url.host_str().is_none())
            {
                return Err(Error::Source);
            }
        }
        Ok(())
    }
}

/// The owner of the software files; this is never inferred from writability.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    /// The official direct installer owns this installation.
    Direct,
    /// Cargo owns the executable.
    Cargo,
    /// npm owns the pinned wrapper and cache.
    Npm,
    /// Homebrew owns the executable.
    Homebrew,
    /// Scoop owns the executable.
    Scoop,
    /// Windows Package Manager owns the executable.
    Winget,
    /// A native operating-system package owns the executable.
    System,
    /// The deployment owns a container image.
    Container,
    /// A source checkout or unknown installation owns the executable.
    Unmanaged,
}

/// Receipt installed beside a direct installation's stable command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    /// Receipt format, currently 1.
    pub format_version: u32,
    /// Software file owner; only direct installations can activate natively.
    pub owner: Owner,
    /// Exact Rust target triple.
    pub target: String,
    /// Supported launcher protocol, currently 1.
    pub launcher_protocol: u32,
}

/// One verified engine in its immutable version directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Image {
    /// Image record format, currently 1.
    pub format_version: u32,
    /// Exact stable or explicitly selected preview semantic version.
    pub version: String,
    /// Exact Rust target triple.
    pub target: String,
    /// Lowercase hexadecimal SHA-256 of the complete executable.
    pub sha256: String,
    /// Executable length in bytes.
    pub size: u64,
}

impl Image {
    /// Validates every value before it is used to construct a path.
    pub fn validate(&self) -> Result<()> {
        if self.format_version != FORMAT_VERSION {
            return Err(Error::State("unsupported installed image record"));
        }
        let version =
            Version::parse(&self.version).map_err(|_| Error::State("invalid installed version"))?;
        if self.version.len() > 128
            || version.major == 0
            || !version.build.is_empty()
            || version.to_string() != self.version
        {
            return Err(Error::State("installed version is not a released version"));
        }
        validate_target(&self.target)?;
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.size == 0
            || self.size > crate::manifest::MAX_TARGET_BYTES
        {
            return Err(Error::Integrity);
        }
        Ok(())
    }
}

/// Durable transaction phases; the journal, not a timeout, decides recovery.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Files are verified; the serving image is still the old image.
    Prepared,
    /// Admission is closed while the pointer is being replaced.
    Activating,
    /// The new image is selected; commit/recovery must reconcile the records.
    Activated,
    /// Explicit recovery selected the previous image; database finish is still pending.
    RecoveredOld,
    /// Explicit recovery selected the new image; database finish is still pending.
    RecoveredNew,
}

/// A local activation intent, containing no credentials or workspace data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transaction {
    /// Journal format, currently 1.
    pub format_version: u32,
    /// Unique transaction identifier; never a PID or stale heartbeat.
    pub id: uuid::Uuid,
    /// State of the activation.
    pub phase: Phase,
    /// Previously active verified image.
    pub before: Image,
    /// New verified image.
    pub after: Image,
}

/// A canonical direct installation.
#[derive(Debug, Clone)]
pub struct Install {
    root: PathBuf,
    receipt: Receipt,
}

/// Shared operating-system admission lease, held until all engine work ends.
#[derive(Debug)]
pub struct RuntimeLease {
    _file: File,
}

/// Exclusive updater owner. Dropping this does not delete a transaction.
#[derive(Debug)]
pub struct UpdateLease {
    _file: File,
    root: PathBuf,
}

/// Exclusive runtime admission while activation or recovery takes place.
#[derive(Debug)]
pub struct ExclusiveRuntime {
    _file: File,
    root: PathBuf,
}

/// Launcher replacement admission, independent of engine admission.
#[derive(Debug)]
pub struct LauncherLease {
    _file: File,
}

/// Exclusive launcher replacement; callers may obtain it only from a raw engine.
#[derive(Debug)]
pub struct ExclusiveLauncher {
    _file: File,
    root: PathBuf,
}

impl Install {
    /// Opens an existing receipt without creating or adopting an installation.
    pub fn open(root: &Path) -> Result<Self> {
        reject_link_ancestors(root)?;
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(Error::Ownership);
        }
        let receipt: Receipt = read_record(&root.join("install.json"))?;
        if receipt.format_version != FORMAT_VERSION
            || receipt.launcher_protocol != FORMAT_VERSION
            || receipt.owner != Owner::Direct
        {
            return Err(Error::Ownership);
        }
        validate_target(&receipt.target)?;
        Ok(Self { root, receipt })
    }

    /// Resolves a runtime's installation from its immutable executable path.
    ///
    /// An unrelated native/package executable returns None. A matching layout
    /// with a malformed receipt is an error, never an ownership guess.
    pub fn for_executable(executable: &Path) -> Result<Option<Self>> {
        let executable = executable.canonicalize()?;
        let Some(target) = executable.parent() else {
            return Ok(None);
        };
        let Some(version) = target.parent() else {
            return Ok(None);
        };
        let Some(versions) = version.parent() else {
            return Ok(None);
        };
        if versions.file_name().is_none_or(|name| name != "versions") {
            return Ok(None);
        }
        let Some(root) = versions.parent() else {
            return Ok(None);
        };
        match fs::symlink_metadata(root.join("install.json")) {
            Ok(_) => Self::open(root).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Canonical software root; callers must not place user data beneath it.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Recorded installation target.
    pub fn receipt(&self) -> &Receipt {
        &self.receipt
    }

    /// Loads only an explicitly provisioned public source; absence is unconfigured trust.
    pub fn update_source(&self) -> Result<Option<UpdateSource>> {
        let path = self.root.join("update-source.json");
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        reject_link(&path)?;
        let file = File::open(path)?;
        if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_SOURCE_BYTES {
            return Err(Error::State("invalid provisioned update source"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_SOURCE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SOURCE_BYTES {
            return Err(Error::Limit);
        }
        let source: UpdateSource = serde_json::from_slice(&bytes)
            .map_err(|_| Error::State("invalid provisioned update source"))?;
        source
            .validate()
            .map_err(|_| Error::State("invalid provisioned update source"))?;
        Ok(Some(source))
    }

    /// Saves a source only after the caller has successfully verified its repository.
    pub fn provision_source(&self, source: &UpdateSource, owner: &UpdateLease) -> Result<()> {
        self.require_owner(owner)?;
        source.validate()?;
        write_record(&self.root, "update-source.json", source)
    }

    /// Records the installed launcher's independent executable identity.
    pub fn launcher(&self) -> Result<Image> {
        let image: Image = read_record(&self.root.join("launcher.json"))?;
        image.validate()?;
        if image.target != self.receipt.target {
            return Err(Error::Ownership);
        }
        Ok(image)
    }

    /// Verifies the stable entry point against the installation receipt.
    pub fn verify_launcher(&self) -> Result<()> {
        let image = self.launcher()?;
        verify_file(
            &self
                .root
                .join(if cfg!(windows) { "know.exe" } else { "know" }),
            image.size,
            &image.sha256,
        )
    }

    /// Holds launcher's own admission for its complete child lifetime.
    pub fn launcher_lease(&self) -> Result<LauncherLease> {
        let file = lock_file(&self.root.join("launcher.lock"))?;
        try_shared(&file)?;
        Ok(LauncherLease { _file: file })
    }

    /// Launcher replacement requires every launcher to exit; no running file is overwritten.
    pub fn exclusive_launcher(&self, owner: &UpdateLease) -> Result<ExclusiveLauncher> {
        self.require_owner(owner)?;
        let file = lock_file(&self.root.join("launcher.lock"))?;
        try_exclusive(&file)?;
        Ok(ExclusiveLauncher {
            _file: file,
            root: self.root.clone(),
        })
    }

    /// Reads the exact selected image; this does not choose a fallback.
    pub fn current(&self) -> Result<Image> {
        let image: Image = read_record(&self.root.join("current.json"))?;
        image.validate()?;
        if image.target != self.receipt.target {
            return Err(Error::Ownership);
        }
        Ok(image)
    }

    /// Reads the last committed image available for explicit compatible rollback.
    pub fn previous(&self) -> Result<Option<Image>> {
        let image: Option<Image> = read_optional(&self.root.join("previous.json"))?;
        if let Some(image) = &image {
            image.validate()?;
            if image.target != self.receipt.target {
                return Err(Error::Ownership);
            }
        }
        Ok(image)
    }

    /// Reads and validates the outstanding activation, if present.
    pub fn transaction(&self) -> Result<Option<Transaction>> {
        let tx: Option<Transaction> = read_optional(&self.root.join("transaction.json"))?;
        if let Some(tx) = &tx {
            if tx.format_version != FORMAT_VERSION
                || tx.id.is_nil()
                || tx.before.target != self.receipt.target
                || tx.after.target != self.receipt.target
            {
                return Err(Error::State("invalid update journal"));
            }
            tx.before.validate()?;
            tx.after.validate()?;
        }
        Ok(tx)
    }

    /// Constructs an immutable executable path only from validated identities.
    pub fn executable(&self, image: &Image) -> Result<PathBuf> {
        image.validate()?;
        if image.target != self.receipt.target {
            return Err(Error::Ownership);
        }
        let dir = self
            .root
            .join("versions")
            .join(&image.version)
            .join(&image.target);
        validate_existing_components(&self.root, &dir)?;
        Ok(dir.join(if image.target.contains("-windows-") {
            "know.exe"
        } else {
            "know"
        }))
    }

    /// Validates a complete retained executable before selecting or executing it.
    pub fn verify(&self, image: &Image) -> Result<PathBuf> {
        let path = self.executable(image)?;
        reject_link(&path)?;
        verify_file(&path, image.size, &image.sha256)?;
        Ok(path)
    }

    /// Acquires admission immediately; maintenance never blocks a new MCP launch indefinitely.
    pub fn runtime_lease(&self) -> Result<RuntimeLease> {
        if self.launcher_pending()? {
            return Err(Error::RecoveryRequired);
        }
        if self
            .transaction()?
            .is_some_and(|tx| tx.phase != Phase::Prepared)
        {
            return Err(Error::RecoveryRequired);
        }
        let file = lock_file(&self.root.join("runtime.lock"))?;
        try_shared(&file)?;
        if self.launcher_pending()? {
            return Err(Error::RecoveryRequired);
        }
        if self
            .transaction()?
            .is_some_and(|tx| tx.phase != Phase::Prepared)
        {
            return Err(Error::RecoveryRequired);
        }
        Ok(RuntimeLease { _file: file })
    }

    /// Ensures a direct version-dir invocation cannot bypass active-image admission.
    pub fn admit_executable(&self, executable: &Path) -> Result<RuntimeLease> {
        let lease = self.runtime_lease()?;
        let expected = self.verify(&self.current()?)?.canonicalize()?;
        if expected != executable.canonicalize()? {
            return Err(Error::State(
                "this engine is not the active installed version",
            ));
        }
        Ok(lease)
    }

    /// Claims the single updater without waiting while retaining an executable mapping.
    pub fn update_lease(&self) -> Result<UpdateLease> {
        let file = lock_file(&self.root.join("update.lock"))?;
        try_exclusive(&file)?;
        Ok(UpdateLease {
            _file: file,
            root: self.root.clone(),
        })
    }

    /// Attempts complete local quiescence; active engines yield Busy without being killed.
    pub fn exclusive_runtime(&self, owner: &UpdateLease) -> Result<ExclusiveRuntime> {
        self.require_owner(owner)?;
        let file = lock_file(&self.root.join("runtime.lock"))?;
        try_exclusive(&file)?;
        Ok(ExclusiveRuntime {
            _file: file,
            root: self.root.clone(),
        })
    }

    /// Creates a new private executable staging file without overwriting a retained image.
    pub fn stage(&self, image: &Image, owner: &UpdateLease) -> Result<(PathBuf, File)> {
        self.require_owner(owner)?;
        if self.transaction()?.is_some() {
            return Err(Error::RecoveryRequired);
        }
        let path = self.executable(image)?;
        let parent = path
            .parent()
            .ok_or(Error::State("missing staging directory"))?;
        create_private_dirs(&self.root, parent)?;
        let staged = parent.join(format!(".download-{}", uuid::Uuid::now_v7()));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        restrict_file(&file)?;
        Ok((staged, file))
    }

    /// Retains a completely verified file and records a prepared, non-serving update.
    pub fn prepare(&self, image: Image, staged: &Path, owner: &UpdateLease) -> Result<Transaction> {
        self.require_owner(owner)?;
        if self.transaction()?.is_some() {
            return Err(Error::RecoveryRequired);
        }
        let final_path = self.executable(&image)?;
        if staged.parent() != final_path.parent() {
            return Err(Error::Ownership);
        }
        reject_link(staged)?;
        verify_file(staged, image.size, &image.sha256)?;
        sync_file(staged)?;
        executable_permissions(staged)?;
        match fs::symlink_metadata(&final_path) {
            Ok(_) => {
                self.verify(&image)?;
                fs::remove_file(staged)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::rename(staged, &final_path)?;
            }
            Err(error) => return Err(error.into()),
        }
        sync_dir(
            final_path
                .parent()
                .ok_or(Error::State("missing image directory"))?,
        )?;
        let tx = Transaction {
            format_version: FORMAT_VERSION,
            id: uuid::Uuid::now_v7(),
            phase: Phase::Prepared,
            before: self.current()?,
            after: image,
        };
        write_record(&self.root, "transaction.json", &tx)?;
        Ok(tx)
    }

    /// Closes ordinary admission durably before database maintenance begins.
    pub fn begin_activation(
        &self,
        owner: &UpdateLease,
        exclusive: &ExclusiveRuntime,
    ) -> Result<Transaction> {
        self.require_exclusive(owner, exclusive)?;
        let mut tx = self
            .transaction()?
            .ok_or(Error::State("no prepared update"))?;
        if tx.phase != Phase::Prepared || self.current()? != tx.before {
            return Err(Error::RecoveryRequired);
        }
        self.verify(&tx.before)?;
        self.verify(&tx.after)?;
        tx.phase = Phase::Activating;
        write_record(&self.root, "transaction.json", &tx)?;
        Ok(tx)
    }

    /// Prepares an already retained and reverified image for explicit rollback.
    pub fn prepare_retained(&self, image: Image, owner: &UpdateLease) -> Result<Transaction> {
        self.require_owner(owner)?;
        if self.transaction()?.is_some() {
            return Err(Error::RecoveryRequired);
        }
        self.verify(&image)?;
        let tx = Transaction {
            format_version: FORMAT_VERSION,
            id: uuid::Uuid::now_v7(),
            phase: Phase::Prepared,
            before: self.current()?,
            after: image,
        };
        write_record(&self.root, "transaction.json", &tx)?;
        Ok(tx)
    }

    /// Cancels an unstarted prepared update; retained executables are preserved.
    pub fn abort(&self, owner: &UpdateLease) -> Result<()> {
        self.require_owner(owner)?;
        let tx = self
            .transaction()?
            .ok_or(Error::State("no prepared update"))?;
        if tx.phase != Phase::Prepared {
            return Err(Error::RecoveryRequired);
        }
        fs::remove_file(self.root.join("transaction.json"))?;
        sync_dir(&self.root)
    }

    /// Activates only under complete local admission exclusion.
    ///
    /// The caller must independently prove database/format compatibility before
    /// calling this operation. It never migrates or restores user data.
    pub fn activate(&self, owner: &UpdateLease, exclusive: &ExclusiveRuntime) -> Result<Image> {
        self.require_exclusive(owner, exclusive)?;
        let mut tx = self
            .transaction()?
            .ok_or(Error::State("no prepared update"))?;
        if !matches!(tx.phase, Phase::Prepared | Phase::Activating) || self.current()? != tx.before
        {
            return Err(Error::RecoveryRequired);
        }
        self.verify(&tx.before)?;
        self.verify(&tx.after)?;
        tx.phase = Phase::Activating;
        write_record(&self.root, "transaction.json", &tx)?;
        write_record(&self.root, "previous.json", &tx.before)?;
        write_record(&self.root, "current.json", &tx.after)?;
        tx.phase = Phase::Activated;
        write_record(&self.root, "transaction.json", &tx)?;
        Ok(tx.after)
    }

    /// Clears a committed intent only after database maintenance was explicitly finished.
    pub fn complete(&self, owner: &UpdateLease, exclusive: &ExclusiveRuntime) -> Result<()> {
        self.require_exclusive(owner, exclusive)?;
        let tx = self
            .transaction()?
            .ok_or(Error::State("no committed update"))?;
        let selected = match tx.phase {
            Phase::Activated | Phase::RecoveredNew => &tx.after,
            Phase::RecoveredOld => &tx.before,
            _ => return Err(Error::RecoveryRequired),
        };
        if self.current()? != *selected {
            return Err(Error::RecoveryRequired);
        }
        self.verify(selected)?;
        fs::remove_file(self.root.join("transaction.json"))?;
        sync_dir(&self.root)
    }

    /// Reconciles a journal with an explicit caller-selected compatibility decision.
    ///
    /// Recovery never restores a database or silently chooses an unrelated image.
    pub fn recover(
        &self,
        commit_new: bool,
        owner: &UpdateLease,
        exclusive: &ExclusiveRuntime,
    ) -> Result<Image> {
        let selected = self.reconcile(commit_new, owner, exclusive)?;
        self.complete(owner, exclusive)?;
        Ok(selected)
    }

    /// Reconciles pointers while retaining durable exclusion until database finish.
    pub fn reconcile(
        &self,
        commit_new: bool,
        owner: &UpdateLease,
        exclusive: &ExclusiveRuntime,
    ) -> Result<Image> {
        self.require_exclusive(owner, exclusive)?;
        let mut tx = self
            .transaction()?
            .ok_or(Error::State("no interrupted update"))?;
        let selected = if commit_new { &tx.after } else { &tx.before };
        self.verify(selected)?;
        let current: Option<Image> = read_optional(&self.root.join("current.json"))?;
        if current
            .as_ref()
            .is_some_and(|current| current != &tx.before && current != &tx.after)
        {
            return Err(Error::State(
                "active image does not match the update journal",
            ));
        }
        if commit_new {
            write_record(&self.root, "previous.json", &tx.before)?
        }
        write_record(&self.root, "current.json", selected)?;
        let selected = selected.clone();
        tx.phase = if commit_new {
            Phase::RecoveredNew
        } else {
            Phase::RecoveredOld
        };
        write_record(&self.root, "transaction.json", &tx)?;
        Ok(selected)
    }

    /// Retains a verified paired launcher in the immutable release directory.
    pub fn retain_launcher(&self, image: &Image, staged: &Path, owner: &UpdateLease) -> Result<()> {
        self.require_owner(owner)?;
        let dir = self
            .executable(image)?
            .parent()
            .ok_or(Error::Ownership)?
            .to_path_buf();
        if staged.parent() != Some(dir.as_path()) {
            return Err(Error::Ownership);
        }
        verify_file(staged, image.size, &image.sha256)?;
        sync_file(staged)?;
        executable_permissions(staged)?;
        let destination = dir.join(if cfg!(windows) {
            "know-launcher.exe"
        } else {
            "know-launcher"
        });
        reject_if_present(&destination)?;
        if destination.exists() {
            verify_file(&destination, image.size, &image.sha256)?;
            fs::remove_file(staged)?;
        } else {
            fs::rename(staged, destination)?;
        }
        sync_dir(&dir)
    }

    /// Replaces the launcher only after explicit exclusive launcher admission.
    ///
    /// A durable intent allows the same operation to be retried after a crash.
    /// Ordinary launches fail closed while that intent exists.
    pub fn replace_launcher(
        &self,
        image: &Image,
        owner: &UpdateLease,
        runtime: &ExclusiveRuntime,
        launcher: &ExclusiveLauncher,
    ) -> Result<()> {
        self.require_exclusive(owner, runtime)?;
        if launcher.root != self.root {
            return Err(Error::Ownership);
        }
        let pending: Option<Image> = read_optional(&self.root.join("launcher-pending.json"))?;
        if pending.as_ref().is_some_and(|pending| pending != image) {
            return Err(Error::RecoveryRequired);
        }
        let engine = self.current()?;
        if engine.version != image.version || engine.target != image.target {
            return Err(Error::Compatibility(
                "launcher must match the active verified release",
            ));
        }
        let dir = self
            .executable(image)?
            .parent()
            .ok_or(Error::Ownership)?
            .to_path_buf();
        let source = dir.join(if cfg!(windows) {
            "know-launcher.exe"
        } else {
            "know-launcher"
        });
        verify_file(&source, image.size, &image.sha256)?;
        write_record(&self.root, "launcher-pending.json", image)?;
        let staged = self
            .root
            .join(format!(".launcher-{}", uuid::Uuid::now_v7()));
        fs::copy(&source, &staged)?;
        sync_file(&staged)?;
        executable_permissions(&staged)?;
        fs::rename(
            &staged,
            self.root
                .join(if cfg!(windows) { "know.exe" } else { "know" }),
        )?;
        write_record(&self.root, "launcher.json", image)?;
        fs::remove_file(self.root.join("launcher-pending.json"))?;
        sync_dir(&self.root)
    }

    /// Pending launcher replacement must be recovered from a verified raw engine.
    pub fn launcher_pending(&self) -> Result<bool> {
        let pending: Option<Image> = read_optional(&self.root.join("launcher-pending.json"))?;
        if let Some(image) = &pending {
            image.validate()?;
            if image.target != self.receipt.target {
                return Err(Error::Ownership);
            }
        }
        Ok(pending.is_some())
    }

    fn require_owner(&self, owner: &UpdateLease) -> Result<()> {
        if owner.root != self.root {
            return Err(Error::Ownership);
        }
        Ok(())
    }

    fn require_exclusive(&self, owner: &UpdateLease, runtime: &ExclusiveRuntime) -> Result<()> {
        self.require_owner(owner)?;
        if runtime.root != self.root {
            return Err(Error::Ownership);
        }
        Ok(())
    }
}

/// Computes and checks a bounded executable's complete SHA-256 and byte length.
pub fn verify_file(path: &Path, length: u64, expected: &str) -> Result<()> {
    reject_link(path)?;
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() != length {
        return Err(Error::Integrity);
    }
    let mut digest = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    let mut remaining = length;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        remaining = remaining.checked_sub(n as u64).ok_or(Error::Integrity)?;
        digest.update(buf.get(..n).ok_or(Error::Integrity)?);
    }
    let actual: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if remaining != 0 || actual != expected {
        return Err(Error::Integrity);
    }
    Ok(())
}

fn validate_target(target: &str) -> Result<()> {
    if target.len() > 128
        || target.is_empty()
        || !target
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        || target.split('-').count() < 3
    {
        return Err(Error::State("invalid installation target"));
    }
    Ok(())
}

fn read_record<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    reject_link(path)?;
    let file = File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_RECORD_BYTES {
        return Err(Error::State("invalid update control record"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(Error::State("update control record is too large"));
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::State("invalid update control record"))
}

fn read_optional<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::symlink_metadata(path) {
        Ok(_) => read_record(path).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_record<T: Serialize>(root: &Path, name: &str, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| Error::State("cannot encode update control record"))?;
    let staged = root.join(format!(".record-{}", uuid::Uuid::now_v7()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)?;
    restrict_file(&file)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    let destination = root.join(name);
    reject_if_present(&destination)?;
    fs::rename(&staged, &destination)?;
    sync_dir(root)?;
    Ok(())
}

fn lock_file(path: &Path) -> Result<File> {
    reject_if_present(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    restrict_file(&file)?;
    if !file.metadata()?.is_file() {
        return Err(Error::Ownership);
    }
    Ok(file)
}

fn try_shared(file: &File) -> Result<()> {
    file.try_lock_shared().map_err(map_lock)
}

fn try_exclusive(file: &File) -> Result<()> {
    file.try_lock().map_err(map_lock)
}

fn map_lock(error: std::fs::TryLockError) -> Error {
    match error {
        std::fs::TryLockError::WouldBlock => Error::Busy,
        std::fs::TryLockError::Error(error) => error.into(),
    }
}

fn reject_link(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(Error::Ownership);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(Error::Ownership);
        }
    }
    Ok(())
}

fn reject_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => reject_link(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn reject_link_ancestors(path: &Path) -> Result<()> {
    let absolute = std::path::absolute(path)?;
    for ancestor in absolute.ancestors() {
        reject_link(ancestor)?;
    }
    Ok(())
}

fn validate_existing_components(root: &Path, target: &Path) -> Result<()> {
    let relative = target.strip_prefix(root).map_err(|_| Error::Ownership)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(Error::Ownership);
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                reject_link(&current)?;
                if !metadata.is_dir() {
                    return Err(Error::Ownership);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn create_private_dirs(root: &Path, target: &Path) -> Result<()> {
    validate_existing_components(root, target)?;
    let relative = target.strip_prefix(root).map_err(|_| Error::Ownership)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => restrict_directory(&current)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                reject_link(&current)?
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn restrict_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

fn restrict_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn executable_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o500))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_file(path: &Path) -> Result<()> {
    // Windows FlushFileBuffers requires write access. Unix permits fsync on a
    // read descriptor, including the immutable executable after its handshake.
    #[cfg(windows)]
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    #[cfg(not(windows))]
    let file = File::open(path)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(version: &str, bytes: &[u8]) -> Image {
        Image {
            format_version: 1,
            version: version.to_owned(),
            target: "x86_64-pc-windows-msvc".to_owned(),
            sha256: Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            size: bytes.len() as u64,
        }
    }

    fn installation() -> (tempfile::TempDir, Install, Image) {
        let root = tempfile::tempdir().unwrap();
        let before = image("1.0.0", b"synthetic old executable");
        let receipt = Receipt {
            format_version: 1,
            owner: Owner::Direct,
            target: before.target.clone(),
            launcher_protocol: 1,
        };
        write_record(root.path(), "install.json", &receipt).unwrap();
        write_record(root.path(), "current.json", &before).unwrap();
        let install = Install::open(&root.path().canonicalize().unwrap()).unwrap();
        let path = install.executable(&before).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"synthetic old executable").unwrap();
        (root, install, before)
    }

    fn prepare(install: &Install, owner: &UpdateLease) -> Image {
        let after = image("1.1.0", b"synthetic new executable");
        let (staged, mut file) = install.stage(&after, owner).unwrap();
        file.write_all(b"synthetic new executable").unwrap();
        drop(file);
        install.prepare(after.clone(), &staged, owner).unwrap();
        after
    }

    #[test]
    fn live_engines_exclude_activation_but_allow_prepare() {
        let (_root, install, before) = installation();
        let runtime = install.runtime_lease().unwrap();
        let owner = install.update_lease().unwrap();
        assert!(matches!(install.update_lease(), Err(Error::Busy)));
        let after = prepare(&install, &owner);
        assert_eq!(install.current().unwrap(), before);
        assert!(install.runtime_lease().is_ok());
        assert!(matches!(
            install.exclusive_runtime(&owner),
            Err(Error::Busy)
        ));
        drop(runtime);
        let exclusive = install.exclusive_runtime(&owner).unwrap();
        assert!(matches!(install.runtime_lease(), Err(Error::Busy)));
        install.begin_activation(&owner, &exclusive).unwrap();
        assert!(matches!(
            install.runtime_lease(),
            Err(Error::RecoveryRequired)
        ));
        assert_eq!(install.activate(&owner, &exclusive).unwrap(), after);
        assert_eq!(
            install.transaction().unwrap().unwrap().phase,
            Phase::Activated
        );
        drop(exclusive);
        assert!(matches!(
            install.runtime_lease(),
            Err(Error::RecoveryRequired)
        ));
        let exclusive = install.exclusive_runtime(&owner).unwrap();
        install.complete(&owner, &exclusive).unwrap();
        assert_eq!(install.previous().unwrap(), Some(before));
        drop(exclusive);
        assert!(install.runtime_lease().is_ok());
    }

    #[test]
    fn explicit_recovery_accepts_only_journal_images_and_handles_missing_pointer() {
        let (root, install, before) = installation();
        let owner = install.update_lease().unwrap();
        let after = prepare(&install, &owner);
        let exclusive = install.exclusive_runtime(&owner).unwrap();
        install.begin_activation(&owner, &exclusive).unwrap();
        fs::remove_file(root.path().join("current.json")).unwrap();
        assert_eq!(install.recover(false, &owner, &exclusive).unwrap(), before);
        install.prepare_retained(after.clone(), &owner).unwrap();
        install.begin_activation(&owner, &exclusive).unwrap();
        let unrelated = image("2.0.0", b"unrelated");
        write_record(root.path(), "current.json", &unrelated).unwrap();
        assert!(matches!(
            install.recover(true, &owner, &exclusive),
            Err(Error::State(_))
        ));
        write_record(root.path(), "current.json", &before).unwrap();
        assert_eq!(install.recover(true, &owner, &exclusive).unwrap(), after);
        assert!(install.transaction().unwrap().is_none());
    }

    #[test]
    fn corruption_never_selects_a_fallback_or_overwrites_a_retained_image() {
        let (root, install, before) = installation();
        let owner = install.update_lease().unwrap();
        let after = prepare(&install, &owner);
        install.abort(&owner).unwrap();
        fs::write(root.path().join("current.json"), b"{\"format_version\":1").unwrap();
        assert!(matches!(install.current(), Err(Error::State(_))));
        write_record(root.path(), "current.json", &before).unwrap();
        let other = image("1.1.0", b"different same version image");
        let (path, mut file) = install.stage(&other, &owner).unwrap();
        file.write_all(b"different same version image").unwrap();
        drop(file);
        assert!(matches!(
            install.prepare(other, &path, &owner),
            Err(Error::Integrity)
        ));
        assert!(install.verify(&after).is_ok());
        assert_eq!(install.current().unwrap(), before);
    }

    #[test]
    fn paths_records_digests_and_ownership_are_strict() {
        let (root, install, before) = installation();
        let owner = install.update_lease().unwrap();
        for target in ["../../escape", "C:\\escape", "UPPER-pc-windows", ""] {
            let mut hostile = before.clone();
            hostile.target = target.to_owned();
            assert!(install.executable(&hostile).is_err());
        }
        for version in ["../1.0.0", "0.0.0", "1.0.0+build", "01.0.0"] {
            let mut hostile = before.clone();
            hostile.version = version.to_owned();
            assert!(install.executable(&hostile).is_err());
        }
        let (_other_root, other, _) = installation();
        assert!(matches!(
            other.stage(&before, &owner),
            Err(Error::Ownership)
        ));
        assert!(
            verify_file(
                &install.verify(&before).unwrap(),
                before.size + 1,
                &before.sha256
            )
            .is_err()
        );
        write_record(
            root.path(),
            "current.json",
            &serde_json::json!({"format_version":1,"unknown":true}),
        )
        .unwrap();
        assert!(install.current().is_err());
        fs::write(
            root.path().join("current.json"),
            vec![b' '; MAX_RECORD_BYTES as usize + 1],
        )
        .unwrap();
        assert!(install.current().is_err());
    }

    #[test]
    fn provisioned_source_preserves_the_anchor_and_rejects_sensitive_or_hostile_input() {
        let (root, install, _) = installation();
        let owner = install.update_lease().unwrap();
        assert!(install.update_source().unwrap().is_none());
        let mut source = UpdateSource {
            format_version: 1,
            trusted_root: b"synthetic public trust-anchor bytes".to_vec(),
            metadata_url: "https://updates.example.invalid/metadata/".to_owned(),
            targets_url: "https://releases.example.invalid/download/".to_owned(),
            offline: false,
        };
        for hostile in [
            "https://fake-user:FAKE_PASSWORD@example.invalid/",
            "https://example.invalid/?token=FAKE_TOKEN",
            "https://example.invalid/#FAKE_FRAGMENT",
            "http://example.invalid/",
            "https://example.invalid/missing-slash",
        ] {
            source.metadata_url = hostile.to_owned();
            assert!(matches!(
                install.provision_source(&source, &owner),
                Err(Error::Source)
            ));
            assert!(!root.path().join("update-source.json").exists());
            assert!(!format!("{source:?}").contains(hostile));
        }
        source.metadata_url = "https://updates.example.invalid/metadata/".to_owned();
        install.provision_source(&source, &owner).unwrap();
        let stored = install.update_source().unwrap().unwrap();
        assert_eq!(stored.trusted_root, source.trusted_root);
        assert_eq!(stored.metadata_url, source.metadata_url);
        fs::write(
            root.path().join("update-source.json"),
            b"{\"format_version\":1",
        )
        .unwrap();
        assert!(install.update_source().is_err());
        let mut hostile = serde_json::to_value(&source).unwrap();
        hostile
            .as_object_mut()
            .unwrap()
            .insert("unknown".to_owned(), true.into());
        write_record(root.path(), "update-source.json", &hostile).unwrap();
        assert!(install.update_source().is_err());
    }

    #[test]
    fn recovery_keeps_admission_closed_until_database_finish_is_committed() {
        let (_root, install, _) = installation();
        let owner = install.update_lease().unwrap();
        let after = prepare(&install, &owner);
        let exclusive = install.exclusive_runtime(&owner).unwrap();
        install.begin_activation(&owner, &exclusive).unwrap();
        install.reconcile(true, &owner, &exclusive).unwrap();
        assert_eq!(install.current().unwrap(), after);
        assert_eq!(
            install.transaction().unwrap().unwrap().phase,
            Phase::RecoveredNew
        );
        drop(exclusive);
        assert!(matches!(
            install.runtime_lease(),
            Err(Error::RecoveryRequired)
        ));
        let exclusive = install.exclusive_runtime(&owner).unwrap();
        install.complete(&owner, &exclusive).unwrap();
        drop(exclusive);
        assert!(install.runtime_lease().is_ok());
    }

    #[test]
    fn launcher_replacement_is_excluded_while_a_launcher_runs() {
        let (_root, install, before) = installation();
        let owner = install.update_lease().unwrap();
        let runtime = install.exclusive_runtime(&owner).unwrap();
        let live = install.launcher_lease().unwrap();
        assert!(matches!(
            install.exclusive_launcher(&owner),
            Err(Error::Busy)
        ));
        drop(live);
        let launcher = install.exclusive_launcher(&owner).unwrap();
        let launcher_image = image(&before.version, b"synthetic launcher");
        let (path, mut file) = install.stage(&launcher_image, &owner).unwrap();
        file.write_all(b"synthetic launcher").unwrap();
        drop(file);
        install
            .retain_launcher(&launcher_image, &path, &owner)
            .unwrap();
        install
            .replace_launcher(&launcher_image, &owner, &runtime, &launcher)
            .unwrap();
        assert_eq!(install.launcher().unwrap(), launcher_image);
        assert!(!install.launcher_pending().unwrap());
        install.verify_launcher().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn linked_control_files_and_version_directories_are_rejected() {
        use std::os::unix::fs::symlink;
        let (root, install, _) = installation();
        fs::remove_file(root.path().join("current.json")).unwrap();
        symlink("install.json", root.path().join("current.json")).unwrap();
        assert!(matches!(install.current(), Err(Error::Ownership)));
        let hostile = image("1.2.0", b"synthetic");
        symlink(root.path(), root.path().join("versions/1.2.0")).unwrap();
        assert!(matches!(
            install.executable(&hostile),
            Err(Error::Ownership)
        ));
        let parent = tempfile::tempdir().unwrap();
        symlink(root.path(), parent.path().join("linked-install")).unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        assert!(matches!(
            Install::open(&parent.path().join("linked-install/nested")),
            Err(Error::Ownership)
        ));
    }
}
