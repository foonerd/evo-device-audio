#!/usr/bin/env bash
# plugin-admission-verdict.test.sh — a deploy fails when a plugin it
# shipped did not admit.
#
# The class this exists for: deploy-distribution.sh counted any
# journal line naming a plugin as its admit, so
# `skipping plugin org.evoframework.alarm: admission failed (...)`
# printed `[ok] ... discovered + admitted`, and a plugin with no
# line at all was only a WARN. Two plugins shipped dead behind a
# green verify.
#
# The decision lives in dist/scripts/lib/plugin-admission-verdict.sh
# and is driven here directly. The deploy's own step 7 is then
# extracted from the shipped script and run with `ssh` answered from
# fixtures, so the exit status asserted is the deploy's. The smoke
# gate's websockets prerequisite is checked the same way.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
DEPLOY="$REPO_ROOT/dist/scripts/deploy-distribution.sh"
SMOKE="$REPO_ROOT/scripts/preflight/track-detail-smoke.py"

PASS=0
FAIL=0
ok()  { echo "PASS  $1"; PASS=$((PASS + 1)); }
bad() { echo "FAIL  $1"; FAIL=$((FAIL + 1)); }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# shellcheck source=../lib/plugin-admission-verdict.sh
. "$REPO_ROOT/dist/scripts/lib/plugin-admission-verdict.sh"

# The refusal a steward logged for the unquoted alarm file.
REFUSED_ALARM='skipping plugin org.evoframework.alarm: admission failed (admission error: org.evoframework.alarm: parse privileges.yaml at /opt/evo/plugins/org.evoframework.alarm/privileges.yaml: YAML parse error: mapping values are not allowed in this context at line 17 column 310)'
NAMES=(org.evoframework.network org.evoframework.network.shares org.evoframework.alarm)
ALL="$(printf '%s\n' "${NAMES[@]}")"

verdict() {
    local rc=0
    OUT="$(plugin_admission_verdict "$1" "$2" "${NAMES[@]}")" || rc=$?
    return "$rc"
}

# ----- the decision -----

if verdict "evo ready" "$ALL"; then
    ok "every plugin with a socket and no refusal passes"
else
    bad "a clean start failed: $OUT"
fi

if ! verdict "$REFUSED_ALARM" "$(grep -v alarm <<<"$ALL")" \
    && grep -q '\[FAIL\] plugin org.evoframework.alarm refused at admission (admission error' <<<"$OUT"; then
    ok "the logged alarm refusal fails the deploy and names alarm with its reason"
else
    bad "the logged alarm refusal did not fail: $OUT"
fi

if ! verdict "$REFUSED_ALARM" "$ALL" && grep -q '\[FAIL\] plugin org.evoframework.alarm refused' <<<"$OUT"; then
    ok "a refusal fails even when a socket is present"
else
    bad "a refusal with a socket passed: $OUT"
fi

if ! verdict "evo ready" "$(grep -v alarm <<<"$ALL")" \
    && grep -q '\[FAIL\] plugin org.evoframework.alarm not admitted' <<<"$OUT"; then
    ok "a missing admit is a fail, not a warning"
else
    bad "a missing admit did not fail: $OUT"
fi

if ! verdict "skipping plugin org.evoframework.network.shares: admission failed (x)" "$ALL" \
    && grep -q '\[ok\]  plugin org.evoframework.network admitted' <<<"$OUT" \
    && grep -q '\[FAIL\] plugin org.evoframework.network.shares refused' <<<"$OUT"; then
    ok "a refusal of network.shares does not fail network"
else
    bad "a plugin name prefix crossed over: $OUT"
fi

if [[ "$(grep -c '^  \[' <<<"$OUT")" -eq "${#NAMES[@]}" ]]; then
    ok "one verdict line per shipped plugin"
else
    bad "verdict lines: $OUT"
fi

# ----- the deploy's step 7, run with ssh answered from fixtures -----

STEP7="$(awk '/^echo "\[7\/7\] verify \.\.\."$/ { on = 1 } on && /^# Deploy gate: track_detail/ { exit } on' "$DEPLOY")"
if [[ -n "$STEP7" ]] && grep -q 'plugin_admission_verdict' <<<"$STEP7"; then
    ok "step 7 is extracted and calls the verdict"
else
    bad "could not extract step 7, or it does not call the verdict"
fi

mkdir -p "$WORK/bin"
cat > "$WORK/bin/ssh" <<'EOF'
#!/usr/bin/env bash
cmd="${*: -1}"
case "$cmd" in
    *"systemctl is-active evo"*) echo active ;;
    *"grep -cE"*) echo 1 ;;
    *"/run/evo/plugins"*) cat "$FAKE_SOCKETS" ;;
    *"journalctl"*"-o cat"*) cat "$FAKE_JOURNAL" ;;
    *) echo "unexpected ssh: $cmd" >&2; exit 99 ;;
esac
EOF
chmod 0755 "$WORK/bin/ssh"

# shellcheck source=../lib/oop-plugins.sh
. "$REPO_ROOT/dist/scripts/lib/oop-plugins.sh"
SHIPPED="$(for e in "${OOP_PLUGINS[@]}"; do echo "${e%%:*}"; done)"

