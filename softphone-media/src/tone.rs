//! Local audible feedback for dialpad key presses and other line-status
//! sounds (dial/busy/reorder/disconnect/ringback/incoming-ring) — standard
//! DTMF dual-tone generation, played back through `pw-play` (part of
//! `pipewire-utils`, already present anywhere this app's PipeWire dependency
//! is) rather than opening our own PipeWire client stream.
//!
//! This isn't the "proper" approach and normally wouldn't be — but on the
//! original dev system, this app's own low-level `pw::stream` clients (both
//! the call audio path in `pipewire_io.rs` and an earlier version of this
//! module) measurably get serviced by the graph only once every ~1.5s
//! instead of the normal ~20ms (see `pipewire_io.rs`'s doc comments), while
//! `pw-play` itself — empirically measured with `time pw-play <file>`, not
//! assumed — plays back with normal ~100ms startup latency through the
//! *exact same* WirePlumber loopback route our client lands on
//! (`input.loopback.sink.role.multimedia`, confirmed via `pw-dump`), even
//! with an identical sample rate and an identical `node.latency` hint.
//! Whatever WirePlumber is doing differently for `pw-play` vs. our own
//! client wasn't fixed by any stream property tried (media.role — several
//! values and none at all, node.latency, node.always-process), by dropping
//! the `RT_PROCESS` connect flag, by using a plain `MainLoopRc` instead of
//! `ThreadLoopRc`, or by dropping the `client.conf` context override —
//! all tested directly against this exact symptom, all still ~1.5s.
//! Shelling out to the one client empirically proven fast on this system is
//! a pragmatic fix for what's otherwise an unresolved WirePlumber scheduling
//! quirk.
//!
//! Two earlier iterations of this module both had real problems:
//!
//! - Spawning a brand-new `pw-play <file>` process per sound was audibly
//!   clicky: each fresh process is also a fresh PipeWire stream connection,
//!   which can itself trigger the audio device's own connect/wake transient
//!   (this app's default output is a wireless USB headset that can pop when
//!   waking from idle) — harmless for one isolated tone, but audible as a
//!   click on every keypress while actively dialing, since the device never
//!   got to stay in a settled "already playing" state between digits.
//! - The first fix for that (a *second*, separate long-lived silent `pw-play`
//!   process just to keep the device awake, running alongside the original
//!   per-tone spawns) traded the click for intermittent *lateness*: two
//!   concurrent PipeWire clients contending for the same target is exactly
//!   the kind of multi-client scheduling scenario this system has already
//!   been shown (above) to handle unpredictably.
//!
//! The actual fix is to only ever run *one* `pw-play` process for the whole
//! player's lifetime: a single persistent stream, opened once with `--raw`
//! (reading headerless PCM from stdin rather than a file — confirmed
//! supported via `pw-play --help`'s `[<file>|-]`/`--raw` options), whose
//! stdin a background writer task keeps fed for as long as the player lives.
//! When nothing's queued it writes silence (keeping the device continuously
//! active, so a queued sound never has to wake it from idle); when a sound
//! is queued, the writer streams its pre-rendered samples into the same
//! already-open pipe instead of spawning anything. One connection, made
//! once, never torn down — no reconnect click, and nothing else ever
//! contends with it for the sink.
//!
//! One more thing this single-stream writer had to get right: how it paces
//! writes to that stdin pipe. An early version of the writer paced itself
//! with a manual `sleep` between chunks, sized to match the chunk's own
//! playback duration — that seemed reasonable but is subtly wrong, because
//! the sleep happens *in addition to* however long the write itself and its
//! bookkeeping take, so every loop iteration costs a little more than the
//! chunk's real playback time. For one short DTMF blip that's a few
//! milliseconds, inaudible; for a ringback/incoming-ring cadence that loops
//! indefinitely, that small per-chunk deficit compounds cycle after cycle
//! until it outpaces the pipe's buffered cushion and `pw-play` genuinely
//! underruns — which is exactly what "crunchy"/crackly playback is. The
//! writer now does no manual pacing at all: it just writes each chunk and
//! immediately tries the next, relying entirely on the pipe's own
//! backpressure (`write` blocks until the kernel has room, which only opens
//! up once `pw-play` has actually consumed the previous bytes) to throttle
//! it to real playback speed. There's no clock to drift against, so there's
//! nothing to accumulate.
//!
//! Pure backpressure has its own gotcha, though: the default Linux pipe
//! buffer is 64KiB, which at this player's 8kHz mono s16 format is about 4
//! seconds of audio — plenty of room for the writer to race far *ahead* of
//! real playback rather than being throttled to it, especially while idle
//! (continuously writing silence as fast as the kernel will accept it, with
//! nothing to ever slow it down). A sound queued while several seconds of
//! that silence are already sitting unplayed in the pipe lands at the back
//! of the (FIFO) queue behind all of it — audible as exactly the multi-
//! second "tones take a long time to play" lag this caused. `shrink_pipe_
//! buffer` fixes the worst case by shrinking the pipe to the kernel's
//! minimum (one page, ~256ms of audio) right after opening it, which caps
//! how far ahead of real-time *any* write can get no matter what.
//!
//! That cap alone still left a smaller version of the same lag, though: with
//! nothing to slow it down, idle silence still races to fill that whole
//! ~256ms cushion and keeps it full, so a freshly queued sound typically
//! still waits out close to the full 256ms rather than playing right away.
//! The writer now paces *idle silence specifically* with a plain
//! `sleep(CHUNK_MS)` after each chunk, so in the common case it only stays
//! a chunk or two ahead of real-time instead of maxing out the cushion —
//! a queued sound is then usually only that far from actually playing.
//! This can't reintroduce the drift/crunch bug pacing was removed for
//! elsewhere: that bug came from a *sustained* deficit compounding over a
//! long loop of *audible* content; here it's silence, so even if idle
//! pacing drifts a little behind real-time (letting the pipe run briefly
//! dry) there's nothing to crunch — worst case is an early transition into
//! more silence. Real sound content (once dequeued) still streams via pure
//! backpressure with no pacing of its own, so that half of the fix stands
//! unchanged; `shrink_pipe_buffer`'s cap remains the backstop against
//! runaway lookahead if idle pacing ever falls behind for some other reason.

