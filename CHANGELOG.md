# Changelog

All notable changes to OxideSip are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow SemVer.

## [Unreleased]

## [0.3.0] — 2026-11-18

### Added
- **Automatic volume levelling** (Settings ▸ Calls, on by default). Quiet callers are brought up
  and loud ones down to one standard level, without amplifying background noise or clipping;
  your microphone gets the same treatment before it is sent.
- **Live call quality** on the call card: receive buffer delay, jitter and packet loss.
- **Loss concealment.** A lost or late packet is covered by a short fading replay of the audio
  before it instead of a hard gap.
- **Echo cancellation** (Settings ▸ Calls, off by default) through PipeWire's echo canceller, for
  using speakers instead of a headset.
- "On hold by the other side" when the far end holds the call.

### Fixed
- The microphone and speaker level bars barely moved during normal speech. They now show the
  peak since the last screen update on a decibel scale, so they follow your voice.
- The speaker volume slider changed what was recorded and what a conferenced line heard. It now
  acts only on what you hear, and takes effect immediately.
- Re-INVITE and UPDATE requests from the PBX (remote hold and resume, a moved media address,
  session refresh) were answered with 501. They are now accepted and applied to the running
  call.
- A single lost packet stalled playback for up to 100 ms while later packets were held back.

### Changed
- Volume sliders follow perceived loudness: 100% is unchanged and 200% is now four times the
  amplitude (+12 dB) rather than twice. Slider positions are remembered between calls.

## [0.2.2] — 2026-11-18

### Fixed
- **SRTP now actually encrypts.** With "SRTP media encryption" on, the call was negotiated as
  encrypted but the audio was still sent as plain RTP. Audio is now encrypted and authenticated
  (AES-CM-128 with HMAC-SHA1-80, RFC 3711) in both directions, and packets that fail the check
  are dropped.
- Incoming calls could start audio with the caller's first-listed codec instead of the one the
  answer selected from your codec priority.
- Hold and resume no longer change the SRTP key in the middle of a call.

### Changed
- With SRTP on, a call that cannot be encrypted is refused or ended with a message instead of
  silently continuing unencrypted. Incoming encrypted calls are accepted even when the setting
  is off.
- SRTP offers from the other side are checked properly: only `AES_CM_128_HMAC_SHA1_80` is
  accepted, key lifetimes are tolerated, and the answer echoes the offered crypto tag.

### Added
- Copy buttons for the number in the dial field and on the call card.
- A lock indicator on the call card while a call is encrypted.
- `echo_probe --srtp` to place an encrypted test call.

## [0.2.1] — 2026-11-18

### Fixed
- Call audio from the far end played 1.5–2.5 s late. The playback stream filled PipeWire's whole
  buffer on every callback instead of the amount requested, so it was serviced only every ~1.5 s
  and needed seconds of buffering to cover the gap. It now runs every graph cycle.
- KDE kept showing the 0.1.x icon after upgrading because its per-user pixmap cache is keyed by
  icon name. The app now broadcasts KDE's icon-changed signal whenever it installs new icons,
  so the task bar switches to the new icon without logging out.

### Changed
- The playback jitter buffer is adaptive. It holds only as much audio as the current network
  needs (about 20–50 ms on a typical PBX link, down from 2 s), grows when audio arrives late,
  shrinks back afterwards, and follows the far end's clock by dropping or repeating single
  samples instead of skipping or underrunning. Buffer depth, margin, underruns and corrections
  are logged every 5 s at `RUST_LOG=info`.

### Added
- `echo_probe` example: places a call from a configured account (for instance to the `*43` echo
  test) and measures the network + PBX round trip and packet jitter over RTP, or runs the real
  audio path and prints the buffer statistics.

## [0.2.0] — 2026-10-16

### Added
- A real app icon (SVG master plus 16–512 px PNGs) installed by every package, and a Wayland
  app id on every window so desktops can match them to the launcher.
- Desktop notifications for incoming calls with Answer / Decline actions (`notify-rust`),
  switchable under Settings ▸ Calls.
