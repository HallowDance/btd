//! Varlink interface between the daemon and the CLI / rofi menu.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zlink::{ReplyError, introspect, proxy};

use crate::bluez::Device;

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(dir).join("btd.sock")
}

#[derive(Debug, Serialize, Deserialize, introspect::Type)]
pub struct DeviceList {
    pub devices: Vec<Device>,
}

#[derive(Debug, Serialize, Deserialize, introspect::Type)]
pub struct Done {
    pub name: String,
}

#[derive(Debug, PartialEq, ReplyError, introspect::ReplyError)]
#[zlink(interface = "com.theorycorner.Btd")]
pub enum BtdError {
    Failed { message: String },
}

impl From<anyhow::Error> for BtdError {
    fn from(e: anyhow::Error) -> Self {
        BtdError::Failed {
            message: format!("{e:#}"),
        }
    }
}

#[proxy("com.theorycorner.Btd")]
pub trait BtdProxy {
    async fn list(&mut self) -> zlink::Result<Result<DeviceList, BtdError>>;
    async fn connect(&mut self, device: &str) -> zlink::Result<Result<Done, BtdError>>;
    async fn disconnect(&mut self, device: Option<&str>) -> zlink::Result<Result<Done, BtdError>>;
}