use crate::error::MediaError;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// Standard DTMF dual-tone (row, column) frequency pairs, Hz. `None` for any
/// character with no defined DTMF tone (not reachable from this app's
/// dialpad, which only ever presses `0`-`9`/`*`/`#`).
fn dtmf_frequencies(digit: char) -> Option<(f32, f32)> {
    Some(match digit {
        '1' => (697.0, 1209.0),
        '2' => (697.0, 1336.0),
        '3' => (697.0, 1477.0),
        '4' => (770.0, 1209.0),
        '5' => (770.0, 1336.0),
        '6' => (770.0, 1477.0),
        '7' => (852.0, 1209.0),
        '8' => (852.0, 1336.0),
        '9' => (852.0, 1477.0),
        '*' => (941.0, 1209.0),
        '0' => (941.0, 1336.0),
        '#' => (941.0, 1477.0),
        _ => return None,
    })
}

const SAMPLE_RATE: u32 = 8000;
const TONE_DURATION_MS: u32 = 120;
// 20ms @ 8kHz. A *linear* ramp (the original approach) has a discontinuous
// slope where the ramp meets the sustained tone — mathematically a small
// kink, but audibly a faint click/artifact right at the attack and release,
// especially on a near-pure two-tone signal like DTMF where the ear has
// nothing else to mask it. A raised-cosine (Hann) ramp's derivative eases
// to zero at that boundary instead, which is the standard fix for exactly
// this class of artifact.
const FADE_SAMPLES: usize = 160;
// A few ms of true silence padding at both ends, independent of the fade.
// This isn't about the envelope shape — it's cushioning against a
// connection/wake transient from the *audio device itself* (this app's
// default output is a wireless USB headset receiver, which can audibly pop
// or briefly distort right as a fresh stream connects/wakes it from idle).
// Padding the file means that transient — if it happens — lands in silence
// instead of overlapping the start of the actual tone. 30ms rather than the
// original 20ms — see the module doc comment for why the persistent stream
// now avoids most of that transient entirely rather than just padding
// around it; this stays as cheap extra cushion for whatever's left.
const PAD_SAMPLES: usize = 240;

/// Sum of the two DTMF frequencies at a moderate amplitude, with a
/// raised-cosine fade in/out and silence padding at both ends.
fn generate_tone(digit: char) -> Option<Vec<i16>> {
    let (f1, f2) = dtmf_frequencies(digit)?;
    Some(generate_dual_tone(f1, f2, TONE_DURATION_MS, FADE_SAMPLES))
}

