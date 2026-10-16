//! Demo backend: a scripted stand-in for `softphone-core` that needs no SIP
//! server, PipeWire or network. `oxidesip --demo` runs the real UI against
//! it (seeded accounts, contacts and history), and `--screenshot` uses it to
//! render the documentation images. Nothing is written to disk in this mode
//! (see `paths::set_persistence`).

use crate::app::Message;
use crate::contacts::Contact;
use crate::history::{CallDirection, CallOutcome, HistoryEntry};
use iced::Subscription;
use iced::futures::sink::SinkExt;
use iced::stream;
use softphone_core::config::SipAccountConfig;
use softphone_core::events::{CallState, CoreCommand, CoreEvent, LocalMediaInfo, RemoteMediaInfo, RemoteParty};
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

pub fn accounts() -> Vec<SipAccountConfig> {
    let mut office = SipAccountConfig {
        name: "Office".into(),
        sip_server_host: "pbx.example.lan".into(),
        username: "2001".into(),
        password: "demo".into(),
        ..SipAccountConfig::default()
    };
    office.register_expires = 300;
    let home = SipAccountConfig {
        name: "Home".into(),
        sip_server_host: "sip.example.net".into(),
        username: "1005".into(),
        password: "demo".into(),
        transport: softphone_core::config::SipTransport::Tls,
        sip_server_port: 5061,
        ..SipAccountConfig::default()
    };
    vec![office, home]
}

pub fn contacts() -> Vec<Contact> {
    [
        ("Alice Nguyen", "1001"),
        ("Dana Whitfield", "2004"),
        ("Front Desk", "100"),
        ("Jonas Berg", "+4915123456789"),
        ("Marcus Reed", "1017"),
        ("Priya Patel", "+15551234567"),
        ("Sam Okafor", "2010"),
        ("Support Queue", "7000"),
    ]
    .into_iter()
    .map(|(name, number)| Contact {
        name: name.into(),
        number: number.into(),
    })
    .collect()
}

/// `(number, name, direction, outcome, seconds ago, duration)`.
type HistoryRow = (&'static str, Option<&'static str>, CallDirection, CallOutcome, i64, u32);

pub fn history() -> Vec<HistoryEntry> {
    let now = crate::history::now_unix();
    let rows: &[HistoryRow] = &[
        (
            "7000",
            None,
            CallDirection::Outgoing,
            CallOutcome::Failed,
            9 * 86_400 + 3_600,
            0,
        ),
        (
            "+15559876543",
            Some("Unknown Caller"),
            CallDirection::Incoming,
            CallOutcome::Rejected,
            8 * 86_400,
            0,
        ),
        (
            "1001",
            None,
            CallDirection::Incoming,
            CallOutcome::Answered,
            6 * 86_400 + 900,
            1_254,
        ),
        (
            "2010",
            None,
            CallDirection::Outgoing,
            CallOutcome::Answered,
            5 * 86_400,
            311,
        ),
        (
            "+4915123456789",
            None,
            CallDirection::Outgoing,
            CallOutcome::Answered,
            4 * 86_400 + 7_200,
            2_840,
        ),
        ("100", None, CallDirection::Incoming, CallOutcome::Missed, 3 * 86_400, 0),
        (
            "1017",
            Some("Marcus Reed"),
            CallDirection::Incoming,
            CallOutcome::Answered,
            2 * 86_400 + 600,
            455,
        ),
        (
            "2004",
            None,
            CallDirection::Outgoing,
            CallOutcome::Answered,
            86_400 + 5_400,
            128,
        ),
        (
            "+15551234567",
            None,
            CallDirection::Incoming,
            CallOutcome::Missed,
            86_400 + 1_800,
            0,
        ),
        (
            "1001",
            None,
            CallDirection::Outgoing,
            CallOutcome::Answered,
            20_000,
            1_912,
        ),
        ("2010", None, CallDirection::Incoming, CallOutcome::Answered, 7_300, 61),
        (
            "+15559876543",
            Some("Unknown Caller"),
            CallDirection::Incoming,
            CallOutcome::Rejected,
            3_900,
            0,
        ),
        (
            "1017",
            Some("Marcus Reed"),
            CallDirection::Incoming,
            CallOutcome::Missed,
            2_700,
            0,
        ),
        ("2004", None, CallDirection::Outgoing, CallOutcome::Answered, 600, 734),
    ];
    rows.iter()
        .map(|(number, name, direction, outcome, ago, duration)| HistoryEntry {
            number: (*number).into(),
            name: name.map(str::to_string),
            direction: *direction,
            outcome: *outcome,
            unix_secs: now - ago,
            duration_secs: *duration,
        })
        .collect()
}

