//! Explicit trusted updates for installer-owned software, separate from user data.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{ArgGroup, Args, ValueEnum};
use knowell_update::install::{Image, Install, Phase, UpdateSource};
use knowell_update::manifest::{Channel, ReleaseTarget, RuntimeState, Selection, select_release};
use knowell_update::repository::{RepositoryConfig, TrustedRepository};
use semver::Version;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use url::Url;

use crate::{db, env::Env, output::Output};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum Recovery {
    Old,
    New,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum UpdateChannel {
    Stable,
    Preview,
}

#[derive(Debug, Args)]
#[command(disable_version_flag = true, group(ArgGroup::new("action").args(["status", "plan", "prepare", "apply", "rollback", "recover", "recover_metadata", "configure_source", "abort", "launcher", "inspect_binary"])))]
pub(crate) struct UpdateArgs {
    /// Show installation ownership and the local transaction, without network access.
    #[arg(long)]
    status: bool,
    /// Print an exact signed release plan without downloading executables.
    #[arg(long)]
    plan: bool,
    /// Download and verify a release while current engines keep working.
    #[arg(long)]
    prepare: bool,
    /// Activate the prepared release after all engines have closed.
    #[arg(long)]
    apply: bool,
    /// Explicitly return to the previous compatible, non-revoked release.
    #[arg(long)]
    rollback: bool,
    /// Reconcile an interrupted update using an explicitly selected image.
    #[arg(long, value_enum)]
    recover: Option<Recovery>,
    /// Explicitly resume a valid interrupted trust-state generation; never resets trust history.
    #[arg(long)]
    recover_metadata: bool,
    /// Verify and persist an operator-provided public source for future update commands.
    #[arg(long, requires_all = ["trust_root", "metadata_url", "targets_url"])]
    configure_source: bool,
    /// Cancel a prepared update that has not entered maintenance.
    #[arg(long)]
    abort: bool,
    /// Replace/repair the launcher from the active signed release; run via its raw engine.
    #[arg(long)]
    launcher: bool,
    /// Exact release version; never substitutes another version if unavailable.
    #[arg(long)]
    version: Option<String>,
    /// Explicit release channel; previews require an existing stable 1.0 release.
    #[arg(long, value_enum, default_value = "stable")]
    channel: UpdateChannel,
    /// Authorize only an explicitly pinned downgrade; does not authorize data restoration.
    #[arg(long)]
    allow_downgrade: bool,
    /// Authorize the candidate's embedded migrations under exclusive database maintenance.
    #[arg(long)]
    allow_migration: bool,
    /// Fresh managed-database backup to create before a schema-changing update.
    #[arg(long, requires = "allow_migration")]
    backup: Option<PathBuf>,
    /// Operator attestation that an external database backup has been taken and restore tested.
    #[arg(long, requires = "allow_migration", conflicts_with = "backup")]
    external_backup_confirmed: bool,
    /// Attest that an external database uses direct/session connections and every data client participates in admission or is stopped.
    #[arg(long)]
    session_gates_confirmed: bool,
    /// Maximum wait for participating remote engines to disconnect, in seconds.
    #[arg(long, default_value = "15", value_parser = clap::value_parser!(u64).range(1..=600))]
    maintenance_timeout: u64,
    /// Explicit installer-owned software root, for repair from a verified raw engine.
    #[arg(long)]
    install_root: Option<PathBuf>,
    /// Public, independently trusted TUF root JSON. No production root is inferred.
    #[arg(long)]
    trust_root: Option<PathBuf>,
    /// TUF metadata base URL, using https or an explicit file:/// offline repository.
    #[arg(long)]
    metadata_url: Option<String>,
    /// Raw target base URL, using https or an explicit file:/// offline repository.
    #[arg(long)]
    targets_url: Option<String>,
    /// Explicit offline mode; both repository URLs must use file:///.
    #[arg(long)]
    offline: bool,
    #[arg(long, hide = true)]
    inspect_binary: bool,
}

/// Stateless candidate handshake; signed metadata must agree with the executable.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BinaryInfo {
    format_version: u32,
    version: String,
    target: String,
    schema: u32,
    config: u32,
    index: u32,
    jobs: u32,
    protocol: u32,
    launcher: u32,
}

impl BinaryInfo {
    fn current() -> anyhow::Result<Self> {
        Ok(Self {
            format_version: 1,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            target: env!("KNOWELL_BUILD_TARGET").to_owned(),
            schema: u32::try_from(knowell_store::Store::latest_schema().version)?,
            config: 1,
            index: 1,
            jobs: 1,
            protocol: 1,
            launcher: 1,
        })
    }

