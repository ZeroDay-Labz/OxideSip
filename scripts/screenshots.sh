#!/usr/bin/env bash
# Regenerates docs/screenshots/*.png with the app's own --screenshot mode
# (demo backend, nothing touches your real config). Runs through XWayland:
# on a native Wayland session an untouched window may never repaint after
# its first frame, so staged pages would come out stale.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -p softphone-ui
out=docs/screenshots
export WAYLAND_DISPLAY=            # force X11 / XWayland
export WINIT_X11_SCALE_FACTOR="${SCALE:-1.5}"
target/release/oxidesip --screenshot "$out" --page all --size 1200x800
target/release/oxidesip --screenshot "$out" --page phone --size 1200x800 --theme light
echo "wrote:"; ls -1 "$out"