/// The classic North American dial tone: a continuous 350Hz+440Hz pair.
/// Long enough that it runs right up against `App::DIAL_TIMEOUT` with only
/// a couple of seconds of true silence in between (rather than a short blip
/// followed by a long dead gap before the reorder tone) — the two are meant
/// to read as one continuous "line is open, then giving up" sequence.
const DIAL_TONE_FREQUENCIES: (f32, f32) = (350.0, 440.0);
const DIAL_TONE_DURATION_MS: u32 = 6000;
const DIAL_TONE_FADE_SAMPLES: usize = 400; // 50ms — longer than a DTMF blip since this is sustained

fn generate_dial_tone() -> Vec<i16> {
    generate_dual_tone(
        DIAL_TONE_FREQUENCIES.0,
        DIAL_TONE_FREQUENCIES.1,
        DIAL_TONE_DURATION_MS,
        DIAL_TONE_FADE_SAMPLES,
    )
}

/// Standard North American call-progress frequency pair (480Hz+620Hz) used
/// for both busy and reorder/fast-busy signals — they're the same two
/// tones, just at different cadences. `on_ms`/`off_ms` set that cadence;
/// `cycles` how many on/off repeats.
const CALL_PROGRESS_FREQUENCIES: (f32, f32) = (480.0, 620.0);
const CALL_PROGRESS_FADE_SAMPLES: usize = 60;

/// Shared cadence-tone renderer: `cycles` repeats of `freqs` for `on_ms`
/// (faded in/out over `fade_samples`) followed by `off_ms` of silence.
/// Generalized (rather than hardcoded to `CALL_PROGRESS_FREQUENCIES`) so
/// ringback/incoming-ring — which need their own frequency pairs — can
/// reuse it instead of duplicating this loop.
fn generate_cadence_tone(freqs: (f32, f32), fade_samples: usize, on_ms: u32, off_ms: u32, cycles: usize) -> Vec<i16> {
    let (f1, f2) = freqs;
    let mut samples = Vec::new();
    for _ in 0..cycles {
        samples.extend(generate_dual_tone(f1, f2, on_ms, fade_samples));
        samples.extend(std::iter::repeat_n(
            0i16,
            (SAMPLE_RATE as usize * off_ms as usize) / 1000,
        ));
    }
    samples
}

/// The real North American busy signal (slow cadence: 500ms on, 500ms off)
/// — played when an outbound call attempt comes back genuinely busy (a real
/// 486 from the far end, not a generic failure). Rendered locally, same as
/// a real desk phone does: there's no active RTP session yet for a
/// pre-answer rejection, so there's no in-band audio to pull from the PBX
/// for this — the phone itself always plays this tone in response to the
/// SIP status, which is exactly what this does.
fn generate_busy_tone() -> Vec<i16> {
    generate_cadence_tone(CALL_PROGRESS_FREQUENCIES, CALL_PROGRESS_FADE_SAMPLES, 500, 500, 3)
}

/// Reorder ("fast busy") signal — the same two frequencies at double the
/// cadence speed (250ms on/off). Used for anything that isn't a clean busy
/// signal (other call failures), and for a line that's been left open with
/// dial tone playing and no digits dialed for too long — the same "please
/// hang up and try again" cue a real phone gives you in both situations.
fn generate_reorder_tone() -> Vec<i16> {
    generate_cadence_tone(CALL_PROGRESS_FREQUENCIES, CALL_PROGRESS_FADE_SAMPLES, 250, 250, 4)
}

/// Standard North American audible ringback (440Hz+480Hz, 2s on/4s off) —
/// what the *caller* hears while an outbound call is ringing (180) or
/// playing early media without its own in-band audio. Rendered as a single
/// full cadence cycle; `DtmfTonePlayer` loops it (see `QueuedSound::Ringback`)
/// since ring duration isn't known up front.
const RINGBACK_FREQUENCIES: (f32, f32) = (440.0, 480.0);
const RINGBACK_FADE_SAMPLES: usize = 200;
const RINGBACK_ON_MS: u32 = 2000;
const RINGBACK_OFF_MS: u32 = 4000;

fn generate_ringback_cycle() -> Vec<i16> {
    generate_cadence_tone(RINGBACK_FREQUENCIES, RINGBACK_FADE_SAMPLES, RINGBACK_ON_MS, RINGBACK_OFF_MS, 1)
}

