use crate::agc::Agc;
use crate::codec;
use crate::error::MediaError;
use crate::pipewire_io::{self, PipewireShared, PwThreadHandle};
use crate::rtp::RtpHeader;
use crate::srtp::SrtpContext;
use crate::stats::{CallStats, ReceiverStats, StatsShared};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

// A few hundred ms is plenty for the bridge relay — it only needs to smooth
// out the gap between "this line's recv_loop decoded a packet" and "the
// other line's send_loop next wakes up to drain it" (driven by that other
// line's own ~20ms capture cadence), not to buffer real backlog like
// RING_CAPACITY does. If a joined line's send_loop somehow stalls, letting
// this fill and drop is the right failure mode — better than growing
// latency on a live 3-way call.
const BRIDGE_RING_CAPACITY: usize = 4_000; // ~0.5s of 8kHz mono i16 samples

// Must comfortably exceed the deepest pipewire_io.rs's adaptive playback
// buffer lets the ring get (MARGIN_MAX + a packet + BURST_HEADROOM, 320ms)
// before it skips ahead — this is only the hard limit on how much a burst
// can queue before that happens. Used for
// both rings for simplicity; the capture side needs even less
// (Notify-driven draining keeps it close to empty in steady state).
const RING_CAPACITY: usize = 4_000; // 0.5s of 8kHz mono i16 samples
const SAMPLES_PER_PACKET: usize = 160; // 20ms @ 8000Hz
const NOTIFY_TIMEOUT: Duration = Duration::from_millis(100);
// How many packets may be held back waiting for a missing one. Every packet
// held is 20ms the playback buffer isn't being fed, and that buffer runs
// only a few tens of ms deep, so this is deliberately one: a packet that
// arrives one place out of order is put back in order, and anything later
// than that is treated as lost and concealed (see `ReorderBuffer`). It
// only needs to cover genuine short-range
// out-of-order arrival — keeping it modest bounds the worst-case latency a
// real gap can add.
const REORDER_WINDOW: usize = 1;

// RFC 4733 §2.5.1.3: send the "end" packet multiple times in case one is
// lost — 3 is the commonly-used redundancy count (matches most SIP stacks).
const DTMF_END_PACKET_REPEATS: u8 = 3;
// RFC 4733 §2.3: "volume" is attenuation in dB below full scale, 0 (loudest)
// to 63 (quietest). 10 is a typical value used by other softphones.
const DTMF_VOLUME: u8 = 10;

/// A bound-but-unused UDP socket, reserved synchronously (no PipeWire
/// involved) so its port is known before the SDP answer/offer is built.
pub struct ReservedSocket {
    socket: std::net::UdpSocket,
    local_port: u16,
}

impl ReservedSocket {
    pub fn reserve() -> Result<Self, MediaError> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        let local_port = socket.local_addr()?.port();
        Ok(Self { socket, local_port })
    }

    pub fn local_port(&self) -> u16 {
        self.local_port
    }
}

/// What signaling negotiated for one call's audio.
#[derive(Debug, Clone)]
pub struct MediaParams {
    /// Where the peer wants its RTP.
    pub remote_addr: SocketAddr,
    /// The negotiated audio codec's RTP payload type.
    pub payload_type: u8,
    /// The negotiated RFC 4733 `telephone-event` payload type, if any.
    pub telephone_event_pt: Option<u8>,
    /// `Some` when the call negotiated SDES-SRTP: every packet sent is then
    /// encrypted, and every packet received must authenticate.
    pub srtp: Option<SrtpKeys>,
    /// Run the call through PipeWire's echo canceller, so the far end does
    /// not hear itself when you use speakers instead of a headset. If the
    /// canceller can't be started the call goes ahead without it.
    pub echo_cancel: bool,
}

/// The two base64 `a=crypto` keys of an SRTP call.
#[derive(Clone)]
pub struct SrtpKeys {
    /// Ours: what we encrypt with.
    pub local: String,
    /// The peer's: what we decrypt with.
    pub remote: String,
}

impl std::fmt::Debug for SrtpKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the keys themselves — this ends up in logs.
        f.write_str("SrtpKeys(..)")
    }
}

pub struct MediaSession {
    cancel: CancellationToken,
    pw_handle: Option<PwThreadHandle>,
    send_task: Option<tokio::task::JoinHandle<()>>,
    recv_task: Option<tokio::task::JoinHandle<()>>,
    mic_muted: Arc<AtomicBool>,
    input_gain_bits: Arc<AtomicU32>,
    output_gain_bits: Arc<AtomicU32>,
    input_level_bits: Arc<AtomicU32>,
    output_level_bits: Arc<AtomicU32>,
    /// Set by `join_with` when this call is conferenced with another line:
    /// audio this call *receives* is forwarded here, for the other line's
    /// `send_loop` to mix into what it sends — so each remote party hears
    /// the other, not just us.
    bridge_relay_out: Arc<Mutex<Option<HeapProd<i16>>>>,
    /// Set by `join_with` when conferenced: audio arriving from the other
    /// joined line, mixed into our own mic audio before it's sent.
    bridge_relay_in: Arc<Mutex<Option<HeapCons<i16>>>>,
    /// `Some` while call recording is active for this session — both
    /// directions' audio get additively mixed into it (see
    /// `RecordMixer::mix_in`) as it flows through `send_loop`/`recv_loop`.
    record_mixer: Arc<Mutex<Option<RecordMixer>>>,
    /// Set by `set_secondary_output` when a secondary output target is
    /// configured: a copy of every decoded remote-audio chunk `recv_loop`
    /// produces gets pushed here, for `secondary_pw`'s independent playback
    /// stream to play out to (e.g. a Discord virtual-mic sink) — one-way,
    /// the far end's voice only, never the local mic.
    secondary_relay: Arc<Mutex<Option<HeapProd<i16>>>>,
    /// The playback-only PipeWire stream feeding from `secondary_relay`, if
    /// a secondary output target is currently configured.
    secondary_pw: Mutex<Option<PwThreadHandle>>,
    /// Set by `set_secondary_input` when a secondary input target (e.g.
    /// Discord's own playback stream) is configured: a capture-only
    /// PipeWire stream feeds decoded samples in here for `send_loop` to
    /// mix into the outgoing mic audio, gated by `secondary_input_enabled`.
    secondary_input_relay: Arc<Mutex<Option<HeapCons<i16>>>>,
    /// The capture-only PipeWire stream feeding `secondary_input_relay`, if
    /// a secondary input target is currently configured.
    secondary_input_pw: Mutex<Option<PwThreadHandle>>,
    /// Live on/off switch for mixing `secondary_input_relay` into the
    /// outgoing call audio — separate from whether a target is *configured*
    /// (which requires tearing down/spawning a PipeWire stream) so the
    /// user's "turn it off and on at will" toggle is instant, never
    /// reconnecting anything.
    secondary_input_enabled: Arc<AtomicBool>,
    /// The negotiated RFC 4733 `telephone-event` payload type, if any.
    /// `None` means this call never negotiated RTP-based DTMF, so
    /// `send_dtmf` always returns `false` (caller falls back to SIP INFO).
    telephone_event_pt: Option<u8>,
    /// Queues a digit for `send_loop` to send as an RFC 4733 event train.
    dtmf_send_tx: mpsc::UnboundedSender<DtmfSend>,
    /// Digits `recv_loop` has decoded from the peer's RFC 4733 event
    /// packets, since the last `drain_received_dtmf` call.
    received_dtmf: Arc<Mutex<VecDeque<char>>>,
    encrypted: bool,
    stats: Arc<StatsShared>,
    auto_level: Arc<AtomicBool>,
    /// Where RTP is sent, and whose packets are accepted. Shared with both
    /// loops so `set_remote_addr` can move a running call.
    remote_addr: Arc<RwLock<SocketAddr>>,
    /// The echo canceller this call runs through, if any. Stopped after the
    /// call's own streams.
    echo_cancel_pw: Option<PwThreadHandle>,
}

