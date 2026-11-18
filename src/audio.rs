//! PipeWire side: when a Bluetooth sink appears, make it the default and move
//! every playing stream onto it ("newest wins").

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use pipewire as pw;
use pw::{metadata::Metadata, types::ObjectType};
use tracing::{info, warn};

#[derive(Default)]
struct State {
    /// Set once the registry's initial burst of existing objects is over, so
    /// sinks that were already present at startup don't steal the default.
    synced: bool,
    metadata: Option<(Metadata, Box<dyn pw::proxy::Listener>)>,
    /// Audio output streams: node id to object.serial.
    streams: HashMap<u32, String>,
}

/// Runs the PipeWire main loop on the current thread. Never returns unless
/// PipeWire goes away.
pub fn run() -> anyhow::Result<()> {
    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;
    let state = Rc::new(RefCell::new(State::default()));

    let initial_sync = core.sync(0)?;
    let _core_listener = core
        .add_listener_local()
        .done({
            let state = state.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == initial_sync {
                    state.borrow_mut().synced = true;
                }
            }
        })
        .error({
            let main_loop = main_loop.downgrade();
            move |id, _seq, _res, message| {
                warn!("pipewire error on {id}: {message}");
                if id == pw::core::PW_ID_CORE
                    && let Some(l) = main_loop.upgrade()
                {
                    l.quit();
                }
            }
        })
        .register();

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let state = state.clone();
            let registry = registry.downgrade();
            move |obj| {
                let Some(props) = obj.props else { return };
                let Some(registry) = registry.upgrade() else {
                    return;
                };
                match obj.type_ {
                    ObjectType::Metadata if props.get("metadata.name") == Some("default") => {
                        match registry.bind::<Metadata, _>(obj) {
                            Ok(md) => {
                                let listener = md.add_listener_local().register();
                                state.borrow_mut().metadata = Some((md, Box::new(listener)));
                            }
                            Err(e) => warn!("cannot bind default metadata: {e}"),
                        }
                    }
                    ObjectType::Node => {
                        let class = props.get("media.class").unwrap_or_default();
                        let name = props.get("node.name").unwrap_or_default();
                        let serial = props.get("object.serial").unwrap_or_default();
                        if class == "Stream/Output/Audio" {
                            state.borrow_mut().streams.insert(obj.id, serial.to_owned());
                        } else if class == "Audio/Sink" && name.starts_with("bluez_output.") {
                            on_bluetooth_sink(&state.borrow(), obj.id, name, serial);
                        }
                    }
                    _ => {}
                }
            }
        })
        .global_remove({
            let state = state.clone();
            move |id| {
                state.borrow_mut().streams.remove(&id);
            }
        })
        .register();

    main_loop.run();
    anyhow::bail!("pipewire main loop exited")
}

fn on_bluetooth_sink(state: &State, id: u32, name: &str, serial: &str) {
    if !state.synced {
        return;
    }
    let Some((md, _)) = &state.metadata else {
        warn!("bluetooth sink {name} appeared but default metadata is not bound");
        return;
    };
    info!("new bluetooth sink {name}: making it the default");
    md.set_property(
        0,
        "default.configured.audio.sink",
        Some("Spa:String:JSON"),
        Some(&format!("{{ \"name\": \"{name}\" }}")),
    );
    // Same keys pipewire-pulse writes for `pactl move-sink-input`.
    for (stream, _) in &state.streams {
        md.set_property(
            *stream,
            "target.node",
            Some("Spa:Id"),
            Some(&id.to_string()),
        );
        md.set_property(*stream, "target.object", Some("Spa:Id"), Some(serial));
    }
    info!("moved {} stream(s) to {name}", state.streams.len());
}