/// Local incoming-call ringtone — what the *callee* (this app) plays while
/// a line is `Incoming`. Deliberately a different frequency pair/cadence
/// from ringback so the two are distinguishable by ear and, more
/// importantly, so they can be started/stopped independently: an active
/// call on one line and a fresh incoming call ringing on another are a
/// legitimate simultaneous scenario, and sharing one tone/flag would let
/// stopping one wrongly cut off the other.
const INCOMING_RING_FREQUENCIES: (f32, f32) = (750.0, 800.0);
const INCOMING_RING_FADE_SAMPLES: usize = 150;
const INCOMING_RING_ON_MS: u32 = 1000;
const INCOMING_RING_OFF_MS: u32 = 3000;

fn generate_incoming_ring_cycle() -> Vec<i16> {
    generate_cadence_tone(
        INCOMING_RING_FREQUENCIES,
        INCOMING_RING_FADE_SAMPLES,
        INCOMING_RING_ON_MS,
        INCOMING_RING_OFF_MS,
        1,
    )
}

// A short, soft two-note descending cue — played once whenever a call ends
// (either side hangs up, a call is declined, or a dial attempt fails), so
// the user gets an unambiguous confirmation the call is actually over
// instead of the UI just quietly going back to idle. Deliberately gentle:
// two brief single-frequency notes, not the harsh multi-tone SIT/fast-busy
// signal real telco equipment uses for the same purpose — this is meant to
// read as a calm confirmation, not an alert.
const DISCONNECT_TONE_FADE_SAMPLES: usize = 200;

fn generate_disconnect_tone() -> Vec<i16> {
    let mut samples = generate_dual_tone(520.0, 520.0, 110, DISCONNECT_TONE_FADE_SAMPLES);
    samples.extend(std::iter::repeat_n(0i16, SAMPLE_RATE as usize * 40 / 1000));
    samples.extend(generate_dual_tone(370.0, 370.0, 160, DISCONNECT_TONE_FADE_SAMPLES));
    samples
}

/// Shared synthesis for both DTMF digit tones and the dial tone: sum of two
/// sine waves at a moderate amplitude, raised-cosine fade in/out, with
/// silence padding at both ends (see `PAD_SAMPLES`'s doc comment — cushions
/// against a connection/wake transient from the audio device itself, not
/// part of the tone's own envelope shape).
fn generate_dual_tone(f1: f32, f2: f32, duration_ms: u32, fade_samples: usize) -> Vec<i16> {
    let n = (SAMPLE_RATE * duration_ms / 1000) as usize;
    let mut samples = Vec::with_capacity(n + 2 * PAD_SAMPLES);
    samples.extend(std::iter::repeat_n(0i16, PAD_SAMPLES));
    for i in 0..n {
        let t = i as f32 / SAMPLE_RATE as f32;
        let value = ((2.0 * std::f32::consts::PI * f1 * t).sin()
            + (2.0 * std::f32::consts::PI * f2 * t).sin())
            * 0.22;
        let fade = if i < fade_samples {
            0.5 * (1.0 - (std::f32::consts::PI * i as f32 / fade_samples as f32).cos())
        } else if i >= n.saturating_sub(fade_samples) {
            let j = (n - i) as f32;
            0.5 * (1.0 - (std::f32::consts::PI * j / fade_samples as f32).cos())
        } else {
            1.0
        };
        samples.push((value * fade * i16::MAX as f32) as i16);
    }
    samples.extend(std::iter::repeat_n(0i16, PAD_SAMPLES));
    samples
}

const DIGITS: [char; 12] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '*', '#',
];

enum QueuedSound {
    Digit(char),
    DialTone,
    BusyTone,
    ReorderTone,
    Disconnect,
    /// Ringback/incoming-ring both carry the epoch they were queued under
    /// (see `SoundKind`'s doc comment) so the writer can tell, after a cycle
    /// finishes playing, whether it should queue another cycle (still the
    /// current epoch) or stop looping (a `stop_*`/`play_*` call already
    /// moved on to a new epoch while this cycle was playing) — and, mid-cycle,
    /// whether it should abandon the rest of the samples it's currently
    /// streaming for the same reason.
    Ringback(u64),
    IncomingRing(u64),
}

