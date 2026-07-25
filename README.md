# OxideSip

A professional-grade SIP softphone for Linux, built in Rust. Native PipeWire audio, a custom
`iced`-based UI, and a multi-line, multi-account calling core — aimed at Fedora/KDE/Wayland
desktops, targeting the kind of experience you'd expect from MicroSIP or a Cisco desk phone rather
than a bare-bones reference client.

> Early-stage, actively developed. Expect rough edges; see [Known limitations](#known-limitations)
> below for what's genuinely not done yet.

## Features

- **Multiple SIP accounts, registered simultaneously** — each with its own set of 5 call lines,
  switchable from the main window without losing state on the others.
- **Full call handling** — place/answer/reject/hang up, hold & resume, blind transfer, DTMF (SIP
  INFO), mute, join two lines into a local 3-way conference, and a compact "mini" window mode.
- **Do Not Disturb, Auto-Answer, and a deny list** — checked before an incoming call ever rings.
- **Call recording** — WAV, both directions mixed into one file per call, saved to a folder you
  pick with a native file dialog.
- **Ordered codec priority list** — reorder which G.711 variant (µ-law/A-law) is preferred per
  account; negotiation walks your priority list against whatever the other side offers, both when
  placing and answering calls.
- **Secondary audio output** — stream the other party's voice to a second PipeWire target: an
  ordinary sink, *or* a live application capture stream (e.g. Discord's own voice-engine node while
  you're in a voice channel), so the other side of a phone call can be heard in a voice chat.
- **Contacts** with JSON import/export (tolerant of other softphones' export formats) and a native
  file picker.
- **Dial-history dropdown**, redial, and per-line call history.
- **SDES-SRTP** media encryption (opt-in) alongside plain RTP, and UDP/TCP/TLS SIP transports.

## Requirements

- **Rust** 1.85+ (edition 2024) — install via [rustup](https://rustup.rs).
- **PipeWire** ≥ 0.3.44, running as your session's audio server (the Fedora/KDE/Wayland default).
- Build-time system packages (headers for `pipewire-sys`'s bindgen, plus `pkg-config`):

  ```bash
  # Fedora
  sudo dnf install pipewire-devel clang-devel pkgconf-pkg-config

  # Debian/Ubuntu
  sudo apt install libpipewire-0.3-dev libclang-dev pkg-config

  # Arch
  sudo pacman -S pipewire clang pkgconf
  ```

- A SIP account/extension on a PBX (FreePBX, Asterisk, etc.) to register against.

## Building

```bash
git clone https://github.com/ZeroDay-Labz/OxideSip.git
cd OxideSip
cargo build --release
```

The workspace has three crates:

| Crate             | What it is                                                             |
|--------------------|-------------------------------------------------------------------------|
| `softphone-core`  | SIP signaling (via [`rsipstack`](https://crates.io/crates/rsipstack)), SDP negotiation, registration — no UI or audio code. |
| `softphone-media` | PipeWire capture/playback, RTP send/recv, DTMF/dial tones, call recording, codec en/decoding. |
| `softphone-ui`    | The [`iced`](https://github.com/iced-rs/iced) 0.14 desktop app tying both together. |

### Prebuilt releases

Tagged pushes (`v*`, e.g. `v0.1.0`) trigger [`.github/workflows/release.yml`](.github/workflows/release.yml),
which builds a release binary and publishes two artifacts to the repo's
[Releases](https://github.com/ZeroDay-Labz/OxideSip/releases) page:

- `softphone-ui-linux-x86_64.tar.gz` — a plain binary tarball. Your system needs PipeWire ≥ 0.3.44
  already installed (true by default on Fedora/KDE); nothing else is bundled.
- an RPM, installable with `sudo dnf install ./softphone-ui-*.rpm` on Fedora and other RPM-based
  distros — its runtime dependencies (PipeWire, etc.) are declared automatically from the binary's
  actual shared-library links, so `dnf` will pull in anything missing.

To cut a release yourself: bump `version` in the root `Cargo.toml`, then

```bash
git tag v0.1.0
git push origin v0.1.0
```

## Running

```bash
cargo run --release -p softphone-ui
```

On first launch with no configuration, the app opens straight to account setup — no config file
needs to exist beforehand.

### Configuration

All persistent state — accounts, call-handling settings, audio device selection, contacts, call
history — lives under `$XDG_CONFIG_HOME/oxidesip/` (typically `~/.config/oxidesip/`), regardless of
where the binary is installed or what directory it's launched from. Accounts specifically are in
`accounts.toml` there, created/edited from the app's SIP Settings screen — you don't need to
hand-edit it. If an old CWD-relative copy of any of these files exists from a previous version of
this app (which used to store them next to the binary), it's migrated into the XDG location
automatically the first time it's needed.

If you're upgrading from a version of this app that only supported one account, an existing
single-account `./oxidesip.toml` (in the directory you launch from) or `OXIDESIP_*` environment
variables are migrated into `accounts.toml` automatically on first run.

To seed a first account non-interactively, copy the example config and adjust it, or set
`OXIDESIP_*` environment variables directly:

```bash
cp softphone-core/config.example.toml oxidesip.toml
$EDITOR oxidesip.toml
```

Recognized environment variables (override the legacy single-account config; only ever seed the
*first* migrated account, not a way to configure multiple accounts):
`OXIDESIP_SIP_SERVER_HOST`, `OXIDESIP_SIP_SERVER_PORT`, `OXIDESIP_TRANSPORT` (`udp`/`tcp`/`tls`),
`OXIDESIP_USERNAME`, `OXIDESIP_PASSWORD`, `OXIDESIP_REGISTER_EXPIRES`, `OXIDESIP_LOCAL_PORT`,
`OXIDESIP_SRTP`, `OXIDESIP_CA_CERT_PATH`, `OXIDESIP_CLIENT_CERT_PATH`, `OXIDESIP_CLIENT_KEY_PATH`,
`OXIDESIP_PREFERRED_CODECS` (comma-separated, e.g. `ulaw,alaw`).

### Routing a call into Discord (or another voice app)

Open **Settings → Secondary Output**, hit **Refresh** while you're actually in the target app's
voice channel, and pick its entry from the dropdown (prefixed `App:`) — e.g. Discord's own
`WEBRTC VoiceEngine [recStream]` node only exists in the PipeWire graph while you're connected to a
voice channel. Selecting it links the far end's voice straight into that app's live input stream,
the same way tools like `qpwgraph`/`helvum` patch one client directly into another. If the app you
want isn't listed, it's either not currently listening for audio, or you'll need to route through a
virtual sink of your own instead (also selectable from the same dropdown).

## Known limitations

- **Call Forwarding** has a toggle and number field in Settings, but doesn't yet redirect calls —
  `rsipstack`'s dialog-reject API doesn't currently expose a way to send a proper SIP 3xx redirect.
  This is disclosed, not silently faked.
- **No echo cancellation** yet — a real AEC integration is planned but not implemented.
- **DTMF** is sent via SIP INFO only; RFC 2833/in-band tone negotiation isn't implemented yet.
- **No STUN/ICE** — works well on a LAN or over a VPN to the PBX; NAT traversal beyond basic
  registration isn't handled yet.
- Some PipeWire configurations exhibit high playback-callback latency for client streams; the app
  includes buffering/catch-up logic to keep audio continuous, but very high-latency setups may
  still feel sluggish on call answer.
- **Keyboard navigation is partial.** Tab/Shift+Tab cycles focus between text fields (in the
  Settings/SIP/Audio windows and the dial pad), and fields wired to submit-on-Enter do. Buttons,
  toggles, and dropdowns aren't keyboard-focusable or activatable — this is a limitation of the
  `iced` GUI toolkit itself (only text inputs support focus in the version this app uses), not
  something toggled off; supporting it fully would mean building a custom focus/activation system
  for every button-like widget.

## Contributing

Issues and PRs welcome. This is a young project with an actively evolving feature set — if you're
planning a larger change, opening an issue first to discuss scope is a good idea.

## License

MIT
