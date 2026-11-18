mod agent;
mod audio;
mod bluez;
mod daemon;
mod ipc;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use futures_util::{Stream, StreamExt, pin_mut};
use tracing_subscriber::{EnvFilter, prelude::*};
use zlink::tokio::unix;

use bluez::Device;
use ipc::{BtdError, BtdProxy, Event};

#[derive(Parser)]
#[command(version, about = "Bluetooth manager for BlueZ + PipeWire")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the background daemon (reconnects, audio switching, pairing).
    Daemon,
    /// List known devices: paired first, then ones seen by the last scan.
    List,
    /// Look for nearby devices and show them as they are found.
    Scan {
        /// How long to scan.
        #[arg(short, long, default_value_t = 15)]
        seconds: u32,
    },
    /// Pair, trust and connect a device. Without one, pairs the single new
    /// headset that is in pairing mode.
    Pair { device: Option<String> },
    /// Unpair a device and forget it.
    Remove { device: String },
    /// Connect a device by number from `btd list`, name or address.
    Connect { device: String },
    /// Disconnect a device by number, name or address; defaults to the only
    /// connected device.
    Disconnect { device: Option<String> },
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
            list.devices.iter().for_each(print_device);
            Ok(())
        }
        Command::Scan { seconds } => {
            println!("scanning for {seconds}s…");
            let mut client = client().await?;
            print_events(client.scan(seconds).await?).await
        }
        Command::Pair { device } => {
            let mut client = client().await?;
            print_events(client.pair(device.as_deref()).await?).await
        }
        Command::Remove { device } => {
            let done = client().await?.remove(&device).await?.map_err(fail)?;
            println!("removed {}", done.name);
            Ok(())
        }
        Command::Connect { device } => {
            let done = client().await?.connect(&device).await?.map_err(fail)?;
            println!("connected {}", done.name);
            Ok(())
        }
        Command::Disconnect { device } => {
            let done = client()
                .await?
                .disconnect(device.as_deref())
                .await?
                .map_err(fail)?;
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

fn fail(e: BtdError) -> anyhow::Error {
    match e {
        BtdError::Failed { message } => anyhow!(message),
    }
}

fn print_device(d: &Device) {
    let state = if d.connected {
        "connected"
    } else if d.paired {
        "paired"
    } else {
        "seen"
    };
    let kind = if d.audio { "audio" } else { "" };
    println!(
        "{:>2}  {}  {:<10} {:<5}  {}",
        d.number, d.address, state, kind, d.name
    );
}

/// Print a streaming reply: devices as list lines, everything else as text.
async fn print_events(
    events: impl Stream<Item = zlink::Result<Result<Event, BtdError>>>,
) -> Result<()> {
    pin_mut!(events);
    while let Some(event) = events.next().await {
        let event = event?.map_err(fail)?;
        if let Some(d) = &event.device {
            print_device(d);
        }
        if let Some(message) = &event.message {
            println!("{message}");
        }
    }
    Ok(())
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
