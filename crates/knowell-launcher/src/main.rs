//! Stable entry point for an explicitly owned direct installation.
//!
//! Standard streams belong to the engine, including MCP. A private loopback
//! lifetime channel lets the engine stop if its supervising launcher disappears;
//! PostgreSQL is deliberately not part of a kill-all process tree.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use knowell_update::install::Install;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::process::Command;

fn main() -> ExitCode {
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "cannot start the Knowell launcher")
        .and_then(|runtime| runtime.block_on(run()));
    match result {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            let _ = writeln!(std::io::stderr(), "knowell launcher: {message}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<u8, &'static str> {
    let executable = std::env::current_exe().map_err(|_| "cannot identify this launcher")?;
    let root = executable
        .parent()
        .ok_or("launcher has no installation directory")?;
    let install = Install::open(root)
        .map_err(|_| "invalid installation receipt; run the verified repair installer")?;
    let _launcher = install
        .launcher_lease()
        .map_err(|_| "launcher replacement is in progress; retry after it finishes")?;
    if install
        .launcher_pending()
        .map_err(|_| "invalid launcher recovery record")?
    {
        return Err(
            "launcher replacement was interrupted; recover with the retained raw engine update --launcher",
        );
    }
    install.verify_launcher().map_err(
        |_| "launcher integrity verification failed; use the retained raw engine for repair",
    )?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let recovery = args.iter().any(|a| a == "doctor" || a == "maintain")
        || args.iter().any(|a| a == "update")
            && args.iter().any(|a| {
                a == "--recover"
                    || a == "--rollback"
                    || a == "--status"
                    || a == "--recover-metadata"
            });
    // Recovery remains explicit: a journal's known previous image is permitted
    // only to run the updater, never silently to serve ordinary requests.
    let image = if recovery {
        match install
            .transaction()
            .map_err(|_| "invalid update journal")?
        {
            Some(transaction) => transaction.before,
            None => install
                .current()
                .map_err(|_| "active image is missing; run the verified repair installer")?,
        }
    } else {
        if install
            .transaction()
            .map_err(|_| "invalid update journal")?
            .is_some_and(|tx| tx.phase != knowell_update::install::Phase::Prepared)
        {
            return Err(
                "an update was interrupted; inspect know update --status, then select --recover old or --recover new",
            );
        }
        install
            .current()
            .map_err(|_| "active image is missing; run the verified repair installer")?
    };
    let runtime = install
        .verify(&image)
        .map_err(|_| "engine integrity verification failed; run the verified repair installer")?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|_| "cannot create the launcher lifetime channel")?;
    let address = listener
        .local_addr()
        .map_err(|_| "cannot identify the launcher lifetime channel")?;
    let nonce = uuid::Uuid::now_v7().to_string();
    let mut child = Command::new(runtime)
        .args(args)
        .env("KNOWELL_PARENT_ENDPOINT", address.to_string())
        .env("KNOWELL_PARENT_TOKEN", &nonce)
        .env("KNOWELL_INSTALL_ROOT", install.root())
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "cannot start the installed engine")?;
    // The channel is not an externally exposed administrative endpoint. Invalid
    // connections are bounded and discarded without logging their contents.
    let connection = async {
        loop {
            let (mut stream, _) = listener
                .accept()
                .await
                .map_err(|_| "lifetime channel failed")?;
            let mut token = vec![0; nonce.len()];
            if matches!(
                tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut token)).await,
                Ok(Ok(_))
            ) && token == nonce.as_bytes()
            {
                return Ok::<_, &'static str>(stream);
            }
        }
    };
    let mut channel = None;
    tokio::pin!(connection);
    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status.map_err(|_| "cannot observe the engine exit")?;
                return Ok(status.code().and_then(|code| u8::try_from(code).ok()).unwrap_or(1));
            }
            accepted = &mut connection, if channel.is_none() => {
                channel = Some(accepted?);
            }
            _ = stop_signal() => {
                // SIGINT already reaches both processes in a terminal group.
                // For a directed SIGTERM, closing the channel asks the engine
                // to drain; its bounded watchdog also prevents an orphan.
                channel.take();
                match tokio::time::timeout(Duration::from_secs(15), child.wait()).await {
                    Ok(Ok(status)) => return Ok(status.code().and_then(|code| u8::try_from(code).ok()).unwrap_or(130)),
                    _ => {
                        let _ = child.kill().await;
                        return Ok(130);
                    }
                }
            }
        }
    }
}

async fn stop_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
