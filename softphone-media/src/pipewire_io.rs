//! All PipeWire-crate-touching code, isolated from `session.rs`'s tokio
//! orchestration. Runs a capture + playback stream pair on one dedicated
//! blocking OS thread, bridging PCM i16 samples to/from ring buffers, and
//! signaling `session.rs`'s RTP send loop via a shared `Notify` so
//! packetization tracks PipeWire's real audio callback cadence instead of
//! an independent timer.

use crate::error::MediaError;
use crate::stats::StatsShared;
use pipewire as pw;
use pw::{properties::properties, spa};
use ringbuf::traits::{Consumer, Observer, Producer};
use ringbuf::{HeapCons, HeapProd};
use spa::pod::Pod;
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Once};
use tokio::sync::Notify;

pub(crate) const SAMPLE_RATE: u32 = 8000;
const CHANNELS: u32 = 1;
// SPA_AUDIO_CHANNEL_MONO. Confirmed from the generated libspa-sys bindgen
// output (target/debug/build/libspa-sys-*/out/bindings.rs) since the
// constant isn't in plain-text vendored source (bindgen-generated at build
// time). Hardcoded rather than adding a direct dependency on the `-sys`
// crate for one constant.
const SPA_AUDIO_CHANNEL_MONO: u32 = 2;
const STRIDE: usize = mem::size_of::<i16>();

// Playback buffering, in samples (8kHz mono, so 8 samples = 1ms). This is
// the receive-side latency the app itself adds, so it is kept as small as
// ordinary network jitter allows.
//
// History, because these used to be ~2s and the code around them said so:
// the playback callback filled the *whole* mapped stream buffer (~12288
// frames) on every call instead of the `requested` frames the graph asked
// for (43-171, depending on the graph's quantum). PipeWire doesn't call `process()`
// again until what was queued has played, so the callback fired once every
// 12288 / 8000Hz = 1.536s, and every buffer here had to be sized to survive
// that. It was long misread as a WirePlumber/scheduling quirk; `pw-play` was
// fast on the same route simply because it honours `requested`.
// `fill_playback` does now too, so the callback runs once per graph cycle
// (every 5-21ms).
//
// The buffer is adaptive rather than a fixed depth, since how much jitter
// there is to absorb depends on the network and the PBX and changes during a
// call. See `fill_playback` for the mechanism; in short, it keeps the
// *lowest* depth the ring reaches (its trough) at `margin`, and learns
// `margin` from underruns:
//
// - Every ADAPT_WINDOW it looks at the trough over that window. Anything
//   above `margin` there was never used — pure delay — and is removed; a
//   trough below `margin` (the far end's clock running slower than ours,
//   draining the ring) is topped back up before it becomes an underrun.
//   Both are done by dropping or repeating one sample in every
//   SLEW_INTERVAL, which is inaudible, unless the excess is large.
// - An underrun raises `margin` by however long the ring stayed dry, i.e. by
//   how late the audio actually was.
// - `margin` then decays slowly, so a brief bad patch doesn't cost latency
//   for the rest of the call.
//
// On a quiet LAN this settles at a ~10ms trough (~20ms average depth); on a
// jittery link it settles at whatever that link needs.
const PACKET_SAMPLES: usize = 160; // one 20ms RTP packet
const MARGIN_START: usize = 160; // 20ms
const MARGIN_MIN: usize = 80; // 10ms
const MARGIN_MAX: usize = 1_600; // 200ms
// Smallest raise per underrun, and the smallest excess worth skipping.
const MARGIN_STEP: usize = 80; // 10ms
// A ring dry for longer than this was a paused stream (hold, silence
// suppression), not jitter — don't learn from it.
const STALL_SAMPLES: usize = 2_400; // 300ms
// More than this far above the starting depth is a burst (a network stall
// releasing, or audio that queued before the stream started): skip it now
// rather than waiting for the next window.
const BURST_HEADROOM: usize = 800; // 100ms
const ADAPT_WINDOW: usize = 2 * SAMPLE_RATE as usize; // 2s of played frames
// Drop/repeat at most one sample per this many played: a 1.25% speed change.
const SLEW_INTERVAL: usize = 80;
// Smallest trough error worth correcting.
const SLEW_MIN: usize = 16; // 2ms
// An excess too big to slew away within one window is skipped instead.
const SKIP_ABOVE: usize = 200; // 25ms
// Lower `margin` by MARGIN_DECAY once per this many underrun-free windows:
// 1ms per 32s, so a 10ms raise is held for about five minutes.
const DECAY_EVERY_WINDOWS: u32 = 16;
const MARGIN_DECAY: usize = 8; // 1ms
// When the ring runs dry mid-call, the last packet's worth of audio is
// replayed, fading to silence over this long, instead of cutting straight to
// silence: a short gap then sounds like a soft dip rather than a click.
const CONCEAL_SAMPLES: usize = 480; // 60ms
// Frames to write if a server doesn't report `requested` (it has since
// PipeWire 0.3.49): one RTP packet's worth, never the whole buffer.
const FALLBACK_FRAMES: usize = 160;
// Log playback stats once per this many frames played (~5s), whatever
// quantum the graph happens to be running at.
const STATS_EVERY_FRAMES: usize = 5 * SAMPLE_RATE as usize;

