//! rofi script mode: `rofi -show btd -modi "btd:btd menu" -show-icons`.
//!
//! rofi runs `btd menu` for the entries and runs it again with the chosen
//! entry's `info` in `$ROFI_INFO`. Printing entries keeps rofi open; printing
//! nothing closes it. Slow actions run in a detached `btd --notify ...` so
//! rofi closes at once and the result shows up as a notification.

use std::{
    os::unix::process::CommandExt,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};
use futures_util::{StreamExt, pin_mut};

use crate::{bluez::Device, client, fail, ipc::BtdProxy};

const SCAN_SECONDS: u32 = 8;

pub async fn run() -> Result<()> {
    let info = std::env::var("ROFI_INFO").unwrap_or_default();
    let (action, address) = info.split_once(':').unwrap_or((&info, ""));
    match action {
        "toggle" => {
            let devices = client().await?.list().await?.map_err(fail)?.devices;
            let connected = devices.iter().any(|d| d.address == address && d.connected);
            background(&[if connected { "disconnect" } else { "connect" }, address])
        }
        "pair-new" => background(&["pair"]),
        "pair" => background(&["pair", address]),
        "scan" => scan_menu().await,
        "remove-menu" => remove_menu().await,
        "remove" => {
            let done = client().await?.remove(address).await?.map_err(fail)?;
            main_menu(Some(&format!("Removed {}", done.name))).await
        }
        _ => main_menu(None).await,
    }
}

async fn main_menu(message: Option<&str>) -> Result<()> {
    let devices = client().await?.list().await?.map_err(fail)?.devices;
    header("bluetooth", message);
    for d in devices.iter().filter(|d| d.paired) {
        let (mark, state) = if d.connected {
            ("●", "  connected")
        } else {
            ("○", "")
        };
        row(
            &format!("{mark} {}{state}", d.name),
            icon(d),
            &format!("toggle:{}", d.address),
        );
    }
    row("Pair new headset", "list-add", "pair-new");
    row(
        &format!("Scan and pair… ({SCAN_SECONDS} s)"),
        "system-search",
        "scan",
    );
    row("Remove…", "list-remove", "remove-menu");
    Ok(())
}

async fn scan_menu() -> Result<()> {
    let mut client = client().await?;
    let events = client.scan(SCAN_SECONDS, false).await?;
    pin_mut!(events);
    let mut found: Vec<Device> = Vec::new();
    while let Some(event) = events.next().await {
        if let Some(d) = event?.map_err(fail)?.device
            && !d.paired
        {
            found.push(d);
        }
    }
    let message = if found.is_empty() {
        "Nothing new found. Is the device in pairing mode?"
    } else {
        "Pick a device to pair"
    };
    header("pair", Some(message));
    for d in &found {
        row(
            &format!("{}  {}", d.name, d.address),
            icon(d),
            &format!("pair:{}", d.address),
        );
    }
    row("← Back", "go-previous", "back");
    Ok(())
}

async fn remove_menu() -> Result<()> {
    let devices = client().await?.list().await?.map_err(fail)?.devices;
    header("remove", Some("Unpair and forget a device"));
    for d in devices.iter().filter(|d| d.paired) {
        row(&d.name, icon(d), &format!("remove:{}", d.address));
    }
    row("← Back", "go-previous", "back");
    Ok(())
}

fn header(prompt: &str, message: Option<&str>) {
    println!("\0prompt\x1f{prompt}");
    println!("\0no-custom\x1ftrue");
    if let Some(m) = message {
        println!("\0message\x1f{m}");
    }
}

fn row(text: &str, icon: &str, info: &str) {
    println!("{text}\0icon\x1f{icon}\x1finfo\x1f{info}");
}

fn icon(d: &Device) -> &str {
    if d.icon.is_empty() {
        "bluetooth"
    } else {
        &d.icon
    }
}

/// Run `btd --notify <args>` detached, so rofi can close right away.
fn background(args: &[&str]) -> Result<()> {
    Command::new(std::env::current_exe()?)
        .arg("--notify")
        .args(args.iter().filter(|a| !a.is_empty()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("cannot start btd in the background")?;
    Ok(())
}
