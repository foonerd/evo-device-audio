#!/usr/bin/env bash
# Cross-compile evo-device-audio plugins for non-host targets,
# wrapping `cross` (https://github.com/cross-rs/cross) with the
# v0.1.13 path-dep workaround.
#
# Why this script exists:
#
#   evo-device-audio's [workspace.dependencies] pins
#   evo-plugin-sdk via a path dep on the sibling
#   evo-core-eng clone for v0.1.13 development. cross-rs
#   auto-mounts each path-dep crate directory into the
#   container at the host-canonical path, but does NOT mount
#   the path-dep's workspace root. evo-plugin-sdk inherits
#   `edition.workspace = true`; cargo must walk up to
#   evo-core-eng's Cargo.toml to resolve the inheritance.
#
#   This script supplies the missing volume mount via
#   CROSS_CONTAINER_OPTS. At v0.1.13 release-cut the SDK pin
#   flips back to a git+tag form and this script's workaround
#   can be removed.
#
# Usage:
#
#   scripts/cross-build.sh aarch64-unknown-linux-gnu \
#       --release --features alsa-substrate \
#       -p org-evoframework-composition-alsa
#
#   The first argument is the target triple. All subsequent
#   arguments are passed verbatim to `cross build`.

set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <target-triple> [cross build flags...]" >&2
    echo "example: $0 aarch64-unknown-linux-gnu --release \\" >&2
    echo "    --features alsa-substrate \\" >&2
    echo "    -p org-evoframework-composition-alsa" >&2
    exit 2
fi

target="$1"
shift

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
eng_root="$(cd "$repo_root/../evo-core-eng" 2>/dev/null && pwd)" || {
    echo "error: sibling evo-core-eng not found at $repo_root/../evo-core-eng" >&2
    exit 1
}

# Mount the eng-line workspace root at the same path inside
# the container so cargo's path-dep workspace inheritance
# walk finds evo-core-eng/Cargo.toml.
export CROSS_CONTAINER_OPTS="-v $eng_root:$eng_root:ro"

cd "$repo_root"
exec cross build --target "$target" "$@"
