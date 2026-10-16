# Contributing to OxideSip

Thanks for helping. Issues and pull requests are welcome; for anything larger than a fix,
open an issue first so we can agree on the scope.

## Setup

```sh
# Fedora
sudo dnf install pipewire-devel clang-devel pkgconf-pkg-config libxkbcommon-devel wayland-devel
# Debian / Ubuntu
sudo apt install libpipewire-0.3-dev libclang-dev pkg-config libxkbcommon-dev libwayland-dev

cargo run -p softphone-ui            # against your real accounts
cargo run -p softphone-ui -- --demo  # scripted backend, nothing saved
```

Rust 1.88 or newer (the code uses let-chains).

## Before you push

CI runs exactly these, and they must be clean:

```sh
cargo fmt --all -- --check
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
desktop-file-validate packaging/io.github.zerodaylabz.OxideSip.desktop
appstreamcli validate --no-net packaging/io.github.zerodaylabz.OxideSip.metainfo.xml
```

Workspace-wide clippy allowances live in the root `Cargo.toml` with a comment each.

## Where things are

| Crate / dir | Contents |
|---|---|
| `softphone-core` | SIP signalling (rsipstack), SDP, registration, `CoreEvent` / `CoreCommand` |
| `softphone-media` | PipeWire I/O, RTP, tones, recording, codecs — the audio path. Treat it as stable; changes here need a real call to verify. |
| `softphone-ui/src/app.rs` | Application state and `update()` |
| `softphone-ui/src/ui/` | Pages and components; `theme.rs` holds every colour and widget style |
| `softphone-ui/src/incoming.rs` | The incoming-call window |
| `softphone-ui/src/demo.rs`, `screenshot.rs` | `--demo` and `--screenshot` |
| `packaging/` | `.desktop`, metainfo, icon SVG and PNGs; `flatpak/` holds the manifest |

## UI changes

- Colours, radii, spacing and type sizes come from `theme.rs` (`tokens`, `space`, `text_size`,
  `radius`). Do not hard-code a colour in a page.
- Icons are Lucide SVGs under `softphone-ui/assets/icons/`, registered in `icons.rs`.
- Check both themes: `--demo --theme light` and `--demo --theme dark`.
- Regenerate the README images when a page changes: `scripts/screenshots.sh` (needs an X11 or
  XWayland display; see the comment in the script for why).

## Commits and releases

- Conventional-style subjects: `feat(ui): …`, `fix(core): …`, `chore: …`.
- Add a line to `CHANGELOG.md` under *Unreleased* when behaviour changes.
- Releases: bump `version` in the root `Cargo.toml`, update the changelog and the
  `<releases>` entry in the metainfo, regenerate screenshots, tag `vX.Y.Z` and push the tag;
  `release.yml` builds and uploads the tarball, RPM, deb and Flatpak bundle.
