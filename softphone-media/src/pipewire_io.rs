//! All PipeWire-crate-touching code, isolated from `session.rs`'s tokio
//! orchestration. Runs a capture + playback stream pair on one dedicated
//! blocking OS thread, bridging PCM i16 samples to/from ring buffers, and
//! signaling `session.rs`'s RTP send loop via a shared `Notify` so
//! packetization tracks PipeWire's real audio callback cadence instead of
//! an independent timer (see the plan doc's root-cause analysis).

use crate::error::MediaError;
use pipewire as pw;
use pw::{properties::properties, spa};
use ringbuf::traits::{Consumer, Observer, Producer};
use ringbuf::{HeapCons, HeapProd};
use spa::pod::Pod;
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Once};
use tokio::sync::Notify;

const SAMPLE_RATE: u32 = 8000;
const CHANNELS: u32 = 1;
// SPA_AUDIO_CHANNEL_MONO. Confirmed from the generated libspa-sys bindgen
// output (target/debug/build/libspa-sys-*/out/bindings.rs) since the
// constant isn't in plain-text vendored source (bindgen-generated at build
// time). Hardcoded rather than adding a direct dependency on the `-sys`
// crate for one constant.
const SPA_AUDIO_CHANNEL_MONO: u32 = 2;
// On this system, the playback stream's `process()` callback was measured
// (via direct instrumentation, not inferred) firing only once every ~1.5s,
// requesting ~12288 frames each time — not the ~170-frame/~21ms cadence a
// PipeWire client normally gets. This was true regardless of target device,
// media.role (several values and none at all), NODE_LATENCY hints,
// node.always-process, realtime thread scheduling (pw_thread_loop +
// client.conf's rt module), dropping the RT_PROCESS connect flag, a plain
// MainLoopRc instead of ThreadLoopRc, or the graph's clock.max-quantum
// setting — all tested directly across two separate investigation passes,
// none changed it. A *separate*, ordinary PipeWire client (`pw-play`)
// confirmed fast (~100ms) through this exact same WirePlumber loopback
// route (`input.loopback.sink.role.multimedia`, verified via `pw-dump`),
// so this is specific to something about a `pw::stream`-API client's
// negotiation on this system, not a global graph/hardware limit — but the
// actual trigger remains unresolved after exhausting every client-side
// lever the pipewire-rs API exposes.
//
// `TARGET_BUFFERED_SAMPLES`/`MAX_BUFFERED_SAMPLES` below are what actually
// keep audio continuous once a call is running (steady-state has to survive
// a callback that can demand ~12288 frames at once). `PREBUFFER_SAMPLES` is
// a much smaller, separate concern: only the *first couple of callbacks at
// the very start of a call* need it, so it doesn't need to be sized to the
// full pathological callback demand the way the steady-state ceiling does —
// by the time the second real callback rolls around ~1.5s later, normal RTP
// arrival has already accumulated a comparable amount "for free". Gating
// audible playback on a full ~2s prebuffer was adding a flat ~2s of pure
// startup latency on top of the ~1.5s callback-interval floor, which is a
// meaningful chunk of the "5 seconds of delay" a live call feels like — cut
// down to just enough to avoid literal dead air the instant a call answers.
const PREBUFFER_SAMPLES: usize = 1_600; // ~200ms @ 8kHz mono

static PW_INIT: Once = Once::new();

pub(crate) fn ensure_init() {
    PW_INIT.call_once(pw::init);
}

/// A signal sent to the thin "owner" thread (see `spawn`/`run` below) asking
/// it to stop the `pw_thread_loop` and exit.
enum PwCommand {
    Terminate,
}

