#!/usr/bin/env bash
# Renders packaging/icons/{16..512}.png from the master SVG (16/32 use the
# simplified "-small" variant so the handset stays legible). Needs inkscape;
# falls back to ImageMagick.
set -euo pipefail
cd "$(dirname "$0")/.."
master=packaging/io.github.zerodaylabz.OxideSip.svg
small=packaging/io.github.zerodaylabz.OxideSip-small.svg
render() { # size source dest
  if command -v inkscape >/dev/null; then
    inkscape -w "$1" -h "$1" "$2" -o "$3" >/dev/null 2>&1
  else
    magick -background none -density 384 "$2" -resize "$1x$1" "$3"
  fi
}
for s in 16 32; do render "$s" "$small" "packaging/icons/$s.png"; done
for s in 48 64 128 256 512; do render "$s" "$master" "packaging/icons/$s.png"; done
cp packaging/icons/256.png packaging/io.github.zerodaylabz.OxideSip.png
echo "rendered: $(ls packaging/icons)"
