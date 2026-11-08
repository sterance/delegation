//! The headless client: run natively on Linux or in the Docker image.

use anyhow::{bail, Result};
use delegation_client::inference::Inference;
use delegation_client::sampler::Sampler;
use delegation_client::session::{self, ClientInfo, ServerIdentity, SessionCmd, SessionStatus};
use delegation_client::{hardware, identity, paths, settings};
use log::{error, info, warn};
use protocol::ClientSettings;
use std::time::Duration;
use tokio::sync::mpsc;

struct Args {
    server: String, // e.g. "http://localhost:8080" or "https://llm.smith-c.com"
    pairing_code: Option<String>,
    dump_specs: bool,
}

fn parse_args() -> Result<Args> {
    let mut server = None;
    let mut pairing_code = None;
    let mut dump_specs = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" => server = args.next(),
            "--pairing-code" => pairing_code = args.next(),
            "--dump-specs" => dump_specs = true,
            other => bail!("unknown argument: {other}"),
        }
    }
    Ok(Args {
        server: server.unwrap_or_else(|| "http://localhost:8080".to_string()),
        pairing_code,
        dump_specs,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    if args.dump_specs {
        // Prints what this machine would report, for checking collectors.
        println!("{}", serde_json::to_string_pretty(&hardware::collect())?);
        return Ok(());
    }
    delegation_client::log::init_stdout();

    // Settings come from the server (`welcome`), not a local config file.
    // The timeouts it last sent are cached for the next run's first connect.
    let settings_path = settings::cli_path();
    let mut cached = settings::load_cli();
    match &cached {
        Some(_) => info!(target: "config", "using the timeouts the server last sent ({})", settings_path.display()),
        None => info!(target: "config", "no cached server timeouts yet, using defaults until connected"),
    }
    let identity = identity::load_or_enroll(&args.server, args.pairing_code.as_deref())?;
    let info = ClientInfo {
        machine: hardware::spawn_collect(),
        sampler: Sampler::spawn(ClientSettings::default().sampler_interval()),
        // Models persist across reconnects, and across runs on disk.
        inference: Inference::new(&paths::data_dir()),
    };

    let (cmd_tx, cmd_rx) = mpsc::channel(4);
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let controller = tokio::spawn(session::controller(
        cmd_rx,
        move |_url: &str, status| {
            let _ = status_tx.send(status);
        },
        info,
    ));
    cmd_tx
        .send(SessionCmd::Connect(ServerIdentity {
            url: args.server,
            identity,
            settings: cached.unwrap_or_default().settings(),
        }))
        .await?;

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let status = tokio::select! {
            status = status_rx.recv() => match status {
                Some(status) => status,
                None => break,
            },
            // Ctrl-C, or SIGTERM from `docker stop`: close the connection
            // and stop llama-server before exiting.
            _ = &mut shutdown => {
                info!(target: "client", "shutting down");
                let _ = cmd_tx.send(SessionCmd::Shutdown).await;
                if tokio::time::timeout(Duration::from_secs(10), controller).await.is_err() {
                    warn!(target: "client", "didn't stop cleanly in time");
                }
                // Not a plain return: the runtime would wait for a download's
                // blocking read to notice its cancel, up to a read timeout.
                std::process::exit(0);
            }
        };
        if let SessionStatus::Connected(pushed) = status {
            let pushed = settings::CachedTimeouts::from(&pushed);
            if cached != Some(pushed) {
                match settings::save(&settings_path, &pushed) {
                    Ok(()) => cached = Some(pushed),
                    Err(e) => warn!(target: "config", "couldn't cache server timeouts: {e:#}"),
                }
            }
        } else if status == SessionStatus::UnknownClient {
            // Most likely the server's database was reset, or this identity
            // was enrolled against a different server instance. Retrying can
            // never succeed, so wipe the stale identity and tell the human
            // what to do rather than spinning on AuthFailed forever.
            identity::clear();
            error!(
                target: "identity",
                "server doesn't recognize this client_id — identity cleared. Re-run with --pairing-code to re-enroll."
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
