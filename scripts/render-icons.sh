#!/usr/bin/env bash
# Renders every platform icon from assets/icon.svg, plus the macOS ones from
# icon-macos.svg and tray-macos.svg (needs ImageMagick and librsvg).
set -euo pipefail
cd "$(dirname "$0")/../assets"
for s in 16 24 32 48 64 128 256 512 1024; do
    magick -background none -density 384 icon.svg -resize ${s}x${s} png/icon-$s.png
done
# Windows: one .ico with the sizes Explorer and the taskbar use.
magick png/icon-{16,24,32,48,64,128,256}.png icon.ico
# Window icon at runtime: raw RGBA, so the app needs no image decoder.
magick png/icon-64.png -depth 8 rgba:icon-64.rgba
# macOS: the icon on Apple's grid (Dock, Silkscreen.app), and the menu bar
# glyph as raw RGBA (a template image: macOS tints it).
mkdir -p png/macos
for s in 16 32 64 128 256 512 1024; do
    rsvg-convert -w $s -h $s icon-macos.svg -o png/macos/icon-$s.png
done
rsvg-convert -w 36 -h 36 tray-macos.svg -o /tmp/silkscreen-tray-36.png
magick /tmp/silkscreen-tray-36.png -depth 8 rgba:tray-macos-36.rgba
rm /tmp/silkscreen-tray-36.png
