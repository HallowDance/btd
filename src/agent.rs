//! BlueZ pairing agent. Just Works only, and only while `btd pair` has a
//! pairing window open.

use std::sync::Arc;

use anyhow::Result;
use tracing::info;
use zbus::{Connection, ObjectPath, OwnedObjectPath, interface};

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

/// Export the agent on `bus` and make it BlueZ's default agent.
pub async fn register(bus: &Connection, shared: Arc<Shared>) -> Result<()> {
    bus.object_server().at(PATH, Agent(shared)).await?;
    let path = ObjectPath::try_from(PATH)?;
    let manager = AgentManager1Proxy::new(bus).await?;
    manager.register_agent(&path, "NoInputNoOutput").await?;
    manager.request_default_agent(&path).await?;
    info!("pairing agent registered");
    Ok(())
}
