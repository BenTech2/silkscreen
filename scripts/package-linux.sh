#!/usr/bin/env bash
# Builds a portable Linux bundle: the silkscreen binary plus its own FFmpeg
# 8.1 (LGPL, BtbN builds) in lib/, found via an $ORIGIN rpath. Built inside
# Debian 12 (glibc 2.36) so it runs on Debian 12+, Ubuntu 22.10+, Fedora,
# Raspberry Pi OS (Bookworm/Trixie, 64-bit) and the like.
#
#   scripts/package-linux.sh x86_64     # PCs
#   scripts/package-linux.sh aarch64    # Raspberry Pi 4/5 and other ARM64 boards
#
# Needs podman (or docker) and, for a foreign architecture, qemu-user-static
# with binfmt registered. Output: dist/silkscreen-linux-<arch>.tar.gz
set -euo pipefail
cd "$(dirname "$0")/.."

arch=${1:-x86_64}
case "$arch" in
    x86_64) platform=linux/amd64; image=docker.io/amd64/debian:bookworm; ff=linux64 ;;
    aarch64) platform=linux/arm64; image=docker.io/arm64v8/debian:bookworm; ff=linuxarm64 ;;
    *) echo "usage: $0 x86_64|aarch64" >&2; exit 1 ;;
esac
engine=$(command -v podman || command -v docker)
ffmpeg_url="https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n8.1-latest-${ff}-lgpl-shared-8.1.tar.xz"

# label=disable: several builds may share the checkout (":Z" would relabel
# it privately for one container at a time).
"$engine" run --rm --platform "$platform" --security-opt label=disable \
    -v "$PWD:/src" \
    -v "silkscreen-cargo-$arch:/cargo" \
    -e CARGO_HOME=/cargo -e RUSTUP_HOME=/cargo/rustup \
    -e ARCH="$arch" -e FFMPEG_URL="$ffmpeg_url" \
    -w /src "$image" bash -euo pipefail -c '
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends \
            ca-certificates curl xz-utils build-essential clang libclang-dev pkg-config \
            libpipewire-0.3-dev >/dev/null
        [ -x /cargo/bin/cargo ] || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
        export PATH=/cargo/bin:$PATH

        ff=/tmp/ffmpeg && mkdir -p $ff
        curl -sSL "$FFMPEG_URL" | tar xJ -C $ff --strip-components=1
        export FFMPEG_DIR=$ff
        # DT_RPATH (not RUNPATH) so the bundled libavcodec also finds its own
        # dependencies (libswresample) next to it.
        export RUSTFLAGS="-C link-arg=-Wl,--disable-new-dtags,-rpath,\$ORIGIN/lib"

        target=/src/target/linux-$ARCH
        cargo build --release -p silkscreen --target-dir $target

        out=/src/dist/silkscreen-linux-$ARCH
        rm -rf $out && mkdir -p $out/lib
        cp $target/release/silkscreen $out/
        strip $out/silkscreen
        # Only what the binary loads: avcodec (+ swresample) and avutil.
        for lib in avcodec avutil swresample; do cp -a $ff/lib/lib$lib.so* $out/lib/; done
        cp /src/README.md $out/
        [ -f $ff/LICENSE.txt ] && cp $ff/LICENSE.txt $out/FFMPEG-LICENSE.txt
        cd /src/dist && tar czf silkscreen-linux-$ARCH.tar.gz silkscreen-linux-$ARCH
        echo "built dist/silkscreen-linux-$ARCH.tar.gz"
    '
