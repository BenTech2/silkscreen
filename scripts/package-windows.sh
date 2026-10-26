#!/usr/bin/env bash
# Cross-compile silkscreen.exe from Linux and bundle it with FFmpeg's DLLs.
#
# Needs: rustup target x86_64-pc-windows-gnu, mingw64-gcc + mingw64-headers,
# clang, CMake (for PyroWave), and a Windows FFmpeg 8 *shared* build (e.g. BtbN's
# ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip) unpacked at $FFMPEG_DIR.
#
#   FFMPEG_DIR=~/ffmpeg-win64 scripts/package-windows.sh
set -euo pipefail

: "${FFMPEG_DIR:?set FFMPEG_DIR to an unpacked win64 FFmpeg shared build}"
cd "$(dirname "$0")/.."
. scripts/no-home-paths.sh
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
# PyroWave (experimental codec), unless built with --no-default-features.
for dll in target/x86_64-pc-windows-gnu/release/*pyrowave*.dll; do
    [ -f "$dll" ] || continue
    cp "$dll" "$out/" && x86_64-w64-mingw32-strip "$out/$(basename "$dll")"
    cp third_party/pyrowave/LICENSE "$out/PYROWAVE-LICENSE.txt"
done
# Only the libraries silkscreen.exe actually loads (avcodec needs swresample).
for lib in avcodec avutil swresample; do cp "$FFMPEG_DIR"/bin/$lib-*.dll "$out/"; done
cp README.md LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.md THIRD-PARTY-LICENSES.md "$out/"
[ -f "$FFMPEG_DIR/LICENSE.txt" ] && cp "$FFMPEG_DIR/LICENSE.txt" "$out/FFMPEG-LICENSE.txt"
check_no_home_paths "$out"
(cd dist && rm -f silkscreen-windows-x64.zip && zip -qr silkscreen-windows-x64.zip silkscreen-windows-x64)
echo "built dist/silkscreen-windows-x64.zip"
