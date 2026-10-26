#!/usr/bin/env bash
# Builds Silkscreen.app for Intel Macs on macOS 11 (Big Sur) and later — for
# instance the 2014 and 2015 Retina iMacs, whose 5K panels have no video
# input, as wireless displays. Built on an Apple Silicon Mac (cross-compiled).
#
# FFmpeg 8.1 is built from source for x86_64 / macOS 11 (LGPL, VideoToolbox,
# no other external libraries) and bundled. PyroWave is left out: its Metal
# version needs an Apple Silicon GPU. Hosting (capturing this Mac's screen)
# needs macOS 12.3+; on older macOS the app works as a display.
#
# Needs: Xcode command line tools, nasm (brew install nasm), the Rust target
# x86_64-apple-darwin. Output: dist/silkscreen-macos-intel.zip
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/no-home-paths.sh

export MACOSX_DEPLOYMENT_TARGET=11.0
ffmpeg_version=8.1
ffmpeg=${FFMPEG_X86_DIR:-$HOME/ffmpeg-macos-x86_64-$ffmpeg_version}
target=x86_64-apple-darwin

if [ ! -f "$ffmpeg/lib/libavcodec.dylib" ]; then
    src=$(mktemp -d)
    curl -sSL "https://ffmpeg.org/releases/ffmpeg-$ffmpeg_version.tar.xz" | tar xJ -C "$src" --strip-components=1
    (
        cd "$src"
        flags="-arch x86_64 -mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET"
        ./configure --prefix="$ffmpeg" --enable-cross-compile --arch=x86_64 --target-os=darwin \
            --cc="clang $flags" --cxx="clang++ $flags" --extra-ldflags="$flags" \
            --x86asmexe=nasm --enable-shared --disable-static --install-name-dir='@rpath' \
            --disable-programs --disable-doc --disable-debug \
            --disable-autodetect --enable-videotoolbox --enable-audiotoolbox >/dev/null
        # FFmpeg keeps its configure line (with the install path) in the
        # libraries; leave the home folder out of it.
        sed -i '' "s#$HOME#/build#g" config.h
        make -j"$(sysctl -n hw.ncpu)" >/dev/null
        make install >/dev/null
        cp COPYING.LGPLv2.1 "$ffmpeg/LICENSE.txt"
    )
    rm -rf "$src"
    echo "built FFmpeg $ffmpeg_version for x86_64 in $ffmpeg"
fi

# The bundled FFmpeg is found through an rpath into the bundle.
FFMPEG_DIR="$ffmpeg" PKG_CONFIG_PATH="$ffmpeg/lib/pkgconfig" \
    BINDGEN_EXTRA_CLANG_ARGS="--target=$target" \
    RUSTFLAGS="$RUSTFLAGS -C link-arg=-Wl,-rpath,@executable_path/../Frameworks" \
    cargo build --release --target $target -p silkscreen --no-default-features

out=dist/silkscreen-macos-intel
app=$out/Silkscreen.app/Contents
rm -rf "$out" && mkdir -p "$app/MacOS" "$app/Frameworks" "$app/Resources"
cp target/$target/release/silkscreen "$app/MacOS/"
# Drop the debug map (paths to the build's object files); symbols stay.
strip -S "$app/MacOS/silkscreen"
for lib in avcodec avdevice avfilter avformat avutil swresample swscale; do
    cp "$ffmpeg"/lib/lib$lib.*.dylib "$app/Frameworks/" 2>/dev/null || true
done
# Keep only the versioned files the binary refers to (e.g. libavcodec.62.dylib).
find "$app/Frameworks" -name '*.dylib' | while read -r f; do
    base=$(basename "$f")
    [[ $base =~ ^lib[a-z]+\.[0-9]+\.dylib$ ]] || rm "$f"
done

set=$(mktemp -d)/silkscreen.iconset && mkdir -p "$set"
for s in 16 32 128 256 512; do
    cp assets/png/macos/icon-$s.png "$set/icon_${s}x${s}.png"
    cp assets/png/macos/icon-$((s * 2)).png "$set/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$set" -o "$app/Resources/silkscreen.icns"
version=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "silkscreen"))')
sed -e "s/@VERSION@/$version/g" \
    -e "s|<key>LSMinimumSystemVersion</key><string>[0-9.]*</string>|<key>LSMinimumSystemVersion</key><string>$MACOSX_DEPLOYMENT_TARGET</string>|" \
    assets/Info.plist >"$app/Info.plist"

# Sign with the stable local identity when it is set up
# (scripts/macos-signing-setup.sh) so macOS keeps its privacy permissions
# across updates; otherwise (e.g. on GitHub) ad-hoc.
sign_id=-
kc="$HOME/Library/Keychains/silkscreen-signing.keychain-db"
if [ -f "$kc" ]; then
    security unlock-keychain -p "$(cat "$HOME/Library/Keychains/silkscreen-signing.password")" "$kc"
    sign_id="Silkscreen Local Signing"
fi
for f in "$app"/Frameworks/*.dylib; do codesign --force -s "$sign_id" "$f"; done
codesign --force -s "$sign_id" "$out/Silkscreen.app"
cp README.md LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.md THIRD-PARTY-LICENSES.md "$out/"
cp "$ffmpeg/LICENSE.txt" "$out/FFMPEG-LICENSE.txt"
check_no_home_paths "$out"
(cd dist && rm -f silkscreen-macos-intel.zip && zip -qry silkscreen-macos-intel.zip silkscreen-macos-intel)
echo "built dist/silkscreen-macos-intel.zip"
