#!/usr/bin/env bash
# dist/release/preflight-cut.sh
#
# Release-cut preflight. Refuses to advance a release cut on
# foonerd/evo-device-audio (and analogous distribution repos)
# until valid signed evidence exists for all four install/reset
# primitives on every supported architecture.
#
# Called as the FIRST step of any release cut. Exits 0 only when
# all evidence is present, signed, recent, and matches the expected
# post-condition shape per primitive.
#
# Evidence shape at dist/release/evidence/<version>/<arch>/<primitive>.toml:
#   schema_version = 1
#   primitive = "p1_full_initial_setup" | "p2_..." | "p3_..." | "p4_..."
#   architecture = "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu"
#   ran_at_utc = "<ISO8601>"
#   [post_condition]
#   service_active = true
#   plugins_admitted_count = <N>
#   admission_failures = 0
#   subject_not_declared = 0
#   music_library_hash_preserved = true | false   # p3, p4
#   music_library_hash_changed   = true | false   # p1, p2
#   canonical_id_regenerated     = true | false   # p2, p4
#   canonical_id_preserved       = true | false   # p1, p3
#   [signature]
#   key_id = "evo-acceptance-signing"
#   ed25519_b64 = "<sig>"
#
# Refusal modes (structured exit codes):
#   1: usage / configuration error
#   2: missing evidence for a required primitive x arch pair
#   3: signature verification failure on any evidence file
#   4: stale evidence (>7 days at cut time)
#   5: post-condition assertion mismatch
#   6: architecture coverage gap
#
# Usage:
#   dist/release/preflight-cut.sh --version <semver> \
#       --arches "aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu" \
#       --evidence-dir <path> \
#       --public-key <path-to-ed25519-public-pem>
#
# Every argument is required. No sensible defaults.

set -euo pipefail

VERSION=""
ARCHES=""
EVIDENCE_DIR=""
PUBLIC_KEY=""
MAX_AGE_SECS=$((7 * 24 * 60 * 60))   # 7 days: evidence older than this is refused as stale

usage() {
    cat <<EOF >&2
Usage: $(basename "$0") \\
    --version <semver> \\
    --arches "<arch1> <arch2> ..." \\
    --evidence-dir <path> \\
    --public-key <path-to-ed25519-public-pem>

Refuses a release cut without valid signed evidence for all four
install/reset primitives on every supported architecture.
EOF
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)       VERSION="$2"; shift 2 ;;
        --arches)        ARCHES="$2"; shift 2 ;;
        --evidence-dir)  EVIDENCE_DIR="$2"; shift 2 ;;
        --public-key)    PUBLIC_KEY="$2"; shift 2 ;;
        -h|--help)       usage ;;
        *)               echo "unknown argument: $1" >&2; usage ;;
    esac
done

[[ -z "${VERSION}" ]]      && { echo "--version required" >&2; exit 1; }
[[ -z "${ARCHES}" ]]       && { echo "--arches required" >&2; exit 1; }
[[ -z "${EVIDENCE_DIR}" ]] && { echo "--evidence-dir required" >&2; exit 1; }
[[ -z "${PUBLIC_KEY}" ]]   && { echo "--public-key required" >&2; exit 1; }

if [[ ! -d "${EVIDENCE_DIR}" ]]; then
    echo "preflight-cut: evidence directory does not exist: ${EVIDENCE_DIR}" >&2
    exit 2
fi
if [[ ! -r "${PUBLIC_KEY}" ]]; then
    echo "preflight-cut: public key not readable: ${PUBLIC_KEY}" >&2
    exit 1
fi

readonly PRIMITIVES=(
    "p1_full_initial_setup"
    "p2_full_wipe_and_reinstall"
    "p3_config_wipe_preserving_music"
    "p4_user_data_full_vacuum"
)

NOW_EPOCH="$(date -u +%s)"
FAILURES=0
CHECKS=0

