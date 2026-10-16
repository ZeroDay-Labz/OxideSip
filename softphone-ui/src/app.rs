use crate::app_settings::{self, AppSettings};
use crate::audio_devices::{self, AudioDeviceConfig};
use crate::contacts::{self, Contact};
use crate::history;
use crate::theme::{self, ThemeSetting, ToastKind};
use crate::toast::Toast;
use crate::{branding, bridge, incoming, notify, number, ui, ui_state};
use iced::keyboard::{self, Key, Modifiers};
use iced::{Element, Subscription, Task, window};
use softphone_core::config::{DtmfMode, PREFERRED_CODECS, PreferredCodec, SipAccountConfig, SipTransport};
use softphone_core::events::{CallId, CallState, CoreCommand, CoreEvent, RejectReason, RemoteMediaInfo, RemoteParty};
use softphone_media::{AudioDevice, DtmfTonePlayer, MediaSession, ReservedSocket};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub enum RegistrationStatus {
    Connecting,
    Registered {
        expires: u32,
        /// So the status tooltip can show a live "renews in Ns" countdown
        /// instead of a static number frozen at whatever `expires` was at
        /// the last successful (re-)registration.
        registered_at: Instant,
        /// Round-trip time of the last successful REGISTER, measured in
        /// `registration.rs` (send to 200 OK). Since the PBX is on the same
        /// LAN for this app's actual use case, this is a meaningful signal
        /// distinguishing "registered, network's fine" from "registered,
        /// but something's slow" — which the call-audio latency mystery
        /// documented in `pipewire_io.rs` can't be, since it's confirmed
        /// specific to the playback path, not SIP signaling.
        rtt_ms: u32,
    },
    Failed {
        reason: String,
    },
    /// The core's registration loop has given up entirely (repeated 401/407s
    /// or an outright 403) rather than continuing to retry — distinct from
    /// `Failed`, which still implies "reconnecting." Re-saving SIP Settings
    /// (see `handle_sip_settings_save`'s `reg_epoch` bump) is the only way
    /// to trigger a fresh attempt from here.
    Halted {
        reason: String,
    },
}

pub enum CallUiState {
    Idle,
    Incoming {
        id: CallId,
        caller: RemoteParty,
        #[allow(dead_code)] // kept for parity with the negotiated offer; not read by the view yet
        offer: RemoteMediaInfo,
        /// When this call started ringing — drives the footer's live
        /// per-line elapsed-time display (see `view.rs::footer`).
        ringing_since: Instant,
    },
    Outgoing {
        id: CallId,
        number: String,
        /// When we started dialing out — same purpose as `Incoming`'s
        /// `ringing_since`.
        started_at: Instant,
    },
    /// A 183 Session Progress landed with an SDP body before the final 200
    /// OK — real early media (e.g. in-band ringback/announcement) is
    /// flowing, or at least being set up (`media` is `None` until its
    /// `MediaSession::start` future resolves). `remote` is compared against
    /// the eventual `Answered` event's remote info to decide whether the
    /// final answer can just promote this in place (same target) or needs
    /// to tear it down and rebuild (target changed) — see
    /// `handle_call_state_changed`.
    EarlyMedia {
        id: CallId,
        number: String,
        media: Option<MediaSession>,
        remote: RemoteMediaInfo,
        started_at: Instant,
    },
    Active {
        id: CallId,
        number: String,
        /// The far end's SIP display name (incoming calls only) — shown when
        /// no saved contact matches `number`.
        remote_name: Option<String>,
        direction: history::CallDirection,
        media: Option<MediaSession>,
        dtmf_feedback: Vec<char>,
        answered_at: Instant,
        input_level: f32,
        output_level: f32,
        muted: bool,
        output_volume: f32,
        input_volume: f32,
        on_hold: bool,
        /// Output volume to restore on resume — the actual applied gain is
        /// forced to 0 while `on_hold`, independent of what the slider shows.
        pre_hold_output_volume: f32,
        /// Live on/off state for the secondary-input toggle (mixing another
        /// app's audio, e.g. Discord, into what's sent to the SIP peer) —
        /// mirrored onto `media`'s `MediaSession::set_secondary_input_enabled`
        /// whenever it changes. Always starts `false` for a fresh call,
        /// regardless of whether a target is configured in Settings — the
        /// user opts in per call via the toggle below the line buttons.
        secondary_input_on: bool,
        /// `None` = transfer panel collapsed; `Some(text)` = panel open with
        /// the in-progress target number.
        transfer_input: Option<String>,
        /// Remaining post-dial digits/pauses (`,` = pause) queued from a
        /// comma in the original dial string — drained one step at a time
        /// by `Message::PostDialAdvance` once the call is answered.
        post_dial: VecDeque<char>,
        /// Drives the avatar's live-glow cross-fade (`theme::avatar_state`)
        /// as `media.is_some() && !on_hold` changes over the call's
        /// lifetime, instead of the glow snapping on/off. Updated wherever
        /// either of those two inputs changes; read (via `.interpolate`) at
        /// render time in `view.rs::dialer_tab`.
        avatar_glow: iced::Animation<bool>,
    },
}

/// `live` here means "the avatar's glow should be showing" — audio is
/// actually flowing, not just that a `MediaSession` exists (e.g. it stays
/// `false` while on hold even though `media` is still `Some`).
fn avatar_live(media_ready: bool, on_hold: bool) -> bool {
    media_ready && !on_hold
}

impl CallUiState {
    /// The `(id, media)` slot shared by `Active` and `EarlyMedia` — the two
    /// states that can actually hold a live/in-flight `MediaSession`. Lets
    /// `Message::MediaReady`'s handler land a just-finished
    /// `MediaSession::start` future into whichever of the two states the
    /// line is in by the time it resolves, without duplicating the same
    /// `if let` twice.
    fn media_slot(&mut self) -> Option<(&CallId, &mut Option<MediaSession>)> {
        match self {
            CallUiState::Active { id, media, .. } | CallUiState::EarlyMedia { id, media, .. } => Some((id, media)),
            _ => None,
        }
    }
}

/// Up to 5 concurrent call slots per account, matching `softphone-core`'s
/// `MAX_LINES`.
const LINE_COUNT: usize = 5;

fn line_idx(line: u8) -> usize {
    line.saturating_sub(1) as usize
}

