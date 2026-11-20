//! BlueZ pairing agent. Just Works only, and only while `btd pair` has a
//! pairing window open.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use futures_util::StreamExt;
use tracing::{info, warn};
use zbus::{Connection, ObjectPath, OwnedObjectPath, fdo::DBusProxy, interface};

use crate::{bluez::AgentManager1Proxy, daemon::Shared};

const PATH: &str = "/com/theorycorner/btd/agent";

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    Rejected(String),
}

struct Agent(Arc<Shared>);

impl Agent {
    fn allow(&self, device: &OwnedObjectPath, what: &str) -> Result<(), AgentError> {
        if self.0.pair_window_open() {
            info!("agent: accepting {what} for {device}");
            Ok(())
        } else {
            info!("agent: rejecting {what} for {device}: no pairing window");
            Err(AgentError::Rejected(
                "no pairing window open; run `btd pair`".into(),
            ))
        }
    }
}

#[interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}

    fn request_pin_code(&self, _device: OwnedObjectPath) -> Result<String, AgentError> {
        Err(AgentError::Rejected(
            "btd only supports Just Works pairing".into(),
        ))
    }

    fn display_pin_code(
        &self,
        _device: OwnedObjectPath,
        _pincode: String,
    ) -> Result<(), AgentError> {
        Err(AgentError::Rejected(
            "btd only supports Just Works pairing".into(),
        ))
    }

    fn request_passkey(&self, _device: OwnedObjectPath) -> Result<u32, AgentError> {
        Err(AgentError::Rejected(
            "btd only supports Just Works pairing".into(),
        ))
    }

    fn display_passkey(&self, _device: OwnedObjectPath, _passkey: u32, _entered: u16) {}

    fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        _passkey: u32,
    ) -> Result<(), AgentError> {
        self.allow(&device, "pairing")
    }

    fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        self.allow(&device, "pairing")
    }

    fn authorize_service(&self, device: OwnedObjectPath, uuid: String) -> Result<(), AgentError> {
        self.allow(&device, &format!("service {uuid}"))
    }

    fn cancel(&self) {}
}

/// Export the agent on `bus` and keep it registered as BlueZ's default
/// agent: now if bluetoothd is running, and again every time it (re)starts,
/// since a restart forgets all agents.
pub async fn serve(bus: Connection, shared: Arc<Shared>) -> Result<()> {
    bus.object_server().at(PATH, Agent(shared)).await?;
    // Subscribe before the first attempt so a restart in between isn't missed.
    let dbus = DBusProxy::new(&bus).await?;
    let mut owners = dbus
        .receive_name_owner_changed_with_args(&[(0, "org.bluez")])
        .await?;
    if let Err(e) = register(&bus).await {
        warn!("pairing agent not registered yet (is bluetoothd running?): {e:#}");
    }
    while let Some(signal) = owners.next().await {
        if signal.args()?.new_owner().is_none() {
            warn!("bluetoothd stopped");
            continue;
        }
        info!("bluetoothd started; registering the pairing agent again");
        // BlueZ may take a moment to export AgentManager1 after taking its name.
        for delay in [0, 1, 3] {
            tokio::time::sleep(Duration::from_secs(delay)).await;
            match register(&bus).await {
                Ok(()) => break,
                Err(e) => warn!("cannot register pairing agent: {e:#}"),
            }
        }
    }
    Ok(())
}

async fn register(bus: &Connection) -> Result<()> {
    let path = ObjectPath::try_from(PATH)?;
    let manager = AgentManager1Proxy::new(bus).await?;
    manager.register_agent(&path, "NoInputNoOutput").await?;
    manager.request_default_agent(&path).await?;
    info!("pairing agent registered");
    Ok(())
}