/// A plain `std::thread::JoinHandle` + `mpsc::Sender`, same shape as before —
/// but what runs *inside* that thread changed fundamentally. It used to call
/// `MainLoopRc::run()` directly, meaning all audio callback processing ran on
/// this manually-spawned thread, which gets ordinary `SCHED_OTHER` scheduling
/// (confirmed on this system via `ps -eLo policy,pri` — priority 19, no RT
/// class at all). Under any real desktop load, a non-RT thread with 20ms
/// audio deadlines can go unscheduled for a second or more; when it finally
/// does run, PipeWire's client-side stream code hands it a buffer sized to
/// cover however much time actually elapsed — observed directly on this
/// system as ~12288-frame (1.5s) callbacks instead of the expected
/// ~170-frame/~21ms ones. Since that's larger than any ring buffer we'd
/// reasonably keep, every callback underran into mostly silence: this, not
/// clock drift or reorder-window sizing, was the dominant cause of both the
/// dropouts and the multi-second echo-test latency.
///
/// Now `run()` instead creates a [`pw::thread_loop::ThreadLoopRc`] and calls
/// `.start()`, which spawns *PipeWire's own* internal thread to do the actual
/// callback processing — the same realtime-scheduling mechanism (via rtkit)
/// that the PipeWire server's own threads use (confirmed separately:
/// rtkit-daemon is active and already grants the server RT priority on this
/// system). This "owner" thread just holds the `Rc`-based handles alive
/// (needed since `ThreadLoopRc`/the `Context`/streams aren't `Send`, so they
/// can't be moved into `spawn_blocking` for shutdown the way the old
/// `std::thread::JoinHandle` was) and blocks on `shutdown_rx` until told to
/// stop — it never touches audio itself, so its own lack of RT scheduling is
/// irrelevant.
pub struct PwThreadHandle {
    thread: Option<std::thread::JoinHandle<()>>,
    shutdown: std::sync::mpsc::Sender<PwCommand>,
}

impl PwThreadHandle {
    pub fn stop(mut self) {
        let _ = self.shutdown.send(PwCommand::Terminate);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PwThreadHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(PwCommand::Terminate);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Shared state bridging the PipeWire realtime callback thread with the
/// tokio-side RTP send/recv tasks in `session.rs`.
pub(crate) struct PipewireShared {
    pub notify: Arc<Notify>,
    pub mic_muted: Arc<AtomicBool>,
    pub input_gain_bits: Arc<AtomicU32>,
    pub input_level_bits: Arc<AtomicU32>,
    pub output_level_bits: Arc<AtomicU32>,
}

struct CaptureUserData {
    prod: HeapProd<i16>,
    notify: Arc<Notify>,
    mic_muted: Arc<AtomicBool>,
    input_gain_bits: Arc<AtomicU32>,
    input_level_bits: Arc<AtomicU32>,
}

struct PlaybackUserData {
    cons: HeapCons<i16>,
    output_level_bits: Arc<AtomicU32>,
    prebuffered: bool,
    underrun_frames: u64,
    callback_count: u64,
}

// Target/ceiling for how much decoded audio is allowed to sit in the
// playback ring buffer at once, in samples (8kHz mono, so 1 sample = 0.125ms).
// Sized around the real measured callback cadence on this system (~1.5s,
// ~12288 frames per call — see `PREBUFFER_SAMPLES`'s doc comment) rather
// than the ~20ms cadence a normal PipeWire client gets, so the catch-up
// mechanism below doesn't fight the buffering `PREBUFFER_SAMPLES` actually
// needs. This still protects against unbounded growth from ordinary VoIP
// clock drift (our clock vs. FreePBX's RTP transmit clock) layered on top
// of that baseline: when occupancy exceeds `MAX_BUFFERED_SAMPLES`, the
// playback callback discards the stale backlog down to
// `TARGET_BUFFERED_SAMPLES` rather than playing through it.
const TARGET_BUFFERED_SAMPLES: usize = 16_000; // ~2s
const MAX_BUFFERED_SAMPLES: usize = 20_000; // ~2.5s

/// Spawn the PipeWire thread. Blocks (briefly) until the streams are
/// connected or setup fails, so callers get a real `Result` instead of
/// discovering failure asynchronously later. `capture_target`/
/// `playback_target` (a PipeWire node's `node.name`) pin the streams to a
/// specific device instead of the system default when `Some`.
pub fn spawn(
    capture_prod: HeapProd<i16>,
    playback_cons: HeapCons<i16>,
    shared: PipewireShared,
    capture_target: Option<String>,
    playback_target: Option<String>,
    label: String,
) -> Result<PwThreadHandle, MediaError> {
    ensure_init();

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<PwCommand>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let thread = std::thread::Builder::new()
        .name("pipewire-io-owner".into())
        .spawn(move || {
            if let Err(e) = run(
                shutdown_rx,
                capture_prod,
                playback_cons,
                shared,
                capture_target,
                playback_target,
                &label,
                &ready_tx,
            ) {
                let _ = ready_tx.send(Err(e.to_string()));
            }
        })
        .map_err(MediaError::Io)?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(PwThreadHandle {
            thread: Some(thread),
            shutdown: shutdown_tx,
        }),
        Ok(Err(e)) => Err(MediaError::PipeWire(e)),
        Err(_) => Err(MediaError::PipeWire(
            "pipewire thread exited before starting".into(),
        )),
    }
}

fn pw_err(e: pw::Error) -> MediaError {
    MediaError::PipeWire(e.to_string())
}

struct SecondaryPlaybackUserData {
    cons: HeapCons<i16>,
    prebuffered: bool,
}

/// Spawns a playback-*only* PipeWire stream on its own thread — no capture
/// side at all, unlike `spawn` — for `session.rs`'s secondary audio output
/// (e.g. routing the far end's voice into a Discord virtual-mic sink) so it
/// doesn't also create a spurious second microphone-capture node for a
/// feature that never needs one. `target` (a PipeWire node's `node.name`)
/// pins the stream to a specific sink; `None` follows the system default.
pub fn spawn_playback(
    playback_cons: HeapCons<i16>,
    target: Option<String>,
    label: String,
) -> Result<PwThreadHandle, MediaError> {
    ensure_init();

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<PwCommand>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let thread = std::thread::Builder::new()
        .name("pipewire-io-secondary".into())
        .spawn(move || {
            if let Err(e) = run_playback_only(shutdown_rx, playback_cons, target, &label, &ready_tx) {
                let _ = ready_tx.send(Err(e.to_string()));
            }
        })
        .map_err(MediaError::Io)?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(PwThreadHandle {
            thread: Some(thread),
            shutdown: shutdown_tx,
        }),
        Ok(Err(e)) => Err(MediaError::PipeWire(e)),
        Err(_) => Err(MediaError::PipeWire(
            "pipewire thread exited before starting".into(),
        )),
    }
}

