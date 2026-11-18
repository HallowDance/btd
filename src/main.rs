mod agent;
mod audio;
mod bluez;
mod daemon;
mod ipc;
mod menu;
mod notify;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use futures_util::{Stream, StreamExt, pin_mut};
use tracing_subscriber::{EnvFilter, prelude::*};
use zlink::tokio::unix;

use bluez::Device;
use ipc::{BtdError, BtdProxy, Event};
use notify::Notifier;

#[derive(Parser)]
#[command(version, about = "Bluetooth manager for BlueZ + PipeWire")]
struct Cli {
    /// Report progress and results as desktop notifications instead of
    /// printing them (for key bindings and the rofi menu).
    #[arg(long, global = true)]
    notify: bool,
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
    /// rofi script mode: rofi -show btd -modi "btd:btd menu" -show-icons
    Menu {
        /// The entry rofi passes back; btd uses $ROFI_INFO instead.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        _selection: Vec<String>,
    },
}

/// Where command output goes: the terminal, or one notification bubble.
struct Output(Option<Notifier>);

impl Output {
    async fn say(&mut self, message: &str) -> Result<()> {
        match &mut self.0 {
            Some(n) => n.show(message, false).await,
            None => {
                println!("{message}");
                Ok(())
            }
        }
    }

    async fn device(&mut self, d: &Device) {
        if self.0.is_none() {
            print_device(d);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if !cli.notify {
        return run(cli.command, &mut Output(None)).await;
    }
    let mut out = Output(Some(Notifier::new().await?));
    if let Err(e) = run(cli.command, &mut out).await {
        if let Some(n) = &mut out.0 {
            n.show(&format!("{e:#}"), true).await?;
        }
        std::process::exit(1);
    }
    Ok(())
}

async fn run(command: Command, out: &mut Output) -> Result<()> {
    match command {
        Command::Daemon => {
            init_logging();
            daemon::run().await
        }
        Command::Menu { .. } => menu::run().await,
        Command::List => {
            let list = client().await?.list().await?.map_err(fail)?;
            for d in &list.devices {
                out.device(d).await;
            }
            Ok(())
        }
        Command::Scan { seconds } => {
            out.say(&format!("scanning for {seconds}s…")).await?;
            let mut client = client().await?;
            print_events(client.scan(seconds).await?, out).await
        }
        Command::Pair { device } => {
            let mut client = client().await?;
            print_events(client.pair(device.as_deref()).await?, out).await
        }
        Command::Remove { device } => {
            let done = client().await?.remove(&device).await?.map_err(fail)?;
            out.say(&format!("removed {}", done.name)).await
        }
        Command::Connect { device } => {
            if out.0.is_some() {
                out.say(&format!("connecting {device}…")).await?;
            }
            let done = client().await?.connect(&device).await?.map_err(fail)?;
            out.say(&format!("connected {}", done.name)).await
        }
        Command::Disconnect { device } => {
            let done = client()
                .await?
                .disconnect(device.as_deref())
                .await?
                .map_err(fail)?;
            out.say(&format!("disconnected {}", done.name)).await
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

/// Show a streaming reply: devices as list lines, everything else as text.
async fn print_events(
    events: impl Stream<Item = zlink::Result<Result<Event, BtdError>>>,
    out: &mut Output,
) -> Result<()> {
    pin_mut!(events);
    while let Some(event) = events.next().await {
        let event = event?.map_err(fail)?;
        if let Some(d) = &event.device {
            out.device(d).await;
        }
        if let Some(message) = &event.message {
            out.say(message).await?;
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
