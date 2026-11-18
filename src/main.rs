mod audio;
mod bluez;
mod daemon;
mod ipc;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, prelude::*};
use zlink::tokio::unix;

use ipc::BtdProxy;

#[derive(Parser)]
#[command(version, about = "Bluetooth manager for BlueZ + PipeWire")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the background daemon (reconnects, audio switching).
    Daemon,
    /// List known devices.
    List,
    /// Connect a device by address or name.
    Connect { device: String },
    /// Disconnect a device by address or name.
    Disconnect { device: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon => {
            init_logging();
            daemon::run().await
        }
        Command::List => {
            let list = client().await?.list().await?.map_err(fail)?;
            for d in list.devices {
                let state = if d.connected { "connected" } else if d.paired { "paired" } else { "seen" };
                let kind = if d.audio { "audio" } else { "" };
                println!("{}  {:<10} {:<5}  {}", d.address, state, kind, d.name);
            }
            Ok(())
        }
        Command::Connect { device } => {
            let done = client().await?.connect(&device).await?.map_err(fail)?;
            println!("connected {}", done.name);
            Ok(())
        }
        Command::Disconnect { device } => {
            let done = client().await?.disconnect(&device).await?.map_err(fail)?;
            println!("disconnected {}", done.name);
            Ok(())
        }
    }
}

async fn client() -> Result<impl BtdProxy> {
    let path = ipc::socket_path();
    unix::connect(&path)
        .await
        .with_context(|| format!("cannot reach btd daemon at {}", path.display()))
}

fn fail(e: ipc::BtdError) -> anyhow::Error {
    match e {
        ipc::BtdError::Failed { message } => anyhow!(message),
    }
}

/// Log to the journal when run by systemd, to stderr otherwise.
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);
    if std::env::var_os("JOURNAL_STREAM").is_some()
        && let Ok(journald) = tracing_journald::layer()
    {
        registry.with(journald).init();
    } else {
        registry.with(tracing_subscriber::fmt::layer()).init();
    }
}