/// Same buffering/prebuffer/catch-up behavior as `run`'s playback stream
/// (see its callback and the `PREBUFFER_SAMPLES`/`*_BUFFERED_SAMPLES` doc
/// comments) — deliberately duplicated rather than shared, since this
/// stream's lifecycle (spun up/torn down per-call, independent of the
/// primary capture+playback pair) doesn't fit cleanly into `run`'s combined
/// setup without a much larger refactor for a single extra stream.
fn run_playback_only(
    shutdown_rx: std::sync::mpsc::Receiver<PwCommand>,
    playback_cons: HeapCons<i16>,
    target: Option<String>,
    label: &str,
    ready_tx: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), MediaError> {
    // SAFETY: `pw::init()` has already been called via `ensure_init()` in
    // `spawn_playback()`, before this function runs.
    let thread_loop = unsafe { pw::thread_loop::ThreadLoopRc::new(Some("pipewire-io-secondary"), None) }
        .map_err(pw_err)?;
    let context_props = properties! { *pw::keys::CONFIG_NAME => "client.conf" };
    let context = pw::context::ContextRc::new(&thread_loop, Some(context_props)).map_err(pw_err)?;
    let core = context.connect_rc(None).map_err(pw_err)?;

    let playback_stream = pw::stream::StreamBox::new(
        &core,
        "OxideSip secondary playback",
        stream_properties("Playback", target.as_deref(), label),
    )
    .map_err(pw_err)?;

    let playback_user_data = SecondaryPlaybackUserData {
        cons: playback_cons,
        prebuffered: false,
    };

    let _playback_listener = playback_stream
        .add_local_listener_with_user_data(playback_user_data)
        .process(|stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let stride = mem::size_of::<i16>();
            let n_frames = if let Some(slice) = data.data() {
                let max_frames = slice.len() / stride;
                let mut scratch = [0i16; 1024];
                let mut written = 0usize;

                if !user_data.prebuffered && user_data.cons.occupied_len() >= PREBUFFER_SAMPLES {
                    user_data.prebuffered = true;
                }

                let occupied = user_data.cons.occupied_len();
                if occupied > MAX_BUFFERED_SAMPLES {
                    let mut to_discard = occupied - TARGET_BUFFERED_SAMPLES;
                    let mut trash = [0i16; 1024];
                    while to_discard > 0 {
                        let n = to_discard.min(trash.len());
                        let popped = user_data.cons.pop_slice(&mut trash[..n]);
                        if popped == 0 {
                            break;
                        }
                        to_discard -= popped;
                    }
                }

                while written < max_frames {
                    let want = (max_frames - written).min(scratch.len());
                    let got = if user_data.prebuffered {
                        user_data.cons.pop_slice(&mut scratch[..want])
                    } else {
                        0
                    };
                    for sample in &mut scratch[got..want] {
                        *sample = 0;
                    }
                    for (i, sample) in scratch[..want].iter().enumerate() {
                        let start = (written + i) * stride;
                        slice[start..start + stride].copy_from_slice(&sample.to_le_bytes());
                    }
                    written += want;
                }
                max_frames
            } else {
                0
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (stride * n_frames) as u32;
        })
        .register()
        .map_err(pw_err)?;

    let playback_format = audio_format_pod()?;
    let mut playback_params = [Pod::from_bytes(&playback_format)
        .ok_or_else(|| MediaError::PipeWire("invalid playback format pod".into()))?];
    playback_stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut playback_params,
        )
        .map_err(pw_err)?;

    let _ = ready_tx.send(Ok(()));
    thread_loop.start();

    let _ = shutdown_rx.recv();
    thread_loop.stop();

    Ok(())
}

