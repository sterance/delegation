//! The headless client: run natively on Linux or in the Docker image.

use anyhow::{bail, Result};
use delegation_client::session::{self, ClientInfo, ServerIdentity, SessionCmd, SessionStatus, Timings};
use delegation_client::{hardware, identity, paths};
use log::{error, info};
use protocol::config::Config;
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

    // Natively the working directory is usually wherever the user ran it
    // from (or $HOME under systemd), so also look next to the identity file.
    let config = Config::load(&["config.env".into(), paths::config_dir().join("config.env")]);
    let timings = Timings::from_config(&config);
    match config.source() {
        Some(path) => info!(target: "config", "loaded {}", path.display()),
        None => info!(target: "config", "no config file, using environment and defaults"),
    }
    info!(
        target: "config",
        "heartbeat every {:?}, ack timeout {:?}, reconnect backoff {:?}..{:?}",
        timings.heartbeat_interval,
        timings.ack_timeout,
        timings.reconnect_initial_backoff,
        timings.reconnect_max_backoff
    );
    let identity = identity::load_or_enroll(&args.server, args.pairing_code.as_deref())?;
    let info = ClientInfo {
        machine: hardware::spawn_collect(),
    };

    let (cmd_tx, cmd_rx) = mpsc::channel(4);
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    tokio::spawn(session::controller(
        cmd_rx,
        move |_url: &str, status| {
            let _ = status_tx.send(status);
        },
        timings,
        info,
    ));
    cmd_tx
        .send(SessionCmd::Connect(ServerIdentity {
            url: args.server,
            identity,
        }))
        .await?;

    while let Some(status) = status_rx.recv().await {
        if status == SessionStatus::UnknownClient {
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
