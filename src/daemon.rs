//! `btd daemon`: auto-reconnect, audio switching, pairing and the Varlink
//! service.

use std::{
    collections::HashSet,
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};
use zbus::{Connection, ObjectPath};
use zlink::{Reply, Server, Service, interface, tokio::unix};

use crate::{
    agent, audio,
    bluez::{self, Device},
    ipc::{self, BtdError, DeviceList, Done, Event},
};

const TICK: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(15);
const BACKOFF_MAX: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const PAIR_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `btd pair` without a device waits for a new headset to show up.
const PAIR_SEARCH: Duration = Duration::from_secs(30);

pub struct Shared {
    bus: Connection,
    /// Devices the user disconnected on purpose; not auto-reconnected until
    /// the user connects them again.
    held: Mutex<HashSet<String>>,
    /// Address of the last connected audio device.
    last: Mutex<Option<String>>,
    /// Unpaired devices in the order the current scan first saw them, so
    /// their numbers don't shift while the scan finds more.
    seen: Mutex<Vec<String>>,
    /// Number of scans/pairings currently needing discovery.
    discovery_users: tokio::sync::Mutex<usize>,
    /// While set and in the future, the agent accepts pairing requests.
    pair_window: Mutex<Option<Instant>>,
}

impl Shared {
    pub fn pair_window_open(&self) -> bool {
        self.pair_window
            .lock()
            .unwrap()
            .is_some_and(|until| Instant::now() < until)
    }

    /// All devices, numbered: paired ones first by name, then unpaired ones
    /// in the order the last scan saw them.
    async fn devices(&self) -> Result<Vec<Device>> {
        let mut all = bluez::devices(&self.bus).await?;
        let seen = self.seen.lock().unwrap().clone();
        // Unnamed devices go last so the named ones get the low numbers.
        let rank = |d: &Device| -> (bool, bool, usize, String) {
            let pos = seen.iter().position(|a| a == &d.address);
            (
                !d.paired,
                !d.named,
                pos.unwrap_or(usize::MAX),
                d.name.to_lowercase(),
            )
        };
        all.sort_by_key(|d| rank(d));
        for (i, d) in all.iter_mut().enumerate() {
            d.number = i as u32 + 1;
        }
        Ok(all)
    }

    /// Remember unpaired devices a scan sees right now (those with RSSI).
    fn note_seen(&self, devices: &[Device]) {
        let mut seen = self.seen.lock().unwrap();
        for d in devices.iter().filter(|d| !d.paired && d.rssi.is_some()) {
            if !seen.contains(&d.address) {
                seen.push(d.address.clone());
            }
        }
    }

    async fn start_discovery(&self) -> Result<()> {
        let mut users = self.discovery_users.lock().await;
        if *users == 0 {
            bluez::adapter(&self.bus)
                .await?
                .start_discovery()
                .await
                .context("cannot start discovery")?;
            info!("discovery started");
        }
        *users += 1;
        Ok(())
    }

    async fn stop_discovery(&self) {
        let mut users = self.discovery_users.lock().await;
        *users = users.saturating_sub(1);
        if *users == 0 {
            match bluez::adapter(&self.bus).await {
                Ok(a) => {
                    let _ = a.stop_discovery().await;
                    info!("discovery stopped");
                }
                Err(e) => warn!("cannot stop discovery: {e:#}"),
            }
        }
    }

    fn set_last(&self, address: Option<&str>) {
        let file = last_device_file();
        let _ = std::fs::create_dir_all(file.parent().unwrap());
        if let Err(e) = std::fs::write(&file, address.unwrap_or_default()) {
            warn!("cannot save {}: {e}", file.display());
        }
        *self.last.lock().unwrap() = address.map(str::to_owned);
    }
}

