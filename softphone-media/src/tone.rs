//! Local audible feedback for dialpad key presses and other line-status
//! sounds (dial/busy/reorder/disconnect/ringback/incoming-ring): standard
//! DTMF dual-tone generation, played through one PipeWire playback stream
//! that stays open for the player's whole lifetime.
//!
//! Three properties, each learned the hard way, that the design keeps:
//!
//! - **One stream, never reopened.** Opening a stream per sound made the
//!   output device click (this app's default output is a wireless USB
//!   headset that pops when it wakes from idle). The stream plays silence
//!   between sounds, so the device stays awake and a sound starts cleanly.
//! - **One sound at a time, through a queue.** Digits pressed faster than a
//!   tone lasts play back to back rather than on top of each other.
//! - **The writer is paced by the buffer, not by a clock.** It may only
//!   queue `LOOKAHEAD_SAMPLES` ahead of what has been played and waits for
//!   room otherwise. Sleeping for "one chunk's duration" after each write
//!   instead runs slightly slow, falls a little further behind on every
//!   chunk, and eventually underruns on a long ringing loop — audible as
//!   crackle. And letting it queue far ahead makes a new sound wait behind
//!   everything already queued, and makes `stop_*` slow to take effect.
//!
//! (History: until 0.3.1 this module piped PCM into a `pw-play` child
//! process, because the app's own streams were being serviced only every
//! ~1.5s. That turned out to be a bug in how the app filled its stream
//! buffers — see `pipewire_io.rs` — not anything about PipeWire.)

use crate::error::MediaError;
use crate::pipewire_io::{self, PwThreadHandle};
use ringbuf::traits::{Observer, Producer, Split};
use ringbuf::{HeapProd, HeapRb};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
    generate_cadence_tone(
        RINGBACK_FREQUENCIES,
        RINGBACK_FADE_SAMPLES,
        RINGBACK_ON_MS,
        RINGBACK_OFF_MS,
        1,
    )
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
        let value = ((2.0 * std::f32::consts::PI * f1 * t).sin() + (2.0 * std::f32::consts::PI * f2 * t).sin()) * 0.22;
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

const DIGITS: [char; 12] = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '*', '#'];

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

