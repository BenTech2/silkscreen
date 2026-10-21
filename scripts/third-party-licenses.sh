#!/usr/bin/env bash
# Regenerates THIRD-PARTY-LICENSES.md (the Rust crates' licenses) with
# cargo-about: `cargo install cargo-about --locked --features cli`.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo about generate about.hbs -o THIRD-PARTY-LICENSES.md
echo "wrote THIRD-PARTY-LICENSES.md"
