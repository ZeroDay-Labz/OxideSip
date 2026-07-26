//! Local audible feedback for dialpad key presses — standard DTMF dual-tone
//! generation, rendered once per digit to a temp WAV file and played back by
//! shelling out to `pw-play` (part of `pipewire-utils`, already present
//! anywhere this app's PipeWire dependency is) rather than opening our own
//! PipeWire client stream.
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
//! a pragmatic, immediate fix for what's otherwise an unresolved
//! WirePlumber scheduling quirk — reasonable here since this is a one-shot
//! UI click sound, not the continuous call audio path (which still needs a
//! real streaming client and can't be done by spawning a process per
//! packet).

use crate::error::MediaError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
// instead of overlapping the start of the actual tone.
const PAD_SAMPLES: usize = 160;

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

/// Minimal canonical 44-byte-header PCM WAV — no need for a wav-writing
/// crate for something this small and fixed-format.
fn write_wav(path: &Path, samples: &[i16]) -> std::io::Result<()> {
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = SAMPLE_RATE * 2;
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    bytes.extend_from_slice(&byte_rate.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes()); // block align
    bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, bytes)
}

const DIGITS: [char; 12] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '*', '#',
];

fn digit_filename(digit: char) -> &'static str {
    match digit {
        '0' => "0",
        '1' => "1",
        '2' => "2",
        '3' => "3",
        '4' => "4",
        '5' => "5",
        '6' => "6",
        '7' => "7",
        '8' => "8",
        '9' => "9",
        '*' => "star",
        '#' => "pound",
        _ => "unknown",
    }
}

enum QueuedSound {
    Digit(char),
    DialTone,
    BusyTone,
    ReorderTone,
    Disconnect,
    /// Ringback/incoming-ring both carry the epoch they were queued under
    /// (see `SoundKind`'s doc comment) so the worker can tell, after a cycle
    /// finishes playing, whether it should queue another cycle (still the
    /// current epoch) or stop looping (a `stop_*`/`play_*` call already
    /// moved on to a new epoch while this cycle was playing).
    Ringback(u64),
    IncomingRing(u64),
}

/// Tags whichever `pw-play` child is currently running. `LineTone`
/// (dial/busy/reorder) lets `stop_line_tone` reach in and kill it
/// specifically; `Ringback`/`IncomingRing` are similarly independently
/// killable by their own `stop_*` methods — kept as *separate* kinds
/// (rather than folded into `LineTone`) because an active call on one line
/// and a fresh incoming call ringing on another are a legitimate
/// simultaneous scenario, and a `stop_ringback()` must never be able to cut
/// off an in-flight incoming ring (or vice versa). `Other` covers DTMF
/// digits and the disconnect cue, which are always short and just meant to
/// finish playing on their own — never targeted by any `stop_*` method.
#[derive(Clone, Copy, PartialEq)]
enum SoundKind {
    LineTone,
    Ringback,
    IncomingRing,
    Other,
}

/// `None` whenever nothing's currently playing.
type CurrentChild = Option<(SoundKind, tokio::process::Child)>;

pub struct DtmfTonePlayer {
    wav_paths: HashMap<char, PathBuf>,
    /// Sounds queue through here to a single background worker task, which
    /// plays them one at a time (awaiting each `pw-play` child before
    /// starting the next). Firing an independent `pw-play` process per key
    /// press — the original approach — sounded "really bad" under fast
    /// typing: each ~110ms tone takes ~90-100ms of process/PipeWire-connect
    /// overhead on top, so typing faster than that launched several
    /// overlapping `pw-play` instances all writing to the same sink at
    /// once, producing garbled/overlapping audio instead of a clean
    /// sequence of beeps. Serializing through a queue trades a little
    /// latency on fast typing (digits play back-to-back instead of exactly
    /// on each keystroke) for tones that always sound correct.
    queue_tx: tokio::sync::mpsc::UnboundedSender<QueuedSound>,
    current_child: Arc<Mutex<CurrentChild>>,
    /// Bumped by every `play_ringback_tone()`/`stop_ringback()` call and
    /// carried on each queued `QueuedSound::Ringback` cycle — see
    /// `QueuedSound`'s doc comment for how this drives the looping/stop
    /// logic without needing to distinguish "child was killed" from "child
    /// exited on its own" (both look identical through `try_wait`).
    ringback_epoch: Arc<Mutex<u64>>,
    incoming_ring_epoch: Arc<Mutex<u64>>,
}

