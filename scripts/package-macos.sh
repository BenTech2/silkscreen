#!/usr/bin/env bash
# Builds Silkscreen.app for Apple Silicon with its own copy of Homebrew's
# FFmpeg (so a Homebrew upgrade can't break it), and zips it.
#
#   scripts/package-macos.sh             # dist/silkscreen-macos-arm64.zip
#   scripts/package-macos.sh --install   # ...and copy the app to /Applications
#
# Needs: brew install ffmpeg pkg-config dylibbundler; the PyroWave
# submodules (scripts/fetch-submodules.sh). The release workflow runs this.
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/no-home-paths.sh

cargo build --release -p silkscreen

out=dist/silkscreen-macos-arm64
app=$out/Silkscreen.app/Contents
rm -rf "$out" && mkdir -p "$app/MacOS" "$app/Frameworks" "$app/Resources"
cp target/release/silkscreen "$app/MacOS/"
# Drop the debug map (paths to the build's object files); symbols stay.
strip -S "$app/MacOS/silkscreen"
# Copy Homebrew's FFmpeg and its dependencies into the bundle.
dylibbundler -od -b -x "$app/MacOS/silkscreen" -d "$app/Frameworks" \
    -p @executable_path/../Frameworks/ >/dev/null
# App icon (on Apple's icon grid) rendered by scripts/render-icons.sh.
set=$(mktemp -d)/silkscreen.iconset && mkdir -p "$set"
for s in 16 32 128 256 512; do
    cp assets/png/macos/icon-$s.png "$set/icon_${s}x${s}.png"
    cp assets/png/macos/icon-$((s * 2)).png "$set/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$set" -o "$app/Resources/silkscreen.icns"
version=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "silkscreen"))')
sed "s/@VERSION@/$version/g" assets/Info.plist >"$app/Info.plist"
# Rewriting load paths invalidates signatures; Apple Silicon needs one.
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
cp third_party/pyrowave/LICENSE "$out/PYROWAVE-LICENSE.txt"
"$app/MacOS/silkscreen" doctor >/dev/null
check_no_home_paths "$out"
(cd dist && rm -f silkscreen-macos-arm64.zip && zip -qry silkscreen-macos-arm64.zip silkscreen-macos-arm64)
echo "built dist/silkscreen-macos-arm64.zip (Silkscreen $version)"

if [ "${1:-}" = "--install" ]; then
    rm -rf /Applications/Silkscreen.app
    cp -R "$out/Silkscreen.app" /Applications/
    echo "installed /Applications/Silkscreen.app"
fi
