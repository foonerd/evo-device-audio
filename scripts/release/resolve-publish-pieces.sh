#!/usr/bin/env bash
# Resolve the publish-pieces.yml selector into a steward flag and
# a plugin x target matrix. Source of plugin names is
# dist/scripts/lib/oop-plugins.sh — the same list the installer
# tarball uses. Do not keep a second plugin list in this script.
#
# Usage:
#   scripts/release/resolve-publish-pieces.sh <scope> [plugin]
#
# scope: all | steward | plugin | dist
# plugin: org.evoframework.<name> (required when scope=plugin)
#
# Writes GitHub Actions outputs when GITHUB_OUTPUT is set:
#   build_steward=true|false
#   build_dist=true|false
#   plugin_count=<n>
#   plugin_matrix=<JSON>
#
# Always prints the same keys to stdout.

set -euo pipefail

SCOPE="${1:-}"
PLUGIN="${2:-}"

if [[ -z "${SCOPE}" ]]; then
    echo "usage: $0 <all|steward|plugin|dist> [plugin-name]" >&2
    exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
# shellcheck source=../../dist/scripts/lib/oop-plugins.sh
source "${REPO_ROOT}/dist/scripts/lib/oop-plugins.sh"

TARGETS=(
    x86_64-unknown-linux-gnu
    aarch64-unknown-linux-gnu
    armv7-unknown-linux-gnueabihf
)

plugin_version() {
    local name="$1"
    local manifest="${REPO_ROOT}/plugins/${name}/manifest.oop.toml"
    if [[ ! -f "${manifest}" ]]; then
        echo "resolve-publish-pieces: missing ${manifest}" >&2
        exit 1
    fi
    local version
    version="$(awk -F'"' '/^version *=/{print $2; exit}' "${manifest}")"
    if [[ -z "${version}" ]]; then
        echo "resolve-publish-pieces: no version in ${manifest}" >&2
        exit 1
    fi
    printf '%s' "${version}"
}

lookup_plugin() {
    local want="$1"
    local entry p_name p_crate p_wire p_features
    for entry in "${OOP_PLUGINS[@]}"; do
        IFS=':' read -r p_name p_crate p_wire p_features <<< "${entry}"
        if [[ "${p_name}" == "${want}" ]]; then
            printf '%s' "${entry}"
            return 0
        fi
    done
    return 1
}

SELECTED=()
BUILD_STEWARD=false
BUILD_DIST=false

case "${SCOPE}" in
    all)
        BUILD_STEWARD=true
        BUILD_DIST=true
        SELECTED=("${OOP_PLUGINS[@]}")
        ;;
    steward)
        BUILD_STEWARD=true
        ;;
    dist)
        BUILD_DIST=true
        ;;
    plugin)
        if [[ -z "${PLUGIN}" || "${PLUGIN}" == "none" ]]; then
            echo "resolve-publish-pieces: scope=plugin requires a plugin name" >&2
            exit 1
        fi
        entry="$(lookup_plugin "${PLUGIN}")" || {
            echo "resolve-publish-pieces: unknown plugin '${PLUGIN}' (not in oop-plugins.sh)" >&2
            exit 1
        }
        SELECTED=("${entry}")
        ;;
    *)
        echo "resolve-publish-pieces: scope must be all, steward, plugin, or dist (got '${SCOPE}')" >&2
        exit 1
        ;;
esac

# Build {"include":[...]} without jq so the runner image stays stock.
INCLUDE=""
for entry in "${SELECTED[@]+"${SELECTED[@]}"}"; do
    IFS=':' read -r p_name p_crate p_wire p_features <<< "${entry}"
    p_version="$(plugin_version "${p_name}")"
    for target in "${TARGETS[@]}"; do
        if [[ -n "${INCLUDE}" ]]; then
            INCLUDE+=","
        fi
        INCLUDE+=$(printf '{"name":"%s","crate":"%s","bin":"%s","features":"%s","version":"%s","target":"%s"}' \
            "${p_name}" "${p_crate}" "${p_wire}" "${p_features}" "${p_version}" "${target}")
    done
done

PLUGIN_COUNT=$(( ${#SELECTED[@]} * ${#TARGETS[@]} ))
if [[ ${#SELECTED[@]} -eq 0 ]]; then
    PLUGIN_COUNT=0
fi

MATRIX="{\"include\":[${INCLUDE}]}"

emit() {
    printf 'build_steward=%s\n' "${BUILD_STEWARD}"
    printf 'build_dist=%s\n' "${BUILD_DIST}"
    printf 'plugin_count=%s\n' "${PLUGIN_COUNT}"
    printf 'plugin_matrix=%s\n' "${MATRIX}"
}

emit
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    {
        printf 'build_steward=%s\n' "${BUILD_STEWARD}"
        printf 'build_dist=%s\n' "${BUILD_DIST}"
        printf 'plugin_count=%s\n' "${PLUGIN_COUNT}"
        printf 'plugin_matrix<<EOF\n%s\nEOF\n' "${MATRIX}"
    } >> "${GITHUB_OUTPUT}"
fi