struct SecondaryCaptureUserData {
    prod: HeapProd<i16>,
}

/// Spawns a capture-*only* PipeWire stream on its own thread — the mirror of
/// `spawn_playback`, used for `session.rs`'s secondary audio *input*: mixing
/// another app's own playback stream (e.g. Discord's "what other members are
/// saying," `Stream/Output/Audio` — never that app's *capture* class, which
/// would loop our own injected audio back on itself) into what gets sent as
/// RTP. `target` (a PipeWire node's `node.name`) pins the stream to that
/// app's playback node; `None` follows the system default capture device.
pub fn spawn_capture(
    prod: HeapProd<i16>,
    target: Option<String>,
    label: String,
) -> Result<PwThreadHandle, MediaError> {
    ensure_init();

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<PwCommand>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let thread = std::thread::Builder::new()
        .name("pipewire-io-secondary-in".into())
        .spawn(move || {
            if let Err(e) = run_capture_only(shutdown_rx, prod, target, &label, &ready_tx) {
                let _ = ready_tx.send(Err(e.to_string()));
            }
        })
        .map_err(MediaError::Io)?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(PwThreadHandle {
            thread: Some(thread),
            shutdown: shutdown_tx,
        }),
        Ok(Err(e)) => Err(MediaError::PipeWire(e)),
        Err(_) => Err(MediaError::PipeWire(
            "pipewire thread exited before starting".into(),
        )),
    }
}