impl DtmfTonePlayer {
    /// Pre-renders each digit's tone (and the dial tone) to a temp WAV file
    /// once, so the worker loop only ever has to spawn `pw-play`, not also
    /// synthesize+write audio on every key press.
    pub fn start(playback_target: Option<String>) -> Result<Self, MediaError> {
        let dir = std::env::temp_dir().join("oxidesip-dtmf");
        std::fs::create_dir_all(&dir).map_err(MediaError::Io)?;

        let mut wav_paths = HashMap::new();
        for digit in DIGITS {
            let Some(samples) = generate_tone(digit) else {
                continue;
            };
            let path = dir.join(format!("{}.wav", digit_filename(digit)));
            write_wav(&path, &samples).map_err(MediaError::Io)?;
            wav_paths.insert(digit, path);
        }
        let dial_tone_path = dir.join("dial-tone.wav");
        write_wav(&dial_tone_path, &generate_dial_tone()).map_err(MediaError::Io)?;
        let busy_tone_path = dir.join("busy-tone.wav");
        write_wav(&busy_tone_path, &generate_busy_tone()).map_err(MediaError::Io)?;
        let reorder_tone_path = dir.join("reorder-tone.wav");
        write_wav(&reorder_tone_path, &generate_reorder_tone()).map_err(MediaError::Io)?;
        let disconnect_tone_path = dir.join("disconnect-tone.wav");
        write_wav(&disconnect_tone_path, &generate_disconnect_tone()).map_err(MediaError::Io)?;
        let ringback_path = dir.join("ringback.wav");
        write_wav(&ringback_path, &generate_ringback_cycle()).map_err(MediaError::Io)?;
        let incoming_ring_path = dir.join("incoming-ring.wav");
        write_wav(&incoming_ring_path, &generate_incoming_ring_cycle()).map_err(MediaError::Io)?;

        let (queue_tx, mut queue_rx) = tokio::sync::mpsc::unbounded_channel::<QueuedSound>();
        let worker_paths = wav_paths.clone();
        let current_child: Arc<Mutex<CurrentChild>> = Arc::new(Mutex::new(None));
        let worker_current_child = current_child.clone();
        let ringback_epoch: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let incoming_ring_epoch: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let worker_ringback_epoch = ringback_epoch.clone();
        let worker_incoming_ring_epoch = incoming_ring_epoch.clone();
        let worker_requeue_tx = queue_tx.clone();
        tokio::spawn(async move {
            while let Some(sound) = queue_rx.recv().await {
                let kind = match &sound {
                    QueuedSound::DialTone | QueuedSound::BusyTone | QueuedSound::ReorderTone => {
                        SoundKind::LineTone
                    }
                    QueuedSound::Ringback(_) => SoundKind::Ringback,
                    QueuedSound::IncomingRing(_) => SoundKind::IncomingRing,
                    QueuedSound::Digit(_) | QueuedSound::Disconnect => SoundKind::Other,
                };
                let path = match &sound {
                    QueuedSound::Digit(digit) => worker_paths.get(digit),
                    QueuedSound::DialTone => Some(&dial_tone_path),
                    QueuedSound::BusyTone => Some(&busy_tone_path),
                    QueuedSound::ReorderTone => Some(&reorder_tone_path),
                    QueuedSound::Disconnect => Some(&disconnect_tone_path),
                    QueuedSound::Ringback(_) => Some(&ringback_path),
                    QueuedSound::IncomingRing(_) => Some(&incoming_ring_path),
                };
                let Some(path) = path else {
                    continue;
                };
                let mut command = tokio::process::Command::new("pw-play");
                command
                    .arg(path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Some(target) = &playback_target {
                    command.arg("--target").arg(target);
                }
                let child = match command.spawn() {
                    Ok(child) => child,
                    Err(e) => {
                        tracing::warn!(%e, "failed to launch pw-play for tone playback");
                        continue;
                    }
                };
                *worker_current_child.lock().unwrap() = Some((kind, child));
                // Polled rather than a single `child.wait().await` — that
                // would need to hold the child (and thus the lock guarding
                // it) for the whole playback, which would block
                // `stop_line_tone` from ever reaching in to kill it early.
                // Each poll is a quick non-blocking `try_wait`, with the
                // lock released before sleeping, so a `stop_line_tone` call
                // in between polls gets a clear shot at the child.
                loop {
                    let still_running = {
                        let mut guard = worker_current_child.lock().unwrap();
                        match guard.as_mut() {
                            Some((_, child)) => match child.try_wait() {
                                Ok(None) => true,
                                Ok(Some(_)) | Err(_) => {
                                    *guard = None;
                                    false
                                }
                            },
                            // Already taken (killed) by `stop_dial_tone`.
                            None => false,
                        }
                    };
                    if !still_running {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
                // A ringback/incoming-ring cycle just ended, either because
                // it finished playing on its own or because a `stop_*` call
                // killed it — `try_wait` can't tell those apart, but the
                // epoch can: if nothing has bumped it since this cycle was
                // queued, keep looping by queuing another cycle under the
                // same epoch; if it changed (a `stop_*`/fresh `play_*` call
                // happened), this loop's job is done.
                match sound {
                    QueuedSound::Ringback(epoch) if *worker_ringback_epoch.lock().unwrap() == epoch => {
                        let _ = worker_requeue_tx.send(QueuedSound::Ringback(epoch));
                    }
                    QueuedSound::IncomingRing(epoch) if *worker_incoming_ring_epoch.lock().unwrap() == epoch => {
                        let _ = worker_requeue_tx.send(QueuedSound::IncomingRing(epoch));
                    }
                    _ => {}
                }
            }
        });

        Ok(DtmfTonePlayer {
            wav_paths,
            queue_tx,
            current_child,
            ringback_epoch,
            incoming_ring_epoch,
        })
    }

    /// Queues `digit`'s DTMF tone to play once the worker gets to it (see
    /// `queue_tx`'s docs for why this doesn't play immediately/directly).
    /// Best-effort: silently does nothing for a digit with no defined tone.
    pub fn play(&self, digit: char) {
        if !self.wav_paths.contains_key(&digit) {
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
    /// whichever line-status tone was playing kept running out its full WAV
    /// regardless of whether the line was still "open." Does nothing if
    /// none of those is what's currently playing (e.g. it already
    /// finished, or a DTMF digit is playing instead) — this only ever
    /// targets dial/busy/reorder specifically, never interrupts anything
    /// else.
    pub fn stop_line_tone(&self) {
        let mut guard = self.current_child.lock().unwrap();
        if let Some((SoundKind::LineTone, child)) = guard.as_mut() {
            let _ = child.start_kill();
        }
    }

    /// Queues the short "call ended" cue — see `generate_disconnect_tone`'s
    /// doc comment for why it's deliberately soft rather than alarming.
    pub fn play_disconnect_tone(&self) {
        let _ = self.queue_tx.send(QueuedSound::Disconnect);
    }

    /// Starts (or restarts) looping outbound ringback — played while an
    /// outbound call is ringing (180) or between ringing and real early
    /// media landing. Loops indefinitely (re-queuing its own cadence cycle,
    /// see the worker's epoch check) until `stop_ringback` is called.
    pub fn play_ringback_tone(&self) {
        let epoch = {
            let mut epoch = self.ringback_epoch.lock().unwrap();
            *epoch = epoch.wrapping_add(1);
            *epoch
        };
        let _ = self.queue_tx.send(QueuedSound::Ringback(epoch));
    }

    /// Stops a looping ringback started by `play_ringback_tone`, if one is
    /// running — bumps the epoch (so an in-flight cycle won't re-queue
    /// itself) and kills the current child if it's actually the ringback
    /// tone (never touches an unrelated in-flight sound).
    pub fn stop_ringback(&self) {
        {
            let mut epoch = self.ringback_epoch.lock().unwrap();
            *epoch = epoch.wrapping_add(1);
        }
        let mut guard = self.current_child.lock().unwrap();
        if let Some((SoundKind::Ringback, child)) = guard.as_mut() {
            let _ = child.start_kill();
        }
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
        {
            let mut epoch = self.incoming_ring_epoch.lock().unwrap();
            *epoch = epoch.wrapping_add(1);
        }
        let mut guard = self.current_child.lock().unwrap();
        if let Some((SoundKind::IncomingRing, child)) = guard.as_mut() {
            let _ = child.start_kill();
        }
    }
}