/// How much of a sound the writer queues between interruption checks —
/// small enough that `stop_line_tone`/`stop_ringback`/
/// `stop_incoming_ringtone` feel immediate. Only a cancellation-check
/// granularity, not a playback rate (see `RingSink::write`).
const CHUNK_MS: u64 = 20;
const CHUNK_SAMPLES: usize = (SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;
/// How often the idle writer looks for a newly queued sound.
const IDLE_POLL_MS: u64 = 5;

/// How far ahead of playback the writer may queue: 40ms. Small enough that
/// a new sound or a `stop_*` takes effect almost at once, large enough to
/// ride out the writer task being scheduled a little late.
const LOOKAHEAD_SAMPLES: usize = (SAMPLE_RATE as usize * 40) / 1000;
/// Ring size; only `LOOKAHEAD_SAMPLES` of it is ever used.
const RING_SAMPLES: usize = SAMPLE_RATE as usize / 4;
/// If the stream takes nothing for this long it is gone (device removed,
/// PipeWire restarted): drop what is being written rather than wait forever.
const STALLED_AFTER: Duration = Duration::from_secs(2);

/// The writer's end of the ring the tone stream plays from.
struct RingSink {
    prod: HeapProd<i16>,
}

impl RingSink {
    /// Queues `chunk`, waiting whenever `LOOKAHEAD_SAMPLES` are already
    /// queued — so it returns about as fast as the audio is played, which is
    /// what paces the writer task. Returns `false` if the stream has stopped
    /// taking audio.
    async fn write(&mut self, mut chunk: &[i16]) -> bool {
        let mut waited = Duration::ZERO;
        while !chunk.is_empty() {
            let room = LOOKAHEAD_SAMPLES.saturating_sub(self.prod.occupied_len());
            if room == 0 {
                if waited >= STALLED_AFTER {
                    return false;
                }
                let nap = Duration::from_millis(5);
                tokio::time::sleep(nap).await;
                waited += nap;
                continue;
            }
            waited = Duration::ZERO;
            let pushed = self.prod.push_slice(&chunk[..room.min(chunk.len())]);
            chunk = &chunk[pushed..];
        }
        true
    }
}

pub struct DtmfTonePlayer {
    digit_samples: HashMap<char, Vec<i16>>,
    /// Sounds queue through here to a single background writer task, which
    /// streams them one at a time into the one persistent playback stream
    /// (see the module doc comment) — never opening a new stream per sound. Serializing through a queue means digits played
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
    /// The one playback stream every sound goes through (see the module doc
    /// comment). Held only so it lives exactly as long as this player:
    /// dropping it stops the stream.
    _stream: PwThreadHandle,
}

impl DtmfTonePlayer {
    /// Pre-renders every digit's tone (and the other line-status sounds)
    /// once into memory, and opens the one playback stream everything goes
    /// through for the rest of this player's lifetime. Must be called from
    /// within a tokio runtime (it spawns the writer task).
    pub fn start(playback_target: Option<String>) -> Result<Self, MediaError> {
        let mut digit_samples = HashMap::new();
        for digit in DIGITS {
            if let Some(samples) = generate_tone(digit) {
                digit_samples.insert(digit, samples);
            }
        }
        let dial_tone = generate_dial_tone();
        let busy_tone = generate_busy_tone();
        let reorder_tone = generate_reorder_tone();
        let disconnect_tone = generate_disconnect_tone();
        let ringback_cycle = generate_ringback_cycle();
        let incoming_ring_cycle = generate_incoming_ring_cycle();

        let (prod, cons) = HeapRb::<i16>::new(RING_SAMPLES).split();
        let stream = pipewire_io::spawn_tone_playback(cons, playback_target, "OxideSip tones".into())?;
        let mut sink = RingSink { prod };

        let (queue_tx, mut queue_rx) = mpsc::unbounded_channel::<QueuedSound>();
        let line_tone_epoch: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let ringback_epoch: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let incoming_ring_epoch: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let worker_line_tone_epoch = line_tone_epoch.clone();
        let worker_ringback_epoch = ringback_epoch.clone();
        let worker_incoming_ring_epoch = incoming_ring_epoch.clone();
        let worker_requeue_tx = queue_tx.clone();
        let worker_digit_samples = digit_samples.clone();

        // Paced by `RingSink::write` (see the module doc comment): no sleep
        // per chunk of sound. While idle nothing is written at all — the
        // stream plays silence on its own — so a newly queued sound starts
        // on an empty buffer, and the loop only naps between queue checks.
        tokio::spawn(async move {
            loop {
                let sound = match queue_rx.try_recv() {
                    Ok(sound) => sound,
                    Err(mpsc::error::TryRecvError::Empty) => {
                        tokio::time::sleep(Duration::from_millis(IDLE_POLL_MS)).await;
                        continue;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                };

                let kind = match &sound {
                    QueuedSound::DialTone | QueuedSound::BusyTone | QueuedSound::ReorderTone => SoundKind::LineTone,
                    QueuedSound::Ringback(_) => SoundKind::Ringback,
                    QueuedSound::IncomingRing(_) => SoundKind::IncomingRing,
                    QueuedSound::Digit(_) | QueuedSound::Disconnect => SoundKind::Other,
                };
                let samples: &[i16] = match &sound {
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
                'chunks: for chunk in samples.chunks(CHUNK_SAMPLES) {
                    let cancel = match (kind, &sound) {
                        (SoundKind::LineTone, _) => *worker_line_tone_epoch.lock().unwrap() != started_line_epoch,
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
                    if !sink.write(chunk).await {
                        tracing::warn!("tone stream stopped taking audio; dropping this sound");
                        aborted = true;
                        break 'chunks;
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
                        QueuedSound::IncomingRing(epoch) if *worker_incoming_ring_epoch.lock().unwrap() == epoch => {
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
            _stream: stream,
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