/// Which interruption rule applies to a sound currently being streamed.
/// `LineTone` (dial/busy/reorder) is interruptible by `stop_line_tone`;
/// `Ringback`/`IncomingRing` are independently interruptible by their own
/// `stop_*` methods — kept as *separate* kinds (rather than folded into
/// `LineTone`) because an active call on one line and a fresh incoming call
/// ringing on another are a legitimate simultaneous scenario, and a
/// `stop_ringback()` must never be able to cut off an in-flight incoming
/// ring (or vice versa). `Other` covers DTMF digits and the disconnect cue,
/// which are always short and just meant to finish playing on their own —
/// never targeted by any `stop_*` method.
#[derive(Clone, Copy, PartialEq)]
enum SoundKind {
    LineTone,
    Ringback,
    IncomingRing,
    Other,
}

/// How much of a sound's samples the writer streams to `pw-play`'s stdin
/// between interruption checks (and how much silence it writes at a time
/// while idle) — small enough that `stop_line_tone`/`stop_ringback`/
/// `stop_incoming_ringtone` feel immediate, large enough not to be a
/// busy-loop. This is purely a *cancellation-check granularity* now, not a
/// playback rate: see the writer task's doc comment for why pacing is left
/// entirely to the pipe's own backpressure instead of a manual sleep.
const CHUNK_MS: u64 = 20;
const CHUNK_SAMPLES: usize = (SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;

/// Shrinks the kernel pipe backing `stdin` down to one page (Linux's
/// minimum for `F_SETPIPE_SZ`, typically 4096 bytes — about 256ms of this
/// player's 8kHz mono s16 audio) instead of the default 64KiB (~4s). The
/// writer task relies entirely on this pipe's backpressure to pace itself
/// (see the module doc comment's third iteration) — `write_all` blocks once
/// the kernel has no room left, which only opens back up as `pw-play`
/// actually consumes bytes. At the default 64KiB size that still lets the
/// writer race up to ~4 seconds *ahead* of real playback (most visibly
/// during idle, continuously writing silence as fast as the kernel will
/// accept it): a sound queued at that moment lands at the back of a FIFO
/// pipe behind several seconds of already-buffered-but-unplayed silence,
/// which is exactly the "tones take a long time to play" regression this
/// fixes. Shrinking the buffer caps how far ahead *any* write — silence or
/// real audio — can ever get, so a freshly queued sound is always within
/// one small buffer's worth of real time from actually playing, and
/// `stop_line_tone`/`stop_ringback`/`stop_incoming_ringtone` can't have
/// several seconds of already-buffered audio left to drain out before they
/// take effect either. Best-effort: if this fails (non-Linux, or the kernel
/// refuses), playback still works, just with the wider default look-ahead.
fn shrink_pipe_buffer(stdin: &tokio::process::ChildStdin) {
    use std::os::fd::AsRawFd;
    // 4096 is Linux's documented minimum for F_SETPIPE_SZ (`man fcntl`) —
    // the kernel rounds any smaller request up to this, so pass it directly.
    const MIN_PIPE_BYTES: libc::c_int = 4096;
    let fd = stdin.as_raw_fd();
    // SAFETY: `fd` is a valid, open pipe file descriptor for as long as
    // `stdin` (borrowed for this call) is alive; `fcntl(F_SETPIPE_SZ)` only
    // resizes the kernel's internal buffer for it and has no other effect
    // on the process.
    unsafe {
        libc::fcntl(fd, libc::F_SETPIPE_SZ, MIN_PIPE_BYTES);
    }
}

fn samples_to_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    bytes
}