static PW_INIT: Once = Once::new();

pub(crate) fn ensure_init() {
    PW_INIT.call_once(pw::init);
}

/// A signal sent to the thin "owner" thread (see `spawn`/`run` below) asking
/// it to stop the `pw_thread_loop` and exit.
enum PwCommand {
    Terminate,
}

/// A plain `std::thread::JoinHandle` + `mpsc::Sender` for the thin "owner"
/// thread. That thread does no audio work: `run()` creates a
/// [`pw::thread_loop::ThreadLoopRc`] and calls `.start()`, which spawns
/// *PipeWire's own* internal thread to run the stream callbacks — with
/// realtime scheduling via rtkit once `client.conf` is loaded (see `run`).
/// The owner thread just holds the `Rc`-based handles alive (`ThreadLoopRc`/
/// the `Context`/streams aren't `Send`, so they can't be moved elsewhere for
/// shutdown) and blocks on `shutdown_rx` until told to stop, so its own
/// ordinary `SCHED_OTHER` scheduling doesn't matter.
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
    /// Linear playback volume, applied in the playback callback itself so a
    /// change is heard on the next graph cycle and touches nothing but what
    /// comes out of the local speakers (not recordings or bridged lines).
    pub output_gain_bits: Arc<AtomicU32>,
    /// Peak level played since the UI last read it (see `store_peak`).
    pub output_level_bits: Arc<AtomicU32>,
    pub stats: Arc<StatsShared>,
}

/// Records a 0.0–1.0 peak level for a meter as "highest since last read":
/// the reader swaps it back to zero. A plain store would hand the UI
/// whichever few-millisecond chunk happened to be last, which is most often
/// a quiet instant between syllables even while someone is speaking.
/// (Non-negative `f32` bit patterns order the same way the values do.)
pub(crate) fn store_peak(level_bits: &AtomicU32, peak: u16) {
    let level = f32::from(peak) / f32::from(i16::MAX);
    level_bits.fetch_max(level.to_bits(), Ordering::Relaxed);
}

/// Scales the first `frames` S16LE samples of `out` by `gain` in place and
/// returns their peak afterwards.
fn apply_gain(out: &mut [u8], frames: usize, gain: f32) -> u16 {
    let mut peak = 0u16;
    for bytes in out[..frames * STRIDE].chunks_exact_mut(STRIDE) {
        let sample = i16::from_le_bytes([bytes[0], bytes[1]]);
        let scaled = (f32::from(sample) * gain).clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
        bytes.copy_from_slice(&scaled.to_le_bytes());
        peak = peak.max(scaled.unsigned_abs());
    }
    peak
}

struct CaptureUserData {
    prod: HeapProd<i16>,
    notify: Arc<Notify>,
    mic_muted: Arc<AtomicBool>,
}

struct PlaybackUserData {
    cons: HeapCons<i16>,
    output_gain_bits: Arc<AtomicU32>,
    output_level_bits: Arc<AtomicU32>,
    stats: Arc<StatsShared>,
    state: PlaybackState,
    callback_count: u64,
    frames_since_stats: usize,
}

/// Per-stream adaptive playback buffering state, owned by the realtime
/// callback. See the constants above for the idea.
struct PlaybackState {
    /// `false` until the ring holds `margin` plus one packet, and again
    /// after every underrun — silence is played while it's `false`.
    prebuffered: bool,
    /// Depth, in samples, the ring's trough is steered to.
    margin: usize,
    /// Lowest depth seen (after serving a callback) in the current window.
    low_water: usize,
    /// Frames played in the current window.
    window_frames: usize,
    /// Samples still to drop (positive) or repeat (negative), one per
    /// `SLEW_INTERVAL` played frames; `slew_credit` counts those frames.
    slew: isize,
    slew_credit: usize,
    /// Last sample played, for repeating.
    last_sample: i16,
    /// The most recent `PACKET_SAMPLES` of real audio played (a ring;
    /// `history_pos` is the oldest sample / next write), and how many
    /// concealment samples the current gap has been given from it.
    history: [i16; PACKET_SAMPLES],
    history_pos: usize,
    concealed: usize,
    /// Consecutive windows without an underrun, for `margin`'s decay.
    clean_windows: u32,
    /// Set by an underrun until audio arrives again; `dry_samples` counts
    /// how much silence was played in between.
    recovering: bool,
    dry_samples: usize,
    /// Number of times the ring ran dry mid-playback.
    underruns: u64,
    /// Samples discarded to shed delay (slewed, trimmed or burst-skipped).
    trimmed_samples: u64,
    /// Samples repeated to top the ring back up.
    stretched_samples: u64,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            prebuffered: false,
            margin: MARGIN_START,
            low_water: usize::MAX,
            window_frames: 0,
            slew: 0,
            slew_credit: 0,
            last_sample: 0,
            history: [0; PACKET_SAMPLES],
            history_pos: 0,
            concealed: 0,
            clean_windows: 0,
            recovering: false,
            dry_samples: 0,
            underruns: 0,
            trimmed_samples: 0,
            stretched_samples: 0,
        }
    }
}

