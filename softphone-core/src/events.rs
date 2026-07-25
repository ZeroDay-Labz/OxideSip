use std::net::SocketAddr;

/// Opaque call identifier handed to the UI. Derived from rsipstack's
/// `DialogId.call_id` (the bare SIP Call-ID) rather than `DialogId::to_string()`:
/// for an outbound dialog, `remote_tag` is empty until the callee responds, so
/// the stringified `DialogId` changes value mid-call, but the bare call_id is
/// stable for the whole dialog lifetime in both directions.
pub type CallId = String;

#[derive(Debug, Clone)]
pub struct RemoteMediaInfo {
    pub remote_addr: SocketAddr,
    pub payload_type: u8,
    pub crypto_key: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LocalMediaInfo {
    pub crypto_key: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CallState {
    Ringing,
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
    TransferResult { id: CallId, ok: bool },
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