fn idx_line(idx: usize) -> u8 {
    (idx + 1) as u8
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Page {
    #[default]
    Phone,
    Contacts,
    History,
    Settings,
}

impl Page {
    pub const ALL: [Page; 4] = [Page::Phone, Page::Contacts, Page::History, Page::Settings];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingsSection {
    #[default]
    Accounts,
    Audio,
    Calls,
    Appearance,
    About,
}

impl SettingsSection {
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Accounts => "Accounts",
            SettingsSection::Audio => "Audio & codecs",
            SettingsSection::Calls => "Calls",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::About => "About",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            SettingsSection::Accounts => "SIP accounts and registration",
            SettingsSection::Audio => "Devices, codecs, encryption, DTMF",
            SettingsSection::Calls => "Incoming calls, deny list, recording, routing",
            SettingsSection::Appearance => "Theme and window",
            SettingsSection::About => "Version, shortcuts, credits",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryFilter {
    #[default]
    All,
    Missed,
    Incoming,
    Outgoing,
}

/// Drives the sidebar's selected-item cross-fade — one `Animation<bool>`
/// per page so the newly selected one eases in while the old one eases out.
pub struct NavAnimations {
    items: [(Page, iced::Animation<bool>); 4],
}

impl NavAnimations {
    fn new(selected: Page) -> Self {
        NavAnimations {
            items: Page::ALL.map(|page| (page, iced::Animation::new(page == selected).quick())),
        }
    }

    fn select(&mut self, selected: Page) {
        let now = Instant::now();
        for (page, anim) in &mut self.items {
            anim.go_mut(*page == selected, now);
        }
    }

    pub(crate) fn amount(&self, page: Page, now: Instant) -> f32 {
        self.items
            .iter()
            .find(|(p, _)| *p == page)
            .map(|(_, anim)| anim.interpolate(0.0_f32, 1.0_f32, now))
            .unwrap_or(0.0)
    }
}

#[derive(Default)]
pub struct SipSettingsForm {
    pub name: String,
    pub host: String,
    pub port: String,
    pub username: String,
    pub password: String,
    pub transport: SipTransport,
}

impl SipSettingsForm {
    fn from_config(config: &SipAccountConfig) -> Self {
        SipSettingsForm {
            name: config.name.clone(),
            host: config.sip_server_host.clone(),
            port: config.sip_server_port.to_string(),
            username: config.username.clone(),
            password: config.password.clone(),
            transport: config.transport,
        }
    }
}

pub struct AudioSettingsForm {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub srtp: bool,
    /// Every known codec, in the priority order this account will offer
    /// them — always a full permutation of `config::PREFERRED_CODECS`
    /// (see `from_config`), never partial, so the list editor can freely
    /// reorder without an "empty list" edge case to guard against.
    pub codecs: Vec<PreferredCodec>,
    /// Which DTMF transport this account uses during calls. See `DtmfMode`
    /// — `Auto` (the default) negotiates RFC 4733 and falls back to SIP
    /// INFO; the other two are an interop-troubleshooting override.
    pub dtmf_mode: DtmfMode,
}

impl AudioSettingsForm {
    /// `srtp`/`codecs` come from the *account* being edited (they're
    /// per-trunk negotiation preferences, e.g. one PBX trunk might support
    /// SRTP and another might not) — device selection is process-wide, not
    /// tied to any one account, so it's read from `audio` instead.
    fn from_config(config: &SipAccountConfig, audio: &AudioDeviceConfig) -> Self {
        let mut codecs = config.preferred_codecs.clone();
        for &c in PREFERRED_CODECS.iter() {
            if !codecs.contains(&c) {
                codecs.push(c);
            }
        }
        AudioSettingsForm {
            input_device: audio.input_device.clone(),
            output_device: audio.output_device.clone(),
            srtp: config.srtp,
            codecs,
            dtmf_mode: config.dtmf_mode,
        }
    }
}

/// General call-handling preferences — global, not tied to any one SIP
/// account (see `app_settings.rs`). `deny_list_input` is the in-progress
/// text for the entry about to be added, separate from the persisted
/// `deny_list` itself.
#[derive(Default)]
pub struct SettingsForm {
    pub dnd: bool,
    pub auto_answer: bool,
    pub deny_list: Vec<String>,
    pub deny_list_input: String,
    pub notify_incoming: bool,
    pub theme: ThemeSetting,
    pub recording_enabled: bool,
    pub recording_path: String,
    pub secondary_output_target: Option<String>,
    pub secondary_input_target: Option<String>,
}

impl SettingsForm {
    fn from_settings(settings: &AppSettings) -> Self {
        SettingsForm {
            dnd: settings.dnd,
            auto_answer: settings.auto_answer,
            deny_list: settings.deny_list.clone(),
            deny_list_input: String::new(),
            notify_incoming: settings.notify_incoming,
            theme: settings.theme,
            recording_enabled: settings.recording_enabled,
            recording_path: settings.recording_path.clone(),
            secondary_output_target: settings.secondary_output_target.clone(),
            secondary_input_target: settings.secondary_input_target.clone(),
        }
    }
}

#[derive(Default)]
pub struct ContactForm {
    pub name: String,
    pub number: String,
    pub editing_index: Option<usize>,
}

/// How the contacts list is displayed — cycled by the sort button in
/// `view.rs`'s `contacts_tab`. `contacts.rs::save` already writes contacts
/// to disk in name order, but that's an implementation detail of
/// persistence, not a substitute for the user being able to see (and
/// change) the actual display order themselves.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum ContactSort {
    #[default]
    NameAsc,
    NameDesc,
    /// Most recently added first.
    Recent,
}

impl ContactSort {
    fn next(self) -> Self {
        match self {
            ContactSort::NameAsc => ContactSort::NameDesc,
            ContactSort::NameDesc => ContactSort::Recent,
            ContactSort::Recent => ContactSort::NameAsc,
        }
    }
}

/// Everything that used to be a single top-level `App` field, now
/// multiplied per registered SIP account: its own registration state, its
/// own command channel to its own `SoftphoneCore` instance (see
/// `bridge.rs` — one core per account, no shared state between them), and
/// its own 5 lines. Device selection, contacts, and call history stay
/// shared at the `App` level — those aren't tied to any one SIP identity.
pub struct AccountSession {
    pub(crate) config: SipAccountConfig,
    pub(crate) registration: RegistrationStatus,
    command_tx: Option<mpsc::Sender<CoreCommand>>,
    pub(crate) lines: [CallUiState; LINE_COUNT],
    pub(crate) selected_line: u8,
    /// Whether the *currently selected* line has been explicitly "seized"
    /// while idle — drives the line sidebar's corner LED for the idle case
    /// (an in-progress/ringing/active call always reads as "on" regardless
    /// of this flag; see `view.rs::line_sidebar`). Only ever meaningful for
    /// whichever line is selected: switching to a different idle line
    /// always re-opens it fresh (sets this back to `true`), and switching
    /// away makes the line you left stop being "selected," which alone
    /// turns its LED off — so this never needs per-line storage or explicit
    /// resetting on the way out.
    pub(crate) line_open: bool,
    /// When the currently selected line was opened (armed idle, dial tone
    /// played) — `Some` exactly when `line_open` is `true`.
    pub(crate) line_open_at: Option<Instant>,
    /// `joined[i]` = `Some(partner_line)` when line `i+1` is bridged into a
    /// local 3-way conference with `partner_line` (see
    /// `MediaSession::join_with`) — always symmetric, set/cleared on both
    /// sides together by `complete_join`/`split_join`. Joining only ever
    /// happens between two lines on the *same* account (each account is
    /// its own independent phone line/trunk).
    pub(crate) joined: [Option<u8>; LINE_COUNT],
    /// `Some(line)` while the user has pressed JOIN on `line` and is being
    /// asked to tap another active line's sidebar button to complete it.
    pub(crate) pending_join: Option<u8>,
    pending_sockets: [Option<ReservedSocket>; LINE_COUNT],
    /// Set by `handle_call_pressed` from anything typed after a `,` in the
    /// dial field, drained into `CallUiState::Active::post_dial` once the
    /// call is actually answered (see `Message::PostDialAdvance`).
    pending_post_dials: [String; LINE_COUNT],
    /// Set by `handle_call_pressed`, consumed by the `OutgoingCallStarted`
    /// handler — deliberately *not* re-derived from `App::dial_input` at
    /// that point, since the user could in principle switch lines/accounts
    /// (which clears `dial_input`) in the brief window between sending
    /// `PlaceCall` and the core echoing `OutgoingCallStarted` back.
    pending_numbers: [String; LINE_COUNT],
    /// Short label for how each line's most recent call ended — "hung up",
    /// "busy", "declined", etc. (see `terminated_reason_label` in
    /// `softphone-core`) — shown in the footer's bottom-left corner for
    /// whichever line is selected. Cleared the moment a fresh call starts on
    /// that line so it never shows a stale result for the call in progress.
    pub(crate) last_call_status: [Option<String>; LINE_COUNT],
}

impl AccountSession {
    fn new(config: SipAccountConfig) -> Self {
        AccountSession {
            config,
            registration: RegistrationStatus::Connecting,
            command_tx: None,
            lines: [
                CallUiState::Idle,
                CallUiState::Idle,
                CallUiState::Idle,
                CallUiState::Idle,
                CallUiState::Idle,
            ],
            selected_line: 1,
            line_open: false,
            line_open_at: None,
            joined: [None; LINE_COUNT],
            pending_join: None,
            pending_sockets: [None, None, None, None, None],
            pending_post_dials: [
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ],
            pending_numbers: [
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ],
            last_call_status: [None, None, None, None, None],
        }
    }

    pub(crate) fn selected_idx(&self) -> usize {
        line_idx(self.selected_line)
    }

    pub(crate) fn selected_call(&self) -> &CallUiState {
        &self.lines[self.selected_idx()]
    }

    fn send_command(&self, command: CoreCommand) {
        if let Some(tx) = &self.command_tx {
            let _ = tx.try_send(command);
        }
    }

    /// Sends one DTMF digit for line `idx`'s active call, choosing between
    /// RFC 4733 (RTP telephone-event) and SIP INFO per `self.config.dtmf_mode`:
    /// `Auto` tries RFC 4733 first and falls back to INFO if this call never
    /// negotiated it; `Rfc4733Only` never falls back (fails silently rather
    /// than degrading, matching the mode's name — no interop workaround is
    /// attempted); `InfoOnly` always uses INFO, reproducing this app's exact
    /// pre-RFC4733 behavior. A no-op if line `idx` isn't `Active`.
    fn send_dtmf_digit(&self, idx: usize, digit: char) {
        let CallUiState::Active { id, media, .. } = &self.lines[idx] else {
            return;
        };
        let sent_via_rtp = self.config.dtmf_mode != DtmfMode::InfoOnly
            && media
                .as_ref()
                .is_some_and(|m| m.send_dtmf(digit, DTMF_EVENT_DURATION_MS));
        if !sent_via_rtp && self.config.dtmf_mode != DtmfMode::Rfc4733Only {
            self.send_command(CoreCommand::SendDtmf { id: id.clone(), digit });
        }
    }

    /// Holds this account's currently-selected line if it's a live
    /// (unheld, not part of a conference) call — used both when switching
    /// lines within this account and when switching *away* from this
    /// account entirely, since only one line across the whole app should
    /// ever have live audio at a time (matching a real multi-line phone).
    fn hold_current_if_live(&self) {
        let idx = self.selected_idx();
        if self.joined[idx].is_none()
            && let CallUiState::Active { id, on_hold: false, .. } = &self.lines[idx]
        {
            self.send_command(CoreCommand::HoldCall(id.clone()));
        }
    }

    /// Resumes this account's currently-selected line if it was held —
    /// the counterpart to `hold_current_if_live`.
    fn resume_current_if_held(&self) {
        let idx = self.selected_idx();
        if self.joined[idx].is_none()
            && let CallUiState::Active { id, on_hold: true, .. } = &self.lines[idx]
        {
            self.send_command(CoreCommand::ResumeCall(id.clone()));
        }
    }

    pub(crate) fn line_index_for_call(&self, call_id: &CallId) -> Option<usize> {
        self.lines.iter().position(|line| match line {
            CallUiState::Incoming { id, .. }
            | CallUiState::Outgoing { id, .. }
            | CallUiState::EarlyMedia { id, .. }
            | CallUiState::Active { id, .. } => id == call_id,
            _ => false,
        })
    }

    /// Clears line `idx`'s join pairing (if any) and tears down the
    /// partner's half of the bridge too, so it doesn't keep relaying audio
    /// to a leg that's going away. Called when line `idx` hangs up or
    /// terminates, not when the user explicitly presses Split.
    fn unjoin_line(&mut self, idx: usize) {
        if let Some(partner_line) = self.joined[idx].take() {
            let partner_idx = line_idx(partner_line);
            if let CallUiState::Active {
                media: Some(session), ..
            } = &self.lines[partner_idx]
            {
                session.unjoin();
            }
            self.joined[partner_idx] = None;
        }
        if self.pending_join == Some(idx_line(idx)) {
            self.pending_join = None;
        }
    }

    /// Two simultaneous immutable borrows into `self.lines` at different
    /// indices — plain indexing is fine here since neither is `mut`.
    fn two_media(&self, a_idx: usize, b_idx: usize) -> Option<(&MediaSession, &MediaSession)> {
        if a_idx == b_idx {
            return None;
        }
        let CallUiState::Active {
            media: Some(a_media), ..
        } = &self.lines[a_idx]
        else {
            return None;
        };
        let CallUiState::Active {
            media: Some(b_media), ..
        } = &self.lines[b_idx]
        else {
            return None;
        };
        Some((a_media, b_media))
    }

    /// Bridges lines `a` and `b` (both on this account) into a local 3-way
    /// conference: resumes whichever side is still on hold, wires up the
    /// `MediaSession`-level audio relay, and records the pairing.
    fn complete_join(&mut self, a: u8, b: u8) -> Result<(), &'static str> {
        let a_idx = line_idx(a);
        let b_idx = line_idx(b);
        if !matches!(self.lines[b_idx], CallUiState::Active { .. }) {
            return Err("pick another active call to join");
        }
        if let CallUiState::Active { id, on_hold: true, .. } = &self.lines[a_idx] {
            self.send_command(CoreCommand::ResumeCall(id.clone()));
        }
        if let CallUiState::Active { id, on_hold: true, .. } = &self.lines[b_idx] {
            self.send_command(CoreCommand::ResumeCall(id.clone()));
        }
        if let Some((sa, sb)) = self.two_media(a_idx, b_idx) {
            sa.join_with(sb);
        }
        self.joined[a_idx] = Some(b);
        self.joined[b_idx] = Some(a);
        self.selected_line = a;
        Ok(())
    }

    /// Tears a conference pairing back apart — both lines stay active as
    /// independent (non-bridged) calls, same as they were pre-join.
    fn split_join(&mut self, a: u8, b: u8) {
        let a_idx = line_idx(a);
        let b_idx = line_idx(b);
        if let Some((sa, sb)) = self.two_media(a_idx, b_idx) {
            sa.unjoin();
            sb.unjoin();
        }
        self.joined[a_idx] = None;
        self.joined[b_idx] = None;
    }
}

pub struct App {
    pub(crate) accounts: Vec<AccountSession>,
    /// Which account's lines/registration status the Dialer tab and account
    /// switcher currently show — index into `accounts`.
    pub(crate) selected_account: usize,
    pub(crate) page: Page,
    pub(crate) settings_section: SettingsSection,
    pub(crate) nav_animations: NavAnimations,
    pub(crate) history_filter: HistoryFilter,
    /// Manual collapse (Ctrl+B); the sidebar also auto-collapses on narrow
    /// windows regardless of this.
    pub(crate) sidebar_collapsed: bool,
    /// Missed calls since the History page was last viewed (sidebar badge).
    pub(crate) unseen_missed: u32,
    /// The "a call is in progress — quit anyway?" modal.
    pub(crate) quit_prompt: bool,
    /// Whether the in-call keypad is shown on the call card.
    pub(crate) keypad_open: bool,
    /// Esc pressed once during a call — a second press within 2 s hangs up.
    last_escape: Option<Instant>,
    /// The main window's size outside compact mode (restored on expand).
    normal_window_size: iced::Size,
    ui_state: ui_state::UiState,
    /// When `ui_state` last changed and still needs writing (debounced).
    ui_state_dirty_at: Option<Instant>,
    pub(crate) dial_input: String,
    /// Whether the recent-outgoing-numbers panel below the dial input is
    /// expanded. Toggled by its own button rather than on focus, since
    /// iced's `text_input` has no on-key-down hook to react to a down-arrow
    /// press the way a native combo box would.
    pub(crate) dial_history_open: bool,
    pub(crate) sip_settings_form: SipSettingsForm,
    /// Which account the SIP settings form is currently editing —
    /// `Some(index)` into `accounts`, or `None` while composing a
    /// brand-new not-yet-saved account. See `view.rs`'s account sidebar.
    pub(crate) editing_account: Option<usize>,
    pub(crate) audio_settings_form: AudioSettingsForm,
    audio_devices: AudioDeviceConfig,
    pub(crate) input_devices: Vec<AudioDevice>,
    pub(crate) output_devices: Vec<AudioDevice>,
    /// Live application capture streams (e.g. Discord's voice-engine node,
    /// only present while it's actually in a voice channel) — separate
    /// from `output_devices` since these are ephemeral and shown as a
    /// distinct group in the secondary-output picker. Re-scanned via
    /// `Message::RefreshSecondaryOutputTargets`, not just once at boot.
    pub(crate) app_capture_streams: Vec<AudioDevice>,
    /// Live application *playback* streams (e.g. Discord's own outgoing
    /// audio — what other voice-channel members are saying), the input
    /// counterpart to `app_capture_streams`, shown in the secondary-input
    /// picker. Same "re-scanned on demand, not cached" treatment.
    pub(crate) app_playback_streams: Vec<AudioDevice>,
    /// Transient feedback (errors, confirmations) — see `toast.rs`.
    pub(crate) toasts: Vec<Toast>,
    next_toast_id: u64,
    /// The desktop's colour scheme as last reported by the portal; used
    /// when `settings.theme` is `System`.
    system_mode: theme::Mode,
    main_window: window::Id,
    pub(crate) main_window_size: iced::Size,
    /// Whether the main window is currently shrunk to the compact
    /// floating call bar. A window-level concept, not tied to any one
    /// line's data — it just reflects whatever line is selected.
    pub(crate) compact_mode: bool,
    /// The single always-on-top incoming-call window (see `incoming.rs`),
    /// open while at least one line is ringing. Its content is derived from
    /// `accounts[*].lines` every render, so it needs no per-call bookkeeping;
    /// `sync_incoming_ui` opens/resizes/closes it after every update.
    incoming_popup: Option<window::Id>,
    /// How many cards the popup was last sized for — resized when it changes.
    incoming_popup_cards: usize,
    /// Whether `tone_player.play_incoming_ringtone()` is currently looping;
    /// stopped by `sync_incoming_ui` once nothing is ringing, or by Silence.
    ringtone_playing: bool,
    /// When the popup last asked the compositor for attention — re-issued
    /// every few seconds while ringing so the task-bar pulse persists.
    last_attention: Option<Instant>,
    /// Desktop notification ids by call, so each can be dismissed once its
    /// call stops ringing (answered, declined, cancelled or timed out).
    notifications: HashMap<CallId, u32>,
    pub(crate) settings: AppSettings,
    pub(crate) settings_form: SettingsForm,
    pub(crate) call_history: Vec<history::HistoryEntry>,
    /// Index into `call_history` whose inline "Add to Contacts" row is
    /// expanded (opened via right-click on that entry) — `None` when no
    /// entry's menu is open. A single `Option` since only one can be open
    /// at a time.
    pub(crate) history_context_menu: Option<usize>,
    pub(crate) contacts: Vec<Contact>,
    pub(crate) contact_filter: String,
    pub(crate) contact_form: Option<ContactForm>,
    pub(crate) contact_sort: ContactSort,
    /// Path used by both the Import and Export actions in the Contacts tab
    /// — one shared field rather than two, since round-tripping (export,
    /// edit elsewhere, re-import) is the common case.
    pub(crate) contacts_io_path: String,
    /// Result of the last import/export attempt, shown inline in the
    /// Contacts tab until the next attempt replaces it.
    pub(crate) contacts_io_status: Option<String>,
    /// `None` until `Message::TonePlayerReady` arrives (or forever, if
    /// startup failed — non-fatal, dialpad presses just stay silent).
    /// Started once against the output device configured at boot; doesn't
    /// follow later Settings changes without an app restart, same as a
    /// call's `MediaSession` needing a fresh call to pick up a device change.
    tone_player: Option<Arc<DtmfTonePlayer>>,
    /// The most recently pressed dialpad key and when — `view.rs`'s
    /// `dialpad` reads this to give the just-pressed key a brief colored
    /// flash that decays over a couple hundred ms, so a tap gets its own
    /// visible confirmation instead of relying solely on the (easy to miss
    /// under a fast finger) held-down hover state. Shared by both the idle
    /// dialer and in-call DTMF pad, since both render through the same
    /// `dialpad` function. No cleanup needed: `dialpad`'s own elapsed-time
    /// check just stops drawing the flash once its short window has
    /// passed — the stale `Some` sitting in state afterward costs nothing.
    pub(crate) dialpad_flash: Option<(char, Instant)>,
}

/// `iced`'s interactive widgets (`on_press`, `on_input`, ...) require
/// `Message: Clone`, so `MediaSession` (holds a `JoinHandle`, not `Clone`)
/// can't be carried directly — it's wrapped in `Arc<Mutex<Option<_>>>` so the
/// `Message` variant is cheaply `Clone` (cloning the `Arc`) while ownership of
/// the real session is taken exactly once in `update()` via `.take()`.
#[derive(Clone)]
pub enum Message {
    /// `usize` is the account index this connection belongs to (see
    /// `bridge.rs`, which spawns one `SoftphoneCore` per account).
    CoreConnected(usize, mpsc::Sender<CoreCommand>),
    Core(usize, CoreEvent),
    DialInputChanged(String),
    /// Fired instead of `DialInputChanged` specifically for Ctrl+V paste
    /// (see the dial `text_input`'s `.on_paste` in `view.rs`) — sets the
    /// field without playing a dial tone per pasted character, since a
    /// pasted number wasn't actually dialed.
    DialInputPasted(String),
    DialHistoryToggled,
    DialHistorySelected(String),
    DialpadPressed(char),
    CallPressed,
    AnswerPressed,
    RejectPressed,
    /// Answer/Decline pressed on a specific call's incoming-call popup
    /// window (as opposed to `AnswerPressed`/`RejectPressed`, which act on
    /// whichever line is currently selected in the main window) — carries
    /// the account index and call id so the right line can be resolved
    /// regardless of what's currently selected.
    AnswerIncomingCall(usize, CallId),
    RejectIncomingCall(usize, CallId),
    /// Stop the ringtone without answering or declining anything.
    SilenceRingtone,
    ToastDismissed(u64),
    SystemThemeChanged(iced::theme::Mode),
    SettingsThemeChanged(ThemeSetting),
    /// A key press nothing else consumed, tagged with the window it landed in.
    KeyPressed(window::Id, Key, Modifiers),
    /// Lifecycle of the desktop notification shown for a ringing call.
    Notification(CallId, notify::Event),
    HangUpPressed,
    MediaReady(usize, CallId, Result<Arc<Mutex<Option<MediaSession>>>, String>),
    OpenSipSettings,
    OpenAudioSettings,
    SipSettingsNameChanged(String),
    SipSettingsHostChanged(String),
    SipSettingsPortChanged(String),
    SipSettingsUsernameChanged(String),
    SipSettingsPasswordChanged(String),
    SipSettingsTransportChanged(SipTransport),
    SipSettingsSavePressed,
    SipSettingsCancelPressed,
    /// Loads an existing account into the SIP settings editor.
    SelectAccountForEditing(usize),
    /// Clears the SIP settings editor to compose a brand-new account.
    AddAccountPressed,
    DeleteAccountPressed(usize),
    /// Switches which account's lines/registration the main window shows.
    AccountSwitched(usize),
    AudioSettingsInputDeviceChanged(String),
    AudioSettingsOutputDeviceChanged(String),
    AudioSettingsSrtpToggled(bool),
    /// Moves the codec at this index one slot earlier/later in priority
    /// (`true` = move up/earlier). No-op at the relevant end of the list.
    AudioSettingsCodecMoved(usize, bool),
    AudioSettingsDtmfModeChanged(DtmfMode),
    AudioSettingsSavePressed,
    AudioSettingsCancelPressed,
    DevicesLoaded(Vec<AudioDevice>, Vec<AudioDevice>),
    OpenSettings,
    SettingsDndToggled(bool),
    SettingsAutoAnswerToggled(bool),
    SettingsNotifyToggled(bool),
    SettingsDenyListInputChanged(String),
    SettingsDenyListAddPressed,
    SettingsDenyListRemovePressed(usize),
    SettingsRecordingToggled(bool),
    SettingsRecordingPathChanged(String),
    BrowseRecordingPathPressed,
    /// `None` selects the "None" (disabled) option in the picker.
    SettingsSecondaryOutputChanged(Option<String>),
    /// Re-scans both hardware/virtual sinks and live app capture streams —
    /// dispatched when Settings opens and by the picker's own Refresh
    /// button, since app streams like Discord's only exist in the graph
    /// while that app is actually in a voice channel.
    RefreshSecondaryOutputTargets,
    SecondaryOutputTargetsLoaded(Vec<AudioDevice>, Vec<AudioDevice>),
    /// `None` selects the "None" (disabled) option in the picker.
    SettingsSecondaryInputChanged(Option<String>),
    /// Fired alongside `SecondaryOutputTargetsLoaded` by the same refresh —
    /// see `refresh_secondary_output_targets`.
    SecondaryInputTargetsLoaded(Vec<AudioDevice>, Vec<AudioDevice>),
    SettingsSavePressed,
    SettingsCancelPressed,
    /// Tab/Shift+Tab pressed anywhere, not already consumed by the focused
    /// widget — cycles keyboard focus forward/backward among this window's
    /// text fields (the only widgets iced itself supports focusing; see
    /// `subscription()`'s doc comment for the rest of the story).
    Tick,
    MuteToggled,
    /// Toggles mixing the Settings-configured secondary input (e.g.
    /// Discord's own playback) into the selected line's outgoing call
    /// audio. No-op if no secondary input target is configured.
    ToggleSecondaryInputInjection,
    OutputVolumeChanged(f32),
    InputVolumeChanged(f32),
    WindowClosed(window::Id),
    WindowResized(window::Id, iced::Size),
    PageSelected(Page),
    SettingsSectionSelected(SettingsSection),
    SidebarToggled,
    DndQuickToggled(bool),
    ThemeQuickToggled,
    DialBackspace,
    KeypadToggled,
    HistoryFilterChanged(HistoryFilter),
    /// The compositor asked to close a window (title-bar X, Alt+F4).
    CloseRequested(window::Id),
    QuitConfirmed,
    QuitCancelled,
    OpenUrl(String),
    OpenConfigFolder,
    TestRingPressed,
    TestRingStop,
    DialNumber(String),
    RedialPressed,
    HoldToggled,
    TransferPanelToggled,
    TransferTargetChanged(String),
    TransferConfirmed,
    CompactToggled,
    PostDialAdvance(usize, CallId),
    LineIdleTimeout(usize, u8),
    LineSelected(u8),
    AddCallPressed,
    JoinCallsPressed,
    SplitCallPressed,
    ContactFilterChanged(String),
    ContactSortToggled,
    AddContactPressed,
    EditContactPressed(usize),
    ContactNameChanged(String),
    ContactNumberChanged(String),
    ContactSavePressed,
    ContactCancelPressed,
    DeleteContactPressed(usize),
    ClearHistoryPressed,
    /// Right-click on a history entry — toggles that entry's inline
    /// "Add to Contacts" action row (opening it closes any other entry's,
    /// since only one can be open at a time).
    HistoryContextMenuToggled(usize),
    AddHistoryEntryToContacts(usize),
    ContactsIoPathChanged(String),
    ContactsImportPressed,
    ContactsExportPressed,
    ImportFileChosen(Option<String>),
    ExportFileChosen(Option<String>),
    BrowseContactsImportPressed,
    BrowseContactsExportPressed,
    TonePlayerReady(Result<Arc<DtmfTonePlayer>, String>),
}

/// Legacy single-account migration lives in `main.rs` (it needs to run once
/// at process startup, before `App` exists) — this is just where the
/// resulting multi-account file is persisted from then on.
const ACCOUNTS_FILENAME: &str = "accounts.toml";

const MAIN_WINDOW_SIZE: iced::Size = iced::Size::new(980.0, 660.0);
const MAIN_WINDOW_MIN_SIZE: iced::Size = iced::Size::new(820.0, 560.0);
const COMPACT_WINDOW_SIZE: iced::Size = iced::Size::new(400.0, 190.0);
const COMPACT_WINDOW_MIN_SIZE: iced::Size = iced::Size::new(360.0, 170.0);

/// Widget ids for keyboard focus (`Ctrl+K`, opening the contact form).
pub const DIAL_INPUT_ID: &str = "dial-input";
pub const SEARCH_INPUT_ID: &str = "contact-search";
pub const CONTACT_NAME_ID: &str = "contact-name";
/// How long a line stays "open" (dial tone played, ready to dial) with
/// nothing dialed before it times out on its own — see
/// `App::schedule_dial_timeout`/`Message::LineIdleTimeout`. Set just a
/// couple of seconds past the dial tone's own ~6s runtime
/// (`tone.rs::DIAL_TONE_DURATION_MS`) so the tone and the reorder-tone
/// timeout read as one continuous sequence — tone, brief silence, reorder —
/// instead of a short blip followed by a long dead gap.
const DIAL_TIMEOUT: Duration = Duration::from_secs(8);
/// Tuned to feel like a real-time meter without redrawing so often it wastes
/// cycles — noticeably snappier than a 200ms poll (which reads as laggy).
const TICK_INTERVAL: Duration = Duration::from_millis(80);
/// Matches the `Duration=250` sent in the SIP INFO body (`dialog.rs`), so a
/// digit sounds/feels the same length regardless of which DTMF transport
/// actually carried it.
const DTMF_EVENT_DURATION_MS: u32 = 250;

impl App {
    /// Opens the main window and constructs the initial `App` state — the
    /// `iced::daemon()` boot function. A daemon doesn't open any window by
    /// default (unlike `iced::application()`), so this has to do it
    /// explicitly. Unlike the settings windows, the main window's `Id` *is*
    /// stored (`main_window`) — compact call mode needs to resize it.
    pub fn boot(accounts: Vec<SipAccountConfig>) -> (Self, Task<Message>) {
        // `min_size` is set once, here, to the *smallest* size the window
        // ever needs (compact mode's floor) — not the main layout's floor —
        // and never touched again. `resize_for_compact` used to toggle
        // `min_size` back and forth between the two modes' floors before
        // each resize (relaxing it before shrinking, restoring it after
        // growing), but that's a two-step client→compositor round trip on
        // Wayland with no guarantee the first has actually landed before
        // the second goes out; when it lost that race, the resize request
        // silently got clamped back up against the stale (still large)
        // `min_size`, leaving the tiny compact layout rendered inside an
        // unchanged, mostly-empty window — exactly the "squished until you
        // drag the corner" bug, since manually dragging is what finally
        // forced the compositor to reconcile the size. A single min_size
        // that's always low enough for either mode needs no such
        // coordination — `resize_for_compact` is now just one resize call.
        let saved_ui = ui_state::load();
        let initial_size = saved_ui
            .window_size
            .map(|[w, h]| iced::Size::new(w.max(MAIN_WINDOW_MIN_SIZE.width), h.max(MAIN_WINDOW_MIN_SIZE.height)))
            .unwrap_or(MAIN_WINDOW_SIZE);
        let (main_window, open_task) = window::open(window::Settings {
            resizable: true,
            // Close requests go through `Message::CloseRequested` so an
            // active call can ask for confirmation first.
            exit_on_close_request: false,
            position: match saved_ui.window_position {
                Some([x, y]) => window::Position::Specific(iced::Point::new(x, y)),
                None => window::Position::Centered,
            },
            // One low min size for both normal and compact mode: resizing
            // min_size and size in the same frame races on Wayland.
            ..branding::window_settings(initial_size, Some(COMPACT_WINDOW_MIN_SIZE))
        });

        let audio_devices = audio_devices::load();
        let settings = app_settings::load();
        let needs_setup = accounts.is_empty();
        let initial_page = if needs_setup { Page::Settings } else { saved_ui.page };
        let sessions: Vec<AccountSession> = accounts.into_iter().map(AccountSession::new).collect();
        let sip_settings_form = sessions
            .first()
            .map(|a| SipSettingsForm::from_config(&a.config))
            .unwrap_or_default();
        let audio_settings_form = sessions
            .first()
            .map(|a| AudioSettingsForm::from_config(&a.config, &audio_devices))
            .unwrap_or_else(|| AudioSettingsForm::from_config(&SipAccountConfig::default(), &audio_devices));

        let app = App {
            accounts: sessions,
            selected_account: 0,
            page: initial_page,
            settings_section: saved_ui.settings_section,
            nav_animations: NavAnimations::new(initial_page),
            history_filter: saved_ui.history_filter,
            sidebar_collapsed: saved_ui.sidebar_collapsed,
            unseen_missed: 0,
            quit_prompt: false,
            keypad_open: false,
            last_escape: None,
            normal_window_size: initial_size,
            ui_state: saved_ui.clone(),
            ui_state_dirty_at: None,
            dial_input: String::new(),
            dial_history_open: false,
            sip_settings_form,
            editing_account: Some(0),
            audio_settings_form,
            audio_devices,
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            app_capture_streams: Vec::new(),
            app_playback_streams: Vec::new(),
            toasts: Vec::new(),
            next_toast_id: 1,
            system_mode: theme::Mode::Dark,
            main_window,
            main_window_size: initial_size,
            compact_mode: false,
            incoming_popup: None,
            incoming_popup_cards: 0,
            ringtone_playing: false,
            last_attention: None,
            notifications: HashMap::new(),
            settings_form: SettingsForm::from_settings(&settings),
            settings,
            call_history: history::load(),
            history_context_menu: None,
            contacts: contacts::load(),
            contact_filter: String::new(),
            contact_form: None,
            contact_sort: ContactSort::default(),
            contacts_io_path: saved_ui.contacts_io_path.clone().unwrap_or_else(|| {
                dirs::document_dir()
                    .unwrap_or_else(|| Path::new(".").to_path_buf())
                    .join("oxidesip-contacts.json")
                    .to_string_lossy()
                    .into_owned()
            }),
            contacts_io_status: None,
            tone_player: None,
            dialpad_flash: None,
        };

        let tone_target = app.audio_devices.output_device.clone();
        let tone_task = Task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                DtmfTonePlayer::start(tone_target)
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            Message::TonePlayerReady(result)
        });

        let startup = vec![
            open_task.discard(),
            tone_task,
            iced::system::theme().map(Message::SystemThemeChanged),
        ];
        (app, Task::batch(startup))
    }

    /// Global Tab/Shift+Tab handling for `iced::widget::focus_next`/
    /// `focus_previous`: `iced` only tracks keyboard focus for `text_input`
    /// (and `text_editor`/`combo_box`, unused here) — buttons, togglers, and
    /// `pick_list` have no concept of keyboard focus at all in this
    /// version, so Tab can only ever cycle between text fields, not reach
    /// Save/Cancel or a toggle. That's a real gap for genuinely complete
    /// keyboard navigation, but building one from scratch (a custom focus
    /// ring plus Enter/Space activation for every button-like widget) is a
    /// much larger, riskier change than this — flagged rather than faked.
    fn global_keys(event: iced::Event, status: iced::event::Status, window: window::Id) -> Option<Message> {
        if status != iced::event::Status::Ignored {
            return None;
        }
        match event {
            iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                Some(Message::KeyPressed(window, key, modifiers))
            }
            _ => None,
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let configs: Vec<SipAccountConfig> = self.accounts.iter().map(|a| a.config.clone()).collect();
        Subscription::batch([
            bridge::subscription(&configs),
            iced::time::every(TICK_INTERVAL).map(|_| Message::Tick),
            window::close_events().map(Message::WindowClosed),
            window::close_requests().map(Message::CloseRequested),
            window::resize_events().map(|(id, size)| Message::WindowResized(id, size)),
            iced::event::listen_with(Self::global_keys),
            iced::system::theme_changes().map(Message::SystemThemeChanged),
        ])
    }

    pub(crate) fn selected(&self) -> Option<&AccountSession> {
        self.accounts.get(self.selected_account)
    }

    fn selected_mut(&mut self) -> Option<&mut AccountSession> {
        self.accounts.get_mut(self.selected_account)
    }

    pub fn view(&self, window: window::Id) -> Element<'_, Message> {
        if Some(window) == self.incoming_popup {
            incoming::window_view(self)
        } else {
            ui::main_view(self)
        }
    }

    pub fn title(&self, window: window::Id) -> String {
        if Some(window) == self.incoming_popup {
            return incoming::title(self);
        }
        let active = self
            .accounts
            .iter()
            .flat_map(|a| a.lines.iter())
            .find_map(|line| match line {
                CallUiState::Active {
                    number, remote_name, ..
                } => Some(
                    ui::components::contact_name_for(&self.contacts, number)
                        .or(remote_name.as_deref())
                        .unwrap_or(number)
                        .to_string(),
                ),
                _ => None,
            });
        match active {
            Some(who) => format!("{who} — {}", branding::APP_NAME),
            None => branding::APP_NAME.to_string(),
        }
    }

    pub fn theme(&self, _window: window::Id) -> iced::Theme {
        theme::theme(self.mode())
    }

    /// The colour mode currently in effect (settings preference resolved
    /// against the desktop's scheme).
    pub(crate) fn mode(&self) -> theme::Mode {
        match self.settings.theme {
            ThemeSetting::System => self.system_mode,
            ThemeSetting::Dark => theme::Mode::Dark,
            ThemeSetting::Light => theme::Mode::Light,
        }
    }

    pub(crate) fn push_toast(&mut self, kind: ToastKind, title: impl Into<String>, detail: Option<String>) {
        let id = self.next_toast_id;
        self.next_toast_id += 1;
        self.toasts.push(Toast {
            id,
            kind,
            title: title.into(),
            detail,
            created: Instant::now(),
            ttl: crate::toast::default_ttl(kind),
        });
        if self.toasts.len() > 6 {
            self.toasts.remove(0);
        }
    }

    pub(crate) fn toast_error(&mut self, message: impl Into<String>) {
        self.push_toast(ToastKind::Error, message, None);
    }

    pub(crate) fn toast_info(&mut self, message: impl Into<String>) {
        self.push_toast(ToastKind::Info, message, None);
    }

    pub(crate) fn toast_success(&mut self, message: impl Into<String>) {
        self.push_toast(ToastKind::Success, message, None);
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.update_inner(message);
        Task::batch([task, self.sync_incoming_ui()])
    }

    fn update_inner(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::CoreConnected(account, tx) => {
                if let Some(acc) = self.accounts.get_mut(account) {
                    acc.command_tx = Some(tx);
                }
                Task::none()
            }

            Message::Core(account, CoreEvent::Registered { expires, rtt_ms }) => {
                if let Some(acc) = self.accounts.get_mut(account) {
                    acc.registration = RegistrationStatus::Registered {
                        expires,
                        registered_at: Instant::now(),
                        rtt_ms,
                    };
                }
                Task::none()
            }
            Message::Core(account, CoreEvent::RegistrationFailed { reason }) => {
                if let Some(acc) = self.accounts.get_mut(account) {
                    acc.registration = RegistrationStatus::Failed { reason };
                }
                Task::none()
            }
            Message::Core(account, CoreEvent::RegistrationHalted { reason }) => {
                if let Some(acc) = self.accounts.get_mut(account) {
                    acc.registration = RegistrationStatus::Halted { reason };
                }
                Task::none()
            }

            Message::Core(
                account,
                CoreEvent::IncomingCall {
                    id,
                    line,
                    remote,
                    offer,
                },
            ) => {
                let idx = line_idx(line);
                // DND and the deny list are both checked *before* the line
                // ever shows as ringing — this call never occupies a line
                // or shows up as "missed," it's rejected outright, same as
                // a real phone's call-blocking behaves.
                let denied = self.settings.dnd
                    || number::denied(
                        &self.settings.deny_list,
                        remote.display_name.as_deref(),
                        &remote.user,
                        &remote.uri,
                    );
                if denied {
                    if let Some(acc) = self.accounts.get(account) {
                        acc.send_command(CoreCommand::RejectCall {
                            id,
                            reason: RejectReason::Busy,
                        });
                    }
                    Self::push_history_named(
                        &mut self.call_history,
                        remote.user.clone(),
                        remote.display_name.clone(),
                        history::CallDirection::Incoming,
                        history::CallOutcome::Rejected,
                        None,
                    );
                    return Task::none();
                }
                let remote_for_notify = remote.clone();
                if let Some(acc) = self.accounts.get_mut(account)
                    && matches!(acc.lines[idx], CallUiState::Idle)
                {
                    acc.lines[idx] = CallUiState::Incoming {
                        id: id.clone(),
                        caller: remote,
                        offer,
                        ringing_since: Instant::now(),
                    };
                    acc.last_call_status[idx] = None;
                    if self.settings.auto_answer {
                        return self.answer_line(account, idx);
                    }
                    if let Some(player) = &self.tone_player {
                        player.play_incoming_ringtone();
                        self.ringtone_playing = true;
                    }
                    // The incoming-call window itself is opened/resized by
                    // `sync_incoming_ui` (run after every update). What is
                    // started here is the desktop notification, the one
                    // channel that reliably reaches the user even when the
                    // compositor refuses to raise a new window.
                    if self.settings.notify_incoming {
                        let (summary, body) = self.notification_text(account, &remote_for_notify);
                        return Task::stream(notify::incoming_call(id, summary, body))
                            .map(|(id, event)| Message::Notification(id, event));
                    }
                    return Task::none();
                }
                // Every line is busy (or this line is mid-call): the core
                // only assigns free lines, so this is a race with a call
                // that just started — refuse politely instead of leaving
                // the far end ringing into nothing.
                if let Some(acc) = self.accounts.get(account) {
                    acc.send_command(CoreCommand::RejectCall {
                        id,
                        reason: RejectReason::Busy,
                    });
                }
                Self::push_history_named(
                    &mut self.call_history,
                    remote_for_notify.user.clone(),
                    remote_for_notify.display_name.clone(),
                    history::CallDirection::Incoming,
                    history::CallOutcome::Missed,
                    None,
                );
                self.unseen_missed += 1;
                Task::none()
            }
            Message::Core(account, CoreEvent::OutgoingCallStarted { id, line }) => {
                let idx = line_idx(line);
                if let Some(acc) = self.accounts.get_mut(account) {
                    let number = std::mem::take(&mut acc.pending_numbers[idx]);
                    acc.lines[idx] = CallUiState::Outgoing {
                        id,
                        number,
                        started_at: Instant::now(),
                    };
                    acc.last_call_status[idx] = None;
                }
                Task::none()
            }
            Message::Core(account, CoreEvent::PlaceCallFailed { line, reason }) => {
                let idx = line_idx(line);
                if let Some(acc) = self.accounts.get_mut(account) {
                    acc.lines[idx] = CallUiState::Idle;
                    acc.pending_sockets[idx] = None;
                    // Also feed the footer's bottom-left readout
                    // (`line_status_label` in view.rs), same as
                    // `CallState::Terminated` already does — so an
                    // immediate setup failure (bad number, transport error)
                    // shows "why it's not going through" in the same place
                    // a denial/hangup reason would, not just the banner
                    // below.
                    acc.last_call_status[idx] = Some(reason.clone());
                }
                self.toast_error(reason);
                Task::none()
            }
            Message::Core(_, CoreEvent::DtmfResult { ok, digit, .. }) => {
                if !ok {
                    tracing::warn!(%digit, "dtmf send failed");
                }
                Task::none()
            }
            Message::Core(account, CoreEvent::TransferResult { id, ok }) => {
                // `ok` only means the REFER transaction itself was accepted
                // by the server — rsipstack gives no way to correlate the
                // follow-up NOTIFY(sipfrag) that would report whether the
                // transfer target actually answered, so this can't claim
                // more than "requested" (see README's Known limitations).
                if ok {
                    if let Some(acc) = self.accounts.get_mut(account)
                        && let Some(idx) = acc.line_index_for_call(&id)
                    {
                        acc.last_call_status[idx] = Some("transfer requested".to_string());
                    }
                } else {
                    self.toast_error("transfer request rejected".to_string());
                }
                Task::none()
            }
            Message::Core(_, CoreEvent::HoldResumeFailed { hold, reason, .. }) => {
                // `on_hold` is never flipped optimistically (see
                // `HoldToggled`), so there's no state to roll back here —
                // just surface why the button press didn't visibly do
                // anything, instead of leaving it silently swallowed.
                let action = if hold { "hold" } else { "resume" };
                self.toast_error(format!("{action} failed: {reason}"));
                Task::none()
            }

            Message::Core(account, CoreEvent::CallStateChanged { id, state }) => {
                self.handle_call_state_changed(account, id, state)
            }

            Message::DialInputChanged(s) => {
                // Play a tone for whatever digit was just typed (not
                // clicked on the dialpad, which plays its own via
                // `DialpadPressed`) — a real phone gives audible feedback
                // for typed digits too. Paste is routed to
                // `DialInputPasted` instead (see `view.rs`'s `.on_paste`),
                // so this only ever sees genuine keystrokes.
                if s.len() > self.dial_input.len() && s.starts_with(self.dial_input.as_str()) {
                    let added = s[self.dial_input.len()..].to_string();
                    if let Some(player) = &self.tone_player {
                        for c in added.chars() {
                            player.play(c);
                        }
                    }
                }
                self.dial_input = s;
                Task::none()
            }
            Message::DialInputPasted(s) => {
                // No tone playback — a pasted number wasn't dialed.
                self.dial_input = s;
                Task::none()
            }
            Message::DialHistoryToggled => {
                self.dial_history_open = !self.dial_history_open;
                Task::none()
            }
            Message::DialHistorySelected(number) => {
                self.dial_input = number;
                self.dial_history_open = false;
                Task::none()
            }
            Message::DialpadPressed(digit) => {
                if let Some(player) = &self.tone_player {
                    player.play(digit);
                }
                self.dialpad_flash = Some((digit, Instant::now()));
                self.handle_dialpad(digit)
            }
            Message::CallPressed => self.handle_call_pressed(),
            Message::AnswerPressed => self.handle_answer_pressed(),
            Message::RejectPressed => {
                let id = if let Some(acc) = self.selected() {
                    let idx = acc.selected_idx();
                    if let CallUiState::Incoming { id, .. } = &acc.lines[idx] {
                        let id = id.clone();
                        acc.send_command(CoreCommand::RejectCall {
                            id: id.clone(),
                            reason: RejectReason::Decline,
                        });
                        Some(id)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if id.is_some() {
                    self.stop_ringtone();
                }
                Task::none()
            }
            Message::AnswerIncomingCall(account, id) => {
                let Some(acc) = self.accounts.get(account) else {
                    return Task::none();
                };
                let Some(idx) = acc.line_index_for_call(&id) else {
                    return Task::none();
                };
                self.answer_line(account, idx)
            }
            Message::RejectIncomingCall(account, id) => self.decline_call(account, id),
            Message::ToastDismissed(id) => {
                self.toasts.retain(|t| t.id != id);
                Task::none()
            }
            Message::SystemThemeChanged(mode) => {
                self.system_mode = match mode {
                    iced::theme::Mode::Light => theme::Mode::Light,
                    _ => theme::Mode::Dark,
                };
                Task::none()
            }
            Message::SettingsThemeChanged(setting) => {
                self.settings.theme = setting;
                self.settings_form.theme = setting;
                if let Err(e) = app_settings::save(&self.settings) {
                    self.toast_error(format!("couldn't save settings: {e}"));
                }
                Task::none()
            }
            Message::SilenceRingtone => {
                self.stop_ringtone();
                Task::none()
            }
            Message::KeyPressed(window, key, modifiers) => self.handle_key(window, key, modifiers),
            Message::Notification(call_id, event) => {
                match event {
                    notify::Event::Shown(id) => {
                        // Only remember it while the call is still ringing;
                        // otherwise it is already stale — dismiss right away.
                        if self.is_ringing(&call_id) {
                            self.notifications.insert(call_id, id);
                        } else {
                            notify::dismiss(id);
                        }
                    }
                    notify::Event::Action(action) => {
                        self.notifications.remove(&call_id);
                        let owner = self.account_for_call(&call_id);
                        match (action.as_str(), owner) {
                            ("answer", Some((account, idx))) => return self.answer_line(account, idx),
                            ("decline", Some((account, _))) => return self.decline_call(account, call_id),
                            _ => {}
                        }
                    }
                    notify::Event::Failed(reason) => tracing::warn!(%reason, "desktop notification failed"),
                }
                Task::none()
            }
            Message::HangUpPressed => self.handle_hang_up_pressed(),

            Message::MediaReady(account, id, Ok(session_holder)) => {
                let session = session_holder.lock().unwrap().take();
                if let Some(acc) = self.accounts.get_mut(account)
                    && let Some(idx) = acc.line_index_for_call(&id)
                    && let Some((current, media)) = acc.lines[idx].media_slot()
                    && *current == id
                {
                    if let Some(session) = &session {
                        if self.settings.recording_enabled && !self.settings.recording_path.trim().is_empty() {
                            session.start_recording();
                        }
                        if let Some(target) = self.settings.secondary_output_target.clone() {
                            let label = format!("OxideSip Acct{} Line {} (secondary)", account + 1, idx + 1);
                            if let Err(e) = session.set_secondary_output(Some(target), label) {
                                tracing::warn!(%e, "failed to start secondary audio output");
                            }
                        }
                        if let Some(target) = self.settings.secondary_input_target.clone() {
                            let label = format!("OxideSip Acct{} Line {} (secondary in)", account + 1, idx + 1);
                            if let Err(e) = session.set_secondary_input(Some(target), label) {
                                tracing::warn!(%e, "failed to start secondary audio input");
                            }
                        }
                    }
                    *media = session;
                    if let CallUiState::Active {
                        media,
                        on_hold,
                        avatar_glow,
                        secondary_input_on,
                        ..
                    } = &mut acc.lines[idx]
                    {
                        avatar_glow.go_mut(avatar_live(media.is_some(), *on_hold), Instant::now());
                        if let Some(session) = media.as_ref()
                            && *secondary_input_on
                        {
                            session.set_secondary_input_enabled(true);
                        }
                    }
                }
                // else: call already ended before the pipeline finished
                // starting; `session` is dropped here, tearing itself down.
                Task::none()
            }
            Message::MediaReady(account, id, Err(reason)) => {
                tracing::warn!(%reason, "failed to start media session, hanging up");
                self.toast_error(format!("audio pipeline failed: {reason}"));
                if let Some(acc) = self.accounts.get(account) {
                    acc.send_command(CoreCommand::HangUp(id));
                }
                Task::none()
            }

            Message::OpenSipSettings => {
                self.editing_account = if self.accounts.is_empty() {
                    None
                } else {
                    Some(self.selected_account)
                };
                self.sip_settings_form = self
                    .selected()
                    .map(|a| SipSettingsForm::from_config(&a.config))
                    .unwrap_or_default();
                self.settings_section = SettingsSection::Accounts;
                self.navigate(Page::Settings);
                Task::none()
            }
            Message::OpenAudioSettings => {
                self.audio_settings_form = self
                    .selected()
                    .map(|a| AudioSettingsForm::from_config(&a.config, &self.audio_devices))
                    .unwrap_or_else(|| {
                        AudioSettingsForm::from_config(&SipAccountConfig::default(), &self.audio_devices)
                    });
                self.settings_section = SettingsSection::Audio;
                self.navigate(Page::Settings);
                Task::future(async {
                    let (inputs, outputs) = tokio::task::spawn_blocking(|| {
                        let inputs = softphone_media::devices::list_input_devices().unwrap_or_else(|e| {
                            tracing::warn!(%e, "failed to list input devices");
                            Vec::new()
                        });
                        let outputs = softphone_media::devices::list_output_devices().unwrap_or_else(|e| {
                            tracing::warn!(%e, "failed to list output devices");
                            Vec::new()
                        });
                        (inputs, outputs)
                    })
                    .await
                    .unwrap_or_default();
                    Message::DevicesLoaded(inputs, outputs)
                })
            }
            Message::SipSettingsNameChanged(s) => {
                self.sip_settings_form.name = s;
                Task::none()
            }
            Message::SipSettingsHostChanged(s) => {
                self.sip_settings_form.host = s;
                Task::none()
            }
            Message::SipSettingsPortChanged(s) => {
                self.sip_settings_form.port = s;
                Task::none()
            }
            Message::SipSettingsUsernameChanged(s) => {
                self.sip_settings_form.username = s;
                Task::none()
            }
            Message::SipSettingsPasswordChanged(s) => {
                self.sip_settings_form.password = s;
                Task::none()
            }
            Message::SipSettingsTransportChanged(transport) => {
                self.sip_settings_form.transport = transport;
                Task::none()
            }
            Message::SipSettingsSavePressed => {
                self.handle_sip_settings_save();
                Task::none()
            }
            Message::SipSettingsCancelPressed => {
                self.sip_settings_form = self
                    .editing_account
                    .and_then(|i| self.accounts.get(i))
                    .map(|a| SipSettingsForm::from_config(&a.config))
                    .unwrap_or_default();
                Task::none()
            }
            Message::SelectAccountForEditing(index) => {
                if let Some(acc) = self.accounts.get(index) {
                    self.editing_account = Some(index);
                    self.sip_settings_form = SipSettingsForm::from_config(&acc.config);
                }
                Task::none()
            }
            Message::AddAccountPressed => {
                self.editing_account = None;
                self.sip_settings_form = SipSettingsForm::default();
                Task::none()
            }
            Message::DeleteAccountPressed(index) => {
                if let Some(acc) = self.accounts.get(index) {
                    // Removing the account cancels its `SoftphoneCore` task
                    // outright (see `bridge::subscription`), with no
                    // graceful teardown of whatever dialogs it still had —
                    // without sending these first, any non-idle line's call
                    // is dropped purely on our side, leaving the far
                    // end/PBX thinking it's still up until it times out.
                    for line in &acc.lines {
                        match line {
                            CallUiState::Incoming { id, .. } => {
                                acc.send_command(CoreCommand::RejectCall {
                                    id: id.clone(),
                                    reason: RejectReason::Decline,
                                });
                            }
                            CallUiState::Outgoing { id, .. }
                            | CallUiState::EarlyMedia { id, .. }
                            | CallUiState::Active { id, .. } => {
                                acc.send_command(CoreCommand::HangUp(id.clone()));
                            }
                            CallUiState::Idle => {}
                        }
                    }
                }
                if index < self.accounts.len() {
                    self.accounts.remove(index);
                    if self.selected_account >= self.accounts.len() {
                        self.selected_account = self.accounts.len().saturating_sub(1);
                    }
                    if self.editing_account == Some(index) {
                        self.editing_account = if self.accounts.is_empty() { None } else { Some(0) };
                        self.sip_settings_form = self
                            .accounts
                            .first()
                            .map(|a| SipSettingsForm::from_config(&a.config))
                            .unwrap_or_default();
                    } else if let Some(editing) = self.editing_account
                        && editing > index
                    {
                        self.editing_account = Some(editing - 1);
                    }
                    self.persist_accounts();
                }
                Task::none()
            }
            Message::AccountSwitched(index) => {
                if index == self.selected_account || index >= self.accounts.len() {
                    return Task::none();
                }
                if let Some(old) = self.selected() {
                    old.hold_current_if_live();
                    if old.line_open
                        && matches!(old.selected_call(), CallUiState::Idle)
                        && let Some(player) = &self.tone_player
                    {
                        player.stop_line_tone();
                    }
                }
                self.selected_account = index;
                self.dial_input.clear();
                if let Some(acc) = self.selected() {
                    acc.resume_current_if_held();
                }
                Task::none()
            }
            Message::AudioSettingsInputDeviceChanged(id) => {
                self.audio_settings_form.input_device = (!id.is_empty()).then_some(id);
                Task::none()
            }
            Message::AudioSettingsOutputDeviceChanged(id) => {
                self.audio_settings_form.output_device = (!id.is_empty()).then_some(id);
                Task::none()
            }
            Message::AudioSettingsSrtpToggled(enabled) => {
                self.audio_settings_form.srtp = enabled;
                Task::none()
            }
            Message::AudioSettingsCodecMoved(index, up) => {
                let codecs = &mut self.audio_settings_form.codecs;
                let target = if up { index.checked_sub(1) } else { index.checked_add(1) };
                if let Some(target) = target
                    && target < codecs.len()
                {
                    codecs.swap(index, target);
                }
                Task::none()
            }
            Message::AudioSettingsDtmfModeChanged(mode) => {
                self.audio_settings_form.dtmf_mode = mode;
                Task::none()
            }
            Message::AudioSettingsSavePressed => {
                self.handle_audio_settings_save();
                self.toast_success("Audio settings saved — they apply to the next call");
                Task::none()
            }
            Message::AudioSettingsCancelPressed => {
                self.audio_settings_form = self
                    .selected()
                    .map(|a| AudioSettingsForm::from_config(&a.config, &self.audio_devices))
                    .unwrap_or_else(|| {
                        AudioSettingsForm::from_config(&SipAccountConfig::default(), &self.audio_devices)
                    });
                Task::none()
            }
            Message::OpenSettings => {
                self.settings_form = SettingsForm::from_settings(&self.settings);
                self.settings_section = SettingsSection::Calls;
                self.navigate(Page::Settings);
                Task::batch([
                    self.refresh_secondary_output_targets(),
                    self.refresh_secondary_input_targets(),
                ])
            }
            Message::RefreshSecondaryOutputTargets => Task::batch([
                self.refresh_secondary_output_targets(),
                self.refresh_secondary_input_targets(),
            ]),
            Message::SecondaryOutputTargetsLoaded(sinks, app_streams) => {
                self.output_devices = sinks;
                self.app_capture_streams = app_streams;
                Task::none()
            }
            Message::SecondaryInputTargetsLoaded(inputs, app_streams) => {
                self.input_devices = inputs;
                self.app_playback_streams = app_streams;
                Task::none()
            }
            Message::SettingsDndToggled(enabled) => {
                self.settings_form.dnd = enabled;
                Task::none()
            }
            Message::SettingsNotifyToggled(enabled) => {
                self.settings_form.notify_incoming = enabled;
                Task::none()
            }
            Message::SettingsAutoAnswerToggled(enabled) => {
                self.settings_form.auto_answer = enabled;
                Task::none()
            }
            Message::SettingsDenyListInputChanged(s) => {
                self.settings_form.deny_list_input = s;
                Task::none()
            }
            Message::SettingsDenyListAddPressed => {
                let entry = self.settings_form.deny_list_input.trim().to_string();
                if !entry.is_empty() && !self.settings_form.deny_list.iter().any(|e| e == &entry) {
                    self.settings_form.deny_list.push(entry);
                }
                self.settings_form.deny_list_input.clear();
                Task::none()
            }
            Message::SettingsDenyListRemovePressed(index) => {
                if index < self.settings_form.deny_list.len() {
                    self.settings_form.deny_list.remove(index);
                }
                Task::none()
            }
            Message::SettingsRecordingToggled(enabled) => {
                self.settings_form.recording_enabled = enabled;
                Task::none()
            }
            Message::SettingsRecordingPathChanged(s) => {
                self.settings_form.recording_path = s;
                Task::none()
            }
            Message::BrowseRecordingPathPressed => {
                // Falls back to whatever was already typed if the dialog is
                // cancelled, rather than clobbering it with an empty path.
                let current = self.settings_form.recording_path.clone();
                Task::future(async move {
                    let folder = rfd::AsyncFileDialog::new()
                        .set_title("Choose a recordings folder")
                        .pick_folder()
                        .await;
                    Message::SettingsRecordingPathChanged(
                        folder.map(|f| f.path().display().to_string()).unwrap_or(current),
                    )
                })
            }
            Message::SettingsSecondaryOutputChanged(target) => {
                self.settings_form.secondary_output_target = target;
                Task::none()
            }
            Message::SettingsSecondaryInputChanged(target) => {
                self.settings_form.secondary_input_target = target;
                Task::none()
            }
            Message::SettingsSavePressed => {
                self.settings = AppSettings {
                    dnd: self.settings_form.dnd,
                    auto_answer: self.settings_form.auto_answer,
                    forwarding_enabled: self.settings.forwarding_enabled,
                    forwarding_number: self.settings.forwarding_number.clone(),
                    deny_list: self.settings_form.deny_list.clone(),
                    notify_incoming: self.settings_form.notify_incoming,
                    theme: self.settings_form.theme,
                    recording_enabled: self.settings_form.recording_enabled,
                    recording_path: self.settings_form.recording_path.trim().to_string(),
                    secondary_output_target: self.settings_form.secondary_output_target.clone(),
                    secondary_input_target: self.settings_form.secondary_input_target.clone(),
                };
                match app_settings::save(&self.settings) {
                    Ok(()) => self.toast_success("Settings saved"),
                    Err(e) => self.toast_error(format!("couldn't save settings: {e}")),
                }
                Task::none()
            }
            Message::SettingsCancelPressed => {
                self.settings_form = SettingsForm::from_settings(&self.settings);
                Task::none()
            }
            Message::DevicesLoaded(inputs, outputs) => {
                self.input_devices = inputs;
                self.output_devices = outputs;
                Task::none()
            }
            Message::WindowClosed(id) => {
                if Some(id) == self.incoming_popup {
                    // `sync_incoming_ui` clears `incoming_popup` *before*
                    // issuing its own `window::close`, so reaching here with
                    // it still set means the user hit the native close
                    // button — treat that as declining everything that rings.
                    self.incoming_popup = None;
                    self.incoming_popup_cards = 0;
                    let ringing: Vec<(usize, CallId)> = incoming::ringing_calls(self)
                        .iter()
                        .map(|c| (c.account, c.id.clone()))
                        .collect();
                    let tasks: Vec<Task<Message>> = ringing
                        .into_iter()
                        .map(|(account, call_id)| self.decline_call(account, call_id))
                        .collect();
                    Task::batch(tasks)
                } else if id == self.main_window {
                    iced::exit()
                } else {
                    Task::none()
                }
            }
            Message::WindowResized(id, size) => {
                if id == self.main_window {
                    self.main_window_size = size;
                    if !self.compact_mode {
                        self.normal_window_size = size;
                        self.ui_state.window_size = Some([size.width, size.height]);
                        self.mark_ui_state_dirty();
                    }
                }
                Task::none()
            }

            Message::Tick => {
                let now = Instant::now();
                self.toasts.retain(|t| !t.expired(now));
                if self
                    .ui_state_dirty_at
                    .is_some_and(|at| now.duration_since(at) > Duration::from_millis(600))
                {
                    self.flush_ui_state();
                }
                if self
                    .last_escape
                    .is_some_and(|at| now.duration_since(at) > Duration::from_secs(2))
                {
                    self.last_escape = None;
                }
                if let Some(acc) = self.selected_mut() {
                    let idx = acc.selected_idx();
                    if let CallUiState::Active {
                        media: Some(session),
                        input_level,
                        output_level,
                        dtmf_feedback,
                        ..
                    } = &mut acc.lines[idx]
                    {
                        // Exponential smoothing rather than assigning the raw
                        // per-chunk peak directly: a single loud/quiet sample
                        // makes the raw value visibly jump between polls, which
                        // reads as jittery rather than a smooth level meter.
                        // Paired with `TICK_INTERVAL`'s faster polling, this
                        // factor still reaches near the true value within a
                        // couple of ticks instead of trailing noticeably behind.
                        const SMOOTHING: f32 = 0.55;
                        *input_level += (session.input_level() - *input_level) * SMOOTHING;
                        *output_level += (session.output_level() - *output_level) * SMOOTHING;
                        // Digits the peer sent us via RFC 4733 — shares the
                        // same on-screen feedback trail as digits we sent,
                        // same as a real desk phone's display.
                        dtmf_feedback.extend(session.drain_received_dtmf());
                    }
                }
                Task::none()
            }
            Message::MuteToggled => {
                if let Some(acc) = self.selected_mut() {
                    let idx = acc.selected_idx();
                    if let CallUiState::Active { media, muted, .. } = &mut acc.lines[idx] {
                        *muted = !*muted;
                        if let Some(session) = media {
                            session.set_mic_muted(*muted);
                        }
                    }
                }
                Task::none()
            }
            Message::ToggleSecondaryInputInjection => {
                if self.settings.secondary_input_target.is_some()
                    && let Some(acc) = self.selected_mut()
                {
                    let idx = acc.selected_idx();
                    if let CallUiState::Active {
                        media,
                        secondary_input_on,
                        ..
                    } = &mut acc.lines[idx]
                    {
                        *secondary_input_on = !*secondary_input_on;
                        if let Some(session) = media {
                            session.set_secondary_input_enabled(*secondary_input_on);
                        }
                    }
                }
                Task::none()
            }
            Message::OutputVolumeChanged(gain) => {
                if let Some(acc) = self.selected_mut() {
                    let idx = acc.selected_idx();
                    if let CallUiState::Active {
                        media,
                        output_volume,
                        on_hold,
                        ..
                    } = &mut acc.lines[idx]
                    {
                        *output_volume = gain;
                        if let Some(session) = media
                            && !*on_hold
                        {
                            session.set_output_volume(gain);
                        }
                    }
                }
                Task::none()
            }
            Message::InputVolumeChanged(gain) => {
                if let Some(acc) = self.selected_mut() {
                    let idx = acc.selected_idx();
                    if let CallUiState::Active {
                        media, input_volume, ..
                    } = &mut acc.lines[idx]
                    {
                        *input_volume = gain;
                        if let Some(session) = media {
                            session.set_input_gain(gain);
                        }
                    }
                }
                Task::none()
            }

            Message::PageSelected(page) => {
                self.navigate(page);
                Task::none()
            }
            Message::SettingsSectionSelected(section) => {
                self.settings_section = section;
                self.ui_state.settings_section = section;
                self.mark_ui_state_dirty();
                // Entering a section reloads its form from the live state
                // and refreshes whatever device lists it shows.
                match section {
                    SettingsSection::Accounts => self.update_inner(Message::OpenSipSettings),
                    SettingsSection::Audio => self.update_inner(Message::OpenAudioSettings),
                    SettingsSection::Calls => self.update_inner(Message::OpenSettings),
                    SettingsSection::Appearance | SettingsSection::About => Task::none(),
                }
            }
            Message::SidebarToggled => {
                self.sidebar_collapsed = !self.sidebar_collapsed;
                self.ui_state.sidebar_collapsed = self.sidebar_collapsed;
                self.mark_ui_state_dirty();
                Task::none()
            }
            Message::DndQuickToggled(on) => {
                self.settings.dnd = on;
                self.settings_form.dnd = on;
                if let Err(e) = app_settings::save(&self.settings) {
                    self.toast_error(format!("couldn't save settings: {e}"));
                }
                self.toast_info(if on {
                    "Do Not Disturb on — incoming calls are declined"
                } else {
                    "Do Not Disturb off"
                });
                Task::none()
            }
            Message::ThemeQuickToggled => {
                let next = match self.mode() {
                    theme::Mode::Dark => ThemeSetting::Light,
                    theme::Mode::Light => ThemeSetting::Dark,
                };
                self.update_inner(Message::SettingsThemeChanged(next))
            }
            Message::DialBackspace => {
                self.dial_input.pop();
                Task::none()
            }
            Message::KeypadToggled => {
                self.keypad_open = !self.keypad_open;
                Task::none()
            }
            Message::HistoryFilterChanged(filter) => {
                self.history_filter = filter;
                self.ui_state.history_filter = filter;
                self.mark_ui_state_dirty();
                Task::none()
            }
            Message::CloseRequested(id) => {
                if id != self.main_window {
                    return Task::none();
                }
                if self.any_call_in_progress() {
                    self.quit_prompt = true;
                    Task::none()
                } else {
                    self.flush_ui_state();
                    window::close(id)
                }
            }
            Message::QuitConfirmed => {
                self.quit_prompt = false;
                self.hang_up_everything();
                self.flush_ui_state();
                iced::exit()
            }
            Message::QuitCancelled => {
                self.quit_prompt = false;
                Task::none()
            }
            Message::OpenUrl(url) => {
                open_external(&url);
                Task::none()
            }
            Message::OpenConfigFolder => {
                open_external(&crate::paths::config_file("").to_string_lossy());
                Task::none()
            }
            Message::TestRingPressed => {
                if let Some(player) = &self.tone_player {
                    player.play_incoming_ringtone();
                    self.ringtone_playing = true;
                }
                Task::future(async {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    Message::TestRingStop
                })
            }
            Message::TestRingStop => {
                if incoming::ringing_calls(self).is_empty() {
                    self.stop_ringtone();
                }
                Task::none()
            }
            Message::DialNumber(number) => {
                let Some(acc) = self.selected() else {
                    return Task::none();
                };
                let idx = acc.selected_idx();
                if !matches!(acc.lines[idx], CallUiState::Idle) {
                    self.toast_error("selected line is busy — pick a free line first".to_string());
                    return Task::none();
                }
                self.dial_input = number;
                self.navigate(Page::Phone);
                self.handle_call_pressed()
            }
            Message::RedialPressed => {
                let last = self.last_outgoing_number();
                if let Some(number) = last {
                    self.dial_input = number;
                    return self.handle_call_pressed();
                }
                Task::none()
            }

            Message::HoldToggled => {
                if let Some(acc) = self.selected() {
                    let idx = acc.selected_idx();
                    if acc.joined[idx].is_some() {
                        self.toast_error("can't hold a joined call — split it first".to_string());
                        return Task::none();
                    }
                    if let CallUiState::Active { id, on_hold, .. } = &acc.lines[idx] {
                        let id = id.clone();
                        let cmd = if *on_hold {
                            CoreCommand::ResumeCall(id)
                        } else {
                            CoreCommand::HoldCall(id)
                        };
                        acc.send_command(cmd);
                    }
                }
                Task::none()
            }
            Message::TransferPanelToggled => {
                if let Some(acc) = self.selected_mut() {
                    let idx = acc.selected_idx();
                    if let CallUiState::Active { transfer_input, .. } = &mut acc.lines[idx] {
                        *transfer_input = if transfer_input.is_some() {
                            None
                        } else {
                            Some(String::new())
                        };
                    }
                }
                Task::none()
            }
            Message::TransferTargetChanged(s) => {
                if let Some(acc) = self.selected_mut() {
                    let idx = acc.selected_idx();
                    if let CallUiState::Active {
                        transfer_input: Some(target),
                        ..
                    } = &mut acc.lines[idx]
                    {
                        *target = s;
                    }
                }
                Task::none()
            }
            Message::TransferConfirmed => {
                if let Some(acc) = self.selected_mut() {
                    let idx = acc.selected_idx();
                    if let CallUiState::Active { id, transfer_input, .. } = &mut acc.lines[idx]
                        && let Some(target) = transfer_input.take()
                    {
                        let target = target.trim().to_string();
                        if !target.is_empty() {
                            let id = id.clone();
                            acc.send_command(CoreCommand::BlindTransfer { id, target });
                        }
                    }
                }
                Task::none()
            }
            Message::CompactToggled => {
                self.compact_mode = !self.compact_mode;
                self.resize_for_compact(self.compact_mode)
            }
            Message::PostDialAdvance(account, id) => {
                let Some(acc) = self.accounts.get_mut(account) else {
                    return Task::none();
                };
                let Some(idx) = acc.line_index_for_call(&id) else {
                    return Task::none();
                };
                let step = if let CallUiState::Active { post_dial, .. } = &mut acc.lines[idx] {
                    post_dial.pop_front().map(|next| (next, !post_dial.is_empty()))
                } else {
                    None
                };
                let Some((next, has_more)) = step else {
                    return Task::none();
                };
                if next == ',' {
                    // A pause: nothing to send yet, just wait and advance.
                    // Each comma is its own queued step (see `split_dial_
                    // input`'s doc comment — consecutive commas aren't
                    // collapsed), so a run of N commas in a row stacks N of
                    // these waits back to back, giving an N-second pause —
                    // e.g. "5,,,2" sends `5`, waits 3s, then sends `2`.
                    let call_id = id.clone();
                    return Task::future(async move {
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                        Message::PostDialAdvance(account, call_id)
                    });
                }
                if let CallUiState::Active { dtmf_feedback, .. } = &mut acc.lines[idx] {
                    dtmf_feedback.push(next);
                }
                if let Some(player) = &self.tone_player {
                    player.play(next);
                }
                acc.send_dtmf_digit(idx, next);
                if has_more {
                    let call_id = id.clone();
                    Task::future(async move {
                        tokio::time::sleep(Duration::from_millis(350)).await;
                        Message::PostDialAdvance(account, call_id)
                    })
                } else {
                    Task::none()
                }
            }
            Message::LineIdleTimeout(account, line) => {
                let idx = line_idx(line);
                if account == self.selected_account
                    && let Some(acc) = self.accounts.get_mut(account)
                    && acc.selected_line == line
                    && acc.line_open
                    && self.dial_input.is_empty()
                    && matches!(acc.lines[idx], CallUiState::Idle)
                {
                    acc.line_open = false;
                    acc.line_open_at = None;
                    if let Some(player) = &self.tone_player {
                        player.play_reorder_tone();
                    }
                }
                Task::none()
            }
            Message::LineSelected(line) => self.select_line(line),
            Message::JoinCallsPressed => {
                let Some(acc) = self.selected_mut() else {
                    return Task::none();
                };
                if acc.pending_join.is_some() {
                    acc.pending_join = None;
                    return Task::none();
                }
                let idx = acc.selected_idx();
                if !matches!(acc.lines[idx], CallUiState::Active { .. }) || acc.joined[idx].is_some() {
                    return Task::none();
                }
                let selected_line = acc.selected_line;
                let has_other_active = (1..=LINE_COUNT as u8)
                    .any(|l| l != selected_line && matches!(acc.lines[line_idx(l)], CallUiState::Active { .. }));
                if has_other_active {
                    acc.pending_join = Some(selected_line);
                } else {
                    self.toast_error("no other active call to join".to_string());
                }
                Task::none()
            }
            Message::SplitCallPressed => {
                // Split is the corrective action for "can't hold a joined
                // call — split it first" (see `HoldToggled`) — clear it
                // unconditionally so it doesn't linger once followed.
                if let Some(acc) = self.selected_mut() {
                    let idx = acc.selected_idx();
                    if let Some(partner) = acc.joined[idx] {
                        let selected_line = acc.selected_line;
                        acc.split_join(selected_line, partner);
                    }
                }
                Task::none()
            }
            Message::AddCallPressed => {
                let Some(acc) = self.selected() else {
                    return Task::none();
                };
                let selected_line = acc.selected_line;
                let free = (1..=LINE_COUNT as u8)
                    .find(|&l| l != selected_line && matches!(acc.lines[line_idx(l)], CallUiState::Idle));
                match free {
                    Some(line) => self.select_line(line),
                    None => {
                        self.toast_error("all lines are busy".to_string());
                        Task::none()
                    }
                }
            }

            Message::ContactFilterChanged(s) => {
                self.contact_filter = s;
                Task::none()
            }
            Message::ContactSortToggled => {
                self.contact_sort = self.contact_sort.next();
                Task::none()
            }
            Message::AddContactPressed => {
                self.contact_form = Some(ContactForm::default());
                iced::widget::operation::focus(CONTACT_NAME_ID)
            }
            Message::EditContactPressed(index) => {
                if let Some(c) = self.contacts.get(index) {
                    self.contact_form = Some(ContactForm {
                        name: c.name.clone(),
                        number: c.number.clone(),
                        editing_index: Some(index),
                    });
                }
                Task::none()
            }
            Message::ContactNameChanged(s) => {
                if let Some(form) = &mut self.contact_form {
                    form.name = s;
                }
                Task::none()
            }
            Message::ContactNumberChanged(s) => {
                if let Some(form) = &mut self.contact_form {
                    form.number = s;
                }
                Task::none()
            }
            Message::ContactSavePressed => {
                if let Some(form) = self.contact_form.take() {
                    let name = form.name.trim().to_string();
                    let number = form.number.trim().to_string();
                    if !name.is_empty() && !number.is_empty() {
                        let contact = Contact { name, number };
                        match form.editing_index {
                            Some(index) if index < self.contacts.len() => {
                                self.contacts[index] = contact;
                            }
                            _ => self.contacts.push(contact),
                        }
                        self.persist_contacts();
                    }
                }
                Task::none()
            }
            Message::ContactCancelPressed => {
                self.contact_form = None;
                Task::none()
            }
            Message::DeleteContactPressed(index) => {
                if index < self.contacts.len() {
                    self.contacts.remove(index);
                    self.persist_contacts();
                }
                Task::none()
            }
            Message::ClearHistoryPressed => {
                self.call_history.clear();
                self.history_context_menu = None;
                history::save(&self.call_history);
                Task::none()
            }
            Message::HistoryContextMenuToggled(index) => {
                self.history_context_menu = if self.history_context_menu == Some(index) {
                    None
                } else {
                    Some(index)
                };
                Task::none()
            }
            Message::AddHistoryEntryToContacts(index) => {
                if let Some(entry) = self.call_history.get(index) {
                    self.contact_form = Some(ContactForm {
                        name: String::new(),
                        number: entry.number.clone(),
                        editing_index: None,
                    });
                    self.navigate(Page::Contacts);
                }
                self.history_context_menu = None;
                Task::none()
            }
            Message::ContactsIoPathChanged(path) => {
                self.ui_state.contacts_io_path = Some(path.clone());
                self.mark_ui_state_dirty();
                self.contacts_io_path = path;
                Task::none()
            }
            Message::ContactsImportPressed => {
                let path = Path::new(self.contacts_io_path.trim());
                match contacts::import_json(path) {
                    Ok(imported) => {
                        let added = imported.len();
                        for contact in imported {
                            match self.contacts.iter_mut().find(|c| c.number == contact.number) {
                                Some(existing) => *existing = contact,
                                None => self.contacts.push(contact),
                            }
                        }
                        self.persist_contacts();
                        self.contacts_io_status =
                            Some(format!("Imported {added} contact{}", if added == 1 { "" } else { "s" }));
                    }
                    Err(e) => self.contacts_io_status = Some(format!("Import failed: {e}")),
                }
                Task::none()
            }
            Message::ContactsExportPressed => {
                let path = Path::new(self.contacts_io_path.trim());
                self.contacts_io_status = Some(match contacts::export_json(&self.contacts, path) {
                    Ok(()) => format!("Exported {} contacts", self.contacts.len()),
                    Err(e) => format!("Export failed: {e}"),
                });
                Task::none()
            }
            Message::BrowseContactsImportPressed => {
                let current = self.contacts_io_path.clone();
                Task::future(async move {
                    let file = rfd::AsyncFileDialog::new()
                        .set_title("Choose a contacts JSON file to import")
                        .add_filter("JSON", &["json"])
                        .pick_file()
                        .await;
                    let _ = current;
                    Message::ImportFileChosen(file.map(|f| f.path().display().to_string()))
                })
            }
            Message::ImportFileChosen(path) => match path {
                Some(path) => {
                    self.contacts_io_path = path.clone();
                    self.ui_state.contacts_io_path = Some(path);
                    self.mark_ui_state_dirty();
                    self.update_inner(Message::ContactsImportPressed)
                }
                None => Task::none(),
            },
            Message::ExportFileChosen(path) => match path {
                Some(path) => {
                    self.contacts_io_path = path.clone();
                    self.ui_state.contacts_io_path = Some(path);
                    self.mark_ui_state_dirty();
                    self.update_inner(Message::ContactsExportPressed)
                }
                None => Task::none(),
            },
            Message::BrowseContactsExportPressed => {
                let current = self.contacts_io_path.clone();
                Task::future(async move {
                    let file = rfd::AsyncFileDialog::new()
                        .set_title("Choose where to save the contacts export")
                        .add_filter("JSON", &["json"])
                        .set_file_name("contacts_export.json")
                        .save_file()
                        .await;
                    let _ = current;
                    Message::ExportFileChosen(file.map(|f| f.path().display().to_string()))
                })
            }

            Message::TonePlayerReady(Ok(player)) => {
                self.tone_player = Some(player);
                Task::none()
            }
            Message::TonePlayerReady(Err(e)) => {
                tracing::warn!(%e, "failed to start DTMF tone player");
                Task::none()
            }
        }
    }

    /// Switches which line the Dialer tab shows/controls, within the
    /// currently-selected account. Holds whatever line is being left (if
    /// it's actively unheld — reuses the existing hold plumbing verbatim),
    /// resumes the line being switched to (if it was held), and plays a
    /// dial tone when landing on an idle line.
    fn select_line(&mut self, line: u8) -> Task<Message> {
        let account = self.selected_account;
        let Some(acc) = self.accounts.get_mut(account) else {
            return Task::none();
        };
        if let Some(initiator) = acc.pending_join {
            acc.pending_join = None;
            if line != initiator {
                return self.complete_join_selected(initiator, line);
            }
            return Task::none();
        }
        if line_idx(line) >= LINE_COUNT {
            return Task::none();
        }
        if line == acc.selected_line {
            // Re-tapping the line you're already on: while it's idle, this
            // is a real on/off toggle of `line_open`.
            let idx = acc.selected_idx();
            if matches!(acc.lines[idx], CallUiState::Idle) {
                acc.line_open = !acc.line_open;
                acc.line_open_at = acc.line_open.then(Instant::now);
                if let Some(player) = &self.tone_player {
                    if acc.line_open {
                        player.play_dial_tone();
                    } else {
                        player.stop_line_tone();
                    }
                }
                if !acc.line_open {
                    self.dial_input.clear();
                }
                if acc.line_open {
                    return Self::schedule_dial_timeout(account, line);
                }
            }
            return Task::none();
        }
        let old_idx = acc.selected_idx();
        // A joined line stays live regardless of which line is "selected" —
        // holding it would pause its SIP media and starve the conference's
        // other leg of real audio to relay (see `MediaSession::join_with`).
        if acc.joined[old_idx].is_none()
            && let CallUiState::Active { id, on_hold: false, .. } = &acc.lines[old_idx]
        {
            acc.send_command(CoreCommand::HoldCall(id.clone()));
        } else if acc.line_open && matches!(acc.lines[old_idx], CallUiState::Idle) {
            // Leaving an open-but-idle line behind — cut its dial tone
            // immediately rather than letting it keep playing out on top
            // of whatever the newly-selected line does next.
            if let Some(player) = &self.tone_player {
                player.stop_line_tone();
            }
        }
        acc.selected_line = line;
        self.dial_input.clear();
        let new_idx = acc.selected_idx();
        if acc.joined[new_idx].is_none()
            && let CallUiState::Active { id, on_hold: true, .. } = &acc.lines[new_idx]
        {
            acc.send_command(CoreCommand::ResumeCall(id.clone()));
        } else if matches!(acc.lines[new_idx], CallUiState::Idle) {
            // Landing on a *different* idle line always seizes it fresh,
            // regardless of whatever `line_open` was left set to by the
            // line we just came from.
            acc.line_open = true;
            acc.line_open_at = Some(Instant::now());
            if let Some(player) = &self.tone_player {
                player.play_dial_tone();
            }
            return Self::schedule_dial_timeout(account, line);
        }
        Task::none()
    }

    /// A line left open (dial tone played, nothing dialed) for this long
    /// times out — see `Message::LineIdleTimeout` — same as a real phone
    /// eventually giving up on an off-hook line with no input and telling
    /// you to hang up.
    fn schedule_dial_timeout(account: usize, line: u8) -> Task<Message> {
        Task::future(async move {
            tokio::time::sleep(DIAL_TIMEOUT).await;
            Message::LineIdleTimeout(account, line)
        })
    }

    fn complete_join_selected(&mut self, a: u8, b: u8) -> Task<Message> {
        let Some(acc) = self.selected_mut() else {
            return Task::none();
        };
        match acc.complete_join(a, b) {
            Ok(()) => {}
            Err(e) => self.toast_error(e.to_string()),
        }
        Task::none()
    }

    fn persist_contacts(&mut self) {
        contacts::save(&self.contacts);
        self.contacts = contacts::load();
    }

    fn persist_accounts(&self) {
        let configs: Vec<SipAccountConfig> = self.accounts.iter().map(|a| a.config.clone()).collect();
        if let Err(e) = softphone_core::config::save_accounts(&crate::paths::config_file(ACCOUNTS_FILENAME), &configs) {
            tracing::warn!(%e, "failed to save accounts");
        }
    }

    fn last_outgoing_number(&self) -> Option<String> {
        self.call_history
            .iter()
            .rev()
            .find(|e| e.direction == history::CallDirection::Outgoing)
            .map(|e| e.number.clone())
    }

    /// A free function rather than a `&mut self` method — several call
    /// sites need to push a history entry while still holding a live
    /// `&mut AccountSession` borrowed from `self.accounts`, and a `&mut
    /// self` method call doesn't compose with that (the borrow checker
    /// can't see through the method-call boundary that it'd only touch the
    /// disjoint `call_history` field). Taking `&mut Vec<HistoryEntry>`
    /// directly lets call sites borrow just that field instead.
    fn push_history(
        call_history: &mut Vec<history::HistoryEntry>,
        number: String,
        direction: history::CallDirection,
        outcome: history::CallOutcome,
        duration: Option<Duration>,
    ) {
        Self::push_history_named(call_history, number, None, direction, outcome, duration);
    }

    fn push_history_named(
        call_history: &mut Vec<history::HistoryEntry>,
        number: String,
        name: Option<String>,
        direction: history::CallDirection,
        outcome: history::CallOutcome,
        duration: Option<Duration>,
    ) {
        let entry = history::HistoryEntry {
            number,
            name,
            direction,
            outcome,
            unix_secs: history::now_unix(),
            duration_secs: duration.map(|d| d.as_secs() as u32).unwrap_or(0),
        };
        call_history.push(entry);
        history::save(call_history);
    }

    /// Same reasoning as `push_history`: an associated function taking
    /// `settings: &AppSettings` directly (not a `&self`/`&mut self` method)
    /// so call sites can invoke it while still holding a live `&mut
    /// AccountSession`/`&MediaSession` borrowed from `self.accounts` — a
    /// method call would need the whole `self` and conflict with that.
    ///
    /// Stops recording on `session` (if any) and, if recording is enabled
    /// with a save path configured, spawns a background task to write the
    /// WAV — fire-and-forget, since there's no live call left to attach an
    /// error to by the time a call actually ends. Named `"{number}
    /// ({mm}m{ss}s) {unix_secs}.wav"` — number and duration for easy
    /// recognition per the ask, plus a timestamp so calling the same number
    /// twice doesn't overwrite the first recording.
    fn save_recording_if_any(settings: &AppSettings, session: Option<&MediaSession>, number: &str, duration: Duration) {
        let Some(session) = session else { return };
        let Some(samples) = session.stop_recording() else {
            return;
        };
        if samples.is_empty() {
            return;
        }
        let dir = settings.recording_path.trim();
        if !settings.recording_enabled || dir.is_empty() {
            return;
        }
        let safe_number: String = number
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '-')
            .collect();
        let safe_number = if safe_number.is_empty() {
            "unknown".to_string()
        } else {
            safe_number
        };
        let secs = duration.as_secs();
        let filename = format!(
            "{safe_number} ({}m{:02}s) {}.wav",
            secs / 60,
            secs % 60,
            history::now_unix()
        );
        let path = Path::new(dir).join(filename);
        tokio::spawn(async move {
            match tokio::task::spawn_blocking(move || softphone_media::recording::write_wav(&path, &samples)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(%e, "failed to write call recording"),
                Err(e) => tracing::warn!(%e, "recording write task panicked"),
            }
        });
    }

    /// Saves the SIP settings form into `accounts[editing_account]` (or
    /// appends a new account if `editing_account` is `None`), persists the
    /// whole account list, and closes the settings window only if there's
    /// at least one valid account afterward — the window stays open (with
    /// an error) if validation fails, same as the previous single-account
    /// behavior.
    fn handle_sip_settings_save(&mut self) {
        let port: u16 = match self.sip_settings_form.port.trim().parse() {
            Ok(p) => p,
            Err(_) => {
                self.toast_error(format!("invalid port: {:?}", self.sip_settings_form.port));
                return;
            }
        };
        if self.sip_settings_form.host.trim().is_empty() || self.sip_settings_form.username.trim().is_empty() {
            self.toast_error("server and username are required".to_string());
            return;
        }
        let name = self.sip_settings_form.name.trim();
        let name = if name.is_empty() {
            format!(
                "{}@{}",
                self.sip_settings_form.username.trim(),
                self.sip_settings_form.host.trim()
            )
        } else {
            name.to_string()
        };

        let mut new_config = match self.editing_account.and_then(|i| self.accounts.get(i)) {
            Some(acc) => acc.config.clone(),
            None => SipAccountConfig::default(),
        };
        new_config.name = name;
        new_config.sip_server_host = self.sip_settings_form.host.trim().to_string();
        new_config.sip_server_port = port;
        new_config.username = self.sip_settings_form.username.trim().to_string();
        new_config.password = self.sip_settings_form.password.clone();
        new_config.transport = self.sip_settings_form.transport;
        // Force a fresh `SoftphoneCore`/registration attempt on every Save,
        // even when nothing else changed — `bridge::subscription` only
        // respawns when `Vec<SipAccountConfig>`'s `Hash` changes, and this
        // is the designated recovery path once the registration loop halts
        // on repeated auth failures (see `RegistrationStatus::Halted`).
        new_config.reg_epoch = new_config.reg_epoch.wrapping_add(1);

        match self.editing_account {
            Some(index) if index < self.accounts.len() => {
                self.accounts[index].config = new_config;
            }
            _ => {
                self.accounts.push(AccountSession::new(new_config));
                self.editing_account = Some(self.accounts.len() - 1);
                if self.accounts.len() == 1 {
                    self.selected_account = 0;
                }
            }
        }
        self.persist_accounts();
        self.toast_success("Account saved — registering");
    }

    /// Scans both hardware/virtual sinks and live app capture streams
    /// (e.g. Discord's voice-engine node) fresh, rather than relying on
    /// whatever `output_devices` happened to be loaded at boot or from the
    /// Audio settings window — the app-stream half in particular only
    /// exists while the owning app is actually listening, so it has to be
    /// re-scanned on demand, not cached.
    fn refresh_secondary_output_targets(&self) -> Task<Message> {
        Task::future(async {
            let (sinks, app_streams) = tokio::task::spawn_blocking(|| {
                let sinks = softphone_media::devices::list_output_devices().unwrap_or_else(|e| {
                    tracing::warn!(%e, "failed to list output devices");
                    Vec::new()
                });
                let app_streams = softphone_media::devices::list_app_capture_streams().unwrap_or_else(|e| {
                    tracing::warn!(%e, "failed to list app capture streams");
                    Vec::new()
                });
                (sinks, app_streams)
            })
            .await
            .unwrap_or_default();
            Message::SecondaryOutputTargetsLoaded(sinks, app_streams)
        })
    }

    /// Input counterpart to `refresh_secondary_output_targets` — see that
    /// method's doc comment for why this is re-scanned on demand rather than
    /// cached from boot/the Audio settings window.
    fn refresh_secondary_input_targets(&self) -> Task<Message> {
        Task::future(async {
            let (inputs, app_streams) = tokio::task::spawn_blocking(|| {
                let inputs = softphone_media::devices::list_input_devices().unwrap_or_else(|e| {
                    tracing::warn!(%e, "failed to list input devices");
                    Vec::new()
                });
                let app_streams = softphone_media::devices::list_app_playback_streams().unwrap_or_else(|e| {
                    tracing::warn!(%e, "failed to list app playback streams");
                    Vec::new()
                });
                (inputs, app_streams)
            })
            .await
            .unwrap_or_default();
            Message::SecondaryInputTargetsLoaded(inputs, app_streams)
        })
    }

    fn handle_audio_settings_save(&mut self) {
        let srtp = self.audio_settings_form.srtp;
        let codecs = self.audio_settings_form.codecs.clone();
        let dtmf_mode = self.audio_settings_form.dtmf_mode;
        if let Some(acc) = self.selected_mut() {
            acc.config.srtp = srtp;
            acc.config.preferred_codecs = codecs;
            acc.config.dtmf_mode = dtmf_mode;
            self.persist_accounts();
        }

        let new_audio_devices = AudioDeviceConfig {
            input_device: self.audio_settings_form.input_device.clone(),
            output_device: self.audio_settings_form.output_device.clone(),
        };
        if let Err(e) = audio_devices::save(&new_audio_devices) {
            tracing::warn!(%e, "failed to save audio device config");
        }
        self.audio_devices = new_audio_devices;
    }

    fn handle_call_state_changed(&mut self, account: usize, id: CallId, state: CallState) -> Task<Message> {
        let Some(acc) = self.accounts.get_mut(account) else {
            return Task::none();
        };
        match state {
            CallState::Ringing => {
                // Only ring back while the line is still plainly `Outgoing`
                // — guards against a stray 180 arriving after a 183 already
                // moved this call into `EarlyMedia` with real audio
                // flowing, which must not be interrupted by ringback.
                if let Some(idx) = acc.line_index_for_call(&id)
                    && matches!(acc.lines[idx], CallUiState::Outgoing { .. })
                    && let Some(player) = &self.tone_player
                {
                    player.play_ringback_tone();
                }
                Task::none()
            }
            CallState::EarlyMedia { remote } => {
                if let Some(player) = &self.tone_player {
                    player.stop_ringback();
                }
                let Some(idx) = acc.line_index_for_call(&id) else {
                    return Task::none();
                };
                if matches!(acc.lines[idx], CallUiState::EarlyMedia { .. }) {
                    // A second 183 for the same call — already building/
                    // built early media, nothing new to do.
                    return Task::none();
                }
                let CallUiState::Outgoing { number, started_at, .. } = &acc.lines[idx] else {
                    return Task::none();
                };
                let (number, started_at) = (number.clone(), *started_at);
                let Some(reserved) = acc.pending_sockets[idx].take() else {
                    return Task::none();
                };
                acc.lines[idx] = CallUiState::EarlyMedia {
                    id: id.clone(),
                    number,
                    media: None,
                    remote: remote.clone(),
                    started_at,
                };
                let capture_target = self.audio_devices.input_device.clone();
                let playback_target = self.audio_devices.output_device.clone();
                let label = format!("OxideSip Acct{} Line {} (early)", account + 1, idx + 1);
                Task::future(async move {
                    let result = MediaSession::start(
                        reserved,
                        remote.remote_addr,
                        remote.payload_type,
                        remote.telephone_event_pt,
                        capture_target,
                        playback_target,
                        label,
                    )
                    .await
                    .map(|session| Arc::new(Mutex::new(Some(session))))
                    .map_err(|e| e.to_string());
                    Message::MediaReady(account, id, result)
                })
            }
            CallState::Answered { remote, .. } => {
                let Some(idx) = acc.line_index_for_call(&id) else {
                    tracing::warn!("answered event for a call we're not tracking");
                    return Task::none();
                };
                if let Some(player) = &self.tone_player {
                    player.stop_ringback();
                    player.stop_incoming_ringtone();
                }
                if let CallUiState::Active {
                    on_hold,
                    pre_hold_output_volume,
                    output_volume,
                    muted,
                    media,
                    avatar_glow,
                    ..
                } = &mut acc.lines[idx]
                {
                    // A re-INVITE resume confirmation for an already-active
                    // call, not the initial answer — restore local audio
                    // instead of rebuilding the whole call state.
                    *on_hold = false;
                    *output_volume = *pre_hold_output_volume;
                    avatar_glow.go_mut(avatar_live(media.is_some(), false), Instant::now());
                    if let Some(session) = media {
                        session.set_mic_muted(*muted);
                        session.set_output_volume(*output_volume);
                    }
                    return Task::none();
                }

                if matches!(acc.lines[idx], CallUiState::EarlyMedia { .. }) {
                    let CallUiState::EarlyMedia {
                        number,
                        media,
                        remote: early_remote,
                        started_at,
                        ..
                    } = std::mem::replace(&mut acc.lines[idx], CallUiState::Idle)
                    else {
                        unreachable!()
                    };
                    if early_remote == remote {
                        // Same target early media was already pointed at —
                        // promote in place and reuse whatever `MediaSession`
                        // exists (possibly still `None` if its `start`
                        // future hasn't resolved yet — the in-flight
                        // `MediaReady` future still lands correctly via
                        // `media_slot`, since it targets by `CallId`, not
                        // exact enum variant). There's no cheap "retarget"
                        // method on `MediaSession`, so same-address reuse is
                        // the only path that avoids a pointless rebuild of a
                        // session that may already be carrying real audio.
                        let post_dial: VecDeque<char> = acc.pending_post_dials[idx].drain(..).collect();
                        let avatar_glow = iced::Animation::new(avatar_live(media.is_some(), false)).quick();
                        acc.lines[idx] = CallUiState::Active {
                            id: id.clone(),
                            number,
                            remote_name: None,
                            direction: history::CallDirection::Outgoing,
                            media,
                            dtmf_feedback: Vec::new(),
                            answered_at: Instant::now(),
                            input_level: 0.0,
                            output_level: 0.0,
                            muted: false,
                            output_volume: 1.0,
                            input_volume: 1.0,
                            on_hold: false,
                            pre_hold_output_volume: 1.0,
                            secondary_input_on: false,
                            transfer_input: None,
                            post_dial,
                            avatar_glow,
                        };
                        return Task::none();
                    }
                    // Rare: the final answer's target differs from what
                    // early media was already pointed at. Drop the
                    // mismatched early session — `MediaSession`'s `Drop`
                    // impl tears its PipeWire pipeline down, the same
                    // safety net `CallState::Terminated` already relies on
                    // elsewhere — and fall through to the generic build path
                    // below by resetting to `Outgoing`, re-reserving a
                    // socket since `EarlyMedia` already consumed the
                    // original one from `pending_sockets`.
                    drop(media);
                    acc.lines[idx] = CallUiState::Outgoing {
                        id: id.clone(),
                        number,
                        started_at,
                    };
                    match ReservedSocket::reserve() {
                        Ok(reserved) => acc.pending_sockets[idx] = Some(reserved),
                        Err(e) => {
                            tracing::warn!(%e, "failed to reserve socket rebuilding after early-media target change");
                            acc.lines[idx] = CallUiState::Idle;
                            return Task::none();
                        }
                    }
                }

                let Some(reserved) = acc.pending_sockets[idx].take() else {
                    tracing::warn!("call answered with no reserved local socket");
                    return Task::none();
                };
                let (number, remote_name, direction) = match &acc.lines[idx] {
                    CallUiState::Outgoing { number, .. } => (number.clone(), None, history::CallDirection::Outgoing),
                    CallUiState::Incoming { caller, .. } => (
                        caller.user.clone(),
                        caller.display_name.clone(),
                        history::CallDirection::Incoming,
                    ),
                    _ => (String::new(), None, history::CallDirection::Outgoing),
                };
                let post_dial: VecDeque<char> = acc.pending_post_dials[idx].drain(..).collect();
                let has_post_dial = !post_dial.is_empty();
                acc.lines[idx] = CallUiState::Active {
                    id: id.clone(),
                    number,
                    remote_name,
                    direction,
                    media: None,
                    dtmf_feedback: Vec::new(),
                    answered_at: Instant::now(),
                    input_level: 0.0,
                    output_level: 0.0,
                    muted: false,
                    output_volume: 1.0,
                    input_volume: 1.0,
                    on_hold: false,
                    pre_hold_output_volume: 1.0,
                    secondary_input_on: false,
                    transfer_input: None,
                    post_dial,
                    avatar_glow: iced::Animation::new(false).quick(),
                };
                let capture_target = self.audio_devices.input_device.clone();
                let playback_target = self.audio_devices.output_device.clone();
                let label = format!("OxideSip Acct{} Line {}", account + 1, idx + 1);
                let post_dial_id = id.clone();
                let media_task = Task::future(async move {
                    let result = MediaSession::start(
                        reserved,
                        remote.remote_addr,
                        remote.payload_type,
                        remote.telephone_event_pt,
                        capture_target,
                        playback_target,
                        label,
                    )
                    .await
                    .map(|session| Arc::new(Mutex::new(Some(session))))
                    .map_err(|e| e.to_string());
                    Message::MediaReady(account, id, result)
                });
                if has_post_dial {
                    // A short pause before the very first post-dial digit —
                    // gives the far end's audio path a moment to actually be
                    // up before we start feeding it DTMF.
                    Task::batch([
                        media_task,
                        Task::future(async move {
                            tokio::time::sleep(Duration::from_millis(600)).await;
                            Message::PostDialAdvance(account, post_dial_id)
                        }),
                    ])
                } else {
                    media_task
                }
            }
            CallState::Held => {
                if let Some(idx) = acc.line_index_for_call(&id)
                    && let CallUiState::Active {
                        on_hold,
                        pre_hold_output_volume,
                        output_volume,
                        media,
                        avatar_glow,
                        ..
                    } = &mut acc.lines[idx]
                {
                    *on_hold = true;
                    *pre_hold_output_volume = *output_volume;
                    avatar_glow.go_mut(false, Instant::now());
                    if let Some(session) = media {
                        session.set_mic_muted(true);
                        session.set_output_volume(0.0);
                    }
                }
                Task::none()
            }
            CallState::Rejected => {
                if let Some(idx) = acc.line_index_for_call(&id) {
                    if let CallUiState::Incoming { caller, .. } = &acc.lines[idx] {
                        Self::push_history_named(
                            &mut self.call_history,
                            caller.user.clone(),
                            caller.display_name.clone(),
                            history::CallDirection::Incoming,
                            history::CallOutcome::Rejected,
                            None,
                        );
                    }
                    acc.lines[idx] = CallUiState::Idle;
                    acc.pending_sockets[idx] = None;
                    acc.last_call_status[idx] = Some("declined".to_string());
                    if let Some(player) = &self.tone_player {
                        player.play_disconnect_tone();
                    }
                }
                Task::none()
            }
            CallState::Terminated(reason) => {
                if !reason.is_empty() {
                    tracing::info!(%reason, "call terminated");
                }
                let Some(idx) = acc.line_index_for_call(&id) else {
                    return Task::none();
                };
                // An outbound call that never got answered gets the real
                // call-progress tone that reason implies — a genuine busy
                // signal for an actual 486 from the far end, reorder/fast-
                // busy for anything else (no answer, rejected some other
                // way, network failure) — same as what a real desk phone
                // plays you. A call that *was* connected (or an incoming
                // call that just stops ringing) gets the softer generic
                // disconnect cue instead; a busy/reorder cadence there would
                // be a strange thing to hear right after an actual
                // conversation.
                let was_outgoing_unanswered = matches!(
                    acc.lines[idx],
                    CallUiState::Outgoing { .. } | CallUiState::EarlyMedia { .. }
                );
                if let Some(player) = &self.tone_player {
                    player.stop_ringback();
                    player.stop_incoming_ringtone();
                    if was_outgoing_unanswered && reason == "busy" {
                        player.play_busy_tone();
                    } else if was_outgoing_unanswered {
                        player.play_reorder_tone();
                    } else {
                        player.play_disconnect_tone();
                    }
                }
                acc.last_call_status[idx] = (!reason.is_empty()).then_some(reason);
                match &acc.lines[idx] {
                    CallUiState::Incoming { caller, .. } => {
                        Self::push_history_named(
                            &mut self.call_history,
                            caller.user.clone(),
                            caller.display_name.clone(),
                            history::CallDirection::Incoming,
                            history::CallOutcome::Missed,
                            None,
                        );
                        self.unseen_missed += 1;
                    }
                    CallUiState::Outgoing { number, .. } | CallUiState::EarlyMedia { number, .. } => {
                        Self::push_history(
                            &mut self.call_history,
                            number.clone(),
                            history::CallDirection::Outgoing,
                            history::CallOutcome::Failed,
                            None,
                        );
                    }
                    CallUiState::Active {
                        number,
                        direction,
                        answered_at,
                        media,
                        ..
                    } => {
                        Self::save_recording_if_any(&self.settings, media.as_ref(), number, answered_at.elapsed());
                        Self::push_history(
                            &mut self.call_history,
                            number.clone(),
                            *direction,
                            history::CallOutcome::Answered,
                            Some(answered_at.elapsed()),
                        );
                    }
                    CallUiState::Idle => {}
                }
                acc.unjoin_line(idx);
                // Dropping the old `CallUiState` here drops any `MediaSession`
                // inside it, whose `Drop` impl tears down the PipeWire
                // thread/tasks (a safety net, not a clean awaited join — see
                // MediaSession::stop's docs).
                acc.lines[idx] = CallUiState::Idle;
                acc.pending_sockets[idx] = None;
                // (If this call was still ringing — the caller hung up —
                // `sync_incoming_ui` drops its card and closes the window.)
                if self.compact_mode && account == self.selected_account && idx == acc.selected_idx() {
                    self.compact_mode = false;
                    return self.resize_for_compact(false);
                }
                Task::none()
            }
        }
    }

    /// `min_size` is fixed at window creation (see `boot`'s comment) to a
    /// floor that already fits both modes, so switching is just one resize
    /// request — no `min_size` juggling, no ordering race with the
    /// compositor to lose.
    ///
    /// `self.main_window_size` is updated *here*, immediately, to the
    /// requested size — not left to wait for the `WindowResized` event that
    /// confirms the compositor actually applied it. On Wayland that event
    /// can lag noticeably behind a programmatic resize request (that's the
    /// whole reason dragging the corner "fixes" things: the drag forces a
    /// fresh, correct event). In the meantime `ui_scale()` was computing
    /// its scale factor from the *stale* pre-resize size — e.g. leaving a
    /// wide, previously-maximized `main_window_size` in place right as
    /// compact mode exits back to the small main layout would peg
    /// `ui_scale()` at its 1.6x ceiling and render everything oversized
    /// into a window that hadn't visually caught up yet, which is exactly
    /// the "gigantic and ugly until I drag a corner" symptom. Setting it to
    /// our own request up front keeps `ui_scale()` correct immediately,
    /// regardless of how long the compositor takes to visually catch up —
    /// and if the compositor's own confirming event arrives shortly after
    /// with the same value, it's a harmless no-op update.
    fn resize_for_compact(&mut self, compact: bool) -> Task<Message> {
        let size = if compact {
            COMPACT_WINDOW_SIZE
        } else {
            self.normal_window_size
        };
        self.main_window_size = size;
        window::resize(self.main_window, size)
    }

    fn handle_dialpad(&mut self, digit: char) -> Task<Message> {
        let Some(acc) = self.selected_mut() else {
            return Task::none();
        };
        let idx = acc.selected_idx();
        match &mut acc.lines[idx] {
            CallUiState::Idle => {
                self.dial_input.push(digit);
                Task::none()
            }
            CallUiState::Active { dtmf_feedback, .. } => {
                dtmf_feedback.push(digit);
                acc.send_dtmf_digit(idx, digit);
                Task::none()
            }
            _ => Task::none(),
        }
    }

    fn handle_call_pressed(&mut self) -> Task<Message> {
        let account = self.selected_account;
        let Some(acc) = self.accounts.get_mut(account) else {
            return Task::none();
        };
        let idx = acc.selected_idx();
        if !matches!(acc.lines[idx], CallUiState::Idle) || self.dial_input.is_empty() {
            return Task::none();
        }
        let (number, post_dial) = split_dial_input(&self.dial_input);
        if number.is_empty() {
            self.toast_error("enter a number to call".to_string());
            return Task::none();
        }
        match ReservedSocket::reserve() {
            Ok(reserved) => {
                let local_rtp_port = reserved.local_port();
                acc.pending_sockets[idx] = Some(reserved);
                acc.pending_post_dials[idx] = post_dial;
                acc.pending_numbers[idx] = number.clone();
                let selected_line = acc.selected_line;
                acc.send_command(CoreCommand::PlaceCall {
                    line: selected_line,
                    callee: number,
                    local_rtp_port,
                });
            }
            Err(e) => self.toast_error(format!("failed to reserve local port: {e}")),
        }
        Task::none()
    }

    fn handle_answer_pressed(&mut self) -> Task<Message> {
        let account = self.selected_account;
        let Some(idx) = self.selected().map(|acc| acc.selected_idx()) else {
            return Task::none();
        };
        self.answer_line(account, idx)
    }

    /// Declines `id` (603) and stops the ringtone if nothing else rings.
    fn decline_call(&mut self, account: usize, id: CallId) -> Task<Message> {
        if let Some(acc) = self.accounts.get(account) {
            acc.send_command(CoreCommand::RejectCall {
                id,
                reason: RejectReason::Decline,
            });
        }
        // Stop ringing right away rather than on the core's `Rejected` echo
        // — unless another line is still ringing.
        if incoming::ringing_calls(self).len() <= 1 {
            self.stop_ringtone();
        }
        Task::none()
    }

    fn stop_ringtone(&mut self) {
        if self.ringtone_playing
            && let Some(player) = &self.tone_player
        {
            player.stop_incoming_ringtone();
        }
        self.ringtone_playing = false;
    }

    fn is_ringing(&self, call_id: &CallId) -> bool {
        incoming::ringing_calls(self).iter().any(|c| c.id == call_id)
    }

    /// `(account, line index)` of the call with this id, in any state.
    fn account_for_call(&self, call_id: &CallId) -> Option<(usize, usize)> {
        self.accounts
            .iter()
            .enumerate()
            .find_map(|(account, acc)| acc.line_index_for_call(call_id).map(|idx| (account, idx)))
    }

    fn notification_text(&self, account: usize, remote: &RemoteParty) -> (String, String) {
        let name = ui::components::party_name(&self.contacts, remote)
            .unwrap_or(&remote.user)
            .to_string();
        let account_name = self
            .accounts
            .get(account)
            .map(|a| {
                if a.config.name.trim().is_empty() {
                    a.config.username.clone()
                } else {
                    a.config.name.clone()
                }
            })
            .unwrap_or_default();
        let body = if name == remote.user {
            format!("Incoming call · {account_name}")
        } else {
            format!("{} · {account_name}", remote.user)
        };
        (name, body)
    }

    /// Keyboard shortcuts that no focused widget consumed. In the incoming
    /// window: Enter / A answer the newest call, Esc / D decline it, S
    /// silences the ringtone. Everywhere: Tab cycles text-input focus.
    fn handle_key(&mut self, window: window::Id, key: Key, modifiers: Modifiers) -> Task<Message> {
        use iced::keyboard::key::Named;
        if let Key::Named(Named::Tab) = key {
            return if modifiers.shift() {
                iced::widget::operation::focus_previous()
            } else {
                iced::widget::operation::focus_next()
            };
        }
        if Some(window) == self.incoming_popup && !modifiers.command() && !modifiers.alt() {
            let newest = incoming::ringing_calls(self)
                .first()
                .map(|c| (c.account, c.line_idx, c.id.clone()));
            let Some((account, idx, id)) = newest else {
                return Task::none();
            };
            return match key.as_ref() {
                Key::Named(Named::Enter) | Key::Character("a") | Key::Character("A") => self.answer_line(account, idx),
                Key::Named(Named::Escape) | Key::Character("d") | Key::Character("D") => self.decline_call(account, id),
                Key::Character("s") | Key::Character("S") => {
                    self.stop_ringtone();
                    Task::none()
                }
                _ => Task::none(),
            };
        }
        if window != self.main_window {
            return Task::none();
        }
        if modifiers.command() {
            return match key.as_ref() {
                Key::Character("1") => self.update_inner(Message::PageSelected(Page::Phone)),
                Key::Character("2") => self.update_inner(Message::PageSelected(Page::Contacts)),
                Key::Character("3") => self.update_inner(Message::PageSelected(Page::History)),
                Key::Character("4") | Key::Character(",") => self.update_inner(Message::PageSelected(Page::Settings)),
                Key::Character("k") | Key::Character("K") => match self.page {
                    Page::Contacts => iced::widget::operation::focus(SEARCH_INPUT_ID),
                    _ => {
                        self.navigate(Page::Phone);
                        iced::widget::operation::focus(DIAL_INPUT_ID)
                    }
                },
                Key::Character("m") | Key::Character("M") => self.update_inner(Message::MuteToggled),
                Key::Character("h") | Key::Character("H") => self.update_inner(Message::HoldToggled),
                Key::Character("t") | Key::Character("T") => self.update_inner(Message::TransferPanelToggled),
                Key::Character("b") | Key::Character("B") => self.update_inner(Message::SidebarToggled),
                Key::Character("c") | Key::Character("C") if modifiers.shift() => {
                    self.update_inner(Message::CompactToggled)
                }
                Key::Character("q") | Key::Character("Q") => {
                    self.update_inner(Message::CloseRequested(self.main_window))
                }
                _ => Task::none(),
            };
        }
        if modifiers.alt() {
            return Task::none();
        }
        let selected_state = self.selected().map(|acc| match acc.selected_call() {
            CallUiState::Idle => 0,
            CallUiState::Incoming { .. } => 1,
            CallUiState::Active { .. } => 2,
            _ => 3,
        });
        match key.as_ref() {
            Key::Named(Named::Escape) => {
                if self.quit_prompt {
                    self.quit_prompt = false;
                } else if self.contact_form.is_some() {
                    self.contact_form = None;
                } else if self.history_context_menu.is_some() {
                    self.history_context_menu = None;
                } else if selected_state == Some(1) {
                    return self.update_inner(Message::RejectPressed);
                } else if matches!(selected_state, Some(2) | Some(3)) {
                    if self.last_escape.is_some() {
                        self.last_escape = None;
                        return self.handle_hang_up_pressed();
                    }
                    self.last_escape = Some(Instant::now());
                    self.push_toast(ToastKind::Warning, "Press Esc again to hang up", None);
                }
                Task::none()
            }
            Key::Named(Named::Enter) if selected_state == Some(1) => self.update_inner(Message::AnswerPressed),
            Key::Named(Named::Enter) if selected_state == Some(0) && self.page == Page::Phone => {
                self.handle_call_pressed()
            }
            Key::Named(Named::Backspace) if self.page == Page::Phone && selected_state == Some(0) => {
                self.dial_input.pop();
                Task::none()
            }
            Key::Character(c) if self.page == Page::Phone && c.len() == 1 => {
                let ch = c.chars().next().unwrap_or(' ');
                if ch.is_ascii_digit() || ch == '*' || ch == '#' || ch == '+' {
                    return self.update_inner(Message::DialpadPressed(ch));
                }
                Task::none()
            }
            _ => Task::none(),
        }
    }

    /// Switches the main window to `page`.
    fn navigate(&mut self, page: Page) {
        self.page = page;
        self.nav_animations.select(page);
        if page == Page::History {
            self.unseen_missed = 0;
        }
        if self.ui_state.page != page {
            self.ui_state.page = page;
            self.mark_ui_state_dirty();
        }
    }

    fn mark_ui_state_dirty(&mut self) {
        if self.ui_state_dirty_at.is_none() {
            self.ui_state_dirty_at = Some(Instant::now());
        }
    }

    fn flush_ui_state(&mut self) {
        self.ui_state_dirty_at = None;
        if let Err(e) = ui_state::save(&self.ui_state) {
            tracing::warn!(%e, "failed to save ui state");
        }
    }

    fn any_call_in_progress(&self) -> bool {
        self.accounts
            .iter()
            .any(|acc| acc.lines.iter().any(|line| !matches!(line, CallUiState::Idle)))
    }

    /// Hangs up / declines every call on every account (before quitting).
    fn hang_up_everything(&mut self) {
        for acc in &self.accounts {
            for line in &acc.lines {
                match line {
                    CallUiState::Incoming { id, .. } => acc.send_command(CoreCommand::RejectCall {
                        id: id.clone(),
                        reason: RejectReason::Busy,
                    }),
                    CallUiState::Outgoing { id, .. }
                    | CallUiState::EarlyMedia { id, .. }
                    | CallUiState::Active { id, .. } => acc.send_command(CoreCommand::HangUp(id.clone())),
                    CallUiState::Idle => {}
                }
            }
        }
        self.stop_ringtone();
    }

    /// How long a call may ring unanswered before it is refused as busy and
    /// logged as missed — guards against an INVITE whose CANCEL never
    /// reached us ringing forever.
    const RING_TIMEOUT: Duration = Duration::from_secs(60);

    /// Reconciles the incoming-call window, ringtone, attention requests
    /// and desktop notifications with what is actually ringing. Idempotent
    /// and cheap, so it runs after *every* `update` (including each 80 ms
    /// tick, which is what drives the ring timeout and re-asserted
    /// attention).
    fn sync_incoming_ui(&mut self) -> Task<Message> {
        let mut tasks = Vec::new();

        // Ring timeout.
        let expired: Vec<(usize, usize, CallId)> = incoming::ringing_calls(self)
            .iter()
            .filter(|c| c.ringing_since.elapsed() >= Self::RING_TIMEOUT)
            .map(|c| (c.account, c.line_idx, c.id.clone()))
            .collect();
        for (account, idx, id) in expired {
            if let Some(acc) = self.accounts.get_mut(account) {
                if let CallUiState::Incoming { caller, .. } = &acc.lines[idx] {
                    Self::push_history_named(
                        &mut self.call_history,
                        caller.user.clone(),
                        caller.display_name.clone(),
                        history::CallDirection::Incoming,
                        history::CallOutcome::Missed,
                        None,
                    );
                    self.unseen_missed += 1;
                }
                acc.send_command(CoreCommand::RejectCall {
                    id,
                    reason: RejectReason::Busy,
                });
                acc.lines[idx] = CallUiState::Idle;
                acc.last_call_status[idx] = Some("missed — no answer".to_string());
            }
        }

        let ringing: Vec<CallId> = incoming::ringing_calls(self).iter().map(|c| c.id.clone()).collect();

        // Notifications for calls that stopped ringing.
        let stale: Vec<CallId> = self
            .notifications
            .keys()
            .filter(|id| !ringing.contains(id))
            .cloned()
            .collect();
        for id in stale {
            if let Some(nid) = self.notifications.remove(&id) {
                notify::dismiss(nid);
            }
        }

        if ringing.is_empty() {
            self.stop_ringtone();
            self.last_attention = None;
            self.incoming_popup_cards = 0;
            if let Some(id) = self.incoming_popup.take() {
                tasks.push(window::close(id));
            }
            return Task::batch(tasks);
        }

        let cards = ringing.len();
        match self.incoming_popup {
            None => {
                let (id, open) = window::open(incoming::window_settings(cards));
                self.incoming_popup = Some(id);
                self.incoming_popup_cards = cards;
                self.last_attention = Some(Instant::now());
                tasks.push(open.discard());
                tasks.push(window::gain_focus(id));
                tasks.push(window::request_user_attention(
                    id,
                    Some(window::UserAttention::Critical),
                ));
            }
            Some(id) => {
                if cards != self.incoming_popup_cards {
                    self.incoming_popup_cards = cards;
                    tasks.push(window::resize(id, incoming::window_size(cards)));
                }
                let stale = self
                    .last_attention
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(4));
                if stale {
                    self.last_attention = Some(Instant::now());
                    tasks.push(window::request_user_attention(
                        id,
                        Some(window::UserAttention::Critical),
                    ));
                }
            }
        }
        Task::batch(tasks)
    }

    /// Reserves a socket and sends `AnswerCall` for line `idx` on `account`
    /// — factored out of `handle_answer_pressed` so auto-answer (see
    /// `Message::Core(_, CoreEvent::IncomingCall)`) can answer whichever
    /// line just started ringing, not only the currently-selected one.
    fn answer_line(&mut self, account: usize, idx: usize) -> Task<Message> {
        let Some(acc) = self.accounts.get_mut(account) else {
            return Task::none();
        };
        let CallUiState::Incoming { id, .. } = &acc.lines[idx] else {
            return Task::none();
        };
        let id = id.clone();
        match ReservedSocket::reserve() {
            Ok(reserved) => {
                let local_rtp_port = reserved.local_port();
                acc.pending_sockets[idx] = Some(reserved);
                acc.send_command(CoreCommand::AnswerCall { id, local_rtp_port });
            }
            Err(e) => self.toast_error(format!("failed to reserve local port: {e}")),
        }
        Task::none()
    }

    /// Hangs up *optimistically*: the line goes back to idle the instant
    /// you press the button, not once the core confirms the BYE/CANCEL
    /// actually landed — a real phone gives you that feedback immediately
    /// too, it doesn't make you wait on the network round trip.
    /// `CoreCommand::HangUp` still goes out (fire-and-forget over the
    /// command channel); if a `CallStateChanged::Terminated` for this call
    /// arrives later anyway, `line_index_for_call` won't find it (the line's
    /// already idle) and it's silently ignored.
    fn handle_hang_up_pressed(&mut self) -> Task<Message> {
        // A direct field expression, not `self.selected_mut()` — that
        // method call opaquely borrows the whole `self` for as long as
        // `acc` (or anything derived from it, like `media` below) is
        // live, which would conflict with the separate `&self.settings`
        // borrow `save_recording_if_any` needs at the same statement. See
        // `push_history`'s doc comment for the same reasoning.
        let Some(acc) = self.accounts.get_mut(self.selected_account) else {
            return Task::none();
        };
        let idx = acc.selected_idx();
        let id = match &acc.lines[idx] {
            CallUiState::Active { id, .. } | CallUiState::Outgoing { id, .. } | CallUiState::EarlyMedia { id, .. } => {
                Some(id.clone())
            }
            _ => None,
        };
        let Some(id) = id else { return Task::none() };
        acc.send_command(CoreCommand::HangUp(id));
        acc.unjoin_line(idx);
        match &acc.lines[idx] {
            CallUiState::Active {
                number,
                direction,
                answered_at,
                media,
                ..
            } => {
                let (number, direction, answered_at) = (number.clone(), *direction, *answered_at);
                Self::save_recording_if_any(&self.settings, media.as_ref(), &number, answered_at.elapsed());
                Self::push_history(
                    &mut self.call_history,
                    number,
                    direction,
                    history::CallOutcome::Answered,
                    Some(answered_at.elapsed()),
                );
            }
            CallUiState::Outgoing { number, .. } | CallUiState::EarlyMedia { number, .. } => {
                let number = number.clone();
                Self::push_history(
                    &mut self.call_history,
                    number,
                    history::CallDirection::Outgoing,
                    history::CallOutcome::Failed,
                    None,
                );
            }
            _ => {}
        }
        let Some(acc) = self.selected_mut() else {
            return Task::none();
        };
        let idx = acc.selected_idx();
        acc.lines[idx] = CallUiState::Idle;
        acc.pending_sockets[idx] = None;
        acc.last_call_status[idx] = Some("hung up".to_string());
        if let Some(player) = &self.tone_player {
            player.stop_ringback();
            player.stop_incoming_ringtone();
            player.play_disconnect_tone();
        }
        if self.compact_mode {
            self.compact_mode = false;
            return self.resize_for_compact(false);
        }
        Task::none()
    }
}