impl PlaybackState {
    fn reset_window(&mut self) {
        self.low_water = usize::MAX;
        self.window_frames = 0;
    }
}

/// What one `fill_playback` call wrote.
#[derive(Default)]
struct PlaybackFill {
    /// Frames written to the output buffer (the chunk size to report).
    frames: usize,
    /// Peak absolute sample value among them, for the level meter.
    peak: u16,
}

/// Fills one playback callback's worth of audio from `cons` into `out`
/// (the stream buffer's mapped bytes, S16LE mono). Shared by the primary and
/// secondary playback streams; touches no PipeWire types so it can be
/// unit-tested.
///
/// Writes `requested` frames, not `out.len() / STRIDE`: the mapped buffer is
/// far larger (~12288 frames) than one graph cycle (43-171 frames), and a
/// stream that queues the whole thing is not called again until all of it
/// has played — 1.5s later. See the buffering constants' comment.
fn fill_playback(cons: &mut HeapCons<i16>, state: &mut PlaybackState, out: &mut [u8], requested: u64) -> PlaybackFill {
    let max_frames = out.len() / STRIDE;
    let frames = if requested > 0 {
        usize::try_from(requested).unwrap_or(usize::MAX).min(max_frames)
    } else {
        max_frames.min(FALLBACK_FRAMES)
    };

    if !state.prebuffered && state.recovering && cons.occupied_len() > 0 {
        // Audio is back after an underrun. How long the ring stayed dry
        // (counted below) is how much more cushion would have covered it.
        if state.dry_samples <= STALL_SAMPLES {
            state.margin = (state.margin + state.dry_samples.max(MARGIN_STEP)).min(MARGIN_MAX);
        }
        state.recovering = false;
    }

    // Depth to (re)start playing at: with packets landing 160 samples at a
    // time, starting one packet above `margin` puts the trough at `margin`.
    let start_depth = state.margin + PACKET_SAMPLES;
    let occupied = cons.occupied_len();
    if occupied > start_depth + BURST_HEADROOM {
        state.trimmed_samples += cons.skip(occupied - start_depth) as u64;
        state.slew = 0;
        state.reset_window();
    }
    if !state.prebuffered && cons.occupied_len() >= start_depth {
        state.prebuffered = true;
        state.slew = 0;
        state.reset_window();
    }

    // Pending slew: this callback's share of samples to drop or repeat.
    let mut repeat = 0usize;
    if state.prebuffered && state.slew != 0 {
        state.slew_credit += frames;
        let n = (state.slew_credit / SLEW_INTERVAL)
            .min(state.slew.unsigned_abs())
            .min(frames);
        state.slew_credit -= n * SLEW_INTERVAL;
        if state.slew > 0 {
            state.trimmed_samples += cons.skip(n) as u64;
            state.slew -= n as isize;
        } else {
            repeat = n;
            state.slew += n as isize;
            state.stretched_samples += n as u64;
        }
        if state.slew == 0 {
            // Measure the result from scratch, not mixed with what the
            // trough was before the correction.
            state.slew_credit = 0;
            state.reset_window();
        }
    }

    let mut scratch = [0i16; 256];
    let mut written = 0usize;
    let mut peak: u16 = 0;
    // The last `repeat` frames of this callback are not taken from the ring.
    let from_ring = frames - repeat;
    while written < from_ring {
        let want = (from_ring - written).min(scratch.len());
        let got = if state.prebuffered {
            cons.pop_slice(&mut scratch[..want])
        } else {
            0
        };
        for &sample in &scratch[..got] {
            state.history[state.history_pos] = sample;
            state.history_pos = (state.history_pos + 1) % PACKET_SAMPLES;
        }
        if got > 0 {
            state.concealed = 0;
        }
        // Whatever the ring couldn't supply: concealment while a gap that
        // opened mid-playback is still fresh, silence otherwise (start of
        // call, or a gap longer than CONCEAL_SAMPLES).
        let in_gap = state.recovering || (state.prebuffered && got < want);
        for sample in &mut scratch[got..want] {
            *sample = if in_gap && state.concealed < CONCEAL_SAMPLES {
                let replay = state.history[(state.history_pos + state.concealed) % PACKET_SAMPLES];
                let gain = (CONCEAL_SAMPLES - state.concealed) as i32;
                state.concealed += 1;
                (i32::from(replay) * gain / CONCEAL_SAMPLES as i32) as i16
            } else {
                0
            };
        }
        if state.prebuffered && got < want {
            // Ran dry. Go back to prebuffering so the cushion is rebuilt
            // once, instead of running at zero depth and clicking on every
            // callback that lands a moment before its packet does.
            state.prebuffered = false;
            state.slew = 0;
            state.recovering = true;
            state.dry_samples = want - got;
            state.clean_windows = 0;
            state.underruns += 1;
        } else if state.recovering {
            state.dry_samples = state.dry_samples.saturating_add(want - got);
        }
        for (i, &sample) in scratch[..want].iter().enumerate() {
            peak = peak.max(sample.unsigned_abs());
            let start = (written + i) * STRIDE;
            out[start..start + STRIDE].copy_from_slice(&sample.to_le_bytes());
        }
        if want > 0 {
            state.last_sample = scratch[want - 1];
        }
        written += want;
    }
    for frame in from_ring..frames {
        peak = peak.max(state.last_sample.unsigned_abs());
        let start = frame * STRIDE;
        out[start..start + STRIDE].copy_from_slice(&state.last_sample.to_le_bytes());
    }

    if state.prebuffered && state.slew == 0 {
        state.low_water = state.low_water.min(cons.occupied_len());
        state.window_frames += frames;
        if state.window_frames >= ADAPT_WINDOW {
            // The trough never went below `low_water` this window, so
            // whatever part of it is above `margin` was delay nobody needed;
            // and if it is below `margin`, the ring is draining.
            let excess = state.low_water as isize - state.margin as isize;
            if excess > SKIP_ABOVE as isize {
                state.trimmed_samples += cons.skip(excess as usize) as u64;
            } else if excess.unsigned_abs() >= SLEW_MIN {
                state.slew = excess;
            }
            state.clean_windows += 1;
            if state.clean_windows.is_multiple_of(DECAY_EVERY_WINDOWS) {
                state.margin = state.margin.saturating_sub(MARGIN_DECAY).max(MARGIN_MIN);
            }
            state.reset_window();
        }
    }
    PlaybackFill { frames, peak }
}

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
        Err(_) => Err(MediaError::PipeWire("pipewire thread exited before starting".into())),
    }
}