pub struct DtmfTonePlayer {
    digit_samples: HashMap<char, Vec<i16>>,
    /// Sounds queue through here to a single background writer task, which
    /// streams them one at a time into the one persistent `pw-play`
    /// process's stdin (see the module doc comment) — never spawning a new
    /// process per sound. Serializing through a queue means digits played
    /// faster than one tone's duration play back-to-back instead of exactly
    /// on each keystroke, trading a little latency on very fast typing for
    /// tones that always sound correct (no overlapping/garbled audio).
    queue_tx: mpsc::UnboundedSender<QueuedSound>,
    /// Bumped by `stop_line_tone` to abandon whatever dial/busy/reorder
    /// tone is currently streaming — checked by the writer between chunks
    /// of a `SoundKind::LineTone` sound only, so this can never affect an
    /// unrelated in-flight sound (e.g. a DTMF digit).
    line_tone_epoch: Arc<Mutex<u64>>,
    /// Bumped by every `play_ringback_tone()`/`stop_ringback()` call and
    /// carried on each queued `QueuedSound::Ringback` cycle — see
    /// `QueuedSound`'s doc comment for how this drives both the
    /// end-of-cycle looping decision and mid-cycle abandonment.
    ringback_epoch: Arc<Mutex<u64>>,
    incoming_ring_epoch: Arc<Mutex<u64>>,
    /// The single persistent `pw-play` process every sound streams through
    /// (see the module doc comment). Held only so it lives exactly as long
    /// as this player — `kill_on_drop` on its `Command` means dropping this
    /// field tears the process down; nothing else ever reads or writes it
    /// directly (the writer task owns its stdin handle separately).
    _stream_child: tokio::process::Child,
}

