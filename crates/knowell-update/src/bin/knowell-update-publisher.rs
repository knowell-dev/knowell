//! Offline TUF role signing; public root ceremony and network deployment are separate.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, error::ErrorKind};
use knowell_update::publisher::{PublisherConfig, publish};

#[derive(Parser)]
#[command(
    version,
    about = "Verify raw Knowell assets and sign fresh TUF metadata entirely offline"
)]
struct Args {
    /// Signed public root with operator-provisioned role thresholds.
    #[arg(long)]
    root: PathBuf,
    /// Cumulative unsigned-update-targets.json reviewed by the release operator.
    #[arg(long)]
    targets: PathBuf,
    /// Offline authorized private key file; repeat to satisfy role thresholds.
    #[arg(long = "key", required = true)]
    keys: Vec<PathBuf>,
    /// Positive monotonically advanced targets/snapshot/timestamp sequence.
    #[arg(long)]
    sequence: u64,
    /// Future RFC3339 UTC targets expiration, at most 365 days from now.
    #[arg(long)]
    targets_expires: String,
    /// Future RFC3339 UTC snapshot expiration, at most 30 days from now.
    #[arg(long)]
    snapshot_expires: String,
    /// Future RFC3339 UTC timestamp expiration, at most 7 days from now.
    #[arg(long)]
    timestamp_expires: String,
    /// All current and retained digest-prefixed raw assets in one local directory.
    #[arg(long)]
    assets: PathBuf,
    /// Fresh metadata output directory; its parent must exist.
    #[arg(long)]
    out: PathBuf,
}

fn main() -> ExitCode {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(_) => {
            let _ = writeln!(std::io::stderr(), "invalid publisher arguments; use --help");
            return ExitCode::from(2);
        }
    };
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(knowell_update::Error::Io)
        .and_then(|runtime| {
            runtime.block_on(publish(PublisherConfig {
                root: args.root,
                targets: args.targets,
                keys: args.keys,
                sequence: args.sequence,
                targets_expires: args.targets_expires,
                snapshot_expires: args.snapshot_expires,
                timestamp_expires: args.timestamp_expires,
                assets: args.assets,
                out: args.out,
            }))
        });
    match result {
        Ok(()) => {
            let _ = writeln!(
                std::io::stdout(),
                "verified offline metadata published; no signing keys or verification cache were included"
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "offline publication failed: {error}");
            ExitCode::from(2)
        }
    }
}