/// A running echo canceller: the names of the virtual microphone and
/// speaker it provides, and the handle keeping it alive.
pub struct EchoCancel {
    pub handle: PwThreadHandle,
    /// Capture from this instead of the real microphone...
    pub source: String,
    /// ...and play to this instead of the real speakers.
    pub sink: String,
}

/// Loads PipeWire's own echo-cancel module (WebRTC audio processing) into a
/// context of ours, the same way `pw-loopback` hosts the loopback module. It
/// sits between the real devices and the call: what we play to its sink goes
/// on to the speakers and is also used as the reference to subtract from the
/// microphone signal, which comes out of its source with the far end's
/// voice removed. Everything is torn down when the handle is stopped.
///
/// `capture_target`/`playback_target` are the real devices (`None` = system
/// default). Fails if this PipeWire build has no echo-cancel module.
pub fn spawn_echo_cancel(
    capture_target: Option<String>,
    playback_target: Option<String>,
    label: &str,
) -> Result<EchoCancel, MediaError> {
    ensure_init();

    // Node names must be unique in the graph: two lines can be in a call at
    // once, each with its own canceller.
    let id = format!("oxidesip-ec-{:08x}", rand::random::<u32>());
    let (source, sink) = (format!("{id}-source"), format!("{id}-sink"));
    let target = |t: &Option<String>| match t {
        Some(name) => format!(" target.object = \"{}\"", name.replace('"', "")),
        None => String::new(),
    };
    let description = label.replace('"', "");
    let args = format!(
        "{{ library.name = aec/libspa-aec-webrtc \
           monitor.mode = false \
           capture.props = {{ node.name = \"{id}-capture\" node.passive = true{} }} \
           source.props = {{ node.name = \"{source}\" node.description = \"{description} (echo-cancelled mic)\" }} \
           sink.props = {{ node.name = \"{sink}\" node.description = \"{description} (echo-cancel speaker)\" }} \
           playback.props = {{ node.name = \"{id}-playback\" node.passive = true{} }} }}",
        target(&capture_target),
        target(&playback_target),
    );

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<PwCommand>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let thread = std::thread::Builder::new()
        .name("pipewire-io-aec".into())
        .spawn(move || {
            let run = || -> Result<(), MediaError> {
                // SAFETY: `pw::init()` was called via `ensure_init()` above.
                let thread_loop =
                    unsafe { pw::thread_loop::ThreadLoopRc::new(Some("pipewire-io-aec"), None) }.map_err(pw_err)?;
                let context_props = properties! { *pw::keys::CONFIG_NAME => "client.conf" };
                let context = pw::context::ContextRc::new(&thread_loop, Some(context_props)).map_err(pw_err)?;
                let name = c"libpipewire-module-echo-cancel";
                let args = std::ffi::CString::new(args)
                    .map_err(|_| MediaError::PipeWire("echo-cancel arguments contain a NUL byte".into()))?;
                // SAFETY: `context` is a live pw_context for the whole call;
                // both strings are NUL-terminated and outlive it; the loop
                // is not running yet, so nothing else is using the context.
                // The module is owned by the context and freed with it.
                let module = unsafe {
                    pw::sys::pw_context_load_module(
                        context.as_raw_ptr(),
                        name.as_ptr(),
                        args.as_ptr(),
                        std::ptr::null_mut(),
                    )
                };
                if module.is_null() {
                    return Err(MediaError::PipeWire(
                        "could not load PipeWire's echo-cancel module (is it installed?)".into(),
                    ));
                }
                let _ = ready_tx.send(Ok(()));
                thread_loop.start();
                let _ = shutdown_rx.recv();
                thread_loop.stop();
                Ok(())
            };
            if let Err(e) = run() {
                let _ = ready_tx.send(Err(e.to_string()));
            }
        })
        .map_err(MediaError::Io)?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(EchoCancel {
            handle: PwThreadHandle {
                thread: Some(thread),
                shutdown: shutdown_tx,
            },
            source,
            sink,
        }),
        Ok(Err(e)) => Err(MediaError::PipeWire(e)),
        Err(_) => Err(MediaError::PipeWire("echo-cancel thread exited before starting".into())),
    }
}

