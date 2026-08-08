//! One-shot PipeWire audio device enumeration for a Settings-screen device
//! picker. Not a persistent subscription — live hotplug refresh is out of
//! scope; call again if the list might have changed.

use crate::error::MediaError;
use crate::pipewire_io;
use pipewire as pw;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct AudioDevice {
    /// The PipeWire node's `node.name` — stable enough to pass back as a
    /// `capture_target`/`playback_target` to `MediaSession::start`, unlike
    /// the numeric registry id which isn't stable across sessions.
    pub id: String,
    pub description: String,
}

pub fn list_input_devices() -> Result<Vec<AudioDevice>, MediaError> {
    list_devices("Audio/Source")
}

pub fn list_output_devices() -> Result<Vec<AudioDevice>, MediaError> {
    list_devices("Audio/Sink")
}

/// Currently-active application audio *capture* streams (PipeWire class
/// `Stream/Input/Audio`) — e.g. Discord's own "WEBRTC VoiceEngine
/// [recStream]" node, which only exists in the graph while Discord is
/// actually in a voice channel. This is what makes routing a call straight
/// into a voice chat app possible at all: rather than only offering
/// hardware/virtual *sinks* (which the app would then have to be separately
/// configured to listen to), this lets the secondary-output picker target
/// the app's own live listening stream directly, the same way qpwgraph/
/// helvum patch one client's output straight into another client's input.
/// Ephemeral by nature — call again to refresh after joining a voice
/// channel, don't cache like the hardware device lists.
pub fn list_app_capture_streams() -> Result<Vec<AudioDevice>, MediaError> {
    list_devices("Stream/Input/Audio")
}

/// Currently-active application audio *playback* streams (PipeWire class
/// `Stream/Output/Audio`) — e.g. what Discord itself is playing out (other
/// voice-channel members), as opposed to `list_app_capture_streams`'s "what
/// Discord is listening on." This is the secondary-*input* counterpart:
/// mixing an app's own playback into what we send as RTP. Deliberately a
/// different media class from `list_app_capture_streams` — sourcing from an
/// app's *capture* stream here would mean capturing our own audio right back
/// out after `set_secondary_output` (or a real mic) fed it in, an instant
/// feedback loop.
pub fn list_app_playback_streams() -> Result<Vec<AudioDevice>, MediaError> {
    list_devices("Stream/Output/Audio")
}

fn pw_err(e: pw::Error) -> MediaError {
    MediaError::PipeWire(e.to_string())
}

fn list_devices(media_class: &str) -> Result<Vec<AudioDevice>, MediaError> {
    pipewire_io::ensure_init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_err)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pw_err)?;
    let core = context.connect_rc(None).map_err(pw_err)?;
    let registry = core.get_registry_rc().map_err(pw_err)?;

    let devices: Rc<RefCell<Vec<AudioDevice>>> = Rc::new(RefCell::new(Vec::new()));
    let devices_cb = devices.clone();
    let media_class_owned = media_class.to_string();

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = &global.props else {
                return;
            };
            if props.get("media.class") != Some(media_class_owned.as_str()) {
                return;
            }
            // Stream nodes (an app's live capture/playback stream, as
            // opposed to a hardware/virtual device) don't reliably have a
            // useful `node.name`/`node.description` — Discord's, for
            // instance, is a generic PulseAudio-bridge name. `application
            // .name` + `media.name` (what pavucontrol's Recording tab
            // actually shows) is what's recognizable to the user, and is
            // what `set_secondary_output`'s `TARGET_OBJECT` needs to be
            // able to find the node again — falls back to node.name/
            // node.description for ordinary hardware devices, which don't
            // set those stream-specific properties at all.
            let Some(name) = props.get("node.name") else {
                return;
            };
            let description = match (props.get("application.name"), props.get("media.name")) {
                (Some(app), Some(media)) => format!("{app} — {media}"),
                (Some(app), None) => app.to_string(),
                (None, _) => props.get("node.description").unwrap_or(name).to_string(),
            };
            devices_cb.borrow_mut().push(AudioDevice {
                id: name.to_string(),
                description,
            });
        })
        .register();

    // Quit once the registry's initial burst of `global` events is known to
    // be fully delivered (the standard pw_core sync/done handshake), with a
    // fallback timeout in case `done` never fires — same channel-based
    // mainloop-quit mechanism `pipewire_io.rs` uses for normal shutdown, so
    // no new/unproven shutdown pattern is introduced here.
    let pending_seq = core.sync(0).map_err(pw_err)?;
    let mainloop_for_done = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending_seq {
                mainloop_for_done.quit();
            }
        })
        .register();

    let (timeout_tx, timeout_rx) = pw::channel::channel::<()>();
    let mainloop_for_timeout = mainloop.clone();
    let _timeout_receiver = timeout_rx.attach(mainloop.loop_(), move |()| {
        mainloop_for_timeout.quit();
    });
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        let _ = timeout_tx.send(());
    });

    mainloop.run();

    // `_registry_listener`'s closure holds the other strong `Rc` clone
    // (`devices_cb`); it has to be dropped before `try_unwrap` below, since
    // local variables are otherwise only dropped at the end of the function,
    // after this point.
    drop(_registry_listener);

    let devices = Rc::try_unwrap(devices)
        .map_err(|_| MediaError::PipeWire("device list still borrowed".into()))?
        .into_inner();
    Ok(devices)
}
