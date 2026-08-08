use std::net::SocketAddr;

/// Opaque call identifier handed to the UI. Derived from rsipstack's
/// `DialogId.call_id` (the bare SIP Call-ID) rather than `DialogId::to_string()`:
/// for an outbound dialog, `remote_tag` is empty until the callee responds, so
/// the stringified `DialogId` changes value mid-call, but the bare call_id is
/// stable for the whole dialog lifetime in both directions.
pub type CallId = String;

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteMediaInfo {
    pub remote_addr: SocketAddr,
    pub payload_type: u8,
    /// The negotiated RFC 4733 `telephone-event` payload type, if both sides
    /// advertised it in this offer/answer exchange. `softphone-ui` uses this
    /// to decide whether `MediaSession::send_dtmf` can be used, falling back
    /// to `CoreCommand::SendDtmf` (SIP INFO) when `None`.
    pub telephone_event_pt: Option<u8>,
    pub crypto_key: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LocalMediaInfo {
    pub crypto_key: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CallState {
    Ringing,
    /// A 183 Session Progress arrived with an SDP body before the final 200
    /// OK — the far end offered early media (e.g. an in-band ringback or
    /// announcement) and we can already open an RTP stream toward it.
    EarlyMedia {
        remote: RemoteMediaInfo,
    },
    Answered {
        local: LocalMediaInfo,
        remote: RemoteMediaInfo,
    },
    Held,
    Rejected,
    Terminated(String),
}

#[derive(Debug, Clone)]
pub enum CoreEvent {
    Registered { expires: u32, rtt_ms: u32 },
    RegistrationFailed { reason: String },
    /// The registration retry loop has given up entirely (repeated
    /// 401/407s, or an outright 403 Forbidden) rather than continuing to
    /// retry — see `registration.rs`'s circuit breaker. Distinct from
    /// `RegistrationFailed`, which still implies "will keep retrying":
    /// once this fires, no further REGISTER attempts happen until the
    /// account's config changes (e.g. re-saving SIP Settings).
    RegistrationHalted { reason: String },
    IncomingCall {
        id: CallId,
        /// Which line (1-5) this call was assigned to — the first free line
        /// at the time it arrived, or rejected with Busy if none was free.
        line: u8,
        remote: String,
        offer: RemoteMediaInfo,
    },
    OutgoingCallStarted { id: CallId, line: u8 },
    CallStateChanged { id: CallId, state: CallState },
    PlaceCallFailed { line: u8, reason: String },
    DtmfResult { id: CallId, digit: char, ok: bool },
    /// `ok` reflects only whether the REFER transaction itself was accepted
    /// by the server — rsipstack's `refer()` has no way to correlate the
    /// follow-up NOTIFY(sipfrag) that would report whether the transfer
    /// target actually answered, so this is *not* a "transfer completed"
    /// signal. `softphone-ui` deliberately words this as "requested," not
    /// "succeeded" (see README's Known limitations).
    TransferResult { id: CallId, ok: bool },
    /// A hold/resume re-INVITE failed at the transport/dialog level (e.g.
    /// the peer rejected it). Distinct from a silent no-op: without this,
    /// pressing Hold/Resume against a peer that rejects the re-INVITE left
    /// the UI's `on_hold` flag untouched with no indication anything went
    /// wrong.
    HoldResumeFailed { id: CallId, hold: bool, reason: String },
}

#[derive(Debug, Clone)]
pub enum CoreCommand {
    PlaceCall { line: u8, callee: String, local_rtp_port: u16 },
    AnswerCall { id: CallId, local_rtp_port: u16 },
    RejectCall(CallId),
    HangUp(CallId),
    SendDtmf { id: CallId, digit: char },
    HoldCall(CallId),
    ResumeCall(CallId),
    BlindTransfer { id: CallId, target: String },
    Shutdown,
}