- Dark and light themes with a System / Dark / Light preference that follows the desktop's
  colour scheme live; a quick toggle in the top bar.
- Keyboard shortcuts: Enter / Esc for answer, decline and hang-up, digits for dialling and DTMF,
  Ctrl+1–4 pages, Ctrl+K focus, Ctrl+M / H / T call controls, Ctrl+Shift+C compact bar,
  Ctrl+B sidebar, Ctrl+Q quit.
- History filters (All / Missed / Incoming / Outgoing), day grouping and a missed-call badge in
  the sidebar.
- Persisted window size, page, settings section, history filter, sidebar state and the contacts
  export path (`ui_state.toml`).
- `--demo` mode (scripted backend, seeded data, nothing saved) and `--screenshot` mode that
  renders the documentation images; `scripts/screenshots.sh` and `scripts/render-icons.sh`.
- `sip:` / `tel:` URI handling from the command line and the `.desktop` entry.
- AppStream metainfo, a CI workflow (fmt, build, test, clippy, desktop/metainfo validation,
  headless screenshot smoke test), `SHA256SUMS` on releases, `rust-version`, a release profile
  with thin LTO.
- Core: `RemoteParty` (display name, user, URI) parsed from the typed `From` header with
  `P-Asserted-Identity` fallback; `RejectReason::Decline` sends 603 instead of 486.

### Changed
- The whole interface: a sidebar layout (Phone, Contacts, History, Settings) with a top bar,
  panels and consistent tokens, Inter as the bundled UI font and Lucide SVG icons; the three
  detached settings windows are now sections of one Settings page.
- The compact call bar is now its own small always-on-top window instead of a resize of the
  main window (a resized Wayland surface could show its old buffer stretched until the
  compositor repainted); it closes when the call ends.
- On start the app installs its icons into `~/.local/share/icons` (and a launcher entry when no
  package provides one), so builds run from a source tree or tarball get the right icon.
- The incoming-call window is rebuilt: one always-on-top window sized to its content, stacking
  simultaneous calls, with account chip, name / number / host, ring timer, large Decline and
  Answer buttons, a Silence button, focus and attention requests, and keyboard shortcuts.
- Incoming callers show as name and number everywhere (previously the raw SIP `From` header),
  so saved contacts match inbound calls; history stores the name and repairs old entries.
- Contact matching normalises numbers (URI user part, punctuation, country-code suffixes); the
  deny list matches numbers by digits and names / domains by text.
- Decline stops the ringtone immediately; a call ringing for 60 s unanswered is refused as busy
  and logged as missed; a call arriving on a busy line is refused instead of ignored.
- Closing the main window during a call asks for confirmation.
- Errors and confirmations show as dismissable toasts instead of one inline error string.
- Contacts and history are written off the UI thread; dropped core commands are logged.
- Packaging assets moved from `flatpak/` to `packaging/`; the release tarball is now
  `oxidesip-linux-x86_64.tar.gz`.

### Removed
- The Call Forwarding toggle (never functional; the setting keys are kept for compatibility).
- The Bootstrap icon font dependency.

## [0.1.8] — 2026-08-07
- Version bump; documented the blind-transfer confirmation limitation.

## [0.1.x] — 2026-07 to 2026-08
- History clear and add-to-contacts, secondary audio controls, tab animation, dialpad flash.
- Friendlier call-end reasons, hold / resume failure signal, correct DTMF payload negotiation.
- DTMF / ringback crackle and press-to-tone latency fixes.
- Mixing another application's audio into and out of a call.
- Multi-account support, codec priority, SRTP, TLS, recording, contacts import / export,
  Flatpak / RPM / deb packaging, and the first incoming-call popup.

[0.3.0]: https://github.com/ZeroDay-Labz/OxideSip/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/ZeroDay-Labz/OxideSip/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/ZeroDay-Labz/OxideSip/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/ZeroDay-Labz/OxideSip/compare/v0.1.8...v0.2.0
[0.1.8]: https://github.com/ZeroDay-Labz/OxideSip/releases/tag/v0.1.8
