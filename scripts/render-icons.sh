#!/usr/bin/env bash
# Renders every platform icon from assets/icon.svg (needs ImageMagick).
set -euo pipefail
cd "$(dirname "$0")/../assets"
for s in 16 24 32 48 64 128 256 512 1024; do
    magick -background none -density 384 icon.svg -resize ${s}x${s} png/icon-$s.png
done
# Windows: one .ico with the sizes Explorer and the taskbar use.
magick png/icon-{16,24,32,48,64,128,256}.png icon.ico
# Window icon at runtime: raw RGBA, so the app needs no image decoder.
magick png/icon-64.png -depth 8 rgba:icon-64.rgba