pub fn devices() -> (Vec<softphone_media::AudioDevice>, Vec<softphone_media::AudioDevice>) {
    let dev = |id: &str, description: &str| softphone_media::AudioDevice {
        id: id.into(),
        description: description.into(),
    };
    (
        vec![
            dev("alsa_input.usb-headset", "Jabra Evolve2 65 — Headset microphone"),
            dev("alsa_input.pci-0000_00_1f.3", "Built-in Audio — Analog microphone"),
        ],
        vec![
            dev("alsa_output.usb-headset", "Jabra Evolve2 65 — Headset"),
            dev("alsa_output.pci-0000_00_1f.3", "Built-in Audio — Speakers"),
        ],
    )
}

/// Out-of-band requests the UI can make of the demo core (things a real
/// PBX would do on its own, like placing a call *to* us).
#[derive(Debug, Clone)]
pub enum Trigger {
    /// Ring the given account on the next free line from this caller.
    Incoming { account: usize, caller: RemoteParty },
}

static TRIGGERS: OnceLock<Mutex<Option<mpsc::Sender<Trigger>>>> = OnceLock::new();

pub fn trigger(t: Trigger) {
    if let Some(tx) = TRIGGERS.get().and_then(|m| m.lock().ok()).and_then(|g| g.clone()) {
        let _ = tx.try_send(t);
    }
}

pub fn demo_caller() -> RemoteParty {
    RemoteParty {
        display_name: Some("Marcus Reed".into()),
        user: "1017".into(),
        uri: "sip:1017@pbx.example.lan".into(),
    }
}

fn remote_media() -> RemoteMediaInfo {
    RemoteMediaInfo {
        remote_addr: SocketAddr::from(([127, 0, 0, 1], 40_000)),
        payload_type: 0,
        telephone_event_pt: Some(101),
        crypto_key: None,
    }
}

