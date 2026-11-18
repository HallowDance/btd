# btd

A small Bluetooth manager for Linux that replaces blueman and `bluetoothctl`.
It talks to BlueZ (`bluetoothd`) over D-Bus and to PipeWire natively, and is
built for an i3 desktop where audio devices matter most.

## Goals

- **Persistent connections**: reconnect trusted devices on startup, on resume
  and when the adapter powers on, retrying with backoff when a device refuses.
- **Audio follows the newest headset**: when an audio device connects, it
  becomes the default PipeWire sink and all playing streams move to it.
- **Audio first**: keep the A2DP profile active (AAC where available).
- **No legacy**: no HSP, no OBEX/file transfer.

## Components

One binary, three roles:

- `btd daemon`: systemd user service. Handles reconnects, audio switching and
  the BlueZ pairing agent (Just Works pairing is accepted only while a pairing
  window opened by `btd pair` is active).
- `btd <command>`: CLI with `list`, `scan`, `pair`, `connect`, `disconnect`
  and `remove`. Pairing also trusts the device.
- `btd menu`: rofi script-mode picker, meant to be bound to a key.

## Stack

- Rust, edition 2024
- [zbus](https://crates.io/crates/zbus) for BlueZ and the daemon's own
  session-bus service (`com.theorycorner.Btd`)
- [pipewire](https://crates.io/crates/pipewire) for watching nodes and setting
  the default sink through WirePlumber metadata
- tokio, clap, tracing (journald output)

`bluetoothd` itself is not replaced.

## Status

Prototype. Working: the daemon, `list`, `connect`, `disconnect`, `scan`,
`pair`, `remove`, the pairing agent, auto-reconnect of the last used audio
device, audio switching, and the rofi menu.

## Install

```sh
cargo install --path .
cp contrib/btd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now btd.service
```

## Usage

```sh
btd list                 # numbered list of known devices and their state
btd connect 2            # by number from `btd list`
btd connect rb-m600      # or by name substring or address
btd disconnect           # the only connected device; errors if none or several
btd disconnect rb-m600   # also stops auto-reconnect until the next connect
journalctl --user -u btd -f
```

The daemon remembers the last connected audio device in
`~/.local/state/btd/last-audio-device` and reconnects it when it drops, with
backoff from 15 s up to 10 min. It only does this while no other audio device
is connected.

### rofi menu

Bind it to a key, e.g. in sxhkd:

```
super + x
        rofi -show btd -modi "btd:btd menu" -show-icons
```

Shows paired devices (● connected, ○ not); Enter toggles the connection.
Also has "Pair new headset", "Scan and pair…" and "Remove…". Connecting and
pairing run in the background and report through desktop notifications, so
the menu closes right away.

Any command takes `--notify` to report that way instead of printing, which
suits key bindings, e.g. `btd --notify disconnect`.

### Varlink API

The Varlink API is on `$XDG_RUNTIME_DIR/btd.sock`:

```sh
varlinkctl introspect $XDG_RUNTIME_DIR/btd.sock com.theorycorner.Btd
```

## Credits

Written by Ivo Iliev with Claude (Anthropic's Claude Opus 5.5) via Claude Code.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
