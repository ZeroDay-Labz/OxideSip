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

/// Who is calling, parsed out of the INVITE's `From` header (or
/// `P-Asserted-Identity` when a PBX supplies one) — the UI never has to see
/// raw SIP header syntax.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteParty {
    /// The quoted display name, if the caller sent one ("Alice Nguyen").
    /// `None` when absent or empty.
    pub display_name: Option<String>,
    /// The URI's user part — an extension or phone number ("1001",
    /// "+15551234567"). Falls back to the host when the URI has no user
    /// part (e.g. `sip:pbx.lan`), so it is never empty for a valid URI.
    pub user: String,
    /// The bare URI without header parameters ("sip:1001@pbx.lan").
    pub uri: String,
}

impl RemoteParty {
    /// The most human-friendly single string: display name, else user part.
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.user)
    }

    /// The host part of `uri` ("pbx.lan"), or "" if it can't be found.
    pub fn host(&self) -> &str {
        let uri = self.uri.split(';').next().unwrap_or(&self.uri);
        let after_scheme = uri.split_once(':').map(|(_, rest)| rest).unwrap_or(uri);
        after_scheme
            .rsplit_once('@')
            .map(|(_, host)| host)
            .unwrap_or(after_scheme)
    }

    /// Best-effort parse of a raw `From`/`PAI` header *value* — anything of
    /// the form `"Name" <sip:user@host;p=1>;tag=x`, `<sip:user@host>`,
    /// `sip:user@host;tag=x`, or `tel:+1555…`. Used as the fallback when the
    /// stack's typed parser rejects an unusual header, and to repair history
    /// entries written by older versions that stored the raw header.
    pub fn from_header_value(raw: &str) -> Self {
        let raw = raw.trim();
        let raw = raw.strip_prefix("From:").map(str::trim).unwrap_or(raw);

        let (display_name, rest) = if let Some(stripped) = raw.strip_prefix('"') {
            match stripped.split_once('"') {
                Some((name, rest)) => (Some(name.trim().to_string()), rest.trim()),
                None => (None, raw),
            }
        } else if let Some(idx) = raw.find('<') {
            let name = raw[..idx].trim();
            ((!name.is_empty()).then(|| name.to_string()), &raw[idx..])
        } else {
            (None, raw)
        };

        let uri = match (rest.find('<'), rest.find('>')) {
            (Some(open), Some(close)) if close > open => &rest[open + 1..close],
            _ => rest.split(';').next().unwrap_or(rest),
        };
        let uri = uri.split(';').next().unwrap_or(uri).trim();
        let uri = uri.split('?').next().unwrap_or(uri);

        let after_scheme = uri.split_once(':').map(|(_, rest)| rest).unwrap_or(uri);
        let user = after_scheme
            .rsplit_once('@')
            .map(|(user, _)| user)
            .unwrap_or(after_scheme)
            .to_string();

        Self {
            display_name: display_name.filter(|n| !n.is_empty()),
            user,
            uri: uri.to_string(),
        }
    }
}

impl std::fmt::Display for RemoteParty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.display_name {
            Some(name) if name != &self.user => write!(f, "{name} ({})", self.user),
            _ => f.write_str(&self.user),
        }
    }
}

/// Why an incoming call is being refused — picks the SIP response code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// 486 Busy Here: no free line, Do Not Disturb, deny list, timeout.
    Busy,
    /// 603 Decline: the user pressed Decline. Lets the PBX distinguish an
    /// explicit refusal from "busy" (e.g. for voicemail greeting selection).
    Decline,
}

#[derive(Debug, Clone)]
pub enum CoreEvent {
    Registered {
        expires: u32,
        rtt_ms: u32,
    },
    RegistrationFailed {
        reason: String,
    },
    /// The registration retry loop has given up entirely (repeated
    /// 401/407s, or an outright 403 Forbidden) rather than continuing to
    /// retry — see `registration.rs`'s circuit breaker. Distinct from
    /// `RegistrationFailed`, which still implies "will keep retrying":
    /// once this fires, no further REGISTER attempts happen until the
    /// account's config changes (e.g. re-saving SIP Settings).
    RegistrationHalted {
        reason: String,
    },
    IncomingCall {
        id: CallId,
        /// Which line (1-5) this call was assigned to — the first free line
        /// at the time it arrived, or rejected with Busy if none was free.
        line: u8,
        remote: RemoteParty,
        offer: RemoteMediaInfo,
    },
    OutgoingCallStarted {
        id: CallId,
        line: u8,
    },
    CallStateChanged {
        id: CallId,
        state: CallState,
    },
    PlaceCallFailed {
        line: u8,
        reason: String,
    },
    DtmfResult {
        id: CallId,
        digit: char,
        ok: bool,
    },
    /// `ok` reflects only whether the REFER transaction itself was accepted
    /// by the server — rsipstack's `refer()` has no way to correlate the
    /// follow-up NOTIFY(sipfrag) that would report whether the transfer
    /// target actually answered, so this is *not* a "transfer completed"
    /// signal. `softphone-ui` deliberately words this as "requested," not
    /// "succeeded" (see README's Known limitations).
    TransferResult {
        id: CallId,
        ok: bool,
    },
    /// A hold/resume re-INVITE failed at the transport/dialog level (e.g.
    /// the peer rejected it). Distinct from a silent no-op: without this,
    /// pressing Hold/Resume against a peer that rejects the re-INVITE left
    /// the UI's `on_hold` flag untouched with no indication anything went
    /// wrong.
    HoldResumeFailed {
        id: CallId,
        hold: bool,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub enum CoreCommand {
    PlaceCall {
        line: u8,
        callee: String,
        local_rtp_port: u16,
    },
    AnswerCall {
        id: CallId,
        local_rtp_port: u16,
    },
    RejectCall {
        id: CallId,
        reason: RejectReason,
    },
    HangUp(CallId),
    SendDtmf {
        id: CallId,
        digit: char,
    },
    HoldCall(CallId),
    ResumeCall(CallId),
    BlindTransfer {
        id: CallId,
        target: String,
    },
    Shutdown,
}