fn pw_err(e: pw::Error) -> MediaError {
    MediaError::PipeWire(e.to_string())
}

struct SecondaryPlaybackUserData {
    cons: HeapCons<i16>,
    state: PlaybackState,
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
        Err(_) => Err(MediaError::PipeWire("pipewire thread exited before starting".into())),
    }
}

/// Same buffering behavior as `run`'s playback stream (both go through
/// `fill_playback`). The stream setup itself is deliberately separate rather
/// than folded into `run`, since this stream's lifecycle (spun up/torn down
/// per-call, independent of the primary capture+playback pair) doesn't fit
/// `run`'s combined setup.
fn run_playback_only(
    shutdown_rx: std::sync::mpsc::Receiver<PwCommand>,
    playback_cons: HeapCons<i16>,
    target: Option<String>,
    label: &str,
    ready_tx: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), MediaError> {
    // SAFETY: `pw::init()` has already been called via `ensure_init()` in
    // `spawn_playback()`, before this function runs.
    let thread_loop =
        unsafe { pw::thread_loop::ThreadLoopRc::new(Some("pipewire-io-secondary"), None) }.map_err(pw_err)?;
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
        state: PlaybackState::default(),
    };

    let _playback_listener = playback_stream
        .add_local_listener_with_user_data(playback_user_data)
        .process(|stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            // How many frames the graph wants this cycle. Must be read
            // before `datas_mut()` borrows the buffer — and must be honoured,
            // see `fill_playback`.
            let requested = buffer.requested();
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let fill = match data.data() {
                Some(slice) => fill_playback(&mut user_data.cons, &mut user_data.state, slice, requested),
                None => PlaybackFill::default(),
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = STRIDE as i32;
            *chunk.size_mut() = (STRIDE * fill.frames) as u32;
        })
        .register()
        .map_err(pw_err)?;

    let playback_format = audio_format_pod()?;
    let mut playback_params =
        [Pod::from_bytes(&playback_format)
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
pub fn spawn_capture(prod: HeapProd<i16>, target: Option<String>, label: String) -> Result<PwThreadHandle, MediaError> {
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
        Err(_) => Err(MediaError::PipeWire("pipewire thread exited before starting".into())),
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
    let thread_loop =
        unsafe { pw::thread_loop::ThreadLoopRc::new(Some("pipewire-io-secondary-in"), None) }.map_err(pw_err)?;
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
    let mut capture_params = [
        Pod::from_bytes(&capture_format).ok_or_else(|| MediaError::PipeWire("invalid capture format pod".into()))?
    ];
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

fn stream_properties(media_category: &str, target: Option<&str>, label: &str) -> pw::properties::PropertiesBox {
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
        // matching our RTP packetization interval, so the graph doesn't
        // pick a larger quantum than a call needs on our account.
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
        output_gain_bits,
        output_level_bits,
        stats,
    } = shared;

    // SAFETY: `pw::init()` has already been called via `ensure_init()` in
    // `spawn()`, before this function runs.
    let thread_loop = unsafe { pw::thread_loop::ThreadLoopRc::new(Some("pipewire-io"), None) }.map_err(pw_err)?;
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
            // Levelling, mic gain and the level meter all happen in
            // `session.rs`'s send loop, on whole 20ms frames; only mute is
            // applied here.
            let muted = user_data.mic_muted.load(Ordering::Relaxed);
            let iter = samples[..n_bytes]
                .chunks_exact(2)
                .map(|b| if muted { 0 } else { i16::from_le_bytes([b[0], b[1]]) });
            user_data.prod.push_iter(iter);
            user_data.notify.notify_one();
        })
        .register()
        .map_err(pw_err)?;

    let capture_format = audio_format_pod()?;
    let mut capture_params = [
        Pod::from_bytes(&capture_format).ok_or_else(|| MediaError::PipeWire("invalid capture format pod".into()))?
    ];
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
        output_gain_bits,
        output_level_bits,
        stats,
        state: PlaybackState::default(),
        callback_count: 0,
        frames_since_stats: 0,
    };

    let _playback_listener = playback_stream
        .add_local_listener_with_user_data(playback_user_data)
        .process(|stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            // How many frames the graph wants this cycle. Must be read
            // before `datas_mut()` borrows the buffer — and must be honoured,
            // see `fill_playback`.
            let requested = buffer.requested();
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let fill = match data.data() {
                Some(slice) => {
                    let mut fill = fill_playback(&mut user_data.cons, &mut user_data.state, slice, requested);
                    let gain = f32::from_bits(user_data.output_gain_bits.load(Ordering::Relaxed));
                    fill.peak = apply_gain(slice, fill.frames, gain);
                    fill
                }
                None => PlaybackFill::default(),
            };
            store_peak(&user_data.output_level_bits, fill.peak);
            let stats = &user_data.stats;
            stats
                .buffered_samples
                .store(user_data.cons.occupied_len() as u32, Ordering::Relaxed);
            stats
                .margin_samples
                .store(user_data.state.margin as u32, Ordering::Relaxed);
            stats
                .underruns
                .store(user_data.state.underruns as u32, Ordering::Relaxed);
            user_data.callback_count += 1;
            user_data.frames_since_stats += fill.frames;
            if user_data.frames_since_stats >= STATS_EVERY_FRAMES {
                user_data.frames_since_stats = 0;
                // `buffered_ms` is the receive-side latency this stream is
                // adding right now; counterpart to send_loop's stats.
                tracing::info!(
                    callback_count = user_data.callback_count,
                    requested,
                    frames = fill.frames,
                    buffered_ms = user_data.cons.occupied_len() * 1000 / SAMPLE_RATE as usize,
                    margin_ms = user_data.state.margin * 1000 / SAMPLE_RATE as usize,
                    underruns = user_data.state.underruns,
                    stretched_samples = user_data.state.stretched_samples,
                    trimmed_samples = user_data.state.trimmed_samples,
                    "playback callback stats"
                );
            }

            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = STRIDE as i32;
            *chunk.size_mut() = (STRIDE * fill.frames) as u32;
        })
        .register()
        .map_err(pw_err)?;

    let playback_format = audio_format_pod()?;
    let mut playback_params =
        [Pod::from_bytes(&playback_format)
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

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::HeapRb;
    use ringbuf::traits::Split;

    const START_DEPTH: usize = MARGIN_START + PACKET_SAMPLES;

    /// A ring pre-loaded with `n` non-zero samples, and a stream-sized
    /// output buffer (12288 frames, like the real mapped buffer).
    fn setup(n: usize) -> (HeapProd<i16>, HeapCons<i16>, Vec<u8>) {
        let (mut prod, cons) = HeapRb::<i16>::new(4_000).split();
        push(&mut prod, n);
        (prod, cons, vec![0xAAu8; 12_288 * STRIDE])
    }

    fn push(prod: &mut HeapProd<i16>, n: usize) {
        assert_eq!(prod.push_iter(std::iter::repeat_n(1000i16, n)), n);
    }

    fn sample(out: &[u8], frame: usize) -> i16 {
        i16::from_le_bytes([out[frame * STRIDE], out[frame * STRIDE + 1]])
    }

    /// A steady call for `packets` packets: one 160-sample packet every
    /// 20ms, one 40-frame (5ms) callback four times per packet.
    fn run_steady(
        prod: &mut HeapProd<i16>,
        cons: &mut HeapCons<i16>,
        state: &mut PlaybackState,
        out: &mut [u8],
        packets: usize,
    ) {
        for _ in 0..packets {
            push(prod, PACKET_SAMPLES);
            for _ in 0..4 {
                fill_playback(cons, state, out, 40);
            }
        }
    }

    const PACKETS_PER_WINDOW: usize = ADAPT_WINDOW / PACKET_SAMPLES;

    #[test]
    fn writes_only_the_requested_frames() {
        let (_prod, mut cons, mut out) = setup(START_DEPTH);
        let mut state = PlaybackState::default();
        let fill = fill_playback(&mut cons, &mut state, &mut out, 171);
        assert_eq!(fill.frames, 171);
        assert_eq!(fill.peak, 1000);
        assert_eq!(cons.occupied_len(), START_DEPTH - 171);
        assert_eq!(sample(&out, 170), 1000);
        // Nothing past the requested frames is touched.
        assert_eq!(out[171 * STRIDE], 0xAA);
    }

    #[test]
    fn requested_is_capped_to_the_buffer() {
        let (_prod, mut cons, _) = setup(START_DEPTH);
        let mut out = vec![0u8; 100 * STRIDE];
        let fill = fill_playback(&mut cons, &mut PlaybackState::default(), &mut out, 171);
        assert_eq!(fill.frames, 100);
    }

    #[test]
    fn missing_requested_falls_back_to_one_packet() {
        let (_prod, mut cons, mut out) = setup(START_DEPTH);
        let fill = fill_playback(&mut cons, &mut PlaybackState::default(), &mut out, 0);
        assert_eq!(fill.frames, FALLBACK_FRAMES);
    }

    #[test]
    fn silent_until_start_depth() {
        let (mut prod, mut cons, mut out) = setup(START_DEPTH - 1);
        let mut state = PlaybackState::default();
        let fill = fill_playback(&mut cons, &mut state, &mut out, 171);
        assert_eq!((fill.frames, fill.peak), (171, 0));
        assert_eq!(
            cons.occupied_len(),
            START_DEPTH - 1,
            "nothing consumed while prebuffering"
        );
        assert_eq!(state.underruns, 0, "waiting to prebuffer is not an underrun");

        push(&mut prod, 1);
        let fill = fill_playback(&mut cons, &mut state, &mut out, 171);
        assert_eq!(fill.peak, 1000);
    }

    #[test]
    fn burst_is_skipped_down_to_start_depth() {
        let (_prod, mut cons, mut out) = setup(START_DEPTH + BURST_HEADROOM + 1);
        let mut state = PlaybackState::default();
        fill_playback(&mut cons, &mut state, &mut out, 40);
        assert_eq!(cons.occupied_len(), START_DEPTH - 40);
        assert_eq!(state.trimmed_samples as usize, BURST_HEADROOM + 1);
    }

    #[test]
    fn unused_depth_is_trimmed_after_one_window() {
        // Starts 300ms deep (inside the burst headroom, so not skipped
        // outright) on a perfectly steady stream: none of it is needed.
        let (mut prod, mut cons, mut out) = setup(START_DEPTH + 600);
        let mut state = PlaybackState::default();
        run_steady(&mut prod, &mut cons, &mut state, &mut out, PACKETS_PER_WINDOW);
        // Trough (end of a packet interval) is now exactly `margin`.
        assert_eq!(cons.occupied_len(), MARGIN_START);
        let trimmed = state.trimmed_samples;
        assert!(trimmed > 0);

        // And it stays there: no further trimming, no underruns.
        run_steady(&mut prod, &mut cons, &mut state, &mut out, PACKETS_PER_WINDOW * 3);
        assert_eq!(cons.occupied_len(), MARGIN_START);
        assert_eq!(state.trimmed_samples, trimmed);
        assert_eq!(state.underruns, 0);
    }

    /// Like `run_steady`, but every tenth packet is `skew` samples longer
    /// or shorter: a far end whose clock is ~625ppm off ours per sample.
    fn run_skewed(skew: isize, packets: usize) -> (PlaybackState, usize) {
        let (mut prod, mut cons, mut out) = setup(START_DEPTH);
        let mut state = PlaybackState::default();
        let mut lowest = usize::MAX;
        for i in 0..packets {
            let n = if i % 10 == 0 {
                PACKET_SAMPLES as isize + skew
            } else {
                PACKET_SAMPLES as isize
            };
            push(&mut prod, n as usize);
            for _ in 0..4 {
                fill_playback(&mut cons, &mut state, &mut out, 40);
            }
            lowest = lowest.min(cons.occupied_len());
            assert!(
                cons.occupied_len() <= START_DEPTH + PACKET_SAMPLES,
                "depth grew at packet {i}"
            );
        }
        (state, lowest)
    }

    #[test]
    fn slow_far_end_clock_is_stretched_not_underrun() {
        // Five minutes. Uncorrected, the ring would lose 1500 samples and
        // underrun every ~30s.
        let (state, lowest) = run_skewed(-1, 15_000);
        assert_eq!(state.underruns, 0);
        assert!(
            state.stretched_samples >= 1_300,
            "stretched {}",
            state.stretched_samples
        );
        assert!(lowest >= MARGIN_MIN / 2, "trough fell to {lowest}");
    }

    #[test]
    fn fast_far_end_clock_is_slewed_without_skipping() {
        let (state, _) = run_skewed(1, 15_000);
        assert_eq!(state.underruns, 0);
        // Only what drifted in (plus settling to `margin`) was removed.
        assert!(
            (1_300..2_000).contains(&state.trimmed_samples),
            "trimmed {}",
            state.trimmed_samples
        );
    }

    #[test]
    fn repeated_samples_copy_the_previous_one() {
        let (mut prod, mut cons, mut out) = setup(START_DEPTH);
        let mut state = PlaybackState::default();
        fill_playback(&mut cons, &mut state, &mut out, 40);
        state.slew = -1;
        state.slew_credit = SLEW_INTERVAL;
        push(&mut prod, 40);
        let before = cons.occupied_len();
        let fill = fill_playback(&mut cons, &mut state, &mut out, 40);
        assert_eq!(fill.frames, 40);
        assert_eq!(
            before - cons.occupied_len(),
            39,
            "one frame was not taken from the ring"
        );
        assert_eq!(sample(&out, 39), 1000);
        assert_eq!((state.slew, state.stretched_samples), (0, 1));
    }

    #[test]
    fn underrun_raises_margin_by_how_late_audio_was() {
        let (mut prod, mut cons, mut out) = setup(START_DEPTH);
        let mut state = PlaybackState::default();
        // Eight 40-frame calls drain the 320 samples exactly; the next three
        // find nothing: 120 samples (15ms) of silence.
        for _ in 0..8 {
            assert_eq!(fill_playback(&mut cons, &mut state, &mut out, 40).peak, 1000);
        }
        for _ in 0..3 {
            // Nothing from the ring: what plays is fading concealment.
            let peak = fill_playback(&mut cons, &mut state, &mut out, 40).peak;
            assert!(peak > 0 && peak <= 1000, "peak {peak}");
            assert_eq!(cons.occupied_len(), 0);
        }
        assert_eq!(state.underruns, 1);

        // The late packet lands: margin grows by the 15ms it was late, and
        // one packet is no longer enough to resume...
        push(&mut prod, PACKET_SAMPLES);
        assert_eq!(fill_playback(&mut cons, &mut state, &mut out, 40).peak, 0);
        assert_eq!(state.margin, MARGIN_START + 120);
        assert_eq!(cons.occupied_len(), PACKET_SAMPLES);
        // ...the new start depth is, and the wait wasn't another underrun.
        push(&mut prod, state.margin);
        assert_eq!(fill_playback(&mut cons, &mut state, &mut out, 40).peak, 1000);
        assert_eq!(state.underruns, 1);
    }

    #[test]
    fn short_underrun_raises_margin_by_at_least_one_step() {
        let (mut prod, mut cons, mut out) = setup(START_DEPTH);
        let mut state = PlaybackState::default();
        // 320 samples: seven 43-frame calls leave 19, the eighth comes up 24
        // short — a 3ms shortfall.
        for _ in 0..8 {
            fill_playback(&mut cons, &mut state, &mut out, 43);
        }
        assert_eq!(state.underruns, 1);
        push(&mut prod, PACKET_SAMPLES);
        fill_playback(&mut cons, &mut state, &mut out, 43);
        assert_eq!(state.margin, MARGIN_START + MARGIN_STEP);
    }

    #[test]
    fn a_gap_is_concealed_with_a_fading_replay_then_silence() {
        let (_prod, mut cons, mut out) = setup(START_DEPTH);
        let mut state = PlaybackState::default();
        for _ in 0..8 {
            fill_playback(&mut cons, &mut state, &mut out, 40);
        }
        // Dry from here. The replayed audio starts at full level and fades
        // linearly to nothing over CONCEAL_SAMPLES.
        fill_playback(&mut cons, &mut state, &mut out, CONCEAL_SAMPLES as u64 + 40);
        assert_eq!(sample(&out, 0), 1000);
        assert_eq!(sample(&out, CONCEAL_SAMPLES / 2), 500);
        let mut previous = i16::MAX;
        for frame in 0..CONCEAL_SAMPLES {
            let s = sample(&out, frame);
            assert!(s <= previous && s >= 0, "frame {frame}: {s} after {previous}");
            previous = s;
        }
        assert_eq!(sample(&out, CONCEAL_SAMPLES), 0);
        assert_eq!(sample(&out, CONCEAL_SAMPLES + 39), 0);
        // And stays silent for as long as the gap lasts.
        assert_eq!(fill_playback(&mut cons, &mut state, &mut out, 40).peak, 0);
    }

    #[test]
    fn nothing_is_concealed_before_the_call_has_audio() {
        let (_prod, mut cons, mut out) = setup(0);
        let mut state = PlaybackState::default();
        assert_eq!(fill_playback(&mut cons, &mut state, &mut out, 171).peak, 0);
    }

    #[test]
    fn paused_stream_does_not_raise_margin() {
        let (mut prod, mut cons, mut out) = setup(START_DEPTH);
        let mut state = PlaybackState::default();
        // Drain, then stay dry for a second (hold / silence suppression).
        for _ in 0..(8 + 200) {
            fill_playback(&mut cons, &mut state, &mut out, 40);
        }
        assert_eq!(state.underruns, 1);
        push(&mut prod, START_DEPTH);
        assert_eq!(fill_playback(&mut cons, &mut state, &mut out, 40).peak, 1000);
        assert_eq!(state.margin, MARGIN_START);
    }

    #[test]
    fn margin_decays_slowly_and_stops_at_the_floor() {
        // One packet short of the start depth, so the first packet starts
        // playback with the trough already at `margin`: nothing to correct.
        let (mut prod, mut cons, mut out) = setup(MARGIN_START);
        let mut state = PlaybackState::default();
        let windows = DECAY_EVERY_WINDOWS as usize;
        run_steady(
            &mut prod,
            &mut cons,
            &mut state,
            &mut out,
            PACKETS_PER_WINDOW * (windows - 1),
        );
        assert_eq!(state.margin, MARGIN_START);
        run_steady(&mut prod, &mut cons, &mut state, &mut out, PACKETS_PER_WINDOW);
        assert_eq!(state.margin, MARGIN_START - MARGIN_DECAY);

        state.margin = MARGIN_MIN;
        run_steady(
            &mut prod,
            &mut cons,
            &mut state,
            &mut out,
            PACKETS_PER_WINDOW * windows * 2,
        );
        assert_eq!(state.margin, MARGIN_MIN);
        assert_eq!(state.underruns, 0);
    }
}