impl DtmfTonePlayer {
    /// Pre-renders every digit's tone (and the other line-status sounds)
    /// once into memory, and opens the one persistent `pw-play` process
    /// everything streams through for the rest of this player's lifetime.
    pub fn start(playback_target: Option<String>) -> Result<Self, MediaError> {
        let mut digit_samples = HashMap::new();
        for digit in DIGITS {
            if let Some(samples) = generate_tone(digit) {
                digit_samples.insert(digit, samples);
            }
        }
        let dial_tone = samples_to_bytes(&generate_dial_tone());
        let busy_tone = samples_to_bytes(&generate_busy_tone());
        let reorder_tone = samples_to_bytes(&generate_reorder_tone());
        let disconnect_tone = samples_to_bytes(&generate_disconnect_tone());
        let ringback_cycle = samples_to_bytes(&generate_ringback_cycle());
        let incoming_ring_cycle = samples_to_bytes(&generate_incoming_ring_cycle());

        let mut command = tokio::process::Command::new("pw-play");
        command
            .arg("--raw")
            .arg("--rate")
            .arg(SAMPLE_RATE.to_string())
            .arg("--channels")
            .arg("1")
            .arg("--format")
            .arg("s16")
            .arg("--media-role")
            .arg("Music")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(target) = &playback_target {
            command.arg("--target").arg(target);
        }
        let mut stream_child = command.spawn().map_err(MediaError::Io)?;
        let stdin = stream_child
            .stdin
            .take()
            .ok_or_else(|| MediaError::PipeWire("pw-play stdin unavailable".into()))?;
        shrink_pipe_buffer(&stdin);

        let (queue_tx, mut queue_rx) = mpsc::unbounded_channel::<QueuedSound>();
        let line_tone_epoch: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let ringback_epoch: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let incoming_ring_epoch: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let worker_line_tone_epoch = line_tone_epoch.clone();
        let worker_ringback_epoch = ringback_epoch.clone();
        let worker_incoming_ring_epoch = incoming_ring_epoch.clone();
        let worker_requeue_tx = queue_tx.clone();
        let worker_digit_samples: HashMap<char, Vec<u8>> = digit_samples
            .iter()
            .map(|(&digit, samples)| (digit, samples_to_bytes(samples)))
            .collect();

        // No manual `sleep` anywhere in this loop — pacing is left entirely
        // to the pipe's own backpressure. `write_all` on a pipe blocks until
        // the kernel has room, which only opens up once `pw-play` has
        // actually consumed (i.e. played) the previous bytes, so the writer
        // can never race ahead of or drift behind real playback. An earlier
        // version paced itself with `sleep(CHUNK_MS)` *after* every write —
        // but that made each loop iteration cost `write time + CHUNK_MS`,
        // strictly more than the `CHUNK_MS` a chunk actually takes to play,
        // so production fell behind consumption a little more on every
        // single chunk. For a short DTMF blip that's a few chunks and barely
        // audible; for a looping ringback/incoming-ring cadence (repeating
        // every several seconds, indefinitely) the deficit compounded cycle
        // after cycle until it caught up with the pipe's buffered cushion
        // and `pw-play` underran — audible as crackle/crunch. Backpressure
        // has no such accumulator: there's nothing to drift.
        tokio::spawn(async move {
            let mut stdin = stdin;
            let silence_chunk = vec![0u8; CHUNK_SAMPLES * 2];
            loop {
                let sound = match queue_rx.try_recv() {
                    Ok(sound) => sound,
                    Err(mpsc::error::TryRecvError::Empty) => {
                        if stdin.write_all(&silence_chunk).await.is_err() {
                            break;
                        }
                        // Idle silence is the one place this writer still
                        // paces itself with a `sleep` instead of pure
                        // backpressure — deliberately. Pure backpressure
                        // alone lets the writer race as far ahead as the
                        // (already-shrunk, see `shrink_pipe_buffer`) pipe
                        // allows, and while idle there's nothing to ever
                        // slow that race down, so it keeps the pipe
                        // permanently topped up to its full ~256ms cushion
                        // of already-buffered-but-unplayed silence. A digit
                        // queued at that moment still has to wait for all of
                        // that backlog to drain before it's audible — a
                        // real, if smaller, version of the multi-second
                        // lookahead bug `shrink_pipe_buffer` was written to
                        // fix. Pacing idle writes to roughly real-time
                        // instead keeps that backlog near-empty in the
                        // common case, so a freshly queued sound is usually
                        // only a chunk or two behind, not the full buffer.
                        // This can't reintroduce the drift/crunch bug pure
                        // backpressure was adopted to fix (see the module
                        // doc comment) because silence has no audible
                        // content to crunch — worst case here is a few ms
                        // of early underrun into more silence, inaudible.
                        tokio::time::sleep(Duration::from_millis(CHUNK_MS)).await;
                        continue;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                };

                let kind = match &sound {
                    QueuedSound::DialTone | QueuedSound::BusyTone | QueuedSound::ReorderTone => {
                        SoundKind::LineTone
                    }
                    QueuedSound::Ringback(_) => SoundKind::Ringback,
                    QueuedSound::IncomingRing(_) => SoundKind::IncomingRing,
                    QueuedSound::Digit(_) | QueuedSound::Disconnect => SoundKind::Other,
                };
                let bytes: &[u8] = match &sound {
                    QueuedSound::Digit(digit) => match worker_digit_samples.get(digit) {
                        Some(s) => s,
                        None => continue,
                    },
                    QueuedSound::DialTone => &dial_tone,
                    QueuedSound::BusyTone => &busy_tone,
                    QueuedSound::ReorderTone => &reorder_tone,
                    QueuedSound::Disconnect => &disconnect_tone,
                    QueuedSound::Ringback(_) => &ringback_cycle,
                    QueuedSound::IncomingRing(_) => &incoming_ring_cycle,
                };
                let started_line_epoch = *worker_line_tone_epoch.lock().unwrap();

                let mut aborted = false;
                'chunks: for chunk in bytes.chunks(CHUNK_SAMPLES * 2) {
                    let cancel = match (kind, &sound) {
                        (SoundKind::LineTone, _) => {
                            *worker_line_tone_epoch.lock().unwrap() != started_line_epoch
                        }
                        (SoundKind::Ringback, QueuedSound::Ringback(epoch)) => {
                            *worker_ringback_epoch.lock().unwrap() != *epoch
                        }
                        (SoundKind::IncomingRing, QueuedSound::IncomingRing(epoch)) => {
                            *worker_incoming_ring_epoch.lock().unwrap() != *epoch
                        }
                        _ => false,
                    };
                    if cancel {
                        aborted = true;
                        break 'chunks;
                    }
                    if stdin.write_all(chunk).await.is_err() {
                        return;
                    }
                }

                // A ringback/incoming-ring cycle just ended, either because
                // it finished playing on its own or because a `stop_*` call
                // abandoned it mid-stream — if nothing has bumped the epoch
                // since this cycle was queued, keep looping by queuing
                // another cycle under the same epoch; if it changed, this
                // cycle's job is done.
                if !aborted {
                    match sound {
                        QueuedSound::Ringback(epoch) if *worker_ringback_epoch.lock().unwrap() == epoch => {
                            let _ = worker_requeue_tx.send(QueuedSound::Ringback(epoch));
                        }
                        QueuedSound::IncomingRing(epoch)
                            if *worker_incoming_ring_epoch.lock().unwrap() == epoch =>
                        {
                            let _ = worker_requeue_tx.send(QueuedSound::IncomingRing(epoch));
                        }
                        _ => {}
                    }
                }
            }
        });

        Ok(DtmfTonePlayer {
            digit_samples,
            queue_tx,
            line_tone_epoch,
            ringback_epoch,
            incoming_ring_epoch,
            _stream_child: stream_child,
        })
    }

