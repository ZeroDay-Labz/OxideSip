//! Places a real call from one of the accounts in the app's own
//! `accounts.toml` and measures it — meant for a PBX echo test (`*43` on
//! Asterisk/FreePBX).
//!
//! ```text
//! cargo run -p softphone-ui --example echo_probe -- "<account name>" '*43' rtp 30
//! cargo run -p softphone-ui --example echo_probe -- "<account name>" '*43' media 30
//! ```
//!
//! Add `--srtp` anywhere to place the call with SRTP on, whatever the
//! account's own setting is; both modes then encrypt and decrypt.
//!
//! - `rtp`: no audio devices. Sends silence with a short 1800Hz burst every
//!   two seconds straight over RTP and times how long each burst takes to
//!   come back, plus how unevenly the PBX's packets arrive. That is the
//!   network + PBX part of the echo delay: the floor the app can't go below,
//!   and the jitter its playback buffer has to absorb.
//! - `media`: the app's real audio path (`MediaSession`, your mic and
//!   speakers). Run with `RUST_LOG=info` and read the `playback callback
//!   stats` lines: `buffered_ms` is the delay the app is adding.

use softphone_core::SoftphoneCore;
use softphone_core::config;
use softphone_core::events::{CallState, CoreCommand, CoreEvent, LocalMediaInfo, RemoteMediaInfo};
use softphone_media::codec::Codec;
use softphone_media::rtp::RtpHeader;
use softphone_media::srtp::SrtpContext;
use softphone_media::{MediaParams, MediaSession, ReservedSocket, SrtpKeys};
use std::f32::consts::TAU;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const SAMPLES_PER_PACKET: usize = 160;
const BURST_HZ: f32 = 1800.0;
const BURST_PACKETS: u32 = 3;
const BURST_EVERY_PACKETS: u32 = 100; // 2s

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let force_srtp = std::env::args().any(|a| a == "--srtp");
    let mut args = std::env::args().skip(1).filter(|a| a != "--srtp");
    let usage = "usage: echo_probe <account name> <number> <rtp|media> [seconds] [--srtp]";
    let account_name = args.next().expect(usage);
    let number = args.next().expect(usage);
    let mode = args.next().expect(usage);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);

    let path = dirs::config_dir()
        .expect("no config dir")
        .join("oxidesip/accounts.toml");
    let mut cfg = config::load_accounts(&path)
        .expect("failed to read accounts.toml")
        .into_iter()
        .find(|a| a.name.eq_ignore_ascii_case(&account_name))
        .unwrap_or_else(|| panic!("no account named {account_name:?} in {}", path.display()));
    cfg.srtp |= force_srtp;

    let (event_tx, mut event_rx) = mpsc::channel(32);
    let (command_tx, command_rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();
    let run_cancel = cancel.clone();
    let core_task = tokio::spawn(async move {
        if let Err(e) = SoftphoneCore::run(cfg, event_tx, command_rx, run_cancel).await {
            eprintln!("softphone core exited with error: {e}");
        }
    });

    // In `rtp` mode we keep the socket ourselves; in `media` mode it is
    // handed to the MediaSession once the call is answered.
    let mut reserved = None;
    let mut raw_socket = None;
    let local_rtp_port = if mode == "media" {
        let r = ReservedSocket::reserve().expect("reserve rtp port");
        let port = r.local_port();
        reserved = Some(r);
        port
    } else {
        let s = UdpSocket::bind("0.0.0.0:0").await.expect("bind rtp socket");
        let port = s.local_addr().unwrap().port();
        raw_socket = Some(s);
        port
    };

    let mut call_id = None;
    let mut placed = false;
    let deadline = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(deadline);
    let (local, remote): (LocalMediaInfo, RemoteMediaInfo) = loop {
        tokio::select! {
            _ = &mut deadline => { eprintln!("timed out waiting for the call to be answered"); std::process::exit(1); }
            event = event_rx.recv() => match event.expect("core stopped") {
                CoreEvent::Registered { rtt_ms, .. } if !placed => {
                    println!("registered (REGISTER round trip {rtt_ms}ms), calling {number}");
                    placed = true;
                    command_tx
                        .send(CoreCommand::PlaceCall { line: 1, callee: number.clone(), local_rtp_port })
                        .await
                        .ok();
                }
                CoreEvent::OutgoingCallStarted { id, .. } => call_id = Some(id),
                CoreEvent::CallStateChanged { state: CallState::Answered { local, remote }, .. } => break (local, remote),
                CoreEvent::CallStateChanged { state: CallState::Terminated(why), .. } => {
                    eprintln!("call ended before answer: {why}");
                    std::process::exit(1);
                }
                CoreEvent::RegistrationFailed { reason } | CoreEvent::RegistrationHalted { reason } => {
                    eprintln!("registration failed: {reason}");
                    std::process::exit(1);
                }
                CoreEvent::PlaceCallFailed { reason, .. } => {
                    eprintln!("call failed: {reason}");
                    std::process::exit(1);
                }
                _ => {}
            }
        }
    };
    let srtp = match (local.crypto_key, remote.crypto_key.clone()) {
        (Some(local), Some(remote)) => Some(SrtpKeys { local, remote }),
        (None, None) => None,
        _ => {
            eprintln!("only one side offered an SRTP key; not sending audio");
            std::process::exit(1);
        }
    };
    println!(
        "answered: {} to {} payload type {}; running {mode} for {seconds}s",
        if srtp.is_some() { "SRTP" } else { "plain RTP" },
        remote.remote_addr,
        remote.payload_type
    );

    if let Some(reserved) = reserved {
        let params = MediaParams {
            remote_addr: remote.remote_addr,
            payload_type: remote.payload_type,
            telephone_event_pt: remote.telephone_event_pt,
            srtp,
            echo_cancel: std::env::var("ECHO_CANCEL").is_ok(),
        };
        let session = MediaSession::start(reserved, params, None, None, "OxideSip echo probe".into())
            .await
            .expect("start media session");
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        session.stop().await;
    } else if let Some(socket) = raw_socket {
        rtp_probe(socket, &remote, srtp, seconds).await;
    }

    if let Some(id) = call_id {
        command_tx.send(CoreCommand::HangUp(id)).await.ok();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    command_tx.send(CoreCommand::Shutdown).await.ok();
    tokio::time::sleep(Duration::from_millis(500)).await;
    cancel.cancel();
    core_task.await.ok();
}

/// Fraction of a packet's energy that sits at `BURST_HZ` (Goertzel): ~1.0
/// for our burst, far lower for speech or silence.
fn burst_purity(samples: &[i16]) -> f32 {
    let coeff = 2.0 * (TAU * BURST_HZ / 8000.0).cos();
    let (mut s1, mut s2, mut energy) = (0f32, 0f32, 0f32);
    for &x in samples {
        let x = x as f32;
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
        energy += x * x;
    }
    if energy < 1.0e6 * samples.len() as f32 {
        return 0.0; // quieter than ~1000 RMS: not our burst
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    power / (energy * samples.len() as f32 / 2.0)
}

async fn rtp_probe(socket: UdpSocket, remote: &RemoteMediaInfo, srtp: Option<SrtpKeys>, seconds: u64) {
    let mut contexts = srtp.map(|keys| {
        (
            SrtpContext::new(&keys.local).expect("local srtp key"),
            SrtpContext::new(&keys.remote).expect("remote srtp key"),
        )
    });
    let mut rejected = 0u64;
    let codec = Codec::from_payload_type(remote.payload_type);
    let table = codec.build_decode_table();
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let end = Instant::now() + Duration::from_secs(seconds);

    let (mut seq, mut ts, mut n, mut phase) = (1u16, 0u32, 0u32, 0f32);
    let mut burst_sent: Option<Instant> = None;
    let mut round_trips: Vec<f32> = Vec::new();
    let mut last_arrival: Option<Instant> = None;
    let mut gaps_ms: Vec<f32> = Vec::new();
    let (mut received, mut lost, mut last_seq) = (0u64, 0u64, None::<u16>);
    let mut buf = [0u8; 2048];

    while Instant::now() < end {
        tokio::select! {
            _ = tick.tick() => {
                let in_burst = n % BURST_EVERY_PACKETS < BURST_PACKETS;
                if n % BURST_EVERY_PACKETS == 0 {
                    burst_sent = Some(Instant::now());
                    phase = 0.0;
                }
                let payload: Vec<u8> = (0..SAMPLES_PER_PACKET)
                    .map(|_| {
                        let s = if in_burst { (phase.sin() * 12_000.0) as i16 } else { 0 };
                        phase += TAU * BURST_HZ / 8000.0;
                        codec.encode(&table, s)
                    })
                    .collect();
                let header = RtpHeader {
                    marker: n == 0,
                    payload_type: remote.payload_type,
                    sequence_number: seq,
                    timestamp: ts,
                    ssrc: 0x0DE5_1B43,
                };
                let mut packet = header.build_packet(&payload);
                if let Some((tx, _)) = contexts.as_mut() {
                    tx.protect(&mut packet).expect("srtp protect");
                }
                let _ = socket.send_to(&packet, remote.remote_addr).await;
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(SAMPLES_PER_PACKET as u32);
                n += 1;
            }
            result = socket.recv_from(&mut buf) => {
                let now = Instant::now();
                let Ok((len, from)) = result else { continue };
                if from.ip() != remote.remote_addr.ip() { continue }
                let len = match contexts.as_mut() {
                    Some((_, rx)) => match rx.unprotect(&mut buf[..len]) {
                        Ok(plain_len) => plain_len,
                        Err(_) => { rejected += 1; continue }
                    },
                    None => len,
                };
                let Ok((header, payload)) = RtpHeader::decode(&buf[..len]) else { continue };
                if header.payload_type != remote.payload_type { continue }
                received += 1;
                if let Some(prev) = last_seq {
                    lost += u64::from(header.sequence_number.wrapping_sub(prev).wrapping_sub(1).min(50));
                }
                last_seq = Some(header.sequence_number);
                if let Some(prev) = last_arrival {
                    gaps_ms.push(now.duration_since(prev).as_secs_f32() * 1000.0);
                }
                last_arrival = Some(now);

                let samples: Vec<i16> = payload.iter().map(|&b| codec.decode(b)).collect();
                if let Some(sent) = burst_sent
                    && burst_purity(&samples) > 0.5
                {
                    // Where in this packet the burst starts.
                    let onset = samples.iter().position(|s| s.unsigned_abs() > 2_000).unwrap_or(0);
                    let ms = now.duration_since(sent).as_secs_f32() * 1000.0 + onset as f32 / 8.0;
                    println!("burst came back after {ms:.0}ms");
                    round_trips.push(ms);
                    burst_sent = None;
                }
            }
        }
    }

    println!("\nreceived {received} packets, {lost} lost");
    if contexts.is_some() {
        println!("SRTP: every one of those authenticated and decrypted; {rejected} packets failed the check");
    }
    if !gaps_ms.is_empty() {
        let mut late: Vec<f32> = gaps_ms.iter().map(|g| g - 20.0).collect();
        late.sort_by(f32::total_cmp);
        let pick = |q: f32| late[((late.len() - 1) as f32 * q) as usize];
        println!(
            "packet arrival vs. the ideal 20ms spacing: median {:+.1}ms, 99% within {:+.1}ms, worst {:+.1}ms",
            pick(0.5),
            pick(0.99),
            pick(1.0)
        );
    }
    if round_trips.is_empty() {
        println!("no bursts came back (still in the PBX's intro prompt? try a longer run)");
    } else {
        round_trips.sort_by(f32::total_cmp);
        println!(
            "network + PBX echo round trip over {} bursts: min {:.0}ms, median {:.0}ms, max {:.0}ms",
            round_trips.len(),
            round_trips[0],
            round_trips[round_trips.len() / 2],
            round_trips[round_trips.len() - 1]
        );
    }
}
