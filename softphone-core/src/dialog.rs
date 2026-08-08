use crate::config::{DtmfMode, SipAccountConfig};
use crate::events::{CallId, CallState, CoreCommand, CoreEvent, LocalMediaInfo, RemoteMediaInfo};
use crate::sdp;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::{Dialog, DialogState, TerminatedReason};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::DialogId;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::{Header, Method, StatusCode, StatusCodeKind, Uri};
use rsipstack::transaction::TransactionReceiver;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::warn;

pub async fn incoming_request_loop(
    dialog_layer: Arc<DialogLayer>,
    mut incoming: TransactionReceiver,
    state_sender: rsipstack::dialog::dialog::DialogStateSender,
    contact: Uri,
) {
    while let Some(mut tx) = incoming.recv().await {
        if let Some(mut dialog) = dialog_layer.match_dialog(&tx) {
            tokio::spawn(async move {
                if let Err(e) = dialog.handle(&mut tx).await {
                    warn!(error = %e, "dialog handle error");
                }
            });
            continue;
        }

        match tx.original.method {
            Method::Invite => {
                match dialog_layer.get_or_create_server_invite(
                    &tx,
                    state_sender.clone(),
                    None,
                    Some(contact.clone()),
                ) {
                    Ok(mut invite_dialog) => {
                        tokio::spawn(async move {
                            if let Err(e) = invite_dialog.handle(&mut tx).await {
                                warn!(error = %e, "invite dialog handle error");
                            }
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to create server invite dialog");
                        tx.reply(StatusCode::ServerInternalError).await.ok();
                    }
                }
            }
            _ => {
                tx.reply(StatusCode::MethodNotAllowed).await.ok();
            }
        }
    }
}

enum Direction {
    Inbound,
    Outbound,
}

/// One tracked call. `dialog_id` is the most recently known `DialogId` (its
/// tags update over time for outbound dialogs); `call_id` is the stable
/// identifier handed to the UI; `line` is which of the (up to `MAX_LINES`)
/// concurrent call slots this occupies.
struct CurrentCall {
    call_id: CallId,
    line: u8,
    dialog_id: DialogId,
    direction: Direction,
    remote: Option<RemoteMediaInfo>,
    local_crypto_key: Option<String>,
    /// Our local RTP port, needed to re-offer the same media socket on a
    /// hold/resume re-INVITE. `None` until the media offer/answer is built
    /// (set alongside `local_crypto_key` in the `PlaceCall`/`AnswerCall`
    /// handlers below).
    local_rtp_port: Option<u16>,
    /// Lazily-spawned background worker that sends this call's DTMF SIP
    /// INFO requests one at a time (see the `SendDtmf` handler below for
    /// why: firing them all concurrently let a burst of quick key presses
    /// reach the PBX as overlapping/out-of-order INFO transactions, which
    /// read as "DTMF doesn't go through" or a truncated tone). Dropping
    /// `CurrentCall` drops this sender, which ends the worker's loop —
    /// self-cleaning, no explicit teardown needed.
    dtmf_tx: Option<mpsc::UnboundedSender<char>>,
    /// For an inbound call: every payload type the caller offered (in
    /// *their* priority order), stashed at `DialogState::Calling` time so
    /// `CoreCommand::AnswerCall` — which is when we actually know our own
    /// `SipAccountConfig::preferred_codecs` — can pick the best mutually
    /// supported codec via `sdp::select_payload_type` instead of just
    /// taking whichever one the caller listed first.
    remote_payload_types: Vec<u8>,
    /// The RFC 4733 `telephone-event` payload type the peer offered, if any
    /// — stashed alongside `remote_payload_types` for the same reason:
    /// `AnswerCall` needs it to decide whether to advertise telephone-event
    /// back in the answer (RFC 3264 requires an answer be a subset of what
    /// was offered).
    remote_telephone_event_pt: Option<u8>,
}

/// Up to 5 concurrent calls, matching the UI's Line 1-5 buttons. All lines
/// share the one already-registered SIP account (multiple concurrent calls,
/// not multiple registrations — see the plan doc for why per-line separate
/// extensions is a bigger, deferred effort).
const MAX_LINES: u8 = 5;

#[derive(Default)]
struct DialogTracker {
    lines: HashMap<u8, CurrentCall>,
}

impl DialogTracker {
    fn find(&self, call_id: &CallId) -> Option<&CurrentCall> {
        self.lines.values().find(|c| &c.call_id == call_id)
    }

    fn find_mut(&mut self, call_id: &CallId) -> Option<&mut CurrentCall> {
        self.lines.values_mut().find(|c| &c.call_id == call_id)
    }

    fn first_free_line(&self) -> Option<u8> {
        (1..=MAX_LINES).find(|line| !self.lines.contains_key(line))
    }
}

fn find_outbound_line(tracker: &DialogTracker, id: &DialogId) -> Option<u8> {
    tracker
        .lines
        .values()
        .find(|c| c.call_id == id.call_id && matches!(c.direction, Direction::Outbound))
        .map(|c| c.line)
}

#[allow(clippy::too_many_arguments)]
pub async fn dialog_state_loop(
    dialog_layer: Arc<DialogLayer>,
    mut state_rx: rsipstack::dialog::dialog::DialogStateReceiver,
    state_tx: rsipstack::dialog::dialog::DialogStateSender,
    event_tx: mpsc::Sender<CoreEvent>,
    mut command_rx: mpsc::Receiver<CoreCommand>,
    local_media_addr: IpAddr,
    contact: Uri,
    config: SipAccountConfig,
) {
    let mut tracker = DialogTracker::default();

    loop {
        tokio::select! {
            state = state_rx.recv() => {
                let Some(state) = state else { break };
                handle_dialog_state(&dialog_layer, &mut tracker, &event_tx, state).await;
            }
            cmd = command_rx.recv() => {
                let Some(cmd) = cmd else { break };
                if matches!(cmd, CoreCommand::Shutdown) {
                    break;
                }
                handle_command(&dialog_layer, &mut tracker, &event_tx, &state_tx, &contact, &config, local_media_addr, cmd).await;
            }
        }
    }
}

async fn handle_dialog_state(
    dialog_layer: &Arc<DialogLayer>,
    tracker: &mut DialogTracker,
    event_tx: &mpsc::Sender<CoreEvent>,
    state: DialogState,
) {
    match state {
        DialogState::Calling(id) => {
            let Some(line) = tracker.first_free_line() else {
                if let Some(Dialog::ServerInvite(dlg)) = dialog_layer.get_dialog(&id) {
                    dlg.reject(Some(StatusCode::BusyHere), Some("Busy here".into()))
                        .ok();
                }
                return;
            };

            let Some(Dialog::ServerInvite(dlg)) = dialog_layer.get_dialog(&id) else {
                return;
            };
            let initial_request = dlg.initial_request();
            let offer_sdp = String::from_utf8_lossy(&initial_request.body);
            let remote_offer = match sdp::parse_offer(&offer_sdp) {
                Ok(offer) => offer,
                Err(e) => {
                    warn!(error = %e, "failed to parse offer SDP, rejecting call");
                    dlg.reject(Some(StatusCode::NotAcceptableHere), None).ok();
                    return;
                }
            };
            let Some(remote_addr) = remote_offer.remote_addr else {
                warn!("offer SDP missing connection address, rejecting call");
                dlg.reject(Some(StatusCode::NotAcceptableHere), None).ok();
                return;
            };
            // A placeholder pick (just their first-listed codec) purely for
            // the informational `IncomingCall` event/`CurrentCall.remote`
            // seed — the *real* selection against our own preference order
            // happens in `AnswerCall` below, using `remote_payload_types`.
            let remote_info = RemoteMediaInfo {
                remote_addr,
                payload_type: remote_offer.payload_types.first().copied().unwrap_or(0),
                telephone_event_pt: remote_offer.telephone_event_pt,
                crypto_key: remote_offer.crypto_key,
            };

            let call_id = id.call_id.clone();
            tracker.lines.insert(
                line,
                CurrentCall {
                    call_id: call_id.clone(),
                    line,
                    dialog_id: id.clone(),
                    direction: Direction::Inbound,
                    remote: Some(remote_info.clone()),
                    local_crypto_key: None,
                    local_rtp_port: None,
                    dtmf_tx: None,
                    remote_payload_types: remote_offer.payload_types,
                    remote_telephone_event_pt: remote_offer.telephone_event_pt,
                },
            );

            let remote = initial_request
                .from_header()
                .map(|h| h.to_string())
                .unwrap_or_default();
            event_tx
                .send(CoreEvent::IncomingCall {
                    id: call_id,
                    line,
                    remote,
                    offer: remote_info,
                })
                .await
                .ok();
            dlg.ringing(None, None).ok();
        }
        DialogState::Early(id, resp) => {
            if find_outbound_line(tracker, &id).is_some() {
                match resp.status_code {
                    StatusCode::Ringing => {
                        event_tx
                            .send(CoreEvent::CallStateChanged {
                                id: id.call_id.clone(),
                                state: CallState::Ringing,
                            })
                            .await
                            .ok();
                    }
                    // 183 Session Progress with an SDP body: the far end
                    // offered early media before the final 200 OK (e.g. an
                    // in-band ringback/announcement) — parse it the same
                    // way `Confirmed` below parses the real answer, so the
                    // UI can open an RTP stream toward it right away. A
                    // parse failure or missing connection address is a
                    // silent no-op here (unlike `Confirmed`'s reject-the-
                    // call handling) — this is only early media, not the
                    // final answer, so there's no reason to give up on the
                    // call over it.
                    StatusCode::SessionProgress if !resp.body.is_empty() => {
                        let sdp_str = String::from_utf8_lossy(&resp.body);
                        if let Ok(remote_offer) = sdp::parse_offer(&sdp_str)
                            && let Some(remote_addr) = remote_offer.remote_addr
                        {
                            let remote_info = RemoteMediaInfo {
                                remote_addr,
                                payload_type: remote_offer.payload_types.first().copied().unwrap_or(0),
                                telephone_event_pt: remote_offer.telephone_event_pt,
                                crypto_key: remote_offer.crypto_key,
                            };
                            event_tx
                                .send(CoreEvent::CallStateChanged {
                                    id: id.call_id.clone(),
                                    state: CallState::EarlyMedia { remote: remote_info },
                                })
                                .await
                                .ok();
                        }
                    }
                    _ => {}
                }
            }
        }
        DialogState::Confirmed(id, resp) => {
            let Some(line) = find_outbound_line(tracker, &id) else {
                return;
            };
            let answer_sdp = String::from_utf8_lossy(&resp.body);
            let remote_offer = match sdp::parse_offer(&answer_sdp) {
                Ok(offer) => offer,
                Err(e) => {
                    warn!(error = %e, "failed to parse SDP answer for outbound call");
                    return;
                }
            };
            let Some(remote_addr) = remote_offer.remote_addr else {
                warn!("SDP answer missing connection address");
                return;
            };
            // The far end's *answer* to our multi-codec offer — a proper
            // SDP answer settles on exactly one, so this is already the
            // final negotiated codec, no further selection needed.
            let remote_info = RemoteMediaInfo {
                remote_addr,
                payload_type: remote_offer.payload_types.first().copied().unwrap_or(0),
                telephone_event_pt: remote_offer.telephone_event_pt,
                crypto_key: remote_offer.crypto_key,
            };

            let local_crypto_key = if let Some(current) = tracker.lines.get_mut(&line) {
                current.dialog_id = id.clone();
                current.remote = Some(remote_info.clone());
                current.local_crypto_key.clone()
            } else {
                None
            };

            event_tx
                .send(CoreEvent::CallStateChanged {
                    id: id.call_id.clone(),
                    state: CallState::Answered {
                        local: LocalMediaInfo {
                            crypto_key: local_crypto_key,
                        },
                        remote: remote_info,
                    },
                })
                .await
                .ok();
        }
        DialogState::Terminated(id, reason) => {
            tracker.lines.retain(|_, c| c.call_id != id.call_id);
            dialog_layer.remove_dialog(&id);
            event_tx
                .send(CoreEvent::CallStateChanged {
                    id: id.call_id.clone(),
                    state: CallState::Terminated(terminated_reason_label(&reason)),
                })
                .await
                .ok();
        }
        _ => {}
    }
}

/// Short, human-readable label for why a call ended — surfaced to the UI
/// (`CallState::Terminated(String)`) so the footer's call-status line can
/// show something like "(hung up)" or "(busy)" instead of nothing, or the
/// raw `TerminatedReason` debug format.
fn terminated_reason_label(reason: &TerminatedReason) -> String {
    match reason {
        TerminatedReason::UacBye | TerminatedReason::UasBye => "hung up".to_string(),
        TerminatedReason::UacCancel => "call canceled".to_string(),
        TerminatedReason::UacBusy | TerminatedReason::UasBusy => "busy".to_string(),
        TerminatedReason::UasDecline => "declined".to_string(),
        TerminatedReason::Timeout => "timed out".to_string(),
        TerminatedReason::ProxyAuthRequired => "authentication failed".to_string(),
        TerminatedReason::ProxyError(code) => format!("server error ({code})"),
        // rsipstack's own INVITE transaction handling never actually
        // constructs `UacBusy`/`UasBusy` at runtime (only its test suite
        // does) — a real 486 from the PBX comes through here instead, so
        // this is the code path that actually needs to recognize "busy" for
        // the UI to play the real busy cadence (see `app.rs`'s
        // `CallState::Terminated` handling).
        TerminatedReason::UacOther(code) | TerminatedReason::UasOther(code)
            if *code == StatusCode::BusyHere =>
        {
            "busy".to_string()
        }
        // A few more common codes in plain English, for the same footer
        // readout — everything else still falls through to `code`'s own
        // `Display` (e.g. "488 Not Acceptable Here"), which is a real SIP
        // reason phrase and still readable, just not as casual as these.
        TerminatedReason::UacOther(code) | TerminatedReason::UasOther(code)
            if *code == StatusCode::NotFound =>
        {
            "number not found".to_string()
        }
        TerminatedReason::UacOther(code) | TerminatedReason::UasOther(code)
            if *code == StatusCode::TemporarilyUnavailable =>
        {
            "unavailable".to_string()
        }
        TerminatedReason::UacOther(code) | TerminatedReason::UasOther(code)
            if *code == StatusCode::Forbidden =>
        {
            "forbidden".to_string()
        }
        TerminatedReason::UacOther(code) | TerminatedReason::UasOther(code)
            if code.kind() == StatusCodeKind::ServerFailure =>
        {
            "server error".to_string()
        }
        TerminatedReason::UacOther(code) | TerminatedReason::UasOther(code) => code.to_string(),
    }
}

/// Build the callee URI for an outbound call: a bare digits/extension string
/// is wrapped as `sip:<callee>@<sip_server_host>;transport=<transport>`; a
/// value that already looks like a SIP URI is used as-is (lets a full URI be
/// pasted in, not just dialpad digits).
fn build_callee_uri(callee: &str, config: &SipAccountConfig) -> crate::error::Result<Uri> {
    if callee.starts_with("sip:") || callee.starts_with("sips:") {
        return Uri::try_from(callee)
            .map_err(rsipstack::Error::from)
            .map_err(Into::into);
    }
    let transport = match config.transport {
        crate::config::SipTransport::Udp => "udp",
        crate::config::SipTransport::Tcp => "tcp",
        crate::config::SipTransport::Tls => "tls",
    };
    Uri::try_from(
        format!(
            "sip:{}@{}:{};transport={}",
            callee, config.sip_server_host, config.sip_server_port, transport
        )
        .as_str(),
    )
    .map_err(rsipstack::Error::from)
    .map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    dialog_layer: &Arc<DialogLayer>,
    tracker: &mut DialogTracker,
    event_tx: &mpsc::Sender<CoreEvent>,
    state_tx: &rsipstack::dialog::dialog::DialogStateSender,
    contact: &Uri,
    config: &SipAccountConfig,
    local_media_addr: IpAddr,
    cmd: CoreCommand,
) {
    let srtp = config.srtp;
    match cmd {
        CoreCommand::PlaceCall {
            line,
            callee,
            local_rtp_port,
        } => {
            if tracker.lines.contains_key(&line) {
                event_tx
                    .send(CoreEvent::PlaceCallFailed {
                        line,
                        reason: "line already in a call".into(),
                    })
                    .await
                    .ok();
                return;
            }

            let callee_uri = match build_callee_uri(&callee, config) {
                Ok(uri) => uri,
                Err(e) => {
                    event_tx
                        .send(CoreEvent::PlaceCallFailed {
                            line,
                            reason: e.to_string(),
                        })
                        .await
                        .ok();
                    return;
                }
            };
            let caller_uri = match Uri::try_from(
                format!("sip:{}@{}", config.username, config.sip_server_host).as_str(),
            ) {
                Ok(uri) => uri,
                Err(e) => {
                    event_tx
                        .send(CoreEvent::PlaceCallFailed {
                            line,
                            reason: e.to_string(),
                        })
                        .await
                        .ok();
                    return;
                }
            };

            let preferred_payload_types: Vec<u8> =
                config.preferred_codecs.iter().map(|c| c.payload_type()).collect();
            let local_offer = sdp::generate_offer(
                local_rtp_port,
                srtp,
                false,
                &preferred_payload_types,
                config.dtmf_mode != DtmfMode::InfoOnly,
            );
            let local_crypto_key = local_offer.crypto_key.clone();
            let offer_sdp = sdp::build_sdp(local_media_addr, &local_offer).to_string();
            let sip_call_id = format!("{:x}-oxidesip", rand::random::<u64>());

            let opt = InviteOption {
                caller: caller_uri,
                callee: callee_uri,
                contact: contact.clone(),
                credential: Some(Credential {
                    username: config.username.clone(),
                    password: config.password.clone(),
                    realm: None,
                }),
                content_type: Some("application/sdp".into()),
                offer: Some(offer_sdp.into_bytes()),
                call_id: Some(sip_call_id.clone()),
                ..Default::default()
            };

            // do_invite_async, not the blocking do_invite: this function runs
            // inline inside dialog_state_loop's select!, so awaiting a
            // blocking invite here would stall all other event/command
            // processing until the call is answered or fails.
            let (client_dialog, join) = match dialog_layer.do_invite_async(opt, state_tx.clone())
            {
                Ok(res) => res,
                Err(e) => {
                    warn!(error = %e, "failed to start outbound invite");
                    event_tx
                        .send(CoreEvent::PlaceCallFailed {
                            line,
                            reason: e.to_string(),
                        })
                        .await
                        .ok();
                    return;
                }
            };

            tracker.lines.insert(
                line,
                CurrentCall {
                    call_id: sip_call_id.clone(),
                    line,
                    dialog_id: client_dialog.id(),
                    direction: Direction::Outbound,
                    remote: None,
                    local_crypto_key,
                    local_rtp_port: Some(local_rtp_port),
                    dtmf_tx: None,
                    // Not meaningful for an outbound call — we're the
                    // offerer, so codec selection happens on the far end,
                    // not here.
                    remote_payload_types: Vec::new(),
                    remote_telephone_event_pt: None,
                },
            );

            // Early/Confirmed/Terminated for this call flow through state_tx
            // (handled above in handle_dialog_state) same as any other
            // dialog. This watcher only covers the gap where process_invite
            // fails at a level low enough that no DialogState::Terminated is
            // ever emitted (e.g. a transport send failure).
            let watcher_event_tx = event_tx.clone();
            let fallback_call_id = sip_call_id.clone();
            tokio::spawn(async move {
                match join.await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        warn!(error = %e, "outbound invite failed");
                        watcher_event_tx
                            .send(CoreEvent::CallStateChanged {
                                id: fallback_call_id,
                                state: CallState::Terminated(format!("invite error: {e}")),
                            })
                            .await
                            .ok();
                    }
                    Err(e) => {
                        warn!(error = %e, "outbound invite task failed");
                        watcher_event_tx
                            .send(CoreEvent::CallStateChanged {
                                id: fallback_call_id,
                                state: CallState::Terminated(format!("invite task error: {e}")),
                            })
                            .await
                            .ok();
                    }
                }
            });

            event_tx
                .send(CoreEvent::OutgoingCallStarted {
                    id: sip_call_id,
                    line,
                })
                .await
                .ok();
        }
        CoreCommand::AnswerCall { id, local_rtp_port } => {
            let Some(current) = tracker.find(&id) else {
                return;
            };
            if !matches!(current.direction, Direction::Inbound) {
                return;
            }
            let dialog_id = current.dialog_id.clone();
            let Some(remote) = current.remote.clone() else {
                return;
            };
            let remote_payload_types = current.remote_payload_types.clone();
            let remote_telephone_event_pt = current.remote_telephone_event_pt;
            let Some(Dialog::ServerInvite(dlg)) = dialog_layer.get_dialog(&dialog_id) else {
                return;
            };

            // Pick the best codec *we* can offer that the caller also
            // listed — walks our own `preferred_codecs` priority order
            // rather than just taking whatever they offered first (see
            // `sdp::select_payload_type`). Falls back to whatever they
            // offered if none of our preferred codecs were among them, and
            // to the (pre-negotiation placeholder) `remote.payload_type` if
            // even that's somehow empty — the far end tolerating a
            // mismatch is still better than refusing the call outright.
            let preferred_payload_types: Vec<u8> =
                config.preferred_codecs.iter().map(|c| c.payload_type()).collect();
            let answer_payload_type =
                sdp::select_payload_type(&remote_payload_types, &preferred_payload_types)
                    .unwrap_or(remote.payload_type);
            // RFC 3264: an answer may only include what the offer included —
            // only advertise telephone-event back if the caller offered it
            // *and* our own mode allows it.
            let answer_telephone_event =
                remote_telephone_event_pt.is_some() && config.dtmf_mode != DtmfMode::InfoOnly;
            let local_offer = sdp::generate_offer(
                local_rtp_port,
                srtp,
                false,
                &[answer_payload_type],
                answer_telephone_event,
            );
            let local_crypto_key = local_offer.crypto_key.clone();
            let answer_sdp = sdp::build_sdp(local_media_addr, &local_offer).to_string();
            let headers = vec![Header::ContentType("application/sdp".into())];
            if let Err(e) = dlg.accept(Some(headers), Some(answer_sdp.into_bytes())) {
                warn!(error = %e, "failed to accept call");
                return;
            }
            if let Some(current) = tracker.find_mut(&id) {
                current.local_crypto_key = local_crypto_key.clone();
                current.local_rtp_port = Some(local_rtp_port);
            }

            // `remote` still carries whatever the caller *offered* —
            // override its `telephone_event_pt` with what our own answer
            // actually declared (`answer_telephone_event`, computed above
            // per RFC 3264 and honoring `config.dtmf_mode`), so the UI
            // (which uses this field to pick RTP telephone-event vs. SIP
            // INFO for outgoing DTMF on this call) doesn't send RFC 4733
            // packets our SDP answer never negotiated.
            let remote = RemoteMediaInfo {
                telephone_event_pt: if answer_telephone_event {
                    remote_telephone_event_pt
                } else {
                    None
                },
                ..remote
            };
            event_tx
                .send(CoreEvent::CallStateChanged {
                    id,
                    state: CallState::Answered {
                        local: LocalMediaInfo {
                            crypto_key: local_crypto_key,
                        },
                        remote,
                    },
                })
                .await
                .ok();
        }
        CoreCommand::HoldCall(id) => {
            handle_hold_resume(
                dialog_layer,
                tracker,
                event_tx,
                config,
                local_media_addr,
                id,
                true,
            )
            .await;
        }
        CoreCommand::ResumeCall(id) => {
            handle_hold_resume(
                dialog_layer,
                tracker,
                event_tx,
                config,
                local_media_addr,
                id,
                false,
            )
            .await;
        }
        CoreCommand::BlindTransfer { id, target } => {
            let Some(current) = tracker.find(&id) else {
                return;
            };
            let dialog_id = current.dialog_id.clone();
            let Some(dialog) = dialog_layer.get_dialog(&dialog_id) else {
                return;
            };
            let ok = match build_callee_uri(&target, config) {
                Ok(target_uri) => dialog.refer(target_uri, None, None).await.is_ok(),
                Err(e) => {
                    warn!(error = %e, "invalid transfer target");
                    false
                }
            };
            // Blind transfer means we're handing the call off, not staying
            // on it — a real deskphone drops its own leg the moment the
            // REFER is accepted, leaving the PBX to bridge the transferee to
            // the target. Without this, our leg just sits there `Active`
            // (some PBXes don't tear it down for us), so the line never
            // frees up and looks stuck to the UI.
            if ok {
                // Spawned rather than awaited inline — see the matching
                // comment on `CoreCommand::HangUp` above; same reasoning
                // applies to any BYE issued from inside this loop.
                tokio::spawn(async move {
                    dialog.hangup().await.ok();
                });
            }
            event_tx
                .send(CoreEvent::TransferResult { id, ok })
                .await
                .ok();
        }
        CoreCommand::RejectCall(id) => {
            let Some(current) = tracker.find(&id) else {
                return;
            };
            let dialog_id = current.dialog_id.clone();
            if let Some(Dialog::ServerInvite(dlg)) = dialog_layer.get_dialog(&dialog_id) {
                dlg.reject(Some(StatusCode::BusyHere), Some("Busy here".into()))
                    .ok();
            }
            event_tx
                .send(CoreEvent::CallStateChanged {
                    id,
                    state: CallState::Rejected,
                })
                .await
                .ok();
        }
        CoreCommand::HangUp(id) => {
            let Some(current) = tracker.find(&id) else {
                return;
            };
            let dialog_id = current.dialog_id.clone();
            // `Dialog::hangup()`, not a direct `.bye()` — for an outbound
            // call that hasn't been answered yet (still `Calling`/`Early`),
            // `ClientInviteDialog::bye()` is *silently a no-op* (it only
            // acts on a confirmed dialog). Hanging up an unanswered outgoing
            // call needs a CANCEL instead, which is exactly what `hangup()`
            // picks for an unconfirmed dialog — `bye()` alone left the UI
            // showing "Ending call..." forever with nothing actually
            // canceling the call on the wire.
            //
            // Spawned, not awaited inline — same reasoning as `SendDtmf`
            // below: this runs inside `dialog_state_loop`'s single
            // `select!`, so awaiting a slow/unreachable-peer BYE or CANCEL
            // transaction here (SIP transaction timeouts run several
            // seconds) would stall hold/answer/hang-up for *every other
            // line* until it resolves — that's "the whole app looks stuck,"
            // not just the one call being hung up. The resulting
            // `DialogState::Terminated` still reaches `dialog_state_loop`
            // normally either way, since `hangup()` pushes it straight to
            // `state_tx` itself, independent of who's awaiting the call.
            if let Some(dlg) = dialog_layer.get_dialog(&dialog_id) {
                tokio::spawn(async move {
                    dlg.hangup().await.ok();
                });
            }
        }
        CoreCommand::SendDtmf { id, digit } => {
            let Some(current) = tracker.find_mut(&id) else {
                return;
            };
            // Lazily spawn one background worker per call the first time it
            // sends DTMF, rather than a fresh `tokio::spawn` per digit. The
            // worker itself is still off the `dialog_state_loop` select! (so
            // a slow/dropped INFO response can't stall hold/hang-up/an
            // incoming BYE — the original reason for spawning at all), but
            // *within* the worker, digits are sent strictly one at a time,
            // awaiting each response before starting the next. Firing every
            // digit as an independent concurrent task let a quick burst of
            // key presses reach the PBX as overlapping/out-of-order INFO
            // transactions — that's what was showing up as "DTMF doesn't go
            // through" or a truncated/garbled tone on the far end.
            if current.dtmf_tx.is_none() {
                let dialog_id = current.dialog_id.clone();
                let Some(dialog) = dialog_layer.get_dialog(&dialog_id) else {
                    return;
                };
                let (tx, mut rx) = mpsc::unbounded_channel::<char>();
                let event_tx = event_tx.clone();
                let worker_id = id.clone();
                tokio::spawn(async move {
                    while let Some(digit) = rx.recv().await {
                        let headers = vec![Header::ContentType("application/dtmf-relay".into())];
                        let body = format!("Signal={digit}\r\nDuration=250\r\n").into_bytes();
                        let ok = match dialog.request(Method::Info, Some(headers), Some(body)).await {
                            Ok(_) => true,
                            Err(e) => {
                                warn!(error = %e, "dtmf send failed");
                                false
                            }
                        };
                        event_tx
                            .send(CoreEvent::DtmfResult {
                                id: worker_id.clone(),
                                digit,
                                ok,
                            })
                            .await
                            .ok();
                    }
                });
                current.dtmf_tx = Some(tx);
            }
            if let Some(tx) = &current.dtmf_tx {
                let _ = tx.send(digit);
            }
        }
        CoreCommand::Shutdown => {}
    }
}