    /// Queues `digit`'s DTMF tone to play once the writer gets to it (see
    /// `queue_tx`'s docs for why this doesn't play immediately/directly).
    /// Best-effort: silently does nothing for a digit with no defined tone.
    pub fn play(&self, digit: char) {
        if !self.digit_samples.contains_key(&digit) {
            return;
        }
        let _ = self.queue_tx.send(QueuedSound::Digit(digit));
    }

    /// Queues a ~3s dial tone — played once when the user selects an idle
    /// line, giving the "picked up an empty line" feedback a real phone
    /// gives before you start dialing.
    pub fn play_dial_tone(&self) {
        let _ = self.queue_tx.send(QueuedSound::DialTone);
    }

    /// Queues the real busy cadence — see `generate_busy_tone`'s doc
    /// comment for why this is triggered by (and only by) an actual 486
    /// from the far end, not a generic call failure.
    pub fn play_busy_tone(&self) {
        let _ = self.queue_tx.send(QueuedSound::BusyTone);
    }

    /// Queues the reorder/fast-busy cadence — any other call failure, or a
    /// line left open too long with nothing dialed (see
    /// `generate_reorder_tone`'s doc comment).
    pub fn play_reorder_tone(&self) {
        let _ = self.queue_tx.send(QueuedSound::ReorderTone);
    }

    /// Cuts a currently-playing dial/busy/reorder tone immediately — e.g.
    /// when the user toggles a line back closed right after opening it. A
    /// real phone's dial tone stops the instant you hang up; without this,
    /// whichever line-status tone was playing kept running to completion
    /// regardless of whether the line was still "open." Does nothing if
    /// none of those is what's currently playing (e.g. it already
    /// finished, or a DTMF digit is playing instead) — bumping the epoch
    /// only ever affects a `SoundKind::LineTone` sound's own cancellation
    /// check, never anything else.
    pub fn stop_line_tone(&self) {
        let mut epoch = self.line_tone_epoch.lock().unwrap();
        *epoch = epoch.wrapping_add(1);
    }

    /// Queues the short "call ended" cue — see `generate_disconnect_tone`'s
    /// doc comment for why it's deliberately soft rather than alarming.
    pub fn play_disconnect_tone(&self) {
        let _ = self.queue_tx.send(QueuedSound::Disconnect);
    }

    /// Starts (or restarts) looping outbound ringback — played while an
    /// outbound call is ringing (180) or between ringing and real early
    /// media landing. Loops indefinitely (re-queuing its own cadence cycle,
    /// see the writer's epoch check) until `stop_ringback` is called.
    pub fn play_ringback_tone(&self) {
        let epoch = {
            let mut epoch = self.ringback_epoch.lock().unwrap();
            *epoch = epoch.wrapping_add(1);
            *epoch
        };
        let _ = self.queue_tx.send(QueuedSound::Ringback(epoch));
    }

    /// Stops a looping ringback started by `play_ringback_tone`, if one is
    /// running — bumps the epoch, so both an in-flight cycle currently
    /// streaming (abandoned mid-chunk) and any not-yet-finished cycle
    /// won't re-queue itself or continue (never touches an unrelated
    /// in-flight sound, since the check is scoped to `QueuedSound::Ringback`
    /// specifically).
    pub fn stop_ringback(&self) {
        let mut epoch = self.ringback_epoch.lock().unwrap();
        *epoch = epoch.wrapping_add(1);
    }

    /// Starts (or restarts) the looping incoming-call ringtone — played
    /// while a line is `Incoming`, independent of any ringback tone playing
    /// for a different, simultaneously-in-progress outbound call. See
    /// `play_ringback_tone`'s doc comment for the looping mechanism.
    pub fn play_incoming_ringtone(&self) {
        let epoch = {
            let mut epoch = self.incoming_ring_epoch.lock().unwrap();
            *epoch = epoch.wrapping_add(1);
            *epoch
        };
        let _ = self.queue_tx.send(QueuedSound::IncomingRing(epoch));
    }

    /// Stops a looping incoming ringtone started by `play_incoming_ringtone`,
    /// if one is running. See `stop_ringback`'s doc comment.
    pub fn stop_incoming_ringtone(&self) {
        let mut epoch = self.incoming_ring_epoch.lock().unwrap();
        *epoch = epoch.wrapping_add(1);
    }
}