/// One queued outbound DTMF digit, RFC 4733-style.
struct DtmfSend {
    digit: char,
    duration_ms: u32,
}

enum RecordChannel {
    Mic,
    Remote,
}

/// Accumulates a call recording as one shared timeline that both directions
/// write into independently: `send_loop` (mic, post-gain/post-bridge-mix,
/// exactly what's actually sent) and `recv_loop` (decoded remote audio) each
/// track their own write position into the same growing buffer and *add*
/// their samples in rather than overwrite, so the two sides of the
/// conversation sum together the way they would in a real recording instead
/// of one silently clobbering the other. Not sample-perfectly synchronized
/// (there's no shared clock forcing the two positions to line up exactly),
/// but both sides advance at the same real-time ~20ms cadence driven by
/// their own audio pipeline, so drift stays imperceptible for a
/// conversation-quality recording.
#[derive(Default)]
struct RecordMixer {
    samples: Vec<i16>,
    mic_pos: usize,
    remote_pos: usize,
}

impl RecordMixer {
    fn mix_in(&mut self, channel: RecordChannel, chunk: &[i16]) {
        let pos = match channel {
            RecordChannel::Mic => &mut self.mic_pos,
            RecordChannel::Remote => &mut self.remote_pos,
        };
        let start = *pos;
        let end = start + chunk.len();
        if self.samples.len() < end {
            self.samples.resize(end, 0);
        }
        for (i, &s) in chunk.iter().enumerate() {
            let idx = start + i;
            self.samples[idx] = (self.samples[idx] as i32 + s as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
        *pos = end;
    }
}

impl MediaSession {
    /// Spin up the real audio pipeline: PipeWire capture/playback on a
    /// dedicated thread, plus RTP send/recv tasks over `reserved`'s socket.
    /// Only call once a call is actually answered, never while ringing.
    /// `capture_target`/`playback_target` pin the streams to a specific
    /// PipeWire node (its `node.name`) instead of the system default; pass
    /// `None` to keep the default-device behavior. `label` (e.g. "OxideSip
    /// Line 2") names the underlying PipeWire nodes so concurrent calls'
    /// streams are distinguishable in pw-top/pavucontrol.
    pub async fn start(
        reserved: ReservedSocket,
        params: MediaParams,
        capture_target: Option<String>,
        playback_target: Option<String>,
        label: String,
    ) -> Result<Self, MediaError> {
        let MediaParams {
            remote_addr,
            payload_type,
            telephone_event_pt,
            srtp,
            echo_cancel,
        } = params;
        // Built before anything is started: a call that negotiated SRTP but
        // has an unusable key must fail here, never fall back to plain RTP.
        let (send_srtp, recv_srtp) = match &srtp {
            Some(keys) => (
                Some(SrtpContext::new(&keys.local)?),
                Some(SrtpContext::new(&keys.remote)?),
            ),
            None => (None, None),
        };
        let encrypted = srtp.is_some();
        reserved.socket.set_nonblocking(true)?;
        let socket = Arc::new(UdpSocket::from_std(reserved.socket)?);

        let (capture_prod, capture_cons) = Arc::new(HeapRb::<i16>::new(RING_CAPACITY)).split();
        let (playback_prod, playback_cons) = Arc::new(HeapRb::<i16>::new(RING_CAPACITY)).split();

        let notify = Arc::new(Notify::new());
        let mic_muted = Arc::new(AtomicBool::new(false));
        let input_gain_bits = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let output_gain_bits = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let input_level_bits = Arc::new(AtomicU32::new(0));
        let output_level_bits = Arc::new(AtomicU32::new(0));

        let stats = Arc::new(StatsShared::default());
        let auto_level = Arc::new(AtomicBool::new(true));

        let shared = PipewireShared {
            notify: notify.clone(),
            mic_muted: mic_muted.clone(),
            output_gain_bits: output_gain_bits.clone(),
            output_level_bits: output_level_bits.clone(),
            stats: stats.clone(),
        };

        // With echo cancellation the call talks to the canceller's virtual
        // devices and the canceller talks to the real ones.
        let (echo_cancel_pw, capture_target, playback_target) = if echo_cancel {
            match pipewire_io::spawn_echo_cancel(capture_target.clone(), playback_target.clone(), &label) {
                Ok(ec) => (Some(ec.handle), Some(ec.source), Some(ec.sink)),
                Err(e) => {
                    tracing::warn!(error = %e, "echo cancellation unavailable, continuing without it");
                    (None, capture_target, playback_target)
                }
            }
        } else {
            (None, capture_target, playback_target)
        };

        let pw_handle = pipewire_io::spawn(
            capture_prod,
            playback_cons,
            shared,
            capture_target,
            playback_target,
            label,
        )?;
        let cancel = CancellationToken::new();
        let bridge_relay_out: Arc<Mutex<Option<HeapProd<i16>>>> = Arc::new(Mutex::new(None));
        let bridge_relay_in: Arc<Mutex<Option<HeapCons<i16>>>> = Arc::new(Mutex::new(None));
        let record_mixer: Arc<Mutex<Option<RecordMixer>>> = Arc::new(Mutex::new(None));
        let secondary_relay: Arc<Mutex<Option<HeapProd<i16>>>> = Arc::new(Mutex::new(None));
        let secondary_input_relay: Arc<Mutex<Option<HeapCons<i16>>>> = Arc::new(Mutex::new(None));
        let secondary_input_enabled = Arc::new(AtomicBool::new(false));
        let (dtmf_send_tx, dtmf_send_rx) = mpsc::unbounded_channel::<DtmfSend>();
        let received_dtmf: Arc<Mutex<VecDeque<char>>> = Arc::new(Mutex::new(VecDeque::new()));

        let remote_addr = Arc::new(RwLock::new(remote_addr));
        let send_task = tokio::spawn(send_loop(
            socket.clone(),
            remote_addr.clone(),
            payload_type,
            telephone_event_pt,
            send_srtp,
            capture_cons,
            input_gain_bits.clone(),
            input_level_bits.clone(),
            auto_level.clone(),
            notify,
            bridge_relay_in.clone(),
            record_mixer.clone(),
            secondary_input_relay.clone(),
            secondary_input_enabled.clone(),
            dtmf_send_rx,
            cancel.child_token(),
        ));
        let recv_task = tokio::spawn(recv_loop(
            socket,
            remote_addr.clone(),
            payload_type,
            telephone_event_pt,
            recv_srtp,
            playback_prod,
            stats.clone(),
            auto_level.clone(),
            bridge_relay_out.clone(),
            record_mixer.clone(),
            secondary_relay.clone(),
            received_dtmf.clone(),
            cancel.child_token(),
        ));

        Ok(MediaSession {
            cancel,
            pw_handle: Some(pw_handle),
            send_task: Some(send_task),
            recv_task: Some(recv_task),
            mic_muted,
            input_gain_bits,
            output_gain_bits,
            input_level_bits,
            output_level_bits,
            bridge_relay_out,
            bridge_relay_in,
            record_mixer,
            secondary_relay,
            secondary_pw: Mutex::new(None),
            secondary_input_relay,
            secondary_input_pw: Mutex::new(None),
            secondary_input_enabled,
            telephone_event_pt,
            dtmf_send_tx,
            received_dtmf,
            encrypted,
            stats,
            auto_level,
            remote_addr,
            echo_cancel_pw,
        })
    }

    /// Points a running call at a new RTP address — the other side moved its
    /// media with a re-INVITE. Takes effect on the next packet either way.
    pub fn set_remote_addr(&self, addr: SocketAddr) {
        if let Ok(mut current) = self.remote_addr.write()
            && *current != addr
        {
            tracing::info!(%addr, "remote media address changed");
            *current = addr;
        }
    }

    /// How the incoming audio is doing right now: buffer delay, loss, jitter.
    pub fn stats(&self) -> CallStats {
        self.stats.snapshot(pipewire_io::SAMPLE_RATE)
    }

    /// Turns automatic level control on or off for both directions (see
    /// `agc.rs`). On by default; takes effect on the next 20ms frame.
    pub fn set_auto_level(&self, enabled: bool) {
        self.auto_level.store(enabled, Ordering::Relaxed);
    }

    /// Whether this call's audio is SRTP-encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    /// Starts accumulating a recording of this call — safe to call multiple
    /// times (each call resets to a fresh empty recording).
    pub fn start_recording(&self) {
        if let Ok(mut mixer) = self.record_mixer.lock() {
            *mixer = Some(RecordMixer::default());
        }
    }

    /// Stops recording and returns the accumulated mono 8kHz PCM samples,
    /// if recording was active — `None` if `start_recording` was never
    /// called (or was already stopped) for this session, or if the lock is
    /// poisoned (a degraded no-op, same as every other lock in this file,
    /// rather than propagating a panic from one poisoned call's state into
    /// every future call to this method).
    pub fn stop_recording(&self) -> Option<Vec<i16>> {
        self.record_mixer.lock().ok()?.take().map(|m| m.samples)
    }

    /// Starts, retargets, or stops streaming the far end's decoded voice to
    /// a second PipeWire sink (e.g. a Discord virtual-mic input) — one-way,
    /// never the local mic. `target` is a PipeWire node's `node.name`;
    /// `None` tears down the secondary stream entirely (the "None" option
    /// in the UI's picker). Safe to call repeatedly, including switching
    /// straight from one target to another.
    pub fn set_secondary_output(&self, target: Option<String>, label: String) -> Result<(), MediaError> {
        if let Ok(mut guard) = self.secondary_pw.lock()
            && let Some(handle) = guard.take()
        {
            handle.stop();
        }
        if let Ok(mut relay) = self.secondary_relay.lock() {
            *relay = None;
        }

        let Some(target) = target else {
            return Ok(());
        };
        let (prod, cons) = HeapRb::<i16>::new(RING_CAPACITY).split();
        let handle = pipewire_io::spawn_playback(cons, Some(target), label)?;
        if let Ok(mut relay) = self.secondary_relay.lock() {
            *relay = Some(prod);
        }
        if let Ok(mut guard) = self.secondary_pw.lock() {
            *guard = Some(handle);
        }
        Ok(())
    }

    /// Starts, retargets, or stops mixing another app's own playback stream
    /// (e.g. Discord's "what other members are saying") into this call's
    /// outgoing audio — the input counterpart to `set_secondary_output`.
    /// `target` is a PipeWire node's `node.name`, of `Stream/Output/Audio`
    /// class (see `list_app_playback_streams`'s doc comment for why that
    /// class specifically); `None` tears the stream down. Mixing itself is
    /// still gated by `secondary_input_enabled` — configuring a target here
    /// doesn't start injecting audio on its own.
    pub fn set_secondary_input(&self, target: Option<String>, label: String) -> Result<(), MediaError> {
        if let Ok(mut guard) = self.secondary_input_pw.lock()
            && let Some(handle) = guard.take()
        {
            handle.stop();
        }
        if let Ok(mut relay) = self.secondary_input_relay.lock() {
            *relay = None;
        }

        let Some(target) = target else {
            return Ok(());
        };
        let (prod, cons) = HeapRb::<i16>::new(RING_CAPACITY).split();
        let handle = pipewire_io::spawn_capture(prod, Some(target), label)?;
        if let Ok(mut relay) = self.secondary_input_relay.lock() {
            *relay = Some(cons);
        }
        if let Ok(mut guard) = self.secondary_input_pw.lock() {
            *guard = Some(handle);
        }
        Ok(())
    }

    /// Live on/off switch for `set_secondary_input`'s mixing into outgoing
    /// audio — cheap, no PipeWire reconnect, safe to call at any point
    /// during a live call (this is what backs the UI's quick toggle).
    pub fn set_secondary_input_enabled(&self, enabled: bool) {
        self.secondary_input_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Bridges this call with `other` into a local 3-way conference: audio
    /// each side receives from its remote party is mixed into what the
    /// *other* side sends, so the two remote parties hear each other (not
    /// just us) — the actual thing that makes "Join" a real conference
    /// instead of just two independent calls unmuted at the same time. Local
    /// playback needs no special handling here: both lines' playback streams
    /// already mix naturally in PipeWire as long as neither is on hold.
    pub fn join_with(&self, other: &MediaSession) {
        let (prod_self_to_other, cons_self_to_other) = HeapRb::<i16>::new(BRIDGE_RING_CAPACITY).split();
        let (prod_other_to_self, cons_other_to_self) = HeapRb::<i16>::new(BRIDGE_RING_CAPACITY).split();

        if let Ok(mut relay) = self.bridge_relay_out.lock() {
            *relay = Some(prod_self_to_other);
        }
        if let Ok(mut relay) = other.bridge_relay_in.lock() {
            *relay = Some(cons_self_to_other);
        }
        if let Ok(mut relay) = other.bridge_relay_out.lock() {
            *relay = Some(prod_other_to_self);
        }
        if let Ok(mut relay) = self.bridge_relay_in.lock() {
            *relay = Some(cons_other_to_self);
        }
    }

    /// Tears down this call's half of a conference bridge — call on both
    /// former partners (e.g. when one line hangs up or the conference is
    /// split back apart) so a stale relay doesn't silently keep forwarding
    /// audio nobody asked for anymore.
    pub fn unjoin(&self) {
        if let Ok(mut relay) = self.bridge_relay_out.lock() {
            *relay = None;
        }
        if let Ok(mut relay) = self.bridge_relay_in.lock() {
            *relay = None;
        }
    }

    /// Clean, explicit teardown: cancel + join the tokio tasks, then join
    /// the PipeWire thread via `spawn_blocking` (its `join()` is blocking).
    pub async fn stop(mut self) {
        self.cancel.cancel();
        if let Some(t) = self.send_task.take() {
            let _ = t.await;
        }
        if let Some(t) = self.recv_task.take() {
            let _ = t.await;
        }
        if let Some(h) = self.pw_handle.take() {
            let _ = tokio::task::spawn_blocking(move || h.stop()).await;
        }
        if let Some(h) = self.echo_cancel_pw.take() {
            let _ = tokio::task::spawn_blocking(move || h.stop()).await;
        }
        let secondary_pw = self.secondary_pw.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = secondary_pw {
            let _ = tokio::task::spawn_blocking(move || h.stop()).await;
        }
        let secondary_input_pw = self.secondary_input_pw.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = secondary_input_pw {
            let _ = tokio::task::spawn_blocking(move || h.stop()).await;
        }
    }

    pub fn set_mic_muted(&self, muted: bool) {
        tracing::info!(muted, "set_mic_muted");
        self.mic_muted.store(muted, Ordering::Relaxed);
    }

    pub fn mic_muted(&self) -> bool {
        self.mic_muted.load(Ordering::Relaxed)
    }

    /// `gain` is a linear multiplier applied to what the local speakers play
    /// (1.0 = unchanged); negative values are clamped to 0. Applied in the
    /// playback callback, so it is heard immediately and does not change
    /// recordings, bridged lines or the secondary output.
    pub fn set_output_volume(&self, gain: f32) {
        tracing::info!(gain, "set_output_volume");
        self.output_gain_bits.store(gain.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// `gain` is a linear multiplier applied to captured microphone samples
    /// before they're encoded/sent (1.0 = unchanged); negative values are
    /// clamped to 0.
    pub fn set_input_gain(&self, gain: f32) {
        tracing::info!(gain, "set_input_gain");
        self.input_gain_bits.store(gain.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// Normalized 0.0-1.0 peak amplitude of what was sent (after levelling
    /// and mic gain) since the last call to this. Reading resets it, so poll
    /// it from one place.
    pub fn input_level(&self) -> f32 {
        f32::from_bits(self.input_level_bits.swap(0, Ordering::Relaxed))
    }

    /// Normalized 0.0-1.0 peak amplitude of what was played (after the
    /// volume setting) since the last call to this. Reading resets it.
    pub fn output_level(&self) -> f32 {
        f32::from_bits(self.output_level_bits.swap(0, Ordering::Relaxed))
    }

    /// Queues `digit` to be sent as an RFC 4733 RTP telephone-event train.
    /// Returns `false` without queueing anything if this session never
    /// negotiated a telephone-event payload type (or `digit` isn't a valid
    /// DTMF character) — callers use that to fall back to SIP INFO instead.
    pub fn send_dtmf(&self, digit: char, duration_ms: u32) -> bool {
        if self.telephone_event_pt.is_none() || digit_to_event_code(digit).is_none() {
            return false;
        }
        self.dtmf_send_tx.send(DtmfSend { digit, duration_ms }).is_ok()
    }

    /// Drains DTMF digits the peer has sent via RFC 4733 since the last call
    /// — poll this the same way `input_level`/`output_level` are polled.
    pub fn drain_received_dtmf(&self) -> Vec<char> {
        self.received_dtmf
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        // Safety net for the case a caller drops the session instead of
        // calling stop().await (e.g. a Terminated event handled without
        // awaiting) — cancels the tasks and lets PwThreadHandle's own Drop
        // signal shutdown. Not a clean join; stop() is the preferred path.
        self.cancel.cancel();
    }
}

/// Wait for the PipeWire capture callback to signal new data, with a short
/// timeout as a dead-mic safety net only. Normal pacing comes entirely from
/// `notify`, which tracks PipeWire's real audio callback cadence instead of
/// an independent wall-clock timer — a fixed timer here previously caused
/// periodic dropouts because it ran at a slightly different rate than
/// PipeWire's actual delivery cadence.
async fn wait_for_data(notify: &Notify) {
    let _ = tokio::time::timeout(NOTIFY_TIMEOUT, notify.notified()).await;
}

#[allow(clippy::too_many_arguments)]
async fn send_loop(
    socket: Arc<UdpSocket>,
    remote: Arc<RwLock<SocketAddr>>,
    payload_type: u8,
    telephone_event_pt: Option<u8>,
    mut srtp: Option<SrtpContext>,
    mut capture_cons: ringbuf::HeapCons<i16>,
    input_gain_bits: Arc<AtomicU32>,
    input_level_bits: Arc<AtomicU32>,
    auto_level: Arc<AtomicBool>,
    notify: Arc<Notify>,
    bridge_relay_in: Arc<Mutex<Option<HeapCons<i16>>>>,
    record_mixer: Arc<Mutex<Option<RecordMixer>>>,
    secondary_input_relay: Arc<Mutex<Option<HeapCons<i16>>>>,
    secondary_input_enabled: Arc<AtomicBool>,
    mut dtmf_rx: mpsc::UnboundedReceiver<DtmfSend>,
    cancel: CancellationToken,
) {
    let codec = codec::Codec::from_payload_type(payload_type);
    let table = codec.build_decode_table();
    let mut sequence_number = rand::random::<u16>();
    let mut timestamp = rand::random::<u32>();
    let ssrc = rand::random::<u32>();
    let mut scratch = [0i16; SAMPLES_PER_PACKET];
    let mut sent_count: u64 = 0;
    let mut empty_wakes: u64 = 0;
    let mut agc = Agc::default();
    // Digits queued but not yet in flight — serialized one at a time (like
    // dialog.rs's SIP-INFO worker) so a burst of key presses doesn't
    // interleave multiple events' packets together.
    let mut dtmf_queue: VecDeque<DtmfSend> = VecDeque::new();
    let mut dtmf_active: Option<DtmfEventState> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            Some(req) = dtmf_rx.recv() => {
                if telephone_event_pt.is_some() {
                    dtmf_queue.push_back(req);
                }
            }
            _ = wait_for_data(&notify) => {
                // Drain every full packet currently available (not just
                // one) — `Notify` collapses back-to-back `notify_one()`
                // calls made before we wake into a single permit, so
                // failing to loop here would let backlog silently build up
                // whenever PipeWire fires faster than we drain.
                if capture_cons.occupied_len() < SAMPLES_PER_PACKET {
                    empty_wakes += 1;
                }
                let Ok(remote_addr) = remote.read().map(|addr| *addr) else { break };
                while capture_cons.occupied_len() >= SAMPLES_PER_PACKET {
                    let got = capture_cons.pop_slice(&mut scratch);
                    // The microphone's own processing, before anything else
                    // is mixed in: level it, apply the mic gain setting, and
                    // meter the result — what the far end hears of our voice.
                    if auto_level.load(Ordering::Relaxed) {
                        agc.process(&mut scratch[..got]);
                    }
                    let gain = f32::from_bits(input_gain_bits.load(Ordering::Relaxed));
                    let mut peak = 0u16;
                    for sample in &mut scratch[..got] {
                        *sample = (f32::from(*sample) * gain).clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
                        peak = peak.max(sample.unsigned_abs());
                    }
                    pipewire_io::store_peak(&input_level_bits, peak);
                    // If this call is conferenced with another line, mix in
                    // whatever audio that line's recv_loop has forwarded us
                    // — this is what lets the two remote parties hear each
                    // other, not just us.
                    if let Ok(mut relay) = bridge_relay_in.lock()
                        && let Some(cons) = relay.as_mut()
                    {
                        let mut foreign = [0i16; SAMPLES_PER_PACKET];
                        let n = cons.pop_slice(&mut foreign[..got]);
                        for i in 0..n {
                            scratch[i] = (scratch[i] as i32 + foreign[i] as i32)
                                .clamp(i16::MIN as i32, i16::MAX as i32)
                                as i16;
                        }
                    }
                    // Secondary input (e.g. Discord's own playback, mixed
                    // in so the SIP peer can hear it too). Always drained
                    // when present so a disabled/backgrounded relay never
                    // backs up into stale audio by the time it's re-enabled
                    // — but only actually mixed into what gets sent when
                    // the user's toggle is on.
                    if let Ok(mut relay) = secondary_input_relay.lock()
                        && let Some(cons) = relay.as_mut()
                    {
                        let mut injected = [0i16; SAMPLES_PER_PACKET];
                        let n = cons.pop_slice(&mut injected[..got]);
                        if secondary_input_enabled.load(Ordering::Relaxed) {
                            for i in 0..n {
                                scratch[i] = (scratch[i] as i32 + injected[i] as i32)
                                    .clamp(i16::MIN as i32, i16::MAX as i32)
                                    as i16;
                            }
                        }
                    }
                    if let Ok(mut mixer) = record_mixer.lock()
                        && let Some(mixer) = mixer.as_mut()
                    {
                        mixer.mix_in(RecordChannel::Mic, &scratch[..got]);
                    }
                    let payload: Vec<u8> = scratch[..got]
                        .iter()
                        .map(|&s| codec.encode(&table, s))
                        .collect();
                    let header = RtpHeader {
                        marker: false,
                        payload_type,
                        sequence_number,
                        timestamp,
                        ssrc,
                    };
                    send_packet(&socket, remote_addr, header.build_packet(&payload), srtp.as_mut()).await;
                    sequence_number = sequence_number.wrapping_add(1);
                    timestamp = timestamp.wrapping_add(got as u32);
                    sent_count += 1;
                    if sent_count.is_multiple_of(250) {
                        // Every ~5s at 20ms/packet: cheap visibility into
                        // whether the capture side is keeping up. A rising
                        // `empty_wakes` count means the mic ring buffer is
                        // running dry between wakes (real capture-side
                        // starvation, not just a network-side symptom).
                        tracing::info!(sent_count, empty_wakes, "send_loop stats");
                    }
                }

                // Advance at most one RFC 4733 event packet per wake — the
                // same ~20ms cadence audio packets ride, comfortably inside
                // RFC 4733 §2.5.1.3's "retransmit at least every 50ms". This
                // keeps `sequence_number`/`timestamp`/`ssrc` single-owned
                // (no locking) and means DTMF can never block or race audio.
                if let Some(pt) = telephone_event_pt {
                    if dtmf_active.is_none() {
                        dtmf_active = dtmf_queue
                            .pop_front()
                            .map(|req| DtmfEventState::new(req, timestamp));
                    }
                    if let Some(state) = dtmf_active.as_mut() {
                        let done = state
                            .send_next(&socket, remote_addr, pt, ssrc, &mut sequence_number, srtp.as_mut())
                            .await;
                        if done {
                            dtmf_active = None;
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn recv_loop(
    socket: Arc<UdpSocket>,
    remote: Arc<RwLock<SocketAddr>>,
    payload_type: u8,
    telephone_event_pt: Option<u8>,
    mut srtp: Option<SrtpContext>,
    mut playback_prod: ringbuf::HeapProd<i16>,
    stats: Arc<StatsShared>,
    auto_level: Arc<AtomicBool>,
    bridge_relay_out: Arc<Mutex<Option<HeapProd<i16>>>>,
    record_mixer: Arc<Mutex<Option<RecordMixer>>>,
    secondary_relay: Arc<Mutex<Option<HeapProd<i16>>>>,
    received_dtmf: Arc<Mutex<VecDeque<char>>>,
    cancel: CancellationToken,
) {
    let codec = codec::Codec::from_payload_type(payload_type);
    let mut buf = [0u8; 2048];
    let mut reorder = ReorderBuffer::new();
    let mut received_count: u64 = 0;
    let mut rejected_count: u64 = 0;
    let mut latch_candidate: Option<(SocketAddr, u32)> = None;
    let mut receiver_stats = ReceiverStats::default();
    let mut agc = Agc::default();
    let started = std::time::Instant::now();
    // Dedups a digit to one `received_dtmf` push per key press, keyed by
    // (event code, timestamp) — timestamp, not sequence number, is what the
    // sender holds frozen across the `DTMF_END_PACKET_REPEATS` redundant end
    // packets of one event train (see `DtmfEventState::send_next`).
    let mut last_surfaced_dtmf: Option<(u8, u32)> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = result else { continue };
                // The socket is bound to our port but never `connect()`-ed,
                // so anyone who can reach that port can send to it. Plain
                // RTP is therefore only accepted from the negotiated peer's
                // IP (any port: NATs rewrite source ports). On an SRTP call
                // the authentication check below is the filter instead, and
                // a stronger one, so the source IP is free to differ.
                let known = remote.read().ok().map(|addr| *addr);
                if srtp.is_none() && known.is_none_or(|addr| addr.ip() != from.ip()) {
                    continue;
                }
                // On an SRTP call nothing is looked at, let alone played,
                // before it has authenticated under the peer's key.
                let n = match srtp.as_mut() {
                    Some(ctx) => match ctx.unprotect(&mut buf[..n]) {
                        Ok(plain_len) => plain_len,
                        Err(e) => {
                            rejected_count += 1;
                            if rejected_count == 1 || rejected_count.is_multiple_of(250) {
                                tracing::warn!(error = %e, rejected_count, "dropping packet that failed SRTP checks");
                            }
                            continue;
                        }
                    },
                    None => n,
                };
                let Ok((header, payload)) = RtpHeader::decode(&buf[..n]) else { continue };
                // Symmetric RTP: send to wherever the peer's media really
                // comes from. Behind NAT (theirs or a media relay's) that
                // is often not the address in its SDP, and audio sent to
                // the SDP address goes nowhere. Waits for a few packets in
                // a row so one stray packet can't redirect the call.
                if known == Some(from) {
                    latch_candidate = None;
                } else {
                    match &mut latch_candidate {
                        Some((candidate, count)) if *candidate == from => *count += 1,
                        _ => latch_candidate = Some((from, 1)),
                    }
                    if latch_candidate.is_some_and(|(_, count)| count >= LATCH_AFTER_PACKETS)
                        && let Ok(mut addr) = remote.write()
                    {
                        tracing::info!(sdp_addr = %*addr, actual = %from, "sending RTP to where the peer's media comes from");
                        *addr = from;
                        latch_candidate = None;
                    }
                }
                if Some(header.payload_type) == telephone_event_pt {
                    if let [event_code, byte1, ..] = *payload {
                        let end = byte1 & 0x80 != 0;
                        let key = (event_code, header.timestamp);
                        if end && last_surfaced_dtmf != Some(key) {
                            if let Some(digit) = event_code_to_digit(event_code)
                                && let Ok(mut q) = received_dtmf.lock()
                            {
                                q.push_back(digit);
                            }
                            last_surfaced_dtmf = Some(key);
                        }
                    }
                    continue;
                }
                if header.payload_type != payload_type {
                    tracing::debug!(
                        got = header.payload_type,
                        expected = payload_type,
                        "rtp payload type mismatch, dropping packet"
                    );
                    continue;
                }
                received_count += 1;
                if received_count.is_multiple_of(250) {
                    tracing::info!(received_count, "recv_loop stats");
                }
                receiver_stats.on_packet(
                    header.sequence_number,
                    header.timestamp,
                    started.elapsed().as_secs_f64(),
                    pipewire_io::SAMPLE_RATE,
                );
                stats.received.store(receiver_stats.received(), Ordering::Relaxed);
                stats.lost.store(receiver_stats.lost(), Ordering::Relaxed);
                stats
                    .jitter_ms_bits
                    .store(receiver_stats.jitter_ms(pipewire_io::SAMPLE_RATE).to_bits(), Ordering::Relaxed);

                let mut samples: Vec<i16> = payload.iter().map(|&b| codec.decode(b)).collect();
                // Bring the far end to a standard level before it goes
                // anywhere (speakers, recording, a bridged line). The local
                // volume setting is applied later, at playback.
                if auto_level.load(Ordering::Relaxed) {
                    agc.process(&mut samples);
                }
                let ready = reorder.push(header.sequence_number, samples);
                if !ready.is_empty() {
                    if let Ok(mut relay) = bridge_relay_out.lock()
                        && let Some(prod) = relay.as_mut()
                    {
                        prod.push_iter(ready.iter().copied());
                    }
                    if let Ok(mut mixer) = record_mixer.lock()
                        && let Some(mixer) = mixer.as_mut()
                    {
                        mixer.mix_in(RecordChannel::Remote, &ready);
                    }
                    if let Ok(mut relay) = secondary_relay.lock()
                        && let Some(prod) = relay.as_mut()
                    {
                        prod.push_iter(ready.iter().copied());
                    }
                    playback_prod.push_iter(ready.into_iter());
                }
            }
        }
    }
}

/// Sends one RTP packet, encrypting it first on an SRTP call. A packet that
/// can't be encrypted is dropped, never sent in the clear.
async fn send_packet(socket: &UdpSocket, remote_addr: SocketAddr, mut packet: Vec<u8>, srtp: Option<&mut SrtpContext>) {
    if let Some(ctx) = srtp
        && ctx.protect(&mut packet).is_err()
    {
        return;
    }
    let _ = socket.send_to(&packet, remote_addr).await;
}

/// One RFC 4733 telephone-event 4-byte payload:
/// `event(8) | E(1) R(1) volume(6) | duration(16)`.
fn encode_telephone_event(event_code: u8, end: bool, volume: u8, duration: u16) -> [u8; 4] {
    [
        event_code,
        (u8::from(end) << 7) | (volume & 0x3F),
        (duration >> 8) as u8,
        duration as u8,
    ]
}

/// DTMF digit -> RFC 4733 §3.2 event code (0-9, *, #, then the A-D "letter"
/// events). Returns `None` for anything else, including e.g. a raw ASCII
/// letter this app's dialpad never produces.
fn digit_to_event_code(digit: char) -> Option<u8> {
    match digit {
        '0'..='9' => Some(digit as u8 - b'0'),
        '*' => Some(10),
        '#' => Some(11),
        'A'..='D' => Some(digit as u8 - b'A' + 12),
        'a'..='d' => Some(digit as u8 - b'a' + 12),
        _ => None,
    }
}

/// Inverse of `digit_to_event_code`, for decoding a peer's RFC 4733 events.
fn event_code_to_digit(code: u8) -> Option<char> {
    match code {
        0..=9 => Some((b'0' + code) as char),
        10 => Some('*'),
        11 => Some('#'),
        12..=15 => Some((b'A' + (code - 12)) as char),
        _ => None,
    }
}

/// Tracks one in-flight RFC 4733 event send across multiple `send_loop`
/// wakes: one packet is emitted per call to `send_next` (see `send_loop`'s
/// doc comment on why — this keeps the shared RTP sequence/timestamp state
/// single-owned with no locking, at a cadence that comfortably satisfies
/// RFC 4733 §2.5.1.3's retransmission requirement).
struct DtmfEventState {
    event_code: u8,
    /// The RTP timestamp at the *start* of this event — held fixed across
    /// every packet of the whole event train per RFC 4733 §2.5.1.3, even as
    /// the shared `send_loop` timestamp keeps advancing for audio packets
    /// sent alongside it.
    start_timestamp: u32,
    elapsed_ticks: u32,
    total_ticks: u32,
    marker_sent: bool,
    end_packets_sent: u8,
}

impl DtmfEventState {
    /// `event_code_for(req.digit)` is guaranteed valid here — `send_dtmf`
    /// already rejected anything `digit_to_event_code` can't encode before
    /// it ever reached the queue this state is built from.
    fn new(req: DtmfSend, start_timestamp: u32) -> Self {
        let event_code = digit_to_event_code(req.digit).unwrap_or(0);
        // RTP clock is 8000Hz regardless of codec, so ms -> ticks is *8; at
        // least one packet's worth so a very short/zero duration still
        // produces a valid (marker + immediate end) packet train.
        let total_ticks = (req.duration_ms * 8).max(SAMPLES_PER_PACKET as u32);
        Self {
            event_code,
            start_timestamp,
            elapsed_ticks: 0,
            total_ticks,
            marker_sent: false,
            end_packets_sent: 0,
        }
    }

    /// Sends exactly one packet of this event train. Returns `true` once
    /// the event is fully done (all redundant end packets sent) — the
    /// caller then drops this state and can start the next queued digit.
    async fn send_next(
        &mut self,
        socket: &UdpSocket,
        remote_addr: SocketAddr,
        payload_type: u8,
        ssrc: u32,
        sequence_number: &mut u16,
        srtp: Option<&mut SrtpContext>,
    ) -> bool {
        let marker = !self.marker_sent;
        self.marker_sent = true;

        if self.elapsed_ticks < self.total_ticks {
            self.elapsed_ticks = (self.elapsed_ticks + SAMPLES_PER_PACKET as u32).min(self.total_ticks);
        }
        let end = self.elapsed_ticks >= self.total_ticks;
        let duration = self.elapsed_ticks.min(u16::MAX as u32) as u16;

        let payload = encode_telephone_event(self.event_code, end, DTMF_VOLUME, duration);
        let header = RtpHeader {
            marker,
            payload_type,
            sequence_number: *sequence_number,
            timestamp: self.start_timestamp,
            ssrc,
        };
        send_packet(socket, remote_addr, header.build_packet(&payload), srtp).await;
        *sequence_number = sequence_number.wrapping_add(1);

        if end {
            self.end_packets_sent += 1;
            self.end_packets_sent >= DTMF_END_PACKET_REPEATS
        } else {
            false
        }
    }
}

/// Consecutive packets from a new source address before RTP is sent there.
const LATCH_AFTER_PACKETS: u32 = 3;

/// How many lost packets in a row get a concealment packet (a fading replay
/// of the last good one); the rest of a longer gap is silence.
const CONCEALED_PACKETS: usize = 3;
/// Longest gap that is filled in at all, so a sequence-number jump (a new
/// stream after hold, a restarted sender) can't inject seconds of silence.
const MAX_FILLED_PACKETS: usize = 10;

/// Small RTP-sequence-aware reorder/loss-tolerant stage in front of the
/// playback ring buffer. In-order packets pass straight through. A packet
/// arriving ahead of a missing one is held (up to `REORDER_WINDOW` packets)
/// in case the missing one is merely late; if it still hasn't come, it is
/// lost, and its place in the timeline is filled — first with a fading
/// replay of the last good packet, then with silence — so loss costs a soft
/// dip rather than making the playback buffer run dry.
struct ReorderBuffer {
    expected: Option<u16>,
    pending: Vec<(u16, Vec<i16>)>,
    /// The last packet released, for concealment.
    last: Vec<i16>,
}

impl ReorderBuffer {
    fn new() -> Self {
        Self {
            expected: None,
            pending: Vec::new(),
            last: Vec::new(),
        }
    }

    fn push(&mut self, seq: u16, samples: Vec<i16>) -> Vec<i16> {
        let expected = *self.expected.get_or_insert(seq);
        let diff = seq.wrapping_sub(expected) as i16;

        if diff < 0 {
            tracing::debug!(seq, expected, "reorder buffer: dropping late/duplicate packet");
            return Vec::new(); // duplicate or arrived too late; drop
        }

        self.pending.push((seq, samples));
        self.pending.sort_unstable_by_key(|(s, _)| s.wrapping_sub(expected));

        let mut out = Vec::new();
        let earliest = self.pending[0].0;
        if self.pending.len() > REORDER_WINDOW && earliest != expected {
            // Real loss, not just reordering: stop waiting, and stand in
            // for what never came.
            let missing = earliest.wrapping_sub(expected) as usize;
            tracing::debug!(expected, missing, "reorder buffer: concealing lost packets");
            for i in 0..missing.min(MAX_FILLED_PACKETS) {
                let gain = CONCEALED_PACKETS.saturating_sub(i + 1) as i32;
                out.extend(
                    self.last
                        .iter()
                        .map(|&s| (i32::from(s) * gain / CONCEALED_PACKETS as i32) as i16),
                );
            }
            self.expected = Some(earliest);
        }

        while let Some(pos) = self.pending.iter().position(|(s, _)| Some(*s) == self.expected) {
            let (seq, samples) = self.pending.remove(pos);
            out.extend_from_slice(&samples);
            self.last = samples;
            self.expected = Some(seq.wrapping_add(1));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_passes_through_immediately() {
        let mut rb = ReorderBuffer::new();
        assert_eq!(rb.push(0, vec![1]), vec![1]);
        assert_eq!(rb.push(1, vec![2]), vec![2]);
    }

    #[test]
    fn single_swap_is_reordered() {
        let mut rb = ReorderBuffer::new();
        assert_eq!(rb.push(0, vec![1]), vec![1]);
        assert!(rb.push(2, vec![3]).is_empty());
        assert_eq!(rb.push(1, vec![2]), vec![2, 3]);
    }

    #[test]
    fn duplicate_is_dropped() {
        let mut rb = ReorderBuffer::new();
        assert_eq!(rb.push(0, vec![1]), vec![1]);
        assert!(rb.push(0, vec![1]).is_empty());
    }

    #[test]
    fn a_lost_packet_is_concealed_once_the_next_one_arrives() {
        let mut rb = ReorderBuffer::new();
        assert_eq!(rb.push(0, vec![300, -300]), vec![300, -300]);
        // 1 never arrives. 2 is held in case 1 is only late...
        assert!(rb.push(2, vec![2, 2]).is_empty());
        // ...and when 3 comes instead, 1's place is filled with a quieter
        // replay of packet 0, followed by the real audio in order.
        assert_eq!(rb.push(3, vec![3, 3]), vec![200, -200, 2, 2, 3, 3]);
        // Back to passing straight through.
        assert_eq!(rb.push(4, vec![4, 4]), vec![4, 4]);
    }

    #[test]
    fn a_burst_of_loss_fades_out_and_keeps_its_length() {
        let mut rb = ReorderBuffer::new();
        rb.push(0, vec![300]);
        // 1..=5 lost: five packets' worth comes out ahead of 6 and 7, fading
        // over the first three (two thirds, one third, nothing).
        assert!(rb.push(6, vec![6]).is_empty());
        assert_eq!(rb.push(7, vec![7]), vec![200, 100, 0, 0, 0, 6, 7]);
    }

    #[test]
    fn a_sequence_jump_is_not_filled_with_seconds_of_silence() {
        let mut rb = ReorderBuffer::new();
        rb.push(0, vec![300]);
        assert!(rb.push(5_000, vec![1]).is_empty());
        let out = rb.push(5_001, vec![2]);
        assert_eq!(out.len(), MAX_FILLED_PACKETS + 2);
        assert_eq!(&out[MAX_FILLED_PACKETS..], &[1, 2]);
    }

    #[test]
    fn a_late_packet_after_being_concealed_is_dropped() {
        let mut rb = ReorderBuffer::new();
        rb.push(0, vec![300]);
        rb.push(2, vec![2]);
        rb.push(3, vec![3]);
        assert!(rb.push(1, vec![1]).is_empty());
    }

    #[test]
    fn sequence_number_wraps_around() {
        let mut rb = ReorderBuffer::new();
        assert_eq!(rb.push(65535, vec![1]), vec![1]);
        assert_eq!(rb.push(0, vec![2]), vec![2]);
    }

    #[test]
    fn digit_event_code_round_trips() {
        for digit in "0123456789*#ABCD".chars() {
            let code = digit_to_event_code(digit).unwrap();
            assert_eq!(event_code_to_digit(code), Some(digit));
        }
        // Lowercase letters encode the same events as uppercase, but decode
        // back to uppercase (RFC 4733 events don't carry case).
        assert_eq!(digit_to_event_code('a'), digit_to_event_code('A'));
        assert!(digit_to_event_code('x').is_none());
    }

    async fn drive_event(state: &mut DtmfEventState, packets: usize) -> Vec<(bool, bool, u16)> {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let remote_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut seq = 0u16;
        let mut out = Vec::new();
        for _ in 0..packets {
            let marker_before = !state.marker_sent;
            let done = state
                .send_next(&socket, remote_addr, 101, 0xdead_beef, &mut seq, None)
                .await;
            out.push((marker_before, done, state.elapsed_ticks as u16));
        }
        out
    }

    #[tokio::test]
    async fn dtmf_event_marker_only_on_first_packet() {
        let mut state = DtmfEventState::new(
            DtmfSend {
                digit: '5',
                duration_ms: 250,
            },
            1000,
        );
        let results = drive_event(&mut state, 3).await;
        assert!(results[0].0, "marker must be set on the first packet");
        assert!(!results[1].0, "marker must not repeat on later packets");
        assert!(!results[2].0);
    }

    #[tokio::test]
    async fn dtmf_event_timestamp_stays_frozen_across_packets() {
        let mut state = DtmfEventState::new(
            DtmfSend {
                digit: '5',
                duration_ms: 250,
            },
            42_000,
        );
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let remote_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut seq = 0u16;
        for _ in 0..5 {
            state.send_next(&socket, remote_addr, 101, 1, &mut seq, None).await;
            assert_eq!(state.start_timestamp, 42_000);
        }
    }

    #[tokio::test]
    async fn dtmf_event_sends_end_packet_exactly_three_times() {
        // 250ms @ 8kHz = 2000 ticks = ceil(2000/160) = 13 packets to reach
        // the end, then 3 total end-marked packets (this one plus 2 more).
        let mut state = DtmfEventState::new(
            DtmfSend {
                digit: '5',
                duration_ms: 250,
            },
            0,
        );
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let remote_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut seq = 0u16;
        let mut done_at = None;
        for i in 1..=20 {
            let done = state.send_next(&socket, remote_addr, 101, 1, &mut seq, None).await;
            if done {
                done_at = Some(i);
                break;
            }
        }
        assert_eq!(state.end_packets_sent, DTMF_END_PACKET_REPEATS);
        assert!(done_at.is_some(), "event never completed");
    }

    #[tokio::test]
    async fn dtmf_event_zero_duration_still_completes() {
        // A degenerate very-short digit still produces a valid marker+end
        // packet train rather than looping forever.
        let mut state = DtmfEventState::new(
            DtmfSend {
                digit: '1',
                duration_ms: 0,
            },
            0,
        );
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let remote_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut seq = 0u16;
        let mut completed = false;
        for _ in 0..(DTMF_END_PACKET_REPEATS as usize + 1) {
            if state.send_next(&socket, remote_addr, 101, 1, &mut seq, None).await {
                completed = true;
                break;
            }
        }
        assert!(completed);
    }
}
