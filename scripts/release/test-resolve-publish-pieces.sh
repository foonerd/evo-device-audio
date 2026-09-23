#!/usr/bin/env bash
# test-resolve-publish-pieces.sh — selector contract for
# publish-pieces.yml. all / steward / plugin / dist must keep
# their previous flags. kiosk is an added scope and is not
# implied by all.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RESOLVE="${ROOT}/scripts/release/resolve-publish-pieces.sh"
# shellcheck source=../../dist/scripts/lib/oop-plugins.sh
source "${ROOT}/dist/scripts/lib/oop-plugins.sh"

fail() { printf 'test-resolve FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'test-resolve OK: %s\n' "$*" >&2; }

field() {
    local blob="$1" key="$2"
    printf '%s\n' "${blob}" | awk -F= -v k="${key}" '$1==k {print $2; exit}'
}

EXPECT_PLUGIN_COUNT=$(( ${#OOP_PLUGINS[@]} * 3 ))
SAMPLE_PLUGIN="${OOP_PLUGINS[0]%%:*}"

out="$(bash "${RESOLVE}" all)"
[[ "$(field "${out}" build_steward)" == "true" ]] || fail "all: build_steward"
[[ "$(field "${out}" build_dist)" == "true" ]] || fail "all: build_dist"
[[ "$(field "${out}" build_kiosk)" == "false" ]] || fail "all must not mint kiosk"
[[ "$(field "${out}" plugin_count)" == "${EXPECT_PLUGIN_COUNT}" ]] || fail "all: plugin_count"
pass "all steward+dist, kiosk false, plugin_count=${EXPECT_PLUGIN_COUNT}"

out="$(bash "${RESOLVE}" steward)"
[[ "$(field "${out}" build_steward)" == "true" ]] || fail "steward: build_steward"
[[ "$(field "${out}" build_dist)" == "false" ]] || fail "steward: build_dist"
[[ "$(field "${out}" build_kiosk)" == "false" ]] || fail "steward: build_kiosk"
[[ "$(field "${out}" plugin_count)" == "0" ]] || fail "steward: plugin_count"
pass "steward only"

out="$(bash "${RESOLVE}" dist)"
[[ "$(field "${out}" build_steward)" == "false" ]] || fail "dist: build_steward"
[[ "$(field "${out}" build_dist)" == "true" ]] || fail "dist: build_dist"
[[ "$(field "${out}" build_kiosk)" == "false" ]] || fail "dist: build_kiosk"
[[ "$(field "${out}" plugin_count)" == "0" ]] || fail "dist: plugin_count"
pass "dist only"

out="$(bash "${RESOLVE}" kiosk)"
[[ "$(field "${out}" build_steward)" == "false" ]] || fail "kiosk: build_steward"
[[ "$(field "${out}" build_dist)" == "false" ]] || fail "kiosk: build_dist"
[[ "$(field "${out}" build_kiosk)" == "true" ]] || fail "kiosk: build_kiosk"
[[ "$(field "${out}" plugin_count)" == "0" ]] || fail "kiosk: plugin_count"
pass "kiosk only"

out="$(bash "${RESOLVE}" plugin "${SAMPLE_PLUGIN}")"
[[ "$(field "${out}" build_steward)" == "false" ]] || fail "plugin: build_steward"
[[ "$(field "${out}" build_dist)" == "false" ]] || fail "plugin: build_dist"
[[ "$(field "${out}" build_kiosk)" == "false" ]] || fail "plugin: build_kiosk"
[[ "$(field "${out}" plugin_count)" == "3" ]] || fail "plugin: plugin_count"
pass "plugin ${SAMPLE_PLUGIN} x3"

set +e
bash "${RESOLVE}" >/tmp/resolve-empty.err 2>&1
rc=$?
set -e
[[ "${rc}" -eq 2 ]] || fail "empty scope rc=${rc} want 2"
pass "empty scope rc=2"

set +e
bash "${RESOLVE}" no-such-scope >/tmp/resolve-bad.err 2>&1
rc=$?
set -e
[[ "${rc}" -eq 1 ]] || fail "unknown scope rc=${rc} want 1"
pass "unknown scope rc=1"

printf 'test-resolve: all cases passed\n' >&2
