#!/usr/bin/env bash
# set-timezone.test.sh — regression test for the distribution
# timezone privilege path: the root-owned wrapper that runs
# timedatectl set-timezone, and the narrow sudoers fragment that
# grants the steward tenant that one command.
#
# The steward runs as the install-time service user and is never
# root, so timezone.set reaches the host through
# `sudo -n /usr/local/bin/evo-set-timezone <zone>`. The sudoers
# grant names the wrapper with no argument list, so the wrapper is
# the whole argument check: one zone, the shape of an IANA name,
# not `Factory`, listed by the host, and a TZif file. Anything
# else must not reach timedatectl.
#
# The wrapper under test is the shipped file with its timedatectl
# and zone database redirected at fixtures. The substitution is
# asserted before the copy is ever executed: if a shipped path
# drifts out of the sed pattern, this test stops rather than
# running the host's timedatectl.
#
# Invoke from anywhere:
#
#   bash dist/scripts/tests/set-timezone.test.sh
#
# Exit codes:
#   0 — every case passed.
#   1 — at least one case failed, or a named gate could not run.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
WRAPPER="$DIST_DIR/bin/evo-set-timezone"
SUDOERS="$DIST_DIR/sudoers.d/evo-set-timezone.in"

# The tenant substituted into the fragment for the visudo gate.
# sudoers parses an unresolvable user name, so this deliberately
# does not have to exist on the host running the test.
readonly DUMMY_SERVICE_USER="evo-set-timezone-test-tenant"

PASS=0
FAIL=0

ok() {
    PASS=$((PASS + 1))
    echo "PASS  $1"
}

bad() {
    FAIL=$((FAIL + 1))
    echo "FAIL  $1" >&2
}

refuses() {
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then
        bad "$name (exited 0)"
    else
        ok "$name"
    fi
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# ----------------------------------------------------------
# Shipped-file shape.
# ----------------------------------------------------------
if [[ -x "$WRAPPER" ]]; then
    ok "wrapper is executable"
else
    bad "wrapper is not executable: $WRAPPER"
fi

if grep -q 'TIMEDATECTL="/usr/bin/timedatectl"' "$WRAPPER" \
    && grep -q 'ZONEINFO="/usr/share/zoneinfo"' "$WRAPPER"; then
    ok "wrapper names the host's timedatectl and zone database"
else
    bad "wrapper does not name /usr/bin/timedatectl and /usr/share/zoneinfo"
fi

if grep -qF "exec \"\$TIMEDATECTL\" set-timezone -- \"\$1\"" "$WRAPPER"; then
    ok "wrapper ends in timedatectl set-timezone with its one argument"
else
    bad "wrapper does not exec timedatectl set-timezone -- \"\$1\""
fi

if grep -q 'Cmnd_Alias EVO_SET_TIMEZONE = /usr/local/bin/evo-set-timezone' "$SUDOERS"; then
    ok "sudoers names one enumerated command"
else
    bad "sudoers Cmnd_Alias is missing or not enumerated"
fi

# Every line that is not a comment or blank, exactly: the alias and
# one grant of it. Anything added to either line widens the grant.
expected_rules=$'Cmnd_Alias EVO_SET_TIMEZONE = /usr/local/bin/evo-set-timezone\n@EVO_SERVICE_USER@ ALL=(root) NOPASSWD: EVO_SET_TIMEZONE'
if [[ "$(grep -vE '^[[:space:]]*(#|$)' "$SUDOERS")" == "$expected_rules" ]]; then
    ok "sudoers grants the substituted tenant that alias only"
else
    bad "sudoers grant line is missing or widened"
fi

# ----------------------------------------------------------
# The visudo gate.
# ----------------------------------------------------------
VISUDO=""
if command -v visudo >/dev/null 2>&1; then
    VISUDO="$(command -v visudo)"
elif [[ -x /usr/sbin/visudo ]]; then
    VISUDO=/usr/sbin/visudo
fi

if [[ -z "$VISUDO" ]]; then
    bad "visudo not found — the sudoers gate could not run"
else
    if [[ -n "$(tail -c1 "$SUDOERS")" ]]; then
        bad "sudoers template does not end with a newline"
    else
        ok "sudoers template ends with a newline"
    fi

    rendered="$WORK/evo-set-timezone"
    sed "s|@EVO_SERVICE_USER@|$DUMMY_SERVICE_USER|g" "$SUDOERS" > "$rendered"
    if grep -q '@EVO_SERVICE_USER@' "$rendered"; then
        bad "sudoers render left @EVO_SERVICE_USER@ unsubstituted"
    else
        ok "sudoers renders with the service user substituted"
    fi

    if "$VISUDO" -c -f "$rendered" >/dev/null 2>&1; then
        ok "rendered sudoers fragment passes visudo -c -f"
    else
        bad "rendered sudoers fragment fails visudo -c -f"
    fi

    # Control: visudo must be shown able to reject before its
    # acceptance above counts for anything.
    malformed="$WORK/malformed-sudoers"
    printf 'Cmnd_Alias = = =\nthis is not a sudoers file\n' > "$malformed"
    refuses "visudo -c -f rejects a malformed fragment" \
        "$VISUDO" -c -f "$malformed"
fi

# ----------------------------------------------------------
# The wrapper itself, against a fixture timedatectl and zone
# database.
# ----------------------------------------------------------
record="$WORK/set-timezone.record"
fake="$WORK/timedatectl"
cat > "$fake" <<EOF
#!/usr/bin/env bash
case "\$1" in
    list-timezones)
        [[ -e "$WORK/list-fails" ]] && exit 1
        printf '%s\n' Etc/UTC Europe/London America/Argentina/Buenos_Aires \
            Factory Listed/NoFile Bad/Magic
        ;;
    set-timezone) printf '%s\n' "\$*" > "$record" ;;
    *) exit 2 ;;