/// Mirrors `bridge::subscription`'s shape: one command channel per account,
/// events tagged with the account index, and scripted replies.
pub fn subscription(accounts: &[SipAccountConfig]) -> Subscription<Message> {
    Subscription::run_with(accounts.len(), |count: &usize| {
        let count = *count;
        stream::channel(64, async move |mut output| {
            let (tagged_tx, mut tagged_rx) = mpsc::channel::<(usize, CoreEvent)>(128);
            let (trigger_tx, mut trigger_rx) = mpsc::channel::<Trigger>(8);
            *TRIGGERS.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(trigger_tx);

            let mut command_txs = Vec::new();
            for index in 0..count {
                let (command_tx, mut command_rx) = mpsc::channel::<CoreCommand>(32);
                command_txs.push(command_tx.clone());
                let _ = output.send(Message::CoreConnected(index, command_tx)).await;
                let events = tagged_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(350 + index as u64 * 400)).await;
                    let _ = events
                        .send((
                            index,
                            CoreEvent::Registered {
                                expires: 300,
                                rtt_ms: 9 + index as u32 * 14,
                            },
                        ))
                        .await;
                    let mut next_call = 1u32;
                    while let Some(command) = command_rx.recv().await {
                        match command {
                            CoreCommand::PlaceCall { line, .. } => {
                                let id = format!("demo-{index}-{next_call}");
                                next_call += 1;
                                let _ = events
                                    .send((index, CoreEvent::OutgoingCallStarted { id: id.clone(), line }))
                                    .await;
                                let ev = events.clone();
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_millis(500)).await;
                                    let _ = ev
                                        .send((
                                            index,
                                            CoreEvent::CallStateChanged {
                                                id: id.clone(),
                                                state: CallState::Ringing,
                                            },
                                        ))
                                        .await;
                                    tokio::time::sleep(Duration::from_millis(1_400)).await;
                                    let _ = ev
                                        .send((
                                            index,
                                            CoreEvent::CallStateChanged {
                                                id,
                                                state: CallState::Answered {
                                                    local: LocalMediaInfo::default(),
                                                    remote: remote_media(),
                                                },
                                            },
                                        ))
                                        .await;
                                });
                            }
                            CoreCommand::AnswerCall { id, .. } => {
                                let _ = events
                                    .send((
                                        index,
                                        CoreEvent::CallStateChanged {
                                            id,
                                            state: CallState::Answered {
                                                local: LocalMediaInfo::default(),
                                                remote: remote_media(),
                                            },
                                        },
                                    ))
                                    .await;
                            }
                            CoreCommand::RejectCall { id, .. } => {
                                let _ = events
                                    .send((
                                        index,
                                        CoreEvent::CallStateChanged {
                                            id,
                                            state: CallState::Rejected,
                                        },
                                    ))
                                    .await;
                            }
                            CoreCommand::HangUp(id) => {
                                let _ = events
                                    .send((
                                        index,
                                        CoreEvent::CallStateChanged {
                                            id,
                                            state: CallState::Terminated("hung up".into()),
                                        },
                                    ))
                                    .await;
                            }
                            CoreCommand::HoldCall(id) => {
                                let _ = events
                                    .send((
                                        index,
                                        CoreEvent::CallStateChanged {
                                            id,
                                            state: CallState::Held,
                                        },
                                    ))
                                    .await;
                            }
                            CoreCommand::ResumeCall(id) => {
                                let _ = events
                                    .send((
                                        index,
                                        CoreEvent::CallStateChanged {
                                            id,
                                            state: CallState::Answered {
                                                local: LocalMediaInfo::default(),
                                                remote: remote_media(),
                                            },
                                        },
                                    ))
                                    .await;
                            }
                            CoreCommand::SendDtmf { id, digit } => {
                                let _ = events
                                    .send((index, CoreEvent::DtmfResult { id, digit, ok: true }))
                                    .await;
                            }
                            CoreCommand::BlindTransfer { id, .. } => {
                                let _ = events
                                    .send((
                                        index,
                                        CoreEvent::TransferResult {
                                            id: id.clone(),
                                            ok: true,
                                        },
                                    ))
                                    .await;
                                let _ = events
                                    .send((
                                        index,
                                        CoreEvent::CallStateChanged {
                                            id,
                                            state: CallState::Terminated("transferred".into()),
                                        },
                                    ))
                                    .await;
                            }
                            CoreCommand::Shutdown => break,
                        }
                    }
                });
            }

            // Triggers (simulated inbound calls).
            let trigger_events = tagged_tx.clone();
            tokio::spawn(async move {
                let mut n = 100u32;
                while let Some(trigger) = trigger_rx.recv().await {
                    match trigger {
                        Trigger::Incoming { account, caller } => {
                            n += 1;
                            let _ = trigger_events
                                .send((
                                    account,
                                    CoreEvent::IncomingCall {
                                        id: format!("demo-in-{n}"),
                                        line: 2,
                                        remote: caller,
                                        offer: remote_media(),
                                    },
                                ))
                                .await;
                        }
                    }
                }
            });
            drop(tagged_tx);
            let _ = command_txs;

            while let Some((index, event)) = tagged_rx.recv().await {
                if output.send(Message::Core(index, event)).await.is_err() {
                    break;
                }
            }
        })
    })
}
