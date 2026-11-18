//! `btd daemon`: auto-reconnect, audio switching and the Varlink service.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tracing::{info, warn};
use zbus::Connection;
use zlink::{Server, Service, interface, tokio::unix};

use crate::{
    audio, bluez,
    ipc::{self, BtdError, DeviceList, Done},
};

const TICK: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(15);
const BACKOFF_MAX: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

struct Shared {
    bus: Connection,
    /// Devices the user disconnected on purpose; not auto-reconnected until
    /// the user connects them again.
    held: Mutex<HashSet<String>>,
}

pub async fn run() -> Result<()> {
    let bus = Connection::system().await.context("system D-Bus")?;
    let shared = Arc::new(Shared {
        bus,
        held: Mutex::default(),
    });

    std::thread::Builder::new()
        .name("pipewire".into())
        .spawn(|| {
            if let Err(e) = audio::run() {
                warn!("audio switching stopped: {e:#}");
            }
        })?;

    let path = ipc::socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = unix::bind(&path)?;
    let server = Server::new(listener, BtdService(Btd(shared.clone())));
    info!("listening on {}", path.display());

    tokio::select! {
        r = server.run() => r.context("varlink server")?,
        () = reconnect_loop(shared) => {}
    }
    Ok(())
}

/// Where the last connected audio device's address is kept across restarts.
fn last_device_file() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| "/tmp".into());
    base.join("btd/last-audio-device")
}

/// Keep the last used audio device connected. Only that one device is
/// reconnected, and only while no other audio device is connected, so
/// headsets never fight over the default sink.
async fn reconnect_loop(shared: Arc<Shared>) {
    let file = last_device_file();
    let mut last = std::fs::read_to_string(&file)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let mut next_try = Instant::now();
    let mut delay = BACKOFF_MIN;
    loop {
        match bluez::devices(&shared.bus).await {
            Ok(devices) => {
                if let Some(d) = devices.iter().find(|d| d.audio && d.connected) {
                    if last.as_deref() != Some(&d.address) {
                        info!("last audio device is now {} ({})", d.name, d.address);
                        let _ = std::fs::create_dir_all(file.parent().unwrap());
                        if let Err(e) = std::fs::write(&file, &d.address) {
                            warn!("cannot save {}: {e}", file.display());
                        }
                        last = Some(d.address.clone());
                    }
                    delay = BACKOFF_MIN;
                    next_try = Instant::now();
                } else if let Some(d) = last
                    .as_ref()
                    .filter(|a| !shared.held.lock().unwrap().contains(*a))
                    .and_then(|a| devices.iter().find(|d| &d.address == a && d.paired))
                    && Instant::now() >= next_try
                {
                    info!("reconnecting {} ({})", d.name, d.address);
                    match tokio::time::timeout(
                        CONNECT_TIMEOUT,
                        bluez::connect(&shared.bus, &d.path),
                    )
                    .await
                    {
                        Ok(Ok(())) => info!("reconnected {}", d.name),
                        Ok(Err(e)) => info!("{} not reachable: {e:#}; retry in {delay:?}", d.name),
                        Err(_) => info!("{} timed out; retry in {delay:?}", d.name),
                    }
                    next_try = Instant::now() + delay;
                    delay = (delay * 2).min(BACKOFF_MAX);
                }
            }
            Err(e) => warn!("cannot list devices: {e:#}"),
        }
        tokio::time::sleep(TICK).await;
    }
}

#[derive(Service)]
#[zlink(vendor = "theorycorner", product = "btd", version = "0.1")]
struct BtdService(Btd);

struct Btd(Arc<Shared>);

#[interface("com.theorycorner.Btd")]
impl Btd {
    async fn list(&mut self) -> Result<DeviceList, BtdError> {
        Ok(DeviceList {
            devices: bluez::devices(&self.0.bus).await?,
        })
    }

    async fn connect(&mut self, device: String) -> Result<Done, BtdError> {
        let d = bluez::find(&self.0.bus, &device).await?;
        self.0.held.lock().unwrap().remove(&d.address);
        info!("connecting {} on request", d.name);
        bluez::connect(&self.0.bus, &d.path).await?;
        Ok(Done { name: d.name })
    }

    /// Without a device, disconnects the only connected one.
    async fn disconnect(&mut self, device: Option<String>) -> Result<Done, BtdError> {
        let d = match device {
            Some(device) => bluez::find(&self.0.bus, &device).await?,
            None => bluez::only_connected(&self.0.bus).await?,
        };
        self.0.held.lock().unwrap().insert(d.address.clone());
        info!("disconnecting {} on request", d.name);
        bluez::disconnect(&self.0.bus, &d.path).await?;
        Ok(Done { name: d.name })
    }
}