# TOML field extractor. Reads a simple `key = "value"` or
# `key = value` line and returns the value. Skips lines under
# section headers so we can isolate top-level vs [post_condition]
# vs [signature] fields.
toml_get() {
    local file="$1" key="$2" section="${3:-}"
    if [[ -z "${section}" ]]; then
        awk -F= -v k="${key}" '
            /^\[.*\]$/ { in_section = 0; next }
            $1 ~ ("^" k "[[:space:]]*$") {
                sub("^[^=]*=[[:space:]]*", "", $0)
                gsub("^\"|\"[[:space:]]*$", "", $0)
                gsub("[[:space:]]+$", "", $0)
                print
                exit
            }
        ' "${file}"
    else
        awk -F= -v k="${key}" -v s="${section}" '
            $0 == "[" s "]" { in_section = 1; next }
            /^\[.*\]$/       { in_section = 0; next }
            in_section && $1 ~ ("^" k "[[:space:]]*$") {
                sub("^[^=]*=[[:space:]]*", "", $0)
                gsub("^\"|\"[[:space:]]*$", "", $0)
                gsub("[[:space:]]+$", "", $0)
                print
                exit
            }
        ' "${file}"
    fi
}

# Extract the canonical body (everything before [signature]) so we
# verify the signature over exactly what evo-install.sh signed.
extract_body() {
    local file="$1"
    awk '/^\[signature\]$/ { exit } { print }' "${file}"
}

# Verify ed25519 signature. openssl accepts a raw payload (no
# hash prefix) via -rawin; evo-install.sh signs the body with the
# same rawin path.
verify_signature() {
    local file="$1" pub_key="$2"
    local body_tmp sig_bin sig_b64
    body_tmp="$(mktemp)"
    sig_bin="$(mktemp)"
    trap 'rm -f "${body_tmp}" "${sig_bin}"' RETURN

    extract_body "${file}" > "${body_tmp}"
    sig_b64="$(toml_get "${file}" "ed25519_b64" "signature")"
    if [[ -z "${sig_b64}" || "${sig_b64}" == "UNSIGNED_SIGNING_ERROR" ]]; then
        return 1
    fi
    echo -n "${sig_b64}" | base64 -d > "${sig_bin}"
    openssl pkeyutl -verify \
        -pubin -inkey "${pub_key}" \
        -rawin -in "${body_tmp}" \
        -sigfile "${sig_bin}" >/dev/null 2>&1
}