    fn state(&self, schema: u32, config: u32, launcher: u32) -> RuntimeState {
        // Release 1 declares the existing formats as 1. Changing these requires
        // persisted format inspection and an explicit converter, not inference
        // from what the candidate executable happens to support.
        RuntimeState {
            schema,
            config,
            index: 1,
            jobs: 1,
            protocol: 1,
            launcher,
        }
    }

    fn matches(&self, release: &ReleaseTarget) -> anyhow::Result<()> {
        if self.format_version != 1
            || self.version != release.metadata.version.to_string()
            || self.target != release.metadata.target
            || self.config != 1
            || self.index != 1
            || self.jobs != 1
            || self.protocol != 1
            || self.launcher != 1
        {
            bail!("candidate executable identity differs from its signed release manifest");
        }
        release.metadata.compatibility.check(self.state(
            self.schema,
            self.config,
            self.launcher,
        ))?;
        Ok(())
    }
}

pub(crate) fn run(args: UpdateArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    if args.inspect_binary {
        out.line(serde_json::to_string(&BinaryInfo::current()?)?)?;
        out.flush()?;
        return Ok(ExitCode::SUCCESS);
    }
    let install = match args.install_root.as_deref() {
        Some(root) => Some(Install::open(root)?),
        None => Install::for_executable(&std::env::current_exe()?)?,
    };
    let Some(install) = install else {
        let owner = match std::env::var("KNOWELL_INSTALL_OWNER").as_deref() {
            Ok("npm") => "npm",
            Ok("cargo") => "cargo",
            Ok("homebrew") => "Homebrew",
            Ok("scoop") => "Scoop",
            Ok("winget") => "winget",
            Ok("container") => "container image",
            _ => "original package manager or source build",
        };
        out.line(format!(
            "installation is managed by {owner}; update through that installation method"
        ))?;
        out.flush()?;
        return Ok(if args.status {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    };
    if args.status {
        out.line(format!(
            "owner: direct; platform: {}",
            install.receipt().target
        ))?;
        match install.current() {
            Ok(current) => out.line(format!("current: {}", current.version))?,
            Err(_) => out.line("current: unavailable; explicit recovery or repair required")?,
        }
        if let Some(tx) = install.transaction()? {
            out.line(format!(
                "transaction: {}; phase: {:?}; {} -> {}",
                tx.id, tx.phase, tx.before.version, tx.after.version
            ))?;
        }
        out.line(format!(
            "launcher repair pending: {}",
            install.launcher_pending()?
        ))?;
        out.flush()?;
        return Ok(ExitCode::SUCCESS);
    }
    if args.abort {
        let owner = install.update_lease()?;
        install.abort(&owner)?;
        out.line("prepared update cancelled; retained executables are preserved")?;
        out.flush()?;
        return Ok(ExitCode::SUCCESS);
    }
    db::runtime()?.block_on(run_async(&args, env, &install, out))?;
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

async fn source(args: &UpdateArgs, install: &Install) -> anyhow::Result<UpdateSource> {
    if args.trust_root.is_none() && args.metadata_url.is_none() && args.targets_url.is_none() {
        let source = install
            .update_source()?
            .ok_or(knowell_update::Error::TrustUnconfigured)?;
        if args.offline && !source.offline {
            bail!("offline mode differs from the provisioned source")
        }
        return Ok(source);
    }
    let Some(root) = &args.trust_root else {
        return Err(knowell_update::Error::TrustUnconfigured.into());
    };
    let root_file = tokio::fs::File::open(root).await?;
    if !root_file.metadata().await?.is_file() || root_file.metadata().await?.len() > 1024 * 1024 {
        bail!("trusted root must be a bounded public json file");
    }
    let mut root_bytes = Vec::new();
    root_file
        .take(1024 * 1024 + 1)
        .read_to_end(&mut root_bytes)
        .await?;
    if root_bytes.len() > 1024 * 1024 {
        bail!("trusted root exceeds its byte limit")
    }
    Ok(UpdateSource {
        format_version: 1,
        trusted_root: root_bytes,
        metadata_url: args
            .metadata_url
            .clone()
            .ok_or(knowell_update::Error::TrustUnconfigured)?,
        targets_url: args
            .targets_url
            .clone()
            .ok_or(knowell_update::Error::TrustUnconfigured)?,
        offline: args.offline,
    })
}

async fn repository(
    args: &UpdateArgs,
    install: &Install,
    source: &UpdateSource,
) -> anyhow::Result<TrustedRepository> {
    let parse_url = |value: &str| -> anyhow::Result<Url> {
        if value.len() > 2048 {
            return Err(knowell_update::Error::Source.into());
        }
        Url::parse(value).map_err(|_| knowell_update::Error::Source.into())
    };
    let config = RepositoryConfig {
        trusted_root: Some(source.trusted_root.clone()),
        metadata_url: parse_url(&source.metadata_url)?,
        targets_url: parse_url(&source.targets_url)?,
        datastore: install.root().join("metadata"),
        max_target_bytes: knowell_update::manifest::MAX_TARGET_BYTES,
        offline: source.offline,
        recover_pending: args.recover_metadata,
    };
    Ok(if args.configure_source {
        TrustedRepository::configure(config).await?
    } else {
        TrustedRepository::load(config).await?
    })
}

fn image(release: &ReleaseTarget) -> Image {
    Image {
        format_version: 1,
        version: release.metadata.version.to_string(),
        target: release.metadata.target.clone(),
        sha256: release.artifact.sha256.clone(),
        size: release.artifact.size,
    }
}

fn launcher_image(release: &ReleaseTarget) -> Image {
    Image {
        sha256: release.metadata.launcher.sha256.clone(),
        size: release.metadata.launcher.size,
        ..image(release)
    }
}

async fn run_async(
    args: &UpdateArgs,
    env: &Env,
    install: &Install,
    out: &mut Output,
) -> anyhow::Result<()> {
    let source_owner = if args.configure_source {
        Some(install.update_lease()?)
    } else {
        None
    };
    let source = source(args, install).await?;
    let repository = repository(args, install, &source).await?;
    if let Some(owner) = source_owner {
        install.provision_source(&source, &owner)?;
        out.line(
            "verified public update source provisioned; future updates may omit source options",
        )?;
        return Ok(());
    }
    if args.recover_metadata {
        out.line(format!(
            "trusted metadata recovery completed; {} verified release descriptors",
            repository.releases().len()
        ))?;
        return Ok(());
    }
    let releases = repository.releases();
    let transaction = install.transaction()?;
    let current = match install.current() {
        Ok(current) => current,
        Err(knowell_update::Error::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound && args.recover.is_some() =>
        {
            transaction
                .as_ref()
                .context("missing active image and update journal")?
                .before
                .clone()
        }
        Err(error) => return Err(error.into()),
    };
    if install.launcher_pending()? && !args.launcher {
        bail!(
            "launcher replacement is interrupted; repair with the verified raw engine update --launcher"
        )
    }
    let retained = if let Some(recover) = args.recover {
        let tx = transaction
            .as_ref()
            .context("no interrupted update to recover")?;
        Some(match recover {
            Recovery::Old => tx.before.clone(),
            Recovery::New => tx.after.clone(),
        })
    } else if args.rollback {
        Some(
            install
                .previous()?
                .context("no previous verified release is retained")?,
        )
    } else if args.launcher {
        Some(current.clone())
    } else if args.apply {
        Some(
            transaction
                .as_ref()
                .context("prepare an exact release before applying")?
                .after
                .clone(),
        )
    } else {
        None
    };
    let requested = args
        .version
        .as_deref()
        .map(Version::parse)
        .transpose()
        .map_err(|_| anyhow::anyhow!("invalid release version"))?;
    if retained
        .as_ref()
        .zip(requested.as_ref())
        .is_some_and(|(image, version)| image.version != version.to_string())
    {
        bail!("requested version differs from the durable update transaction");
    }
    let selection = match retained.as_ref() {
        Some(image) => Selection::Pinned(Version::parse(&image.version)?),
        None => requested
            .map(Selection::Pinned)
            .unwrap_or(Selection::Latest),
    };
    let channel = match &selection {
        Selection::Pinned(version)
            if !version.pre.is_empty()
                && (args.rollback || args.recover.is_some() || args.launcher || args.apply) =>
        {
            Channel::Preview
        }
        _ => match args.channel {
            UpdateChannel::Stable => Channel::Stable,
            UpdateChannel::Preview => Channel::Preview,
        },
    };
    let release = select_release(
        releases,
        &Version::parse(&current.version)?,
        &install.receipt().target,
        channel,
        &selection,
        args.allow_downgrade || args.rollback || args.recover.is_some(),
    )?;
    if retained
        .as_ref()
        .is_some_and(|retained| retained != &image(&release))
    {
        bail!("retained executable identity differs from current trusted metadata");
    }
    out.line(format!(
        "verified release: {}; platform: {}; bytes: {}",
        release.metadata.version, release.metadata.target, release.artifact.size
    ))?;
    if !args.prepare && !args.apply && !args.rollback && args.recover.is_none() && !args.launcher {
        out.line(format!(
            "schema read: {}..={}; write: {}..={}; launcher protocol: {}..={}",
            release.metadata.compatibility.schema.read_min,
            release.metadata.compatibility.schema.read_max,
            release.metadata.compatibility.schema.write_min,
            release.metadata.compatibility.schema.write_max,
            release.metadata.compatibility.launcher.min,
            release.metadata.compatibility.launcher.max
        ))?;
        out.line("use --prepare to download, then --apply after active engines have closed")?;
        return Ok(());
    }
    let owner = install.update_lease()?;
    if args.prepare {
        if release.metadata.version.to_string() == current.version {
            bail!("requested release is already active")
        }
        let new_image = image(&release);
        let launcher = launcher_image(&release);
        let (engine_path, engine_file) = install.stage(&new_image, &owner)?;
        let (launcher_path, launcher_file) = install.stage(&launcher, &owner)?;
        let result = async {
            let mut file = tokio::fs::File::from_std(engine_file);
            repository.download(&release.artifact, &mut file).await?;
            file.sync_all().await?;
            drop(file);
            let mut file = tokio::fs::File::from_std(launcher_file);
            repository
                .download(&release.metadata.launcher, &mut file)
                .await?;
            file.sync_all().await?;
            drop(file);
            let info = inspect(&engine_path).await?;
            info.matches(&release)?;
            install.retain_launcher(&launcher, &launcher_path, &owner)?;
            install.prepare(new_image, &engine_path, &owner)?;
            anyhow::Ok(())
        }
        .await;
        if result.is_err() {
            let _ = std::fs::remove_file(&engine_path);
            let _ = std::fs::remove_file(&launcher_path);
        }
        result?;
        out.line("release prepared; current engines may continue; apply explicitly when they have closed")?;
        return Ok(());
    }
    if args.launcher {
        if transaction.is_some() {
            bail!("recover or complete the software transaction before replacing its launcher")
        }
        let exclusive = install.exclusive_runtime(&owner)?;
        let launcher_gate = install.exclusive_launcher(&owner)?;
        let launcher = launcher_image(&release);
        let retained = install
            .executable(&launcher)?
            .parent()
            .context("missing launcher directory")?
            .join(if cfg!(windows) {
                "know-launcher.exe"
            } else {
                "know-launcher"
            });
        if !retained.exists() {
            let (path, file) = install.stage(&launcher, &owner)?;
            let mut file = tokio::fs::File::from_std(file);
            let downloaded = repository
                .download(&release.metadata.launcher, &mut file)
                .await;
            drop(file);
            if let Err(error) = downloaded {
                let _ = std::fs::remove_file(path);
                return Err(error.into());
            }
            install.retain_launcher(&launcher, &path, &owner)?;
        }
        install.replace_launcher(&launcher, &owner, &exclusive, &launcher_gate)?;
        out.line("verified launcher replacement completed")?;
        return Ok(());
    }
    let executable = install.verify(&image(&release))?;
    let info = inspect(&executable).await?;
    info.matches(&release)?;
    let this = BinaryInfo::current()?;
    if info.version != this.version || info.schema != this.schema || info.target != this.target {
        // The candidate's embedded migration definitions and format validators
        // must own activation. The old binary never invents the new schema.
        drop(owner);
        drop(repository);
        handoff(args, env, install, &executable, &release).await?;
        return Ok(());
    }
    let exclusive = install.exclusive_runtime(&owner)?;
    let tx = if args.rollback {
        install.prepare_retained(image(&release), &owner)?
    } else {
        install
            .transaction()?
            .context("no prepared or interrupted update")?
    };
    let selected = match args.recover {
        Some(Recovery::Old) => &tx.before,
        Some(Recovery::New) | None => &tx.after,
    };
    if *selected != image(&release) {
        bail!("update transaction changed after release planning; inspect it before retrying")
    }
    if args.recover.is_none() && tx.phase != Phase::Prepared {
        bail!("interrupted activation requires --recover old or --recover new")
    }
    let cfg = env.load_engine()?;
    let mut store = None;
    if let Some(cfg) = &cfg {
        db::validate_session_gates(cfg, args.session_gates_confirmed)?;
        let admin =
            db::connect_admin(env, cfg, Duration::from_secs(args.maintenance_timeout), 1).await?;
        let schema = u32::try_from(admin.inspect_schema().await?.version)?;
        let needs_migration = schema != info.schema;
        if args.recover.is_some() && needs_migration {
            bail!(
                "recovery image cannot read the current database schema; choose a compatible retained image"
            );
        }
        if needs_migration
            && (!args.allow_migration || args.backup.is_none() && !args.external_backup_confirmed)
        {
            bail!(
                "schema-changing update requires --allow-migration and a managed --backup file or --external-backup-confirmed"
            );
        }
        if needs_migration {
            db::validate_migration_backup(
                cfg,
                args.backup.as_deref(),
                args.external_backup_confirmed,
            )?;
        }
        if !needs_migration {
            release.metadata.compatibility.check(info.state(
                schema,
                cfg.version,
                install.receipt().launcher_protocol,
            ))?;
        }
        store = Some(admin);
    } else if env.home.join("data").exists() || env.home.join("pg").exists() {
        bail!(
            "persisted data exists without an engine configuration; restore its configuration before updating"
        );
    }
    if args.recover.is_none() {
        install.begin_activation(&owner, &exclusive)?;
    }
    if let Some(admin) = &store {
        let mut maintenance = admin.begin_maintenance(tx.id).await?;
        maintenance
            .acquire_exclusive(Duration::from_secs(args.maintenance_timeout))
            .await?;
        let schema = u32::try_from(maintenance.inspect_schema().await?.version)?;
        if schema != info.schema {
            if args.recover.is_some() || !args.allow_migration {
                bail!("database schema changed while acquiring maintenance; recovery is required")
            }
            let cfg = cfg.as_ref().context("missing maintenance configuration")?;
            db::migration_backup(
                env,
                cfg,
                args.backup.as_deref(),
                args.external_backup_confirmed,
            )
            .await?;
            maintenance.migrate().await?;
        }
        let schema = u32::try_from(maintenance.validate_schema().await?.version)?;
        release.metadata.compatibility.check(info.state(
            schema,
            cfg.as_ref().context("missing configuration")?.version,
            install.receipt().launcher_protocol,
        ))?;
        if let Some(recover) = args.recover {
            // No data is restored. The chosen binary has passed exact schema
            // validation before database admission is explicitly reopened.
            install.reconcile(matches!(recover, Recovery::New), &owner, &exclusive)?;
            maintenance.finish().await?;
            install.complete(&owner, &exclusive)?;
        } else {
            install.activate(&owner, &exclusive)?;
            maintenance.finish().await?;
            install.complete(&owner, &exclusive)?;
        }
    } else if let Some(recover) = args.recover {
        install.recover(matches!(recover, Recovery::New), &owner, &exclusive)?;
    } else {
        install.activate(&owner, &exclusive)?;
        install.complete(&owner, &exclusive)?;
    }
    if let Some(store) = store {
        store.close().await;
    }
    out.line(format!(
        "active release: {}; no database backup was restored",
        release.metadata.version
    ))?;
    out.line("launcher replacement, when needed: run the active raw engine with update --launcher and the same trust/source options")?;
    Ok(())
}

async fn inspect(path: &Path) -> anyhow::Result<BinaryInfo> {
    // Staging files are never exposed as a runtime; only this bounded stateless
    // handshake runs before preparation. The file has already passed TUF EOF checks.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o500))?;
    }
    let mut child = tokio::process::Command::new(path)
        .args(["update", "--inspect-binary"])
        .env_remove("KNOWELL_PARENT_ENDPOINT")
        .env_remove("KNOWELL_PARENT_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("cannot inspect the verified candidate executable")?;
    let mut stdout = child
        .stdout
        .take()
        .context("candidate handshake channel is unavailable")?;
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        (&mut stdout)
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > 64 * 1024 {
            bail!("candidate handshake exceeds its byte limit")
        }
        if !child.wait().await?.success() {
            bail!("candidate executable handshake failed")
        }
        anyhow::Ok(())
    })
    .await
    .context("candidate executable handshake timed out")??;
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("candidate executable returned an invalid handshake"))
}

async fn handoff(
    args: &UpdateArgs,
    env: &Env,
    install: &Install,
    executable: &Path,
    release: &ReleaseTarget,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let nonce = uuid::Uuid::now_v7().to_string();
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--config")
        .arg(&env.engine_config)
        .arg("update")
        .arg("--install-root")
        .arg(install.root());
    if let Some(recover) = args.recover {
        command.args([
            "--recover",
            match recover {
                Recovery::Old => "old",
                Recovery::New => "new",
            },
        ]);
    } else if args.rollback {
        command.arg("--rollback");
    } else {
        command.arg("--apply");
    }
    command
        .arg("--version")
        .arg(release.metadata.version.to_string());
    if args.allow_downgrade {
        command.arg("--allow-downgrade");
    }
    if args.allow_migration {
        command.arg("--allow-migration");
    }
    if args.external_backup_confirmed {
        command.arg("--external-backup-confirmed");
    }
    if args.session_gates_confirmed {
        command.arg("--session-gates-confirmed");
    }
    if args.offline {
        command.arg("--offline");
    }
    if let Some(backup) = &args.backup {
        command.arg("--backup").arg(backup);
    }
    command
        .arg("--maintenance-timeout")
        .arg(args.maintenance_timeout.to_string());
    if let Some(root) = &args.trust_root {
        command.arg("--trust-root").arg(root);
    }
    if let Some(url) = &args.metadata_url {
        command.arg("--metadata-url").arg(url);
    }
    if let Some(url) = &args.targets_url {
        command.arg("--targets-url").arg(url);
    }
    command
        .env(
            "KNOWELL_PARENT_ENDPOINT",
            listener.local_addr()?.to_string(),
        )
        .env("KNOWELL_PARENT_TOKEN", &nonce);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let connection = async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut token = vec![0; nonce.len()];
            if matches!(
                tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut token)).await,
                Ok(Ok(_))
            ) && token == nonce.as_bytes()
            {
                return anyhow::Ok(stream);
            }
        }
    };
    let channel = tokio::select! {
        status = child.wait() => {
            if status?.success() { return Ok(()) }
            bail!("candidate maintenance exited before its lifetime handshake");
        }
        channel = tokio::time::timeout(Duration::from_secs(15), connection) => channel.context("candidate maintenance lifetime handshake timed out")??,
        () = env.parent_shutdown.cancelled() => {
            child.kill().await?;
            bail!("candidate maintenance was cancelled by its supervisor");
        }
    };
    let status = tokio::select! {
        status = child.wait() => status?,
        () = env.parent_shutdown.cancelled() => {
            // Closing the chain asks the candidate to drain and preserves its
            // durable intent if bounded teardown interrupts a migration.
            drop(channel);
            match tokio::time::timeout(Duration::from_secs(15), child.wait()).await {
                Ok(status) => status?,
                Err(_) => { child.kill().await?; bail!("candidate maintenance cancellation requires explicit recovery") }
            }
        }
    };
    if !status.success() {
        bail!("candidate maintenance did not complete; inspect update --status before recovery")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn update_actions_are_exclusive_and_recovery_requires_a_decision() {
        assert!(crate::Cli::try_parse_from(["know", "update", "--prepare", "--apply"]).is_err());
        assert!(crate::Cli::try_parse_from(["know", "update", "--recover"]).is_err());
        assert!(crate::Cli::try_parse_from(["know", "update", "--recover", "old"]).is_ok());
        assert!(
            crate::Cli::try_parse_from(["know", "update", "--maintenance-timeout", "0"]).is_err()
        );
    }

    #[test]
    fn binary_handshake_rejects_truncated_and_unexpected_fields() {
        assert!(serde_json::from_slice::<BinaryInfo>(b"{\"format_version\":1").is_err());
        let mut value = serde_json::to_value(BinaryInfo::current().unwrap()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unknown".to_owned(), true.into());
        assert!(serde_json::from_value::<BinaryInfo>(value).is_err());
    }

    #[test]
    fn release_version_option_does_not_replace_root_version_flag() {
        let parsed = crate::Cli::try_parse_from(["know", "update", "--version", "1.2.3"]).unwrap();
        let crate::Command::Update(args) = parsed.command else {
            panic!("expected update command")
        };
        assert_eq!(args.version.as_deref(), Some("1.2.3"));
        let root = crate::Cli::try_parse_from(["know", "--version"]).unwrap_err();
        assert_eq!(root.kind(), clap::error::ErrorKind::DisplayVersion);
    }
}
