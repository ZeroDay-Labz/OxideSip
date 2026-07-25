use crate::app_settings::{self, AppSettings};
use crate::audio_devices::{self, AudioDeviceConfig};
use crate::contacts::{self, Contact};
use crate::history;
use crate::{bridge, view};
use iced::{window, Element, Subscription, Task};
use softphone_core::config::{PreferredCodec, SipAccountConfig, SipTransport, PREFERRED_CODECS};
use softphone_core::events::{CallId, CallState, CoreCommand, CoreEvent, RemoteMediaInfo};
use softphone_media::{AudioDevice, DtmfTonePlayer, MediaSession, ReservedSocket};
use std::collections::VecDeque;
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
    Failed { reason: String },
}

pub enum CallUiState {
    Idle,
    Incoming {
        id: CallId,
        caller: String,
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
    Active {
        id: CallId,
        number: String,
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
        /// `None` = transfer panel collapsed; `Some(text)` = panel open with
        /// the in-progress target number.
        transfer_input: Option<String>,
        /// Remaining post-dial digits/pauses (`,` = pause) queued from a
        /// comma in the original dial string — drained one step at a time
        /// by `Message::PostDialAdvance` once the call is answered.
        post_dial: VecDeque<char>,
    },
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dialer,
    Contacts,
    History,
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
    pub forwarding_enabled: bool,
    pub forwarding_number: String,
    pub deny_list: Vec<String>,
    pub deny_list_input: String,
    pub recording_enabled: bool,
    pub recording_path: String,
    pub secondary_output_target: Option<String>,
}

impl SettingsForm {
    fn from_settings(settings: &AppSettings) -> Self {
        SettingsForm {
            dnd: settings.dnd,
            auto_answer: settings.auto_answer,
            forwarding_enabled: settings.forwarding_enabled,
            forwarding_number: settings.forwarding_number.clone(),
            deny_list: settings.deny_list.clone(),
            deny_list_input: String::new(),
            recording_enabled: settings.recording_enabled,
            recording_path: settings.recording_path.clone(),
            secondary_output_target: settings.secondary_output_target.clone(),
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
}

impl ContactSort {
    fn next(self) -> Self {
        match self {
            ContactSort::NameAsc => ContactSort::NameDesc,
            ContactSort::NameDesc => ContactSort::NameAsc,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            ContactSort::NameAsc => "A-Z",
            ContactSort::NameDesc => "Z-A",
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

    fn line_index_for_call(&self, call_id: &CallId) -> Option<usize> {
        self.lines.iter().position(|line| match line {
            CallUiState::Incoming { id, .. }
            | CallUiState::Outgoing { id, .. }
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
            if let CallUiState::Active { media: Some(session), .. } = &self.lines[partner_idx] {
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
        let CallUiState::Active { media: Some(a_media), .. } = &self.lines[a_idx] else {
            return None;
        };
        let CallUiState::Active { media: Some(b_media), .. } = &self.lines[b_idx] else {
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
    pub(crate) screen: Screen,
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
    pub(crate) error: Option<String>,
    main_window: window::Id,
    main_window_size: iced::Size,
    /// Whether the main window is currently shrunk to the compact
    /// floating call bar. A window-level concept, not tied to any one
    /// line's data — it just reflects whatever line is selected.
    pub(crate) compact_mode: bool,
    sip_settings_window: Option<window::Id>,
    audio_settings_window: Option<window::Id>,
    settings_window: Option<window::Id>,
    pub(crate) settings: AppSettings,
    pub(crate) settings_form: SettingsForm,
    pub(crate) call_history: Vec<history::HistoryEntry>,
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
    DialHistoryToggled,
    DialHistorySelected(String),
    DialpadPressed(char),
    CallPressed,
    AnswerPressed,
    RejectPressed,
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
    AudioSettingsSavePressed,
    AudioSettingsCancelPressed,
    DevicesLoaded(Vec<AudioDevice>, Vec<AudioDevice>),
    OpenSettings,
    SettingsDndToggled(bool),
    SettingsAutoAnswerToggled(bool),
    SettingsForwardingToggled(bool),
    SettingsForwardingNumberChanged(String),
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
    SettingsSavePressed,
    SettingsCancelPressed,
    Tick,
    MuteToggled,
    OutputVolumeChanged(f32),
    InputVolumeChanged(f32),
    WindowClosed(window::Id),
    WindowResized(window::Id, iced::Size),
    TabSelected(Screen),
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
    ContactsIoPathChanged(String),
    ContactsImportPressed,
    ContactsExportPressed,
    BrowseContactsImportPressed,
    BrowseContactsExportPressed,
    TonePlayerReady(Result<Arc<DtmfTonePlayer>, String>),
}

/// Legacy single-account migration lives in `main.rs` (it needs to run once
/// at process startup, before `App` exists) — this is just where the
/// resulting multi-account file is persisted from then on.
const ACCOUNTS_PATH: &str = "./accounts.toml";

const MAIN_WINDOW_SIZE: iced::Size = iced::Size::new(380.0, 640.0);
const COMPACT_WINDOW_SIZE: iced::Size = iced::Size::new(320.0, 190.0);
const COMPACT_WINDOW_MIN_SIZE: iced::Size = iced::Size::new(280.0, 170.0);
/// How long a line stays "open" (dial tone played, ready to dial) with
/// nothing dialed before it times out on its own — see
/// `App::schedule_dial_timeout`/`Message::LineIdleTimeout`. Set just a
/// couple of seconds past the dial tone's own ~6s runtime
/// (`tone.rs::DIAL_TONE_DURATION_MS`) so the tone and the reorder-tone
/// timeout read as one continuous sequence — tone, brief silence, reorder —
/// instead of a short blip followed by a long dead gap.
const DIAL_TIMEOUT: Duration = Duration::from_secs(8);
const SIP_SETTINGS_WINDOW_SIZE: iced::Size = iced::Size::new(560.0, 560.0);
const SIP_SETTINGS_WINDOW_MIN_SIZE: iced::Size = iced::Size::new(440.0, 420.0);
const AUDIO_SETTINGS_WINDOW_SIZE: iced::Size = iced::Size::new(400.0, 560.0);
const AUDIO_SETTINGS_WINDOW_MIN_SIZE: iced::Size = iced::Size::new(360.0, 420.0);
const SETTINGS_WINDOW_SIZE: iced::Size = iced::Size::new(420.0, 600.0);
const SETTINGS_WINDOW_MIN_SIZE: iced::Size = iced::Size::new(360.0, 440.0);
/// Tuned to feel like a real-time meter without redrawing so often it wastes
/// cycles — noticeably snappier than a 200ms poll (which reads as laggy).
const TICK_INTERVAL: Duration = Duration::from_millis(80);

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
        let (main_window, open_task) = window::open(window::Settings {
            size: MAIN_WINDOW_SIZE,
            min_size: Some(COMPACT_WINDOW_MIN_SIZE),
            resizable: true,
            position: window::Position::Centered,
            ..Default::default()
        });

        let audio_devices = audio_devices::load();
        let settings = app_settings::load();
        let needs_setup = accounts.is_empty();
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
            screen: Screen::Dialer,
            dial_input: String::new(),
            dial_history_open: false,
            sip_settings_form,
            editing_account: Some(0),
            audio_settings_form,
            audio_devices,
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            app_capture_streams: Vec::new(),
            error: None,
            main_window,
            main_window_size: MAIN_WINDOW_SIZE,
            compact_mode: false,
            sip_settings_window: None,
            audio_settings_window: None,
            settings_window: None,
            settings_form: SettingsForm::from_settings(&settings),
            settings,
            call_history: history::load(),
            contacts: contacts::load(),
            contact_filter: String::new(),
            contact_form: None,
            contact_sort: ContactSort::default(),
            contacts_io_path: "./contacts_export.json".to_string(),
            contacts_io_status: None,
            tone_player: None,
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

        let mut startup = vec![open_task.discard(), tone_task];
        if needs_setup {
            startup.push(Task::done(Message::OpenSipSettings));
        }
        (app, Task::batch(startup))
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let configs: Vec<SipAccountConfig> = self.accounts.iter().map(|a| a.config.clone()).collect();
        Subscription::batch([
            bridge::subscription(&configs),
            iced::time::every(TICK_INTERVAL).map(|_| Message::Tick),
            window::close_events().map(Message::WindowClosed),
            window::resize_events().map(|(id, size)| Message::WindowResized(id, size)),
        ])
    }

    /// A multiplier applied to key font sizes/paddings/circle diameters in
    /// `view.rs` so the UI scales up on a wider window instead of just
    /// leaving extra empty margin — clamped to a sane range so it can't make
    /// text illegibly tiny or comically huge at the window's resize limits.
    pub(crate) fn ui_scale(&self) -> f32 {
        (self.main_window_size.width / MAIN_WINDOW_SIZE.width).clamp(0.85, 1.6)
    }

    pub(crate) fn selected(&self) -> Option<&AccountSession> {
        self.accounts.get(self.selected_account)
    }

    fn selected_mut(&mut self) -> Option<&mut AccountSession> {
        self.accounts.get_mut(self.selected_account)
    }

    pub fn view(&self, window: window::Id) -> Element<'_, Message> {
        if Some(window) == self.sip_settings_window {
            view::sip_settings_window_view(self)
        } else if Some(window) == self.audio_settings_window {
            view::audio_settings_window_view(self)
        } else if Some(window) == self.settings_window {
            view::settings_window_view(self)
        } else {
            view::main_view(self)
        }
    }

    pub fn title(&self, window: window::Id) -> String {
        if Some(window) == self.sip_settings_window {
            "OxideSip — SIP Setup".to_string()
        } else if Some(window) == self.audio_settings_window {
            "OxideSip — Audio & Codecs".to_string()
        } else if Some(window) == self.settings_window {
            "OxideSip — Settings".to_string()
        } else {
            "OxideSip".to_string()
        }
    }

    pub fn theme(&self, _window: window::Id) -> iced::Theme {
        crate::theme::theme()
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
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

            Message::Core(account, CoreEvent::IncomingCall { id, line, remote, offer }) => {
                let idx = line_idx(line);
                // DND and the deny list are both checked *before* the line
                // ever shows as ringing — this call never occupies a line
                // or shows up as "missed," it's rejected outright, same as
                // a real phone's call-blocking behaves.
                let denied = self.settings.dnd
                    || self
                        .settings
                        .deny_list
                        .iter()
                        .any(|entry| !entry.trim().is_empty() && remote.contains(entry.trim()));
                if denied {
                    if let Some(acc) = self.accounts.get(account) {
                        acc.send_command(CoreCommand::RejectCall(id));
                    }
                    Self::push_history(
                        &mut self.call_history,
                        remote,
                        history::CallDirection::Incoming,
                        history::CallOutcome::Rejected,
                        None,
                    );
                    return Task::none();
                }
                if let Some(acc) = self.accounts.get_mut(account)
                    && matches!(acc.lines[idx], CallUiState::Idle)
                {
                    acc.lines[idx] = CallUiState::Incoming {
                        id,
                        caller: remote,
                        offer,
                        ringing_since: Instant::now(),
                    };
                    acc.last_call_status[idx] = None;
                    if self.settings.auto_answer {
                        return self.answer_line(account, idx);
                    }
                }
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
                self.error = Some(reason);
                if let Some(acc) = self.accounts.get_mut(account) {
                    acc.lines[idx] = CallUiState::Idle;
                    acc.pending_sockets[idx] = None;
                }
                Task::none()
            }
            Message::Core(_, CoreEvent::DtmfResult { ok, digit, .. }) => {
                if !ok {
                    tracing::warn!(%digit, "dtmf send failed");
                }
                Task::none()
            }
            Message::Core(_, CoreEvent::TransferResult { ok, .. }) => {
                if !ok {
                    self.error = Some("call transfer failed".to_string());
                }
                Task::none()
            }

            Message::Core(account, CoreEvent::CallStateChanged { id, state }) => {
                self.handle_call_state_changed(account, id, state)
            }

            Message::DialInputChanged(s) => {
                // Play a tone for whatever digits were just typed/pasted
                // (not just clicked on the dialpad) — a real phone gives
                // audible feedback for typed digits too.
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
                self.handle_dialpad(digit)
            }
            Message::CallPressed => self.handle_call_pressed(),
            Message::AnswerPressed => self.handle_answer_pressed(),
            Message::RejectPressed => {
                if let Some(acc) = self.selected() {
                    let idx = acc.selected_idx();
                    if let CallUiState::Incoming { id, .. } = &acc.lines[idx] {
                        acc.send_command(CoreCommand::RejectCall(id.clone()));
                    }
                }
                Task::none()
            }
            Message::HangUpPressed => self.handle_hang_up_pressed(),

            Message::MediaReady(account, id, Ok(session_holder)) => {
                let session = session_holder.lock().unwrap().take();
                if let Some(acc) = self.accounts.get_mut(account)
                    && let Some(idx) = acc.line_index_for_call(&id)
                    && let CallUiState::Active {
                        id: current, media, ..
                    } = &mut acc.lines[idx]
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
                    }
                    *media = session;
                }
                // else: call already ended before the pipeline finished
                // starting; `session` is dropped here, tearing itself down.
                Task::none()
            }
            Message::MediaReady(account, id, Err(reason)) => {
                tracing::warn!(%reason, "failed to start media session, hanging up");
                self.error = Some(format!("audio pipeline failed: {reason}"));
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
                self.error = None;
                if let Some(id) = self.sip_settings_window {
                    return window::gain_focus(id);
                }
                let (id, open_task) = window::open(window::Settings {
                    size: SIP_SETTINGS_WINDOW_SIZE,
                    min_size: Some(SIP_SETTINGS_WINDOW_MIN_SIZE),
                    resizable: true,
                    position: window::Position::Centered,
                    ..Default::default()
                });
                self.sip_settings_window = Some(id);
                open_task.discard()
            }
            Message::OpenAudioSettings => {
                self.audio_settings_form = self
                    .selected()
                    .map(|a| AudioSettingsForm::from_config(&a.config, &self.audio_devices))
                    .unwrap_or_else(|| {
                        AudioSettingsForm::from_config(&SipAccountConfig::default(), &self.audio_devices)
                    });
                self.error = None;
                if let Some(id) = self.audio_settings_window {
                    return window::gain_focus(id);
                }
                let (id, open_task) = window::open(window::Settings {
                    size: AUDIO_SETTINGS_WINDOW_SIZE,
                    min_size: Some(AUDIO_SETTINGS_WINDOW_MIN_SIZE),
                    resizable: true,
                    position: window::Position::Centered,
                    ..Default::default()
                });
                self.audio_settings_window = Some(id);
                let device_task = Task::future(async {
                    let (inputs, outputs) = tokio::task::spawn_blocking(|| {
                        let inputs = softphone_media::devices::list_input_devices()
                            .unwrap_or_else(|e| {
                                tracing::warn!(%e, "failed to list input devices");
                                Vec::new()
                            });
                        let outputs = softphone_media::devices::list_output_devices()
                            .unwrap_or_else(|e| {
                                tracing::warn!(%e, "failed to list output devices");
                                Vec::new()
                            });
                        (inputs, outputs)
                    })
                    .await
                    .unwrap_or_default();
                    Message::DevicesLoaded(inputs, outputs)
                });
                Task::batch([open_task.discard(), device_task])
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
                self.error = None;
                if let Some(id) = self.sip_settings_window.take() {
                    return window::close(id);
                }
                Task::none()
            }
            Message::SelectAccountForEditing(index) => {
                if let Some(acc) = self.accounts.get(index) {
                    self.editing_account = Some(index);
                    self.sip_settings_form = SipSettingsForm::from_config(&acc.config);
                    self.error = None;
                }
                Task::none()
            }
            Message::AddAccountPressed => {
                self.editing_account = None;
                self.sip_settings_form = SipSettingsForm::default();
                self.error = None;
                Task::none()
            }
            Message::DeleteAccountPressed(index) => {
                if index < self.accounts.len() {
                    self.accounts.remove(index);
                    if self.selected_account >= self.accounts.len() {
                        self.selected_account = self.accounts.len().saturating_sub(1);
                    }
                    if self.editing_account == Some(index) {
                        self.editing_account = self.accounts.is_empty().then_some(0).or(Some(0));
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
                self.error = None;
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
            Message::AudioSettingsSavePressed => {
                self.handle_audio_settings_save();
                if let Some(id) = self.audio_settings_window.take() {
                    return window::close(id);
                }
                Task::none()
            }
            Message::AudioSettingsCancelPressed => {
                self.error = None;
                if let Some(id) = self.audio_settings_window.take() {
                    return window::close(id);
                }
                Task::none()
            }
            Message::OpenSettings => {
                self.settings_form = SettingsForm::from_settings(&self.settings);
                self.error = None;
                let window_task = if let Some(id) = self.settings_window {
                    window::gain_focus(id)
                } else {
                    let (id, open_task) = window::open(window::Settings {
                        size: SETTINGS_WINDOW_SIZE,
                        min_size: Some(SETTINGS_WINDOW_MIN_SIZE),
                        resizable: true,
                        position: window::Position::Centered,
                        ..Default::default()
                    });
                    self.settings_window = Some(id);
                    open_task.discard()
                };
                Task::batch([window_task, self.refresh_secondary_output_targets()])
            }
            Message::RefreshSecondaryOutputTargets => self.refresh_secondary_output_targets(),
            Message::SecondaryOutputTargetsLoaded(sinks, app_streams) => {
                self.output_devices = sinks;
                self.app_capture_streams = app_streams;
                Task::none()
            }
            Message::SettingsDndToggled(enabled) => {
                self.settings_form.dnd = enabled;
                Task::none()
            }
            Message::SettingsAutoAnswerToggled(enabled) => {
                self.settings_form.auto_answer = enabled;
                Task::none()
            }
            Message::SettingsForwardingToggled(enabled) => {
                self.settings_form.forwarding_enabled = enabled;
                Task::none()
            }
            Message::SettingsForwardingNumberChanged(s) => {
                self.settings_form.forwarding_number = s;
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
            Message::SettingsSavePressed => {
                self.settings = AppSettings {
                    dnd: self.settings_form.dnd,
                    auto_answer: self.settings_form.auto_answer,
                    forwarding_enabled: self.settings_form.forwarding_enabled,
                    forwarding_number: self.settings_form.forwarding_number.trim().to_string(),
                    deny_list: self.settings_form.deny_list.clone(),
                    recording_enabled: self.settings_form.recording_enabled,
                    recording_path: self.settings_form.recording_path.trim().to_string(),
                    secondary_output_target: self.settings_form.secondary_output_target.clone(),
                };
                if let Err(e) = app_settings::save(&self.settings) {
                    tracing::warn!(%e, "failed to save settings");
                }
                self.error = None;
                if let Some(id) = self.settings_window.take() {
                    return window::close(id);
                }
                Task::none()
            }
            Message::SettingsCancelPressed => {
                self.error = None;
                if let Some(id) = self.settings_window.take() {
                    return window::close(id);
                }
                Task::none()
            }
            Message::DevicesLoaded(inputs, outputs) => {
                self.input_devices = inputs;
                self.output_devices = outputs;
                Task::none()
            }
            Message::WindowClosed(id) => {
                if Some(id) == self.sip_settings_window {
                    self.sip_settings_window = None;
                    Task::none()
                } else if Some(id) == self.audio_settings_window {
                    self.audio_settings_window = None;
                    Task::none()
                } else if Some(id) == self.settings_window {
                    self.settings_window = None;
                    Task::none()
                } else if id == self.main_window {
                    iced::exit()
                } else {
                    Task::none()
                }
            }
            Message::WindowResized(id, size) => {
                if id == self.main_window {
                    self.main_window_size = size;
                }
                Task::none()
            }

            Message::Tick => {
                if let Some(acc) = self.selected_mut() {
                    let idx = acc.selected_idx();
                    if let CallUiState::Active {
                        media: Some(session),
                        input_level,
                        output_level,
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
                        media,
                        input_volume,
                        ..
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

            Message::TabSelected(screen) => {
                self.screen = screen;
                Task::none()
            }
            Message::DialNumber(number) => {
                let Some(acc) = self.selected() else { return Task::none() };
                let idx = acc.selected_idx();
                if !matches!(acc.lines[idx], CallUiState::Idle) {
                    self.error = Some("selected line is busy — pick a free line first".to_string());
                    return Task::none();
                }
                self.dial_input = number;
                self.screen = Screen::Dialer;
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
                        self.error = Some("can't hold a joined call — split it first".to_string());
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
                    if let CallUiState::Active {
                        id, transfer_input, ..
                    } = &mut acc.lines[idx]
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
                    post_dial
                        .pop_front()
                        .map(|next| (next, !post_dial.is_empty()))
                } else {
                    None
                };
                let Some((next, has_more)) = step else {
                    return Task::none();
                };
                if next == ',' {
                    // A pause: nothing to send yet, just wait and advance.
                    let call_id = id.clone();
                    return Task::future(async move {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        Message::PostDialAdvance(account, call_id)
                    });
                }
                if let CallUiState::Active { dtmf_feedback, .. } = &mut acc.lines[idx] {
                    dtmf_feedback.push(next);
                }
                if let Some(player) = &self.tone_player {
                    player.play(next);
                }
                acc.send_command(CoreCommand::SendDtmf {
                    id: id.clone(),
                    digit: next,
                });
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
                let Some(acc) = self.selected_mut() else { return Task::none() };
                if acc.pending_join.is_some() {
                    acc.pending_join = None;
                    return Task::none();
                }
                let idx = acc.selected_idx();
                if !matches!(acc.lines[idx], CallUiState::Active { .. }) || acc.joined[idx].is_some() {
                    return Task::none();
                }
                let selected_line = acc.selected_line;
                let has_other_active = (1..=LINE_COUNT as u8).any(|l| {
                    l != selected_line && matches!(acc.lines[line_idx(l)], CallUiState::Active { .. })
                });
                if has_other_active {
                    acc.pending_join = Some(selected_line);
                } else {
                    self.error = Some("no other active call to join".to_string());
                }
                Task::none()
            }
            Message::SplitCallPressed => {
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
                let Some(acc) = self.selected() else { return Task::none() };
                let selected_line = acc.selected_line;
                let free = (1..=LINE_COUNT as u8).find(|&l| {
                    l != selected_line && matches!(acc.lines[line_idx(l)], CallUiState::Idle)
                });
                match free {
                    Some(line) => self.select_line(line),
                    None => {
                        self.error = Some("all lines are busy".to_string());
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
                Task::none()
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
            Message::ContactsIoPathChanged(path) => {
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
                    Message::ContactsIoPathChanged(
                        file.map(|f| f.path().display().to_string()).unwrap_or(current),
                    )
                })
            }
            Message::BrowseContactsExportPressed => {
                let current = self.contacts_io_path.clone();
                Task::future(async move {
                    let file = rfd::AsyncFileDialog::new()
                        .set_title("Choose where to save the contacts export")
                        .add_filter("JSON", &["json"])
                        .set_file_name("contacts_export.json")
                        .save_file()
                        .await;
                    Message::ContactsIoPathChanged(
                        file.map(|f| f.path().display().to_string()).unwrap_or(current),
                    )
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
                self.error = None;
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
        self.error = None;
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
        let Some(acc) = self.selected_mut() else { return Task::none() };
        match acc.complete_join(a, b) {
            Ok(()) => self.error = None,
            Err(e) => self.error = Some(e.to_string()),
        }
        Task::none()
    }

    fn persist_contacts(&mut self) {
        contacts::save(&self.contacts);
        self.contacts = contacts::load();
    }

    fn persist_accounts(&self) {
        let configs: Vec<SipAccountConfig> = self.accounts.iter().map(|a| a.config.clone()).collect();
        if let Err(e) = softphone_core::config::save_accounts(Path::new(ACCOUNTS_PATH), &configs) {
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
        let entry = history::HistoryEntry {
            number,
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
    fn save_recording_if_any(
        settings: &AppSettings,
        session: Option<&MediaSession>,
        number: &str,
        duration: Duration,
    ) {
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
        let safe_number = if safe_number.is_empty() { "unknown".to_string() } else { safe_number };
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
                self.error = Some(format!("invalid port: {:?}", self.sip_settings_form.port));
                return;
            }
        };
        if self.sip_settings_form.host.trim().is_empty()
            || self.sip_settings_form.username.trim().is_empty()
        {
            self.error = Some("server and username are required".to_string());
            return;
        }
        let name = self.sip_settings_form.name.trim();
        let name = if name.is_empty() {
            format!("{}@{}", self.sip_settings_form.username.trim(), self.sip_settings_form.host.trim())
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
        self.error = None;
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

    fn handle_audio_settings_save(&mut self) {
        let srtp = self.audio_settings_form.srtp;
        let codecs = self.audio_settings_form.codecs.clone();
        if let Some(acc) = self.selected_mut() {
            acc.config.srtp = srtp;
            acc.config.preferred_codecs = codecs;
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

        self.error = None;
    }

    fn handle_call_state_changed(&mut self, account: usize, id: CallId, state: CallState) -> Task<Message> {
        let Some(acc) = self.accounts.get_mut(account) else {
            return Task::none();
        };
        match state {
            CallState::Ringing => Task::none(),
            CallState::Answered { remote, .. } => {
                let Some(idx) = acc.line_index_for_call(&id) else {
                    tracing::warn!("answered event for a call we're not tracking");
                    return Task::none();
                };
                if let CallUiState::Active {
                    on_hold,
                    pre_hold_output_volume,
                    output_volume,
                    muted,
                    media,
                    ..
                } = &mut acc.lines[idx]
                {
                    // A re-INVITE resume confirmation for an already-active
                    // call, not the initial answer — restore local audio
                    // instead of rebuilding the whole call state.
                    *on_hold = false;
                    *output_volume = *pre_hold_output_volume;
                    if let Some(session) = media {
                        session.set_mic_muted(*muted);
                        session.set_output_volume(*output_volume);
                    }
                    return Task::none();
                }

                let Some(reserved) = acc.pending_sockets[idx].take() else {
                    tracing::warn!("call answered with no reserved local socket");
                    return Task::none();
                };
                let (number, direction) = match &acc.lines[idx] {
                    CallUiState::Outgoing { number, .. } => {
                        (number.clone(), history::CallDirection::Outgoing)
                    }
                    CallUiState::Incoming { caller, .. } => {
                        (caller.clone(), history::CallDirection::Incoming)
                    }
                    _ => (String::new(), history::CallDirection::Outgoing),
                };
                let post_dial: VecDeque<char> = acc.pending_post_dials[idx].drain(..).collect();
                let has_post_dial = !post_dial.is_empty();
                acc.lines[idx] = CallUiState::Active {
                    id: id.clone(),
                    number,
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
                    transfer_input: None,
                    post_dial,
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
                        ..
                    } = &mut acc.lines[idx]
                {
                    *on_hold = true;
                    *pre_hold_output_volume = *output_volume;
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
                        Self::push_history(
                            &mut self.call_history,
                            caller.clone(),
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
                let was_outgoing_unanswered = matches!(acc.lines[idx], CallUiState::Outgoing { .. });
                if let Some(player) = &self.tone_player {
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
                        Self::push_history(
                            &mut self.call_history,
                            caller.clone(),
                            history::CallDirection::Incoming,
                            history::CallOutcome::Missed,
                            None,
                        );
                    }
                    CallUiState::Outgoing { number, .. } => {
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
        let size = if compact { COMPACT_WINDOW_SIZE } else { MAIN_WINDOW_SIZE };
        self.main_window_size = size;
        window::resize(self.main_window, size)
    }

    fn handle_dialpad(&mut self, digit: char) -> Task<Message> {
        let Some(acc) = self.selected_mut() else { return Task::none() };
        let idx = acc.selected_idx();
        match &mut acc.lines[idx] {
            CallUiState::Idle => {
                self.dial_input.push(digit);
                Task::none()
            }
            CallUiState::Active { id, dtmf_feedback, .. } => {
                dtmf_feedback.push(digit);
                let id = id.clone();
                acc.send_command(CoreCommand::SendDtmf { id, digit });
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
            self.error = Some("enter a number to call".to_string());
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
            Err(e) => self.error = Some(format!("failed to reserve local port: {e}")),
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
            Err(e) => self.error = Some(format!("failed to reserve local port: {e}")),
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
            CallUiState::Active { id, .. } | CallUiState::Outgoing { id, .. } => Some(id.clone()),
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
            CallUiState::Outgoing { number, .. } => {
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
        let Some(acc) = self.selected_mut() else { return Task::none() };
        let idx = acc.selected_idx();
        acc.lines[idx] = CallUiState::Idle;
        acc.pending_sockets[idx] = None;
        acc.last_call_status[idx] = Some("hung up".to_string());
        if let Some(player) = &self.tone_player {
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
        .filter(|(i, c)| {
            c.is_ascii_digit() || *c == '*' || *c == '#' || (allow_leading_plus && *c == '+' && *i == 0)
        })
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