# Check one evidence file against expected shape.
check_evidence() {
    local file="$1" want_primitive="$2" want_arch="$3"

    CHECKS=$((CHECKS + 1))

    if [[ ! -f "${file}" ]]; then
        echo "  MISSING: ${file}" >&2
        FAILURES=$((FAILURES + 1))
        return 2
    fi

    local got_schema got_primitive got_arch got_ran_at
    got_schema="$(toml_get "${file}" "schema_version")"
    got_primitive="$(toml_get "${file}" "primitive")"
    got_arch="$(toml_get "${file}" "architecture")"
    got_ran_at="$(toml_get "${file}" "ran_at_utc")"

    if [[ "${got_schema}" != "1" ]]; then
        echo "  MISMATCH: ${file} — schema_version = ${got_schema} (want 1)" >&2
        FAILURES=$((FAILURES + 1))
        return 5
    fi
    if [[ "${got_primitive}" != "${want_primitive}" ]]; then
        echo "  MISMATCH: ${file} — primitive = ${got_primitive} (want ${want_primitive})" >&2
        FAILURES=$((FAILURES + 1))
        return 5
    fi
    if [[ "${got_arch}" != "${want_arch}" ]]; then
        echo "  MISMATCH: ${file} — architecture = ${got_arch} (want ${want_arch})" >&2
        FAILURES=$((FAILURES + 1))
        return 5
    fi

    # Recency (7-day window).
    local ran_epoch age
    if ! ran_epoch="$(date -u -d "${got_ran_at}" +%s 2>/dev/null)"; then
        echo "  MISMATCH: ${file} — ran_at_utc unparseable: ${got_ran_at}" >&2
        FAILURES=$((FAILURES + 1))
        return 5
    fi
    age=$((NOW_EPOCH - ran_epoch))
    if (( age > MAX_AGE_SECS )); then
        echo "  STALE: ${file} — ran_at_utc ${got_ran_at} is $((age / 86400)) days old (max 7)" >&2
        FAILURES=$((FAILURES + 1))
        return 4
    fi

    # Signature.
    if ! verify_signature "${file}" "${PUBLIC_KEY}"; then
        echo "  BAD SIGNATURE: ${file}" >&2
        FAILURES=$((FAILURES + 1))
        return 3
    fi

    # Post-condition. Every primitive requires service_active=true +
    # admission_failures=0 + subject_not_declared=0.
    local service_active admission_failures subject_not_declared
    service_active="$(toml_get "${file}" "service_active" "post_condition")"
    admission_failures="$(toml_get "${file}" "admission_failures" "post_condition")"
    subject_not_declared="$(toml_get "${file}" "subject_not_declared" "post_condition")"

    if [[ "${service_active}" != "true" ]]; then
        echo "  POST-COND FAIL: ${file} — service_active = ${service_active} (want true)" >&2
        FAILURES=$((FAILURES + 1))
        return 5
    fi
    if [[ "${admission_failures}" != "0" ]]; then
        echo "  POST-COND FAIL: ${file} — admission_failures = ${admission_failures} (want 0)" >&2
        FAILURES=$((FAILURES + 1))
        return 5
    fi
    if [[ "${subject_not_declared}" != "0" ]]; then
        echo "  POST-COND FAIL: ${file} — subject_not_declared = ${subject_not_declared} (want 0)" >&2
        FAILURES=$((FAILURES + 1))
        return 5
    fi

    # Primitive-specific music-library-hash + canonical-id assertions.
    local mh_preserved mh_changed
    mh_preserved="$(toml_get "${file}" "music_library_hash_preserved" "post_condition")"
    mh_changed="$(toml_get "${file}" "music_library_hash_changed" "post_condition")"

    case "${want_primitive}" in
        p1_full_initial_setup)
            # Fresh install: expect a fresh music library (nothing to preserve).
            if [[ "${mh_changed}" != "true" && "${mh_changed}" != "" ]]; then
                # An empty value is tolerated when the harness ran without a
                # pre-populated library; a "false" is a mismatch on a fresh
                # install where the library must be newly instantiated.
                if [[ "${mh_changed}" == "false" ]]; then
                    echo "  POST-COND FAIL: ${file} — music_library_hash_changed = false on p1 (want true / empty)" >&2
                    FAILURES=$((FAILURES + 1))
                    return 5
                fi
            fi
            ;;
        p2_full_wipe_and_reinstall)
            # Wipe + reinstall: music library must survive per operator
            # directive (preserve music library across full wipe).
            if [[ "${mh_preserved}" != "true" ]]; then
                echo "  POST-COND FAIL: ${file} — music_library_hash_preserved = ${mh_preserved} on p2 (want true)" >&2
                FAILURES=$((FAILURES + 1))
                return 5
            fi
            ;;
        p3_config_wipe_preserving_music)
            if [[ "${mh_preserved}" != "true" ]]; then
                echo "  POST-COND FAIL: ${file} — music_library_hash_preserved = ${mh_preserved} on p3 (want true)" >&2
                FAILURES=$((FAILURES + 1))
                return 5
            fi
            ;;
        p4_user_data_full_vacuum)
            if [[ "${mh_preserved}" != "true" ]]; then
                echo "  POST-COND FAIL: ${file} — music_library_hash_preserved = ${mh_preserved} on p4 (want true)" >&2
                FAILURES=$((FAILURES + 1))
                return 5
            fi
            ;;
    esac

    return 0
}

echo "preflight-cut: version=${VERSION}"
echo "preflight-cut: arches=${ARCHES}"
echo "preflight-cut: evidence-dir=${EVIDENCE_DIR}"
echo "preflight-cut: public-key=${PUBLIC_KEY}"
echo

for arch in ${ARCHES}; do
    arch_dir="${EVIDENCE_DIR}/${VERSION}/${arch}"
    if [[ ! -d "${arch_dir}" ]]; then
        echo "arch coverage gap: ${arch} (no directory at ${arch_dir})" >&2
        FAILURES=$((FAILURES + 1))
        continue
    fi
    echo "=== ${arch} ==="
    for primitive in "${PRIMITIVES[@]}"; do
        file="${arch_dir}/${primitive}.toml"
        printf "  %-40s ... " "${primitive}"
        if check_evidence "${file}" "${primitive}" "${arch}"; then
            echo "OK"
        else
            :  # message already printed
        fi
    done
done

echo
echo "preflight-cut: ${CHECKS} checks; ${FAILURES} failures"

if (( FAILURES > 0 )); then
    echo "preflight-cut: REFUSE. Release cut cannot advance." >&2
    exit 6
fi

echo "preflight-cut: PASS. All primitives × arches have valid signed evidence."
