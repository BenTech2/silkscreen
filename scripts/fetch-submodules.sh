#!/usr/bin/env bash
# Fetches PyroWave and the parts of Granite it builds with (volk and the
# Vulkan headers, not the rest of Granite's own submodules).
set -euo pipefail
cd "$(dirname "$0")/.."
git submodule update --init --depth 1 third_party/pyrowave third_party/granite
git -C third_party/granite submodule update --init --depth 1 \
    third_party/volk third_party/khronos/vulkan-headers
