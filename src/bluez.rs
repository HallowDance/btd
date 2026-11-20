//! BlueZ access over the system D-Bus.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use zbus::{
    Connection, ObjectPath, OwnedObjectPath, OwnedVariant, fdo::ObjectManagerProxy,
    names::OwnedInterfaceName, proxy,
};

/// A2DP sink: the remote device can play our audio.
const UUID_A2DP_SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";

const MEDIA_TRANSPORT: &str = "org.bluez.MediaTransport1";

#[proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
pub trait Device1 {
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    fn pair(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn set_trusted(&self, trusted: bool) -> zbus::Result<()>;
}

#[proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
pub trait Adapter1 {
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
    fn remove_device(&self, device: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.bluez.AgentManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez"
)]
pub trait AgentManager1 {
    fn register_agent(&self, agent: &ObjectPath<'_>, capability: &str) -> zbus::Result<()>;
    fn request_default_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, zlink::introspect::Type)]
pub struct Device {
    /// Number shown by `btd list`; filled in by the daemon.
    pub number: u32,
    pub address: String,
    pub name: String,
    /// Whether the device broadcasts a name. Without one BlueZ only knows
    /// its address; `name` is then "(no name)".
    pub named: bool,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
    /// BlueZ has a media transport for the device. `connected` is also true
    /// for a bare LE link, which a dual-mode headset opens while it refuses
    /// audio.
    #[serde(default)]
    pub audio_connected: bool,
    pub audio: bool,
    /// BlueZ icon name, e.g. `audio-headset`, `audio-card`, `input-mouse`.
    pub icon: String,
    /// Signal strength; only present while a scan sees the device.
    pub rssi: Option<i16>,
    pub path: String,
}

type Interfaces = HashMap<OwnedInterfaceName, HashMap<String, OwnedVariant>>;

fn interface<'a>(ifaces: &'a Interfaces, name: &str) -> Option<&'a HashMap<String, OwnedVariant>> {
    ifaces
        .iter()
        .find(|(n, _)| n.as_str() == name)
        .map(|(_, props)| props)
}

fn prop<T: TryFrom<OwnedVariant>>(props: &HashMap<String, OwnedVariant>, key: &str) -> Option<T> {
    props.get(key).and_then(|v| T::try_from(v.clone()).ok())
}

async fn managed_objects(conn: &Connection) -> Result<zbus::fdo::ManagedObjects> {
    let om = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")
        .path("/")
        .build()
        .await?;
    om.get_managed_objects()
        .await
        .context("is bluetoothd running?")
}

/// Every device BlueZ knows about, unnumbered and in no particular order.
pub async fn devices(conn: &Connection) -> Result<Vec<Device>> {
    let objects = managed_objects(conn).await?;
    let with_transport: HashSet<OwnedObjectPath> = objects
        .values()
        .filter_map(|ifaces| interface(ifaces, MEDIA_TRANSPORT))
        .filter_map(|props| prop(props, "Device"))
        .collect();
    let mut out = Vec::new();
    for (path, ifaces) in objects {
        let Some(props) = interface(&ifaces, "org.bluez.Device1") else {
            continue;
        };
        let address: String = prop(props, "Address").unwrap_or_default();
        let uuids: Vec<String> = prop(props, "UUIDs").unwrap_or_default();
        let icon: String = prop(props, "Icon").unwrap_or_default();
        // BlueZ fills Alias with the address when there is no Name.
        let alias: Option<String> = prop(props, "Alias");
        let named = prop::<String>(props, "Name").is_some()
            || alias
                .as_ref()
                .is_some_and(|a| *a != address.replace(':', "-"));
        out.push(Device {
            number: 0,
            name: match alias {
                Some(a) if named => a,
                _ => "(no name)".into(),
            },
            named,
            address,
            paired: prop(props, "Paired").unwrap_or(false),
            trusted: prop(props, "Trusted").unwrap_or(false),
            connected: prop(props, "Connected").unwrap_or(false),
            audio_connected: with_transport.contains(&path),
            audio: uuids.iter().any(|u| u == UUID_A2DP_SINK) || icon.starts_with("audio-"),
            icon,
            rssi: prop(props, "RSSI"),
            path: path.to_string(),
        });
    }
    Ok(out)
}

/// The first Bluetooth adapter.
pub async fn adapter(conn: &Connection) -> Result<Adapter1Proxy<'_>> {
    let path = managed_objects(conn)
        .await?
        .into_iter()
        .find(|(_, ifaces)| ifaces.keys().any(|i| i.as_str() == "org.bluez.Adapter1"))
        .map(|(path, _)| path)
        .context("no Bluetooth adapter found")?;
    Ok(Adapter1Proxy::builder(conn).path(path).build().await?)
}

/// Find a device by exact address, its number, or case-insensitive name
/// substring. `devices` must already be numbered.
pub fn find(devices: &[Device], query: &str) -> Result<Device> {
    if let Some(d) = devices
        .iter()
        .find(|d| d.address.eq_ignore_ascii_case(query))
    {
        return Ok(d.clone());
    }
    if let Ok(n) = query.parse::<u32>() {
        return match devices.iter().find(|d| d.number == n) {
            Some(d) => Ok(d.clone()),
            None => bail!("no device number {n}; see `btd list`"),
        };
    }
    let q = query.to_lowercase();
    let hits: Vec<_> = devices
        .iter()
        .filter(|d| d.name.to_lowercase().contains(&q))
        .collect();
    match hits.len() {
        1 => Ok(hits[0].clone()),
        0 => bail!("no device matches '{query}'"),
        _ => bail!(
            "'{query}' matches several devices: {}",
            hits.iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The single connected device; fails if none or several are connected.
pub fn only_connected(devices: &[Device]) -> Result<Device> {
    let connected: Vec<_> = devices.iter().filter(|d| d.connected).collect();
    match connected.len() {
        1 => Ok(connected[0].clone()),
        0 => bail!("no device is connected"),
        _ => bail!(
            "several devices are connected, name one: {}",
            connected
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub async fn device_proxy<'a>(conn: &'a Connection, path: &str) -> Result<Device1Proxy<'a>> {
    let path = OwnedObjectPath::try_from(path.to_owned())?;
    Ok(Device1Proxy::builder(conn).path(path).build().await?)
}

pub async fn connect(conn: &Connection, path: &str) -> Result<()> {
    device_proxy(conn, path).await?.connect().await?;
    Ok(())
}

pub async fn disconnect(conn: &Connection, path: &str) -> Result<()> {
    device_proxy(conn, path).await?.disconnect().await?;
    Ok(())
}