run_step7() {
    printf '%s\n' "$2" > "$WORK/sockets"
    printf '%s\n' "$1" > "$WORK/journal"
    {
        echo 'set -euo pipefail'
        printf 'REPO_ROOT=%q\nSSH_TARGET=operator@192.0.2.1\nSTEWARD_STARTED_AT="2026-10-30 08:13:40"\n' "$REPO_ROOT"
        printf '. %q\n' "$REPO_ROOT/dist/scripts/lib/oop-plugins.sh"
        printf '%s\n' "$STEP7"
    } > "$WORK/step7.sh"
    STEP7_RC=0
    STEP7_OUT="$(PATH="$WORK/bin:$PATH" FAKE_SOCKETS="$WORK/sockets" FAKE_JOURNAL="$WORK/journal" \
        bash "$WORK/step7.sh" 2>&1)" || STEP7_RC=$?
}

run_step7 "evo ready" "$SHIPPED"
if [[ "$STEP7_RC" -eq 0 ]] && [[ "$(grep -c '\[ok\]  plugin .* admitted' <<<"$STEP7_OUT")" -eq "${#OOP_PLUGINS[@]}" ]]; then
    ok "deploy step 7 passes when all ${#OOP_PLUGINS[@]} shipped plugins admit"
else
    bad "deploy step 7 on a clean start: rc=$STEP7_RC $STEP7_OUT"
fi

run_step7 "$REFUSED_ALARM" "$(grep -v '^org.evoframework.alarm$' <<<"$SHIPPED")"
if [[ "$STEP7_RC" -eq 4 ]] && grep -q '\[FAIL\] plugin org.evoframework.alarm refused at admission' <<<"$STEP7_OUT" \
    && ! grep -q '\[ok\].*org.evoframework.alarm' <<<"$STEP7_OUT"; then
    ok "deploy step 7 exits 4 on the logged alarm refusal"
else
    bad "deploy step 7 on the logged alarm refusal: rc=$STEP7_RC $STEP7_OUT"
fi

run_step7 "evo ready" "$(grep -v '^org.evoframework.identity.locale$' <<<"$SHIPPED")"
if [[ "$STEP7_RC" -eq 4 ]] && grep -q '\[FAIL\] plugin org.evoframework.identity.locale not admitted' <<<"$STEP7_OUT"; then
    ok "deploy step 7 exits 4 on a missing admit"
else
    bad "deploy step 7 on a missing admit: rc=$STEP7_RC $STEP7_OUT"
fi

if grep -qE "grep -cE 'plugin\.\*|no discovery / admit signal" "$DEPLOY"; then
    bad "the name-anywhere admit count or its WARN is still in the deploy"
else
    ok "the name-anywhere admit count and its WARN are gone"
fi

# ----- the smoke gate's websockets prerequisite -----

if grep -qxF '# dependencies = ["websockets>=10"]' "$SMOKE" && grep -qxF '# /// script' "$SMOKE"; then
    ok "the smoke script declares websockets>=10 in its script header"
else
    bad "the smoke script does not declare websockets"
fi

check_line="$(grep -n "import sys, websockets" "$DEPLOY" | head -n 1 | cut -d: -f1)"
stop_line="$(grep -n 'sudo -n systemctl stop evo' "$DEPLOY" | head -n 1 | cut -d: -f1)"
build_line="$(grep -n '^echo "\[1/7\]' "$DEPLOY" | cut -d: -f1)"
if [[ -n "$check_line" && -n "$stop_line" && -n "$build_line" \
        && "$check_line" -lt "$build_line" && "$build_line" -lt "$stop_line" ]]; then
    ok "the websockets check runs in pre-flight, before the build and before evo is stopped"
else
    bad "the websockets check is not in pre-flight (check=$check_line build=$build_line stop=$stop_line)"
fi

CHECK="$(awk '/^if ! python3 -c .import sys, websockets/ { on = 1 } on { print } on && /^fi$/ { exit }' "$DEPLOY")"
for case in refuse accept; do
    mkdir -p "$WORK/py-$case"
    if [[ "$case" == refuse ]]; then
        printf '#!/bin/sh\necho "ModuleNotFoundError: No module named websockets" >&2\nexit 1\n' > "$WORK/py-$case/python3"
    else
        printf '#!/bin/sh\nexit 0\n' > "$WORK/py-$case/python3"
    fi
    chmod 0755 "$WORK/py-$case/python3"
    rc=0
    out="$(PATH="$WORK/py-$case:$PATH" bash -c "$CHECK" 2>&1)" || rc=$?
    if [[ "$case" == refuse && "$rc" -eq 1 ]] && grep -q 'cannot import websockets>=10' <<<"$out"; then
        ok "a python3 without websockets refuses with exit 1"
    elif [[ "$case" == accept && "$rc" -eq 0 ]]; then
        ok "a python3 with websockets passes the check"
    else
        bad "websockets check ($case): rc=$rc $out"
    fi
done

echo ""
echo "plugin-admission-verdict.test.sh: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
