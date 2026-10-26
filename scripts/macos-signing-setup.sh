#!/usr/bin/env bash
# One-time setup for signing local macOS builds with a stable identity.
#
# macOS ties privacy permissions (Screen Recording, Accessibility) to an
# app's signature. Ad-hoc signatures change with every build, so each update
# looked like a new app and the permissions had to be granted again. A
# self-signed certificate keeps the identity the same across builds.
#
# Creates "Silkscreen Local Signing" in its own keychain,
# ~/Library/Keychains/silkscreen-signing.keychain-db, unlocked by a random
# password kept next to it, so signing never prompts. package-macos.sh and
# package-macos-intel.sh use it when it exists. Not for distribution: other
# Macs don't know this certificate (that needs an Apple Developer ID).
set -euo pipefail
name="Silkscreen Local Signing"
kc="$HOME/Library/Keychains/silkscreen-signing.keychain-db"
pwfile="$HOME/Library/Keychains/silkscreen-signing.password"
if [ -f "$kc" ]; then
    echo "already set up: $kc"
    exit 0
fi
tmp=$(mktemp -d)
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=$name" \
    -keyout "$tmp/key.pem" -out "$tmp/cert.pem" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=critical,codeSigning" \
    -addext "basicConstraints=critical,CA:false" 2>/dev/null
p12pw=$(openssl rand -hex 16)
openssl pkcs12 -export -legacy -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
    -name "$name" -out "$tmp/id.p12" -passout "pass:$p12pw" 2>/dev/null ||
    openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
        -name "$name" -out "$tmp/id.p12" -passout "pass:$p12pw"
kcpw=$(openssl rand -hex 24)
umask 077
printf '%s' "$kcpw" >"$pwfile"
security create-keychain -p "$kcpw" "$kc"
security set-keychain-settings "$kc" # no auto-lock timeout
security unlock-keychain -p "$kcpw" "$kc"
security import "$tmp/id.p12" -k "$kc" -P "$p12pw" -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$kcpw" "$kc" >/dev/null
# codesign only finds identities in keychains on the search list.
current=$(security list-keychains -d user | tr -d '"' | xargs)
security list-keychains -d user -s $current "$kc"
rm -rf "$tmp"
echo "created \"$name\" in $kc"