esac
EOF
chmod 0755 "$fake"

zoneinfo="$WORK/zoneinfo"
for zone in Etc/UTC Europe/London America/Argentina/Buenos_Aires \
        Factory Not/Listed; do
    mkdir -p "$zoneinfo/$(dirname "$zone")"
    printf 'TZif2\0\0\0' > "$zoneinfo/$zone"
done
mkdir -p "$zoneinfo/Bad"
printf 'not a zone file' > "$zoneinfo/Bad/Magic"

test_wrapper="$WORK/evo-set-timezone.fixture"
sed -e "s|/usr/bin/timedatectl|$fake|" \
    -e "s|/usr/share/zoneinfo|$zoneinfo|" "$WRAPPER" > "$test_wrapper"
chmod 0755 "$test_wrapper"

# Refuse to run a copy that still points at the host.
if grep -qE '/usr/bin/timedatectl|/usr/share/zoneinfo' "$test_wrapper"; then
    bad "fixture redirect failed — copy still names the host's timedatectl or zone database"
    echo "summary: $PASS passed, $FAIL failed" >&2
    exit 1
fi
ok "fixture redirect replaced the host's timedatectl and zone database"

applies() {
    local zone="$1"
    rm -f "$record"
    if ! "$test_wrapper" "$zone" >/dev/null 2>&1; then
        bad "$zone: wrapper exited non-zero"
    elif [[ "$(cat "$record" 2>/dev/null)" == "set-timezone -- $zone" ]]; then
        ok "$zone is set through timedatectl set-timezone"
    else
        bad "$zone: timedatectl saw '$(cat "$record" 2>/dev/null)'"
    fi
}

# A refusal is a non-zero exit and timedatectl set-timezone never
# runs.
not_applied() {
    local name="$1"; shift
    rm -f "$record"
    if "$test_wrapper" "$@" >/dev/null 2>&1; then
        bad "$name (exited 0)"
    elif [[ -e "$record" ]]; then
        bad "$name reached timedatectl: $(cat "$record")"
    else
        ok "$name"
    fi
}

applies Europe/London
applies Etc/UTC
applies America/Argentina/Buenos_Aires

not_applied "no argument is refused"
not_applied "two arguments are refused" Europe/London Etc/UTC
not_applied "an empty zone is refused" ""
not_applied "Factory is refused" Factory
not_applied "a dot-dot path is refused" ../Europe/London
not_applied "a dot-dot component is refused" Europe/../London
not_applied "an absolute path is refused" /Europe/London
not_applied "an option riding the zone is refused" "Europe/London -H host"
not_applied "a zone the host does not list is refused" Not/Listed
not_applied "a listed zone with no file is refused" Listed/NoFile
not_applied "a listed file that is not TZif is refused" Bad/Magic

touch "$WORK/list-fails"
not_applied "a zone list that cannot be read is a refusal" Europe/London
rm -f "$WORK/list-fails"

echo "summary: $PASS passed, $FAIL failed"
if [[ $FAIL -eq 0 ]]; then
    echo "set-timezone.test.sh: passed"
    exit 0
fi
exit 1
