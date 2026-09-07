#!/usr/bin/env bash
#
# check-commit-message-positive.sh — prove the commit-message gate
# fires.
#
# A clean run of check-commit-message.sh is not evidence. It is a
# negative from an instrument nobody has shown can report a
# positive, and a pattern that matches nothing passes every message
# ever written. One wrong escape is all it takes: a doubled
# backslash inside the single-quoted pattern turns an escaped
# parenthesis into a literal backslash and a group, and the
# alternative then matches nothing at all while still looking
# present.
#
# This reads the alternatives out of the gate's own pattern, plants
# a sample for each one in an otherwise clean message, and asserts
# the gate refuses it and names the planted text. An alternative
# added to the gate without a sample here fails this control rather
# than passing unproven — the same property the file-gate control
# has, arrived at the same way.
#
# Nothing is committed and no history is read: the gate is handed
# message files in a temporary directory.
#
# Exits 0 only when every alternative is proved and a clean message
# still passes.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GATE="${REPO_ROOT}/scripts/preflight/check-commit-message.sh"
if [[ ! -x "${GATE}" ]]; then
    echo "message-positive: gate missing: ${GATE}" >&2
    exit 1
fi

# The gate's pattern, read from the gate so the two cannot drift.
PATTERN="$(sed -n "s/^PATTERN='\(.*\)'\$/\1/p" "${GATE}")"
if [[ -z "${PATTERN}" ]]; then
    echo "message-positive: no PATTERN found in ${GATE}" >&2
    exit 1
fi

# Alternatives are split on `|`, which is only sound while no
# alternative carries a group of its own. An unescaped `(` would
# mean a `|` inside it belongs to that group rather than separating
# alternatives, and the split would produce fragments that match
# nothing. Fail rather than split unsoundly.
if printf '%s' "${PATTERN}" | grep -qE '(^|[^\])\('; then
    echo "message-positive: the pattern carries an unescaped group;" >&2
    echo "splitting it on '|' would be unsound. Either escape the" >&2
    echo "parenthesis or teach this control to parse groups." >&2
    exit 1
fi

# Sample text for each alternative, as two parallel lists rather
# than a `case`: a case label is a glob, and alternatives carry
# `[0-9]` and other characters a glob would reinterpret.
#
# Keys are the alternatives exactly as they appear in the gate.
SAMPLE_KEYS=(
    '192\.168\.30\.[0-9]{1,3}'
    'pi5target'
    'x64proto'
    'nucproto'
    'evoproto@'
    'andser@'
    'andrew@dt-ltd'
    'SESSION_LOG [0-9]{4}-'
    '\bADR-[0-9]{3,}'
    '\bR-[0-9]{3,}'
    '\bPD-[0-9]{3,}'
    'M\(edia\) Spot'
    'evo-d674'
    'first cut'
    'swap later'
    'deferred to later'
    'follow-on release'
    'later release'
    'future release'
)
SAMPLE_VALS=(
    '192.168.30.24'
    'pi5target'
    'x64proto'
    'nucproto'
    'evoproto@box'
    'andser@example.test'
    'andrew@dt-ltd.example'
    'SESSION_LOG 2026-'
    'ADR-0161'
    'R-042'
    'PD-017'
    'M(edia) Spot'
    'evo-d674'
    'first cut'
    'swap later'
    'deferred to later'
    'follow-on release'
    'later release'
    'future release'
)

sample_for() {
    local want="$1" i
    for i in "${!SAMPLE_KEYS[@]}"; do
        # POSIX string equality: `[[ = ]]` would glob the right side.
        if [ "${SAMPLE_KEYS[$i]}" = "${want}" ]; then
            printf '%s' "${SAMPLE_VALS[$i]}"
            return 0
        fi
    done
    return 1
}

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/message-positive.XXXXXX")"
cleanup() { rm -rf "${WORKDIR}"; }
trap cleanup EXIT

# A message with nothing to find, used both as the carrier for each
# plant and as the unplanted case.
CLEAN_SUBJECT='fix(scope): describe the change'
CLEAN_BODY='Ordinary prose that names no host, no account and no
internal identifier.'

write_message() {
    local extra="$1"
    {
        printf '%s\n\n%s\n' "${CLEAN_SUBJECT}" "${CLEAN_BODY}"
        if [[ -n "${extra}" ]]; then
            printf '\n%s\n' "${extra}"
        fi
    } >"${WORKDIR}/message.txt"
}

# The unplanted case first: if a clean message is refused, every
# assertion below would pass for the wrong reason.
write_message ''
if ! bash "${GATE}" "${WORKDIR}/message.txt" >"${WORKDIR}/clean.out" 2>&1
then
    echo "message-positive: a clean message was REFUSED." >&2
    cat "${WORKDIR}/clean.out" >&2
    exit 1
fi

PROVED=0
IFS='|' read -r -a ALTERNATIVES <<<"${PATTERN}"
for alt in "${ALTERNATIVES[@]}"; do
    [[ -z "${alt}" ]] && continue
    if ! sample="$(sample_for "${alt}")"; then
        echo "message-positive: no plant for: ${alt}" >&2
        echo "The gate defines this alternative and nothing here" >&2
        echo "triggers it, so it would pass unproven. Add a sample" >&2
        echo "to SAMPLE_KEYS / SAMPLE_VALS." >&2
        exit 1
    fi
    write_message "${sample}"
    if bash "${GATE}" "${WORKDIR}/message.txt" >"${WORKDIR}/planted.out" 2>&1
    then
        echo "message-positive: planted '${sample}' was ACCEPTED." >&2
        echo "The alternative ${alt} matches nothing." >&2
        cat "${WORKDIR}/planted.out" >&2
        exit 1
    fi
    # Refusing is not enough on its own: the gate must have refused
    # because of the planted text, not because of something else in
    # the carrier message.
    if ! grep -Fq "${sample}" "${WORKDIR}/planted.out"; then
        echo "message-positive: ${alt} refused the message without" >&2
        echo "naming the planted text '${sample}'." >&2
        cat "${WORKDIR}/planted.out" >&2
        exit 1
    fi
    PROVED=$((PROVED + 1))
done

if [[ "${PROVED}" -eq 0 ]]; then
    echo "message-positive: no alternatives were proved." >&2
    exit 1
fi

echo "message-positive: ${PROVED} alternatives caught; clean message passes."
exit 0
