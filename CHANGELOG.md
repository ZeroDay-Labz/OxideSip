# Changelog

All notable changes to OxideSip are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow SemVer.

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

[0.2.0]: https://github.com/ZeroDay-Labz/OxideSip/compare/v0.1.8...v0.2.0
[0.1.8]: https://github.com/ZeroDay-Labz/OxideSip/releases/tag/v0.1.8
