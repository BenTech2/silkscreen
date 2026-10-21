#!/usr/bin/env bash
# Cross-compile silkscreen.exe from Linux and bundle it with FFmpeg's DLLs.
#
# Needs: rustup target x86_64-pc-windows-gnu, mingw64-gcc + mingw64-headers,
# clang, and a Windows FFmpeg 8 *shared* build (e.g. BtbN's
# ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip) unpacked at $FFMPEG_DIR.
#
#   FFMPEG_DIR=~/ffmpeg-win64 scripts/package-windows.sh
set -euo pipefail

: "${FFMPEG_DIR:?set FFMPEG_DIR to an unpacked win64 FFmpeg shared build}"
cd "$(dirname "$0")/.."
MINGW_INC=${MINGW_INC:-/usr/x86_64-w64-mingw32/sys-root/mingw/include}

export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
# Skip clang's x86 intrinsics headers (pulled in via windows.h); FFmpeg's API
# doesn't use them and bindgen can't represent some of their types.
export BINDGEN_EXTRA_CLANG_ARGS="--target=x86_64-w64-mingw32 -I$MINGW_INC -D__IMMINTRIN_H -D__X86INTRIN_H"

cargo build --release --target x86_64-pc-windows-gnu -p silkscreen

out=dist/silkscreen-windows-x64
rm -rf "$out" && mkdir -p "$out"
cp target/x86_64-pc-windows-gnu/release/silkscreen.exe "$out/"
x86_64-w64-mingw32-strip "$out/silkscreen.exe"
# Only the libraries silkscreen.exe actually loads (avcodec needs swresample).
for lib in avcodec avutil swresample; do cp "$FFMPEG_DIR"/bin/$lib-*.dll "$out/"; done
cp README.md "$out/"
[ -f "$FFMPEG_DIR/LICENSE.txt" ] && cp "$FFMPEG_DIR/LICENSE.txt" "$out/FFMPEG-LICENSE.txt"
(cd dist && rm -f silkscreen-windows-x64.zip && zip -qr silkscreen-windows-x64.zip silkscreen-windows-x64)
echo "built dist/silkscreen-windows-x64.zip"
