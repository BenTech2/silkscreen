#!/usr/bin/env bash
# Builds the Android app: the Rust core for arm64 + x86_64, then the APK.
#
# Needs: Android SDK + NDK (ANDROID_HOME, ANDROID_NDK_HOME), JDK 17-21
# (JAVA_HOME), cargo-ndk, and the Rust targets aarch64-linux-android and
# x86_64-linux-android. Output: dist/silkscreen-android.apk
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/no-home-paths.sh

export ANDROID_HOME=${ANDROID_HOME:-$HOME/Android/Sdk}
export ANDROID_NDK_HOME=${ANDROID_NDK_HOME:-$(ls -d "$ANDROID_HOME"/ndk/* | sort -V | tail -1)}
[ -z "${JAVA_HOME:-}" ] && [ -d "$HOME/Android/jdk21" ] && export JAVA_HOME=$HOME/Android/jdk21
variant=${1:-release}

cargo ndk --platform 30 -t arm64-v8a -t x86_64 -o android/app/src/main/jniLibs \
    build --release -p silkscreen-android

cd android
./gradlew --quiet "assemble${variant^}"
cd ..
mkdir -p dist
cp "android/app/build/outputs/apk/$variant/app-$variant.apk" dist/silkscreen-android.apk
check_no_home_paths android/app/src/main/jniLibs
echo "built dist/silkscreen-android.apk"