pub async fn run() -> Result<()> {
    let bus = Connection::system().await.context("system D-Bus")?;
    let last = std::fs::read_to_string(last_device_file())
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let shared = Arc::new(Shared {
        bus,
        held: Mutex::default(),
        last: Mutex::new(last),
        seen: Mutex::default(),
        discovery_users: tokio::sync::Mutex::new(0),
        pair_window: Mutex::default(),
    });

    tokio::spawn({
        let (bus, shared) = (shared.bus.clone(), shared.clone());
        async move {
            if let Err(e) = agent::serve(bus, shared).await {
                warn!("pairing agent stopped: {e:#}");
            }
        }
    });

    // PipeWire restarts (e.g. on updates) end the connection; reconnect.
    std::thread::Builder::new()
        .name("pipewire".into())
        .spawn(|| {
            loop {
                if let Err(e) = audio::run() {
                    warn!("lost PipeWire: {e:#}; reconnecting in 3 s");
                }
                std::thread::sleep(Duration::from_secs(3));
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
    let mut next_try = Instant::now();
    let mut delay = BACKOFF_MIN;
    loop {
        match bluez::devices(&shared.bus).await {
            Ok(devices) => {
                let last = shared.last.lock().unwrap().clone();
                if let Some(d) = devices.iter().find(|d| d.audio && d.connected) {
                    if last.as_deref() != Some(&d.address) {
                        info!("last audio device is now {} ({})", d.name, d.address);
                        shared.set_last(Some(&d.address));
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

type EventStream = ReceiverStream<Result<Reply<Event>, BtdError>>;

/// Sends progress events of a streaming call to the client.
#[derive(Clone)]
struct Progress(mpsc::Sender<Result<Reply<Event>, BtdError>>);

impl Progress {
    async fn send(&self, message: Option<String>, device: Option<Device>) {
        let event = Event { message, device };
        // A closed channel means the client went away; the work goes on.
        let _ = self
            .0
            .send(Ok(Reply::new(Some(event)).set_continues(Some(true))))
            .await;
    }

    async fn status(&self, message: impl Into<String>) {
        self.send(Some(message.into()), None).await;
    }
}

/// Run `work` in its own task and stream its progress. The final reply
/// carries the summary `work` returns, or its error.
fn stream<F, Fut>(work: F) -> EventStream
where
    F: FnOnce(Progress) -> Fut,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(32);
    let fut = work(Progress(tx.clone()));
    tokio::spawn(async move {
        let last = match fut.await {
            Ok(message) => Ok(Reply::new(Some(Event {
                message: Some(message),
                device: None,
            }))
            .set_continues(Some(false))),
            Err(e) => Err(BtdError::from(e)),
        };
        let _ = tx.send(last).await;
    });
    ReceiverStream::new(rx)
}

async fn scan(shared: Arc<Shared>, progress: Progress, seconds: u32, all: bool) -> Result<String> {
    shared.seen.lock().unwrap().clear();
    shared.start_discovery().await?;
    let deadline = Instant::now() + Duration::from_secs(seconds.into());
    let mut shown = HashSet::new();
    let result: Result<()> = async {
        while Instant::now() < deadline {
            shared.note_seen(&bluez::devices(&shared.bus).await?);
            for d in shared.devices().await? {
                // Like Android: devices without a name are hidden unless asked for.
                if d.rssi.is_some()
                    && (all || d.named || d.paired)
                    && shown.insert(d.address.clone())
                {
                    progress.send(None, Some(d)).await;
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Ok(())
    }
    .await;
    shared.stop_discovery().await;
    result?;
    Ok(format!("{} device(s) found", shown.len()))
}

/// Scan until exactly one new headset shows up. Only headsets and
/// headphones count, so always-discoverable speakers and TV boxes nearby
/// don't get in the way.
async fn find_new_headset(shared: &Shared, progress: &Progress) -> Result<Device> {
    progress
        .status("looking for a headset in pairing mode…")
        .await;
    shared.start_discovery().await?;
    let deadline = Instant::now() + PAIR_SEARCH;
    let result = async {
        loop {
            let all = bluez::devices(&shared.bus).await?;
            shared.note_seen(&all);
            let new: Vec<_> = shared
                .devices()
                .await?
                .into_iter()
                .filter(|d| {
                    !d.paired
                        && d.named
                        && d.rssi.is_some()
                        && matches!(d.icon.as_str(), "audio-headset" | "audio-headphones")
                })
                .collect();
            match new.len() {
                1 => return Ok(new.into_iter().next().unwrap()),
                0 if Instant::now() >= deadline => {
                    bail!(
                        "no new headset found; is it in pairing mode? \
                         For other devices use `btd scan`, then `btd pair <n>`"
                    )
                }
                0 => tokio::time::sleep(Duration::from_secs(1)).await,
                _ => bail!(
                    "several new headsets found, pick one: {}",
                    new.iter()
                        .map(|d| format!("{} ({})", d.name, d.number))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    }
    .await;
    shared.stop_discovery().await;
    result
}

async fn pair(shared: Arc<Shared>, progress: Progress, device: Option<String>) -> Result<String> {
    let d = match device {
        Some(q) => bluez::find(&shared.devices().await?, &q)?,
        None => find_new_headset(&shared, &progress).await?,
    };
    let proxy = bluez::device_proxy(&shared.bus, &d.path).await?;

    if d.paired {
        progress
            .status(format!("{} is already paired", d.name))
            .await;
    } else {
        progress
            .status(format!("pairing {} ({})…", d.name, d.address))
            .await;
        *shared.pair_window.lock().unwrap() = Some(Instant::now() + PAIR_TIMEOUT);
        let result = tokio::time::timeout(PAIR_TIMEOUT, proxy.pair()).await;
        *shared.pair_window.lock().unwrap() = None;
        match result {
            Ok(r) => r.with_context(|| format!("pairing {} failed", d.name))?,
            Err(_) => bail!("pairing {} timed out", d.name),
        }
        progress.status("paired").await;
    }

    proxy
        .set_trusted(true)
        .await
        .context("cannot trust device")?;
    shared.held.lock().unwrap().remove(&d.address);

    if !d.connected {
        progress.status("connecting…").await;
        tokio::time::timeout(CONNECT_TIMEOUT, proxy.connect())
            .await
            .map_err(|_| anyhow!("connecting {} timed out", d.name))?
            .with_context(|| format!("connecting {} failed", d.name))?;
    }
    Ok(format!("{} paired and connected", d.name))
}

#[derive(Service)]
#[zlink(vendor = "theorycorner", product = "btd", version = "0.1")]
struct BtdService(Btd);

struct Btd(Arc<Shared>);

#[interface("com.theorycorner.Btd")]
impl Btd {
    async fn list(&mut self) -> Result<DeviceList, BtdError> {
        Ok(DeviceList {
            devices: self.0.devices().await?,
        })
    }

    async fn connect(&mut self, device: String) -> Result<Done, BtdError> {
        let d = bluez::find(&self.0.devices().await?, &device)?;
        self.0.held.lock().unwrap().remove(&d.address);
        info!("connecting {} on request", d.name);
        bluez::connect(&self.0.bus, &d.path).await?;
        Ok(Done { name: d.name })
    }

    /// Without a device, disconnects the only connected one.
    async fn disconnect(&mut self, device: Option<String>) -> Result<Done, BtdError> {
        let all = self.0.devices().await?;
        let d = match device {
            Some(device) => bluez::find(&all, &device)?,
            None => bluez::only_connected(&all)?,
        };
        if !d.connected {
            return Err(anyhow!("{} is not connected", d.name).into());
        }
        self.0.held.lock().unwrap().insert(d.address.clone());
        info!("disconnecting {} on request", d.name);
        bluez::disconnect(&self.0.bus, &d.path).await?;
        Ok(Done { name: d.name })
    }

    /// Unpair a device and forget it.
    async fn remove(&mut self, device: String) -> Result<Done, BtdError> {
        let d = bluez::find(&self.0.devices().await?, &device)?;
        info!("removing {} on request", d.name);
        let path = ObjectPath::try_from(d.path.as_str()).map_err(anyhow::Error::from)?;
        bluez::adapter(&self.0.bus)
            .await?
            .remove_device(&path)
            .await
            .map_err(anyhow::Error::from)?;
        self.0.held.lock().unwrap().remove(&d.address);
        if self.0.last.lock().unwrap().as_deref() == Some(&d.address) {
            self.0.set_last(None);
        }
        Ok(Done { name: d.name })
    }

    /// Discover devices for `seconds`, streaming each one as it is found.
    /// Devices without a name are left out unless `all` is set.
    #[zlink(more)]
    async fn scan(
        &mut self,
        more: bool,
        seconds: u32,
        all: bool,
    ) -> impl Stream<Item = Result<Reply<Event>, BtdError>> {
        let _ = more;
        let shared = self.0.clone();
        stream(move |p| scan(shared, p, seconds.clamp(1, 120), all))
    }

    /// Pair, trust and connect a device. Without one, waits for exactly one
    /// new headset in pairing mode.
    #[zlink(more)]
    async fn pair(
        &mut self,
        more: bool,
        device: Option<String>,
    ) -> impl Stream<Item = Result<Reply<Event>, BtdError>> {
        let _ = more;
        let shared = self.0.clone();
        stream(move |p| pair(shared, p, device))
    }
}
