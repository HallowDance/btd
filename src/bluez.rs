//! BlueZ access over the system D-Bus.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use zbus::{Connection, OwnedObjectPath, OwnedVariant, fdo::ObjectManagerProxy, proxy};

/// A2DP sink: the remote device can play our audio.
const UUID_A2DP_SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";

#[proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
pub trait Device1 {
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, zlink::introspect::Type)]
pub struct Device {
    pub address: String,
    pub name: String,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
    pub audio: bool,
    #[serde(skip)]
    pub path: String,
}

fn prop<T: TryFrom<OwnedVariant>>(props: &HashMap<String, OwnedVariant>, key: &str) -> Option<T> {
    props.get(key).and_then(|v| T::try_from(v.clone()).ok())
}

/// Every device BlueZ knows about.
pub async fn devices(conn: &Connection) -> Result<Vec<Device>> {
    let om = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")
        .path("/")
        .build()
        .await?;
    let objects = om
        .get_managed_objects()
        .await
        .context("is bluetoothd running?")?;

    let mut out = Vec::new();
    for (path, ifaces) in objects {
        let Some(props) = ifaces
            .iter()
            .find(|(name, _)| name.as_str() == "org.bluez.Device1")
            .map(|(_, p)| p)
        else {
            continue;
        };
        let address: String = prop(props, "Address").unwrap_or_default();
        let uuids: Vec<String> = prop(props, "UUIDs").unwrap_or_default();
        out.push(Device {
            name: prop(props, "Alias")
                .or_else(|| prop(props, "Name"))
                .unwrap_or_else(|| address.clone()),
            address,
            paired: prop(props, "Paired").unwrap_or(false),
            trusted: prop(props, "Trusted").unwrap_or(false),
            connected: prop(props, "Connected").unwrap_or(false),
            audio: uuids.iter().any(|u| u == UUID_A2DP_SINK),
            path: path.to_string(),
        });
    }
    out.sort_by(|a, b| b.connected.cmp(&a.connected).then(a.name.cmp(&b.name)));
    Ok(out)
}

/// Find a device by exact address or case-insensitive name substring.
pub async fn find(conn: &Connection, query: &str) -> Result<Device> {
    let all = devices(conn).await?;
    if let Some(d) = all.iter().find(|d| d.address.eq_ignore_ascii_case(query)) {
        return Ok(d.clone());
    }
    let q = query.to_lowercase();
    let hits: Vec<_> = all
        .into_iter()
        .filter(|d| d.name.to_lowercase().contains(&q))
        .collect();
    match hits.len() {
        1 => Ok(hits.into_iter().next().unwrap()),
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
pub async fn only_connected(conn: &Connection) -> Result<Device> {
    let mut connected: Vec<_> = devices(conn)
        .await?
        .into_iter()
        .filter(|d| d.connected)
        .collect();
    match connected.len() {
        1 => Ok(connected.remove(0)),
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

async fn device_proxy<'a>(conn: &'a Connection, path: &str) -> Result<Device1Proxy<'a>> {
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
