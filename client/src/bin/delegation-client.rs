//! The headless client: run natively on Linux or in the Docker image.

use anyhow::{bail, Result};
use delegation_client::sampler::Sampler;
use delegation_client::session::{self, ClientInfo, ServerIdentity, SessionCmd, SessionStatus};
use delegation_client::{hardware, identity, settings};
use log::{error, info, warn};
use protocol::ClientSettings;
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

    // Timings come from the server (`welcome`), not a local config file.
    // The last set it sent is cached for the reconnect backoff.
    let settings_path = settings::cli_path();
    let mut cached = settings::load(&settings_path);
    match &cached {
        Some(_) => info!(target: "config", "using the settings the server last sent ({})", settings_path.display()),
        None => info!(target: "config", "no cached server settings yet, using defaults until connected"),
    }
    let identity = identity::load_or_enroll(&args.server, args.pairing_code.as_deref())?;
    let info = ClientInfo {
        machine: hardware::spawn_collect(),
        sampler: Sampler::spawn(ClientSettings::default().sampler_interval()),
    };

    let (cmd_tx, cmd_rx) = mpsc::channel(4);
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    tokio::spawn(session::controller(
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
            settings: cached.unwrap_or_default(),
        }))
        .await?;

    while let Some(status) = status_rx.recv().await {
        if let SessionStatus::Connected(pushed) = status {
            if cached != Some(pushed) {
                match settings::save(&settings_path, &pushed) {
                    Ok(()) => cached = Some(pushed),
                    Err(e) => warn!(target: "config", "couldn't cache server settings: {e:#}"),
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