/// Mirrors `run_playback_only`'s shape, but for a capture-only stream — the
/// `process` callback here is the same as `run`'s primary capture stream
/// (just pushing decoded samples into a ring buffer), minus the mic-mute/
/// gain/level-meter handling that's specific to the real local microphone.
fn run_capture_only(
    shutdown_rx: std::sync::mpsc::Receiver<PwCommand>,
    prod: HeapProd<i16>,
    target: Option<String>,
    label: &str,
    ready_tx: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), MediaError> {
    // SAFETY: `pw::init()` has already been called via `ensure_init()` in
    // `spawn_capture()`, before this function runs.
    let thread_loop = unsafe { pw::thread_loop::ThreadLoopRc::new(Some("pipewire-io-secondary-in"), None) }
        .map_err(pw_err)?;
    let context_props = properties! { *pw::keys::CONFIG_NAME => "client.conf" };
    let context = pw::context::ContextRc::new(&thread_loop, Some(context_props)).map_err(pw_err)?;
    let core = context.connect_rc(None).map_err(pw_err)?;

    let capture_stream = pw::stream::StreamBox::new(
        &core,
        "OxideSip secondary capture",
        stream_properties("Capture", target.as_deref(), label),
    )
    .map_err(pw_err)?;

    let capture_user_data = SecondaryCaptureUserData { prod };

    let _capture_listener = capture_stream
        .add_local_listener_with_user_data(capture_user_data)
        .process(|stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let n_bytes = data.chunk().size() as usize;
            let Some(samples) = data.data() else {
                return;
            };
            let n_bytes = n_bytes.min(samples.len());
            let iter = samples[..n_bytes]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]));
            user_data.prod.push_iter(iter);
        })
        .register()
        .map_err(pw_err)?;

    let capture_format = audio_format_pod()?;
    let mut capture_params = [Pod::from_bytes(&capture_format)
        .ok_or_else(|| MediaError::PipeWire("invalid capture format pod".into()))?];
    capture_stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut capture_params,
        )
        .map_err(pw_err)?;

    let _ = ready_tx.send(Ok(()));
    thread_loop.start();

    let _ = shutdown_rx.recv();
    thread_loop.stop();

    Ok(())
}

fn audio_format_pod() -> Result<Vec<u8>, MediaError> {
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(CHANNELS);
    let mut position = [0u32; spa::param::audio::MAX_CHANNELS];
    position[0] = SPA_AUDIO_CHANNEL_MONO;
    audio_info.set_position(position);

    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|_| MediaError::PipeWire("failed to serialize audio format pod".into()))
}

fn stream_properties(
    media_category: &str,
    target: Option<&str>,
    label: &str,
) -> pw::properties::PropertiesBox {
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => media_category,
        // Deliberately *not* "Communication": on a KDE/WirePlumber desktop
        // that role is treated as a real phone call for the built-in
        // role-based ducking policy (see
        // /usr/share/wireplumber/wireplumber.conf.d/media-role-nodes.conf) —
        // it's routed into `loopback.sink.role.phone`, priority 25, whose
        // `policy.role-based.action.lower-priority = "cork"` fully silences
        // (not just quiets) any lower-priority app's audio, e.g. Discord,
        // for as long as our stream is active. That's intentional OS
        // behavior for a *real* system phone integration, but not what a
        // softphone app should impose unasked — the user should be able to
        // stay on a call and still hear other apps. Omitting the role lands
        // us in the default "Multimedia" bucket (priority 10, `mix`), which
        // just mixes with everything else normally.
        // `label` (e.g. "OxideSip Line 2") rather than a hardcoded
        // "OxideSip" so concurrent lines' capture/playback streams are
        // distinguishable in pw-top/pavucontrol/qpwgraph instead of all
        // showing up as identically-named nodes.
        *pw::keys::NODE_NAME => label,
        *pw::keys::NODE_DESCRIPTION => label,
        // Hint a 20ms (160 samples @ 8000Hz) processing granularity,
        // matching our real RTP packetization interval. Without this,
        // PipeWire was negotiating a default buffer far larger than
        // anything we control (observed ~12288 frames / ~1.5s per
        // callback on this system) — since that's larger than the ring
        // buffer feeding it can ever hold, every single playback callback
        // was guaranteed to underrun into mostly silence, which is what
        // was actually causing both the dropouts and the multi-second
        // echo-test latency (not the capture-timing/clock-drift issues
        // addressed earlier, which were real but secondary).
        *pw::keys::NODE_LATENCY => "160/8000",
    };
    if let Some(target) = target {
        // Pin to a specific device instead of relying purely on
        // StreamFlags::AUTOCONNECT picking the system default.
        props.insert(*pw::keys::TARGET_OBJECT, target);
    }
    props
}