/// Shared implementation for `CoreCommand::HoldCall`/`ResumeCall`: sends a
/// re-INVITE with (`hold = true`) or without (`hold = false`) `a=sendonly`,
/// reusing the same local RTP port/socket already in use — only the SDP
/// direction attribute changes, so `softphone-media`'s `MediaSession` is
/// untouched here; local audio muting happens UI-side off the emitted event.
#[allow(clippy::too_many_arguments)]
async fn handle_hold_resume(
    dialog_layer: &Arc<DialogLayer>,
    tracker: &mut DialogTracker,
    event_tx: &mpsc::Sender<CoreEvent>,
    config: &SipAccountConfig,
    local_media_addr: IpAddr,
    id: CallId,
    hold: bool,
) {
    let Some(current) = tracker.find(&id) else {
        return;
    };
    let Some(local_rtp_port) = current.local_rtp_port else {
        warn!("hold/resume requested before media was negotiated");
        return;
    };
    let Some(remote) = current.remote.clone() else {
        return;
    };
    let dialog_id = current.dialog_id.clone();
    let Some(dialog) = dialog_layer.get_dialog(&dialog_id) else {
        return;
    };

    // Keep using whatever codec is already active for this call rather than
    // re-deriving from `config.preferred_codecs` — a hold/resume re-INVITE
    // shouldn't silently renegotiate the codec mid-call.
    let active_payload_type = remote.payload_type;
    // Don't renegotiate telephone-event support mid-call either — keep
    // whatever was already agreed for this call.
    let active_telephone_event = remote.telephone_event_pt.is_some();
    let local_offer = sdp::generate_offer(
        local_rtp_port,
        config.srtp,
        hold,
        &[active_payload_type],
        active_telephone_event,
    );
    let local_crypto_key = local_offer.crypto_key.clone();
    let body = sdp::build_sdp(local_media_addr, &local_offer)
        .to_string()
        .into_bytes();
    let headers = vec![Header::ContentType("application/sdp".into())];

    // `local_crypto_key` is *our own* freshly-generated offer, not something
    // that depends on the re-INVITE's response, so it's safe (and correct)
    // to record it right away rather than waiting on the network round trip.
    if let Some(current) = tracker.find_mut(&id) {
        current.local_crypto_key = local_crypto_key.clone();
    }

    // The actual `reinvite().await` is spawned, not awaited inline — same
    // reasoning as `SendDtmf`/`HangUp` above: this function runs inside
    // `dialog_state_loop`'s single `select!`, and `select_line` fires a
    // Hold *and* a Resume on every line switch, so awaiting each re-INVITE's
    // full round trip here stalled hold/resume/hang-up for every other line
    // on every single line-switch click — that's what made "hitting any of
    // the line buttons" (and anything else sharing the loop) feel laggy.
    let event_tx = event_tx.clone();
    tokio::spawn(async move {
        let result = match &dialog {
            Dialog::ClientInvite(dlg) => dlg.reinvite(Some(headers), Some(body)).await,
            Dialog::ServerInvite(dlg) => dlg.reinvite(Some(headers), Some(body)).await,
            _ => return,
        };
        if let Err(e) = result {
            warn!(error = %e, hold, "re-invite failed");
            event_tx
                .send(CoreEvent::HoldResumeFailed {
                    id,
                    hold,
                    reason: e.to_string(),
                })
                .await
                .ok();
            return;
        }
        let state = if hold {
            CallState::Held
        } else {
            CallState::Answered {
                local: LocalMediaInfo {
                    crypto_key: local_crypto_key,
                },
                remote,
            }
        };
        event_tx
            .send(CoreEvent::CallStateChanged { id, state })
            .await
            .ok();
    });
}
