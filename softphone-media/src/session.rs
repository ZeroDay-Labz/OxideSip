use crate::codec;
use crate::error::MediaError;
use crate::pipewire_io::{self, PipewireShared, PwThreadHandle};
use crate::rtp::RtpHeader;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

// A few hundred ms is plenty for the bridge relay — it only needs to smooth
// out the gap between "this line's recv_loop decoded a packet" and "the
// other line's send_loop next wakes up to drain it" (driven by that other
// line's own ~20ms capture cadence), not to buffer real backlog like
// RING_CAPACITY does. If a joined line's send_loop somehow stalls, letting
// this fill and drop is the right failure mode — better than growing
// latency on a live 3-way call.
const BRIDGE_RING_CAPACITY: usize = 4_000; // ~0.5s of 8kHz mono i16 samples

// Must comfortably exceed pipewire_io.rs's MAX_BUFFERED_SAMPLES (~2.5s) —
// see that constant's doc comment for why the playback side needs this much
// headroom on this system. Used for both rings for simplicity; the capture
// side doesn't need nearly as much (Notify-driven draining keeps it close
// to empty in steady state) but the memory cost either way is trivial
// (tens of KB).
const RING_CAPACITY: usize = 24_000; // ~3s of 8kHz mono i16 samples
const SAMPLES_PER_PACKET: usize = 160; // 20ms @ 8000Hz
const NOTIFY_TIMEOUT: Duration = Duration::from_millis(100);
// 5 packets = 100ms of reordering tolerance. Sustained clock drift/backlog
// growth (the dominant cause of both audible dropouts and rising latency on
// a real call) is now handled separately by `pipewire_io.rs`'s buffer-depth
// catch-up, so this window only needs to cover genuine short-range
// out-of-order arrival — keeping it modest bounds the worst-case latency a
// real gap can add.
const REORDER_WINDOW: usize = 5;

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
            self.samples[idx] =
                (self.samples[idx] as i32 + s as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
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
        remote_addr: SocketAddr,
        payload_type: u8,
        capture_target: Option<String>,
        playback_target: Option<String>,
        label: String,
    ) -> Result<Self, MediaError> {
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

        let shared = PipewireShared {
            notify: notify.clone(),
            mic_muted: mic_muted.clone(),
            input_gain_bits: input_gain_bits.clone(),
            input_level_bits: input_level_bits.clone(),
            output_level_bits: output_level_bits.clone(),
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

        let send_task = tokio::spawn(send_loop(
            socket.clone(),
            remote_addr,
            payload_type,
            capture_cons,
            notify,
            bridge_relay_in.clone(),
            record_mixer.clone(),
            cancel.child_token(),
        ));
        let recv_task = tokio::spawn(recv_loop(
            socket,
            payload_type,
            playback_prod,
            output_gain_bits.clone(),
            bridge_relay_out.clone(),
            record_mixer.clone(),
            secondary_relay.clone(),
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
        })
    }

    /// Starts accumulating a recording of this call — safe to call multiple
    /// times (each call resets to a fresh empty recording).
    pub fn start_recording(&self) {
        *self.record_mixer.lock().unwrap() = Some(RecordMixer::default());
    }

    /// Stops recording and returns the accumulated mono 8kHz PCM samples,
    /// if recording was active — `None` if `start_recording` was never
    /// called (or was already stopped) for this session.
    pub fn stop_recording(&self) -> Option<Vec<i16>> {
        self.record_mixer.lock().unwrap().take().map(|m| m.samples)
    }

    /// Starts, retargets, or stops streaming the far end's decoded voice to
    /// a second PipeWire sink (e.g. a Discord virtual-mic input) — one-way,
    /// never the local mic. `target` is a PipeWire node's `node.name`;
    /// `None` tears down the secondary stream entirely (the "None" option
    /// in the UI's picker). Safe to call repeatedly, including switching
    /// straight from one target to another.
    pub fn set_secondary_output(&self, target: Option<String>, label: String) -> Result<(), MediaError> {
        if let Some(handle) = self.secondary_pw.lock().unwrap().take() {
            handle.stop();
        }
        *self.secondary_relay.lock().unwrap() = None;

        let Some(target) = target else {
            return Ok(());
        };
        let (prod, cons) = HeapRb::<i16>::new(RING_CAPACITY).split();
        let handle = pipewire_io::spawn_playback(cons, Some(target), label)?;
        *self.secondary_relay.lock().unwrap() = Some(prod);
        *self.secondary_pw.lock().unwrap() = Some(handle);
        Ok(())
    }

    /// Bridges this call with `other` into a local 3-way conference: audio
    /// each side receives from its remote party is mixed into what the
    /// *other* side sends, so the two remote parties hear each other (not
    /// just us) — the actual thing that makes "Join" a real conference
    /// instead of just two independent calls unmuted at the same time. Local
    /// playback needs no special handling here: both lines' playback streams
    /// already mix naturally in PipeWire as long as neither is on hold.
    pub fn join_with(&self, other: &MediaSession) {
        let (prod_self_to_other, cons_self_to_other) =
            HeapRb::<i16>::new(BRIDGE_RING_CAPACITY).split();
        let (prod_other_to_self, cons_other_to_self) =
            HeapRb::<i16>::new(BRIDGE_RING_CAPACITY).split();

        *self.bridge_relay_out.lock().unwrap() = Some(prod_self_to_other);
        *other.bridge_relay_in.lock().unwrap() = Some(cons_self_to_other);
        *other.bridge_relay_out.lock().unwrap() = Some(prod_other_to_self);
        *self.bridge_relay_in.lock().unwrap() = Some(cons_other_to_self);
    }

    /// Tears down this call's half of a conference bridge — call on both
    /// former partners (e.g. when one line hangs up or the conference is
    /// split back apart) so a stale relay doesn't silently keep forwarding
    /// audio nobody asked for anymore.
    pub fn unjoin(&self) {
        *self.bridge_relay_out.lock().unwrap() = None;
        *self.bridge_relay_in.lock().unwrap() = None;
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
        let secondary_pw = self.secondary_pw.lock().unwrap().take();
        if let Some(h) = secondary_pw {
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

    /// `gain` is a linear multiplier applied to decoded playback samples
    /// (1.0 = unchanged); negative values are clamped to 0.
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

    /// Normalized 0.0-1.0 peak amplitude of the most recently captured
    /// microphone chunk.
    pub fn input_level(&self) -> f32 {
        f32::from_bits(self.input_level_bits.load(Ordering::Relaxed))
    }

    /// Normalized 0.0-1.0 peak amplitude of the most recently played-back
    /// audio chunk.
    pub fn output_level(&self) -> f32 {
        f32::from_bits(self.output_level_bits.load(Ordering::Relaxed))
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
/// PipeWire's actual delivery cadence (see the plan doc's root-cause
/// analysis for the exact numbers on this system).
async fn wait_for_data(notify: &Notify) {
    let _ = tokio::time::timeout(NOTIFY_TIMEOUT, notify.notified()).await;
}

async fn send_loop(
    socket: Arc<UdpSocket>,
    remote_addr: SocketAddr,
    payload_type: u8,
    mut capture_cons: ringbuf::HeapCons<i16>,
    notify: Arc<Notify>,
    bridge_relay_in: Arc<Mutex<Option<HeapCons<i16>>>>,
    record_mixer: Arc<Mutex<Option<RecordMixer>>>,
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

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = wait_for_data(&notify) => {
                // Drain every full packet currently available (not just
                // one) — `Notify` collapses back-to-back `notify_one()`
                // calls made before we wake into a single permit, so
                // failing to loop here would let backlog silently build up
                // whenever PipeWire fires faster than we drain.
                if capture_cons.occupied_len() < SAMPLES_PER_PACKET {
                    empty_wakes += 1;
                }
                while capture_cons.occupied_len() >= SAMPLES_PER_PACKET {
                    let got = capture_cons.pop_slice(&mut scratch);
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
                    let _ = socket.send_to(&header.build_packet(&payload), remote_addr).await;
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
            }
        }
    }
}

async fn recv_loop(
    socket: Arc<UdpSocket>,
    payload_type: u8,
    mut playback_prod: ringbuf::HeapProd<i16>,
    output_gain_bits: Arc<AtomicU32>,
    bridge_relay_out: Arc<Mutex<Option<HeapProd<i16>>>>,
    record_mixer: Arc<Mutex<Option<RecordMixer>>>,
    secondary_relay: Arc<Mutex<Option<HeapProd<i16>>>>,
    cancel: CancellationToken,
) {
    let codec = codec::Codec::from_payload_type(payload_type);
    let mut buf = [0u8; 2048];
    let mut reorder = ReorderBuffer::new();
    let mut received_count: u64 = 0;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = socket.recv_from(&mut buf) => {
                let Ok((n, _from)) = result else { continue };
                let Ok((header, payload)) = RtpHeader::decode(&buf[..n]) else { continue };
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
                let gain = f32::from_bits(output_gain_bits.load(Ordering::Relaxed));
                let samples: Vec<i16> = payload
                    .iter()
                    .map(|&b| {
                        let s = codec.decode(b) as f32 * gain;
                        s.clamp(i16::MIN as f32, i16::MAX as f32) as i16
                    })
                    .collect();
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

/// Small RTP-sequence-aware reorder/loss-tolerant buffer sitting in front of
/// the playback ring buffer. Not a full RFC 3550 jitter buffer — just enough
/// to stop minor arrival jitter (normal even on a LAN) from producing an
/// audible gap on every out-of-order packet. Buffers up to `REORDER_WINDOW`
/// packets waiting for a gap to fill; if the gap doesn't fill in time (real
/// loss, not just reordering), forces the window forward rather than
/// stalling playback indefinitely.
struct ReorderBuffer {
    expected: Option<u16>,
    pending: Vec<(u16, Vec<i16>)>,
}

impl ReorderBuffer {
    fn new() -> Self {
        Self {
            expected: None,
            pending: Vec::new(),
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
        self.pending
            .sort_unstable_by_key(|(s, _)| s.wrapping_sub(expected));

        if self.pending.len() > REORDER_WINDOW {
            // Real loss, not just reordering: force the window forward to
            // whatever the earliest still-pending packet is, rather than
            // waiting forever for a gap that may never fill.
            tracing::info!(
                expected,
                forced_to = self.pending[0].0,
                window = REORDER_WINDOW,
                "reorder buffer: forcing window forward, gap never filled"
            );
            self.expected = Some(self.pending[0].0);
        }

        let mut out = Vec::new();
        while let Some(pos) = self
            .pending
            .iter()
            .position(|(s, _)| *s == self.expected.unwrap())
        {
            let (_, samples) = self.pending.remove(pos);
            out.extend(samples);
            self.expected = Some(self.expected.unwrap().wrapping_add(1));
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
    fn sustained_loss_forces_window_forward() {
        let mut rb = ReorderBuffer::new();
        assert_eq!(rb.push(0, vec![1]), vec![1]); // expected -> 1
        // 1 is missing; push enough successors to fill the window exactly
        // (pending.len() == REORDER_WINDOW is still tolerated)...
        let last_tolerated = 1 + REORDER_WINDOW as u16;
        for seq in 2..=last_tolerated {
            assert!(rb.push(seq, vec![seq as i16]).is_empty());
        }
        // ...one more tips pending.len() past the window: force forward,
        // releasing everything buffered plus this packet, in order.
        let overflow_seq = last_tolerated + 1;
        let released = rb.push(overflow_seq, vec![overflow_seq as i16]);
        let expected: Vec<i16> = (2..=overflow_seq).map(|s| s as i16).collect();
        assert_eq!(released, expected);
    }

    #[test]
    fn sequence_number_wraps_around() {
        let mut rb = ReorderBuffer::new();
        assert_eq!(rb.push(65535, vec![1]), vec![1]);
        assert_eq!(rb.push(0, vec![2]), vec![2]);
    }
}