#[allow(clippy::too_many_arguments)]
fn run(
    shutdown_rx: std::sync::mpsc::Receiver<PwCommand>,
    capture_prod: HeapProd<i16>,
    playback_cons: HeapCons<i16>,
    shared: PipewireShared,
    capture_target: Option<String>,
    playback_target: Option<String>,
    label: &str,
    ready_tx: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), MediaError> {
    let PipewireShared {
        notify,
        mic_muted,
        input_gain_bits,
        input_level_bits,
        output_level_bits,
    } = shared;

    // SAFETY: `pw::init()` has already been called via `ensure_init()` in
    // `spawn()`, before this function runs.
    let thread_loop = unsafe { pw::thread_loop::ThreadLoopRc::new(Some("pipewire-io"), None) }
        .map_err(pw_err)?;
    // A bare `pw_context_new` with no config loads no modules at all — in
    // particular not `libpipewire-module-rt`, which is what actually talks
    // to rtkit to grant the processing thread realtime scheduling. Normal
    // PipeWire client executables get this for free because they load the
    // standard `client.conf` (confirmed by reading it directly on this
    // system: it loads `libpipewire-module-rt`, commented-out args meaning
    // rtkit-based defaults). Switching to `ThreadLoopRc` alone (see
    // `PwThreadHandle`'s doc comment) was necessary but not sufficient —
    // verified empirically: even `pw_thread_loop`'s own internal thread
    // stayed at plain `SCHED_OTHER`/priority 19 until this config was loaded
    // too. `pw::keys::CONFIG_NAME` is the documented way to ask
    // `pw_context_new` to load a named config file's modules/settings.
    let context_props = properties! { *pw::keys::CONFIG_NAME => "client.conf" };
    let context = pw::context::ContextRc::new(&thread_loop, Some(context_props)).map_err(pw_err)?;
    let core = context.connect_rc(None).map_err(pw_err)?;

    let capture_stream = pw::stream::StreamBox::new(
        &core,
        "OxideSip capture",
        stream_properties("Capture", capture_target.as_deref(), label),
    )
    .map_err(pw_err)?;

    let capture_user_data = CaptureUserData {
        prod: capture_prod,
        notify,
        mic_muted,
        input_gain_bits,
        input_level_bits,
    };

    let _capture_listener = capture_stream
        .add_local_listener_with_user_data(capture_user_data)
        .process(|stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let n_bytes = data.chunk().size() as usize;
            let Some(samples) = data.data() else {
                return;
            };
            let n_bytes = n_bytes.min(samples.len());
            let muted = user_data.mic_muted.load(Ordering::Relaxed);
            let gain = f32::from_bits(user_data.input_gain_bits.load(Ordering::Relaxed));
            let mut peak: u16 = 0;
            let iter = samples[..n_bytes].chunks_exact(2).map(|b| {
                let s = if muted {
                    0
                } else {
                    let raw = i16::from_le_bytes([b[0], b[1]]) as f32 * gain;
                    raw.clamp(i16::MIN as f32, i16::MAX as f32) as i16
                };
                peak = peak.max(s.unsigned_abs());
                s
            });
            user_data.prod.push_iter(iter);
            let level = peak as f32 / i16::MAX as f32;
            user_data
                .input_level_bits
                .store(level.to_bits(), Ordering::Relaxed);
            user_data.notify.notify_one();
        })
        .register()
        .map_err(pw_err)?;

    let capture_format = audio_format_pod()?;
    let mut capture_params = [Pod::from_bytes(&capture_format)
        .ok_or_else(|| MediaError::PipeWire("invalid capture format pod".into()))?];
    capture_stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut capture_params,
        )
        .map_err(pw_err)?;

    let playback_stream = pw::stream::StreamBox::new(
        &core,
        "OxideSip playback",
        stream_properties("Playback", playback_target.as_deref(), label),
    )
    .map_err(pw_err)?;

    let playback_user_data = PlaybackUserData {
        cons: playback_cons,
        output_level_bits,
        prebuffered: false,
        underrun_frames: 0,
        callback_count: 0,
    };

    let _playback_listener = playback_stream
        .add_local_listener_with_user_data(playback_user_data)
        .process(|stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let stride = mem::size_of::<i16>();
            let n_frames = if let Some(slice) = data.data() {
                let max_frames = slice.len() / stride;
                let mut scratch = [0i16; 1024];
                let mut written = 0usize;
                let mut peak: u16 = 0;

                if !user_data.prebuffered && user_data.cons.occupied_len() >= PREBUFFER_SAMPLES {
                    user_data.prebuffered = true;
                }

                // Clock-drift catch-up: if backlog has grown past the
                // ceiling, fast-forward through the stale excess rather than
                // playing through ever-increasing latency. See the constants'
                // doc comment for why this is needed at all.
                let occupied = user_data.cons.occupied_len();
                if occupied > MAX_BUFFERED_SAMPLES {
                    let mut to_discard = occupied - TARGET_BUFFERED_SAMPLES;
                    let mut trash = [0i16; 1024];
                    while to_discard > 0 {
                        let n = to_discard.min(trash.len());
                        let popped = user_data.cons.pop_slice(&mut trash[..n]);
                        if popped == 0 {
                            break;
                        }
                        to_discard -= popped;
                    }
                    tracing::info!(
                        occupied,
                        discarded = occupied - TARGET_BUFFERED_SAMPLES - to_discard,
                        "playback buffer catch-up: dropped stale backlog"
                    );
                }

                while written < max_frames {
                    let want = (max_frames - written).min(scratch.len());
                    let got = if user_data.prebuffered {
                        user_data.cons.pop_slice(&mut scratch[..want])
                    } else {
                        0
                    };
                    if got < want {
                        user_data.underrun_frames += (want - got) as u64;
                    }
                    for sample in &mut scratch[got..want] {
                        *sample = 0; // underrun/not-yet-prebuffered: silence-fill rather than block/repeat
                    }
                    for &sample in &scratch[..want] {
                        peak = peak.max(sample.unsigned_abs());
                    }
                    for (i, sample) in scratch[..want].iter().enumerate() {
                        let start = (written + i) * stride;
                        slice[start..start + stride].copy_from_slice(&sample.to_le_bytes());
                    }
                    written += want;
                }
                let level = peak as f32 / i16::MAX as f32;
                user_data
                    .output_level_bits
                    .store(level.to_bits(), Ordering::Relaxed);
                user_data.callback_count += 1;
                if user_data.callback_count.is_multiple_of(10) {
                    // This system's real callback cadence is ~1.5s, not the
                    // ~20ms a PipeWire client normally gets (see
                    // PREBUFFER_SAMPLES's doc comment) — every 10 calls is
                    // roughly every 15s, reasonable visibility without
                    // spamming logs. Counterpart to send_loop's stats.
                    tracing::info!(
                        callback_count = user_data.callback_count,
                        underrun_frames = user_data.underrun_frames,
                        "playback callback stats"
                    );
                }
                max_frames
            } else {
                0
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (stride * n_frames) as u32;
        })
        .register()
        .map_err(pw_err)?;

    let playback_format = audio_format_pod()?;
    let mut playback_params = [Pod::from_bytes(&playback_format)
        .ok_or_else(|| MediaError::PipeWire("invalid playback format pod".into()))?];
    playback_stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut playback_params,
        )
        .map_err(pw_err)?;

    let _ = ready_tx.send(Ok(()));
    // Spawns pw_thread_loop's own internal (realtime-scheduled) thread to
    // actually run the loop and dispatch the stream callbacks above — see
    // `PwThreadHandle`'s doc comment for why this replaced `MainLoopRc::run()`
    // on a plain `std::thread`. Returns immediately; this "owner" thread
    // does no audio processing itself, it just blocks below holding the
    // `Rc`-based handles alive until told to shut down.
    thread_loop.start();

    let _ = shutdown_rx.recv();
    // Must be called without the loop's lock held (see `ThreadLoop::stop`'s
    // own doc comment) — we never lock on this thread, so that's satisfied.
    // Blocks until the internal RT thread has actually exited.
    thread_loop.stop();

    Ok(())
}