/// Keeps only characters meaningful to actually dial: digits, `*`/`#`
/// (real feature-code/DTMF characters), and a leading `+` (E.164 country
/// prefix). Strips everything a human might type when formatting a number
/// for readability — spaces, parens, hyphens, dots — none of which are
/// legal in a SIP URI user-part, so passing them through as-is (the
/// previous behavior) made "any format" numbers fail to dial at all.
fn sanitize_dial_chars(input: &str, allow_leading_plus: bool) -> String {
    input
        .chars()
        .enumerate()
        .filter(|(i, c)| c.is_ascii_digit() || *c == '*' || *c == '#' || (allow_leading_plus && *c == '+' && *i == 0))
        .map(|(_, c)| c)
        .collect()
}

/// Splits a raw dial-field string into `(number, post_dial)`. Everything
/// from the first `,` onward is treated as a post-dial sequence — each `,`
/// is a pause, each digit after it is sent as DTMF once the call connects
/// (see `Message::PostDialAdvance`) — matching how real phones handle
/// "dial this, then wait, then send this" (e.g. an extension or a
/// conference PIN behind an auto-attendant).
fn split_dial_input(input: &str) -> (String, String) {
    match input.find(',') {
        Some(idx) => {
            let number = sanitize_dial_chars(&input[..idx], true);
            let post_dial: String = input[idx..]
                .chars()
                .filter(|c| *c == ',' || c.is_ascii_digit() || *c == '*' || *c == '#')
                .collect();
            (number, post_dial)
        }
        None => (sanitize_dial_chars(input, true), String::new()),
    }
}

/// Opens a URL or folder with the desktop's default handler.
fn open_external(target: &str) {
    if let Err(e) = std::process::Command::new("xdg-open").arg(target).spawn() {
        tracing::warn!(%e, target, "xdg-open failed");
    }
}
