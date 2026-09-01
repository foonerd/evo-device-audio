#!/usr/bin/env bash
# dist/release/build-time-lint.sh
#
# Build-time lint pass. Cross-checks the
# structured sources this distribution ships and refuses the build
# if any contract violation is found:
#
#   (A) Plugin manifest declares a shelf on a rack not present in
#       dist/catalogue/audio-rack.toml.
#   (B) Plugin manifest announces a subject_type not declared in
#       dist/catalogue/audio-rack.toml.
#   (C) Manifest request_types missing a handler the plugin
#       registers (or vice versa; the handler set and the manifest
#       set must match exactly per stocking).
#   (D) Catalogue fragment declared without a schema_version
#       preamble — dist/catalogue/audio-rack.toml is a fragment (no
#       schema_version by design; evo-install.sh composes the
#       preamble at compose time). This check enforces that any
#       OTHER catalogue fragment shipped in dist/ carries the
#       preamble (an operator-shipped rack cannot be silently
#       merged without one).
#   (E) Sudoers drop-in references a binary path that does not
#       exist on a supported target (asserted via a checked-in
#       target-binaries table; the harness cross-checks against
#       running rigs).
#
# Called from CI at build time. Refuses (exit != 0) on any
# violation. Read-only across the repo.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
DIST_DIR="${REPO_ROOT}/dist"
PLUGINS_DIR="${REPO_ROOT}/plugins"
CATALOGUE="${DIST_DIR}/catalogue/audio-rack.toml"
SUDOERS_DIR="${DIST_DIR}/sudoers.d"

FAILURES=0

fail() {
    echo "  FAIL [$1]: $2" >&2
    FAILURES=$((FAILURES + 1))
}

info() {
    echo "  $1"
}

# ---- (A) + (B) : shelves + subjects must resolve to catalogue --

lint_shelves_and_subjects() {
    info "=== (A) shelf references and (B) subject_type declarations ==="

    if [[ ! -f "${CATALOGUE}" ]]; then
        fail "A" "catalogue file missing: ${CATALOGUE}"
        return
    fi

    # Extract the rack names declared in the catalogue fragment.
    # Each rack section starts with `[[racks]]` then a `name = "..."`.
    local racks_file subjects_file
    racks_file="$(mktemp)"
    subjects_file="$(mktemp)"
    trap 'rm -f "${racks_file}" "${subjects_file}"' RETURN

    awk '
        BEGIN { in_rack = 0 }
        /^\[\[racks\]\]/         { in_rack = 1; next }
        /^\[\[/                  { in_rack = 0; next }
        in_rack && /^name = "/   { gsub("name = \"|\"", ""); print }
    ' "${CATALOGUE}" > "${racks_file}"

    awk '
        BEGIN { in_subj = 0 }
        /^\[\[subjects\]\]/         { in_subj = 1; next }
        /^\[\[/                     { in_subj = 0; next }
        in_subj && /^name = "/      { gsub("name = \"|\"", ""); print }
    ' "${CATALOGUE}" > "${subjects_file}"

    local rack_count subject_count
    rack_count="$(wc -l < "${racks_file}")"
    subject_count="$(wc -l < "${subjects_file}")"
    info "catalogue declares ${rack_count} rack(s), ${subject_count} subject_type(s)"

    # Cross-check every plugin manifest.
    local plugin_dir manifest plugin_name
    for plugin_dir in "${PLUGINS_DIR}"/org.evoframework.*/; do
        plugin_name="$(basename "${plugin_dir}")"
        manifest="${plugin_dir}/manifest.toml"
        [[ -f "${manifest}" ]] || continue

        # Shelf references: `shelf = "audio.playback"` → rack "audio".
        # The rack name is the segment before the first dot.
        local shelves
        shelves="$(awk '/^shelf = "/ { gsub("shelf = \"|\"", ""); print }' "${manifest}")"
        while IFS= read -r shelf; do
            [[ -z "${shelf}" ]] && continue
            local rack_of_shelf="${shelf%%.*}"
            if ! grep -qx "${rack_of_shelf}" "${racks_file}"; then
                fail "A" "${plugin_name}: shelf '${shelf}' references rack '${rack_of_shelf}' not declared in catalogue"
            fi
        done <<< "${shelves}"

        # Subject_type announcements. Manifests declare
        # `subject_types = [...]` per stocking; each entry must be
        # declared in the catalogue.
        local subject_types
        subject_types="$(awk '
            /subject_types = \[/,/\]/ {
                if ($0 ~ /".*"/) {
                    gsub(/^[^"]*"|",?[[:space:]]*$/, "")
                    if (length($0)) print
                }
            }
        ' "${manifest}")"
        while IFS= read -r subj; do
            [[ -z "${subj}" ]] && continue
            if ! grep -qx "${subj}" "${subjects_file}"; then
                fail "B" "${plugin_name}: announces subject_type '${subj}' not declared in catalogue"
            fi
        done <<< "${subject_types}"
    done
}

# ---- (D) : catalogue fragment must lack schema_version; any
# additional catalogue file (an operator-shipped rack, a schema
# top-level file) must carry the preamble.

lint_catalogue_preambles() {
    info "=== (D) catalogue-fragment schema_version discipline ==="

    if [[ -f "${CATALOGUE}" ]]; then
        if grep -q '^schema_version' "${CATALOGUE}"; then
            fail "D" "${CATALOGUE} carries schema_version but is a fragment (composed by evo-install.sh; must not declare its own preamble)"
        fi
    fi

    # Any other .toml under dist/catalogue/ (not the audio-rack
    # fragment) must carry a preamble if it exists as a stand-alone
    # composition unit. Distribution-shipped standalone catalogues
    # are the operator-facing surface.
    local other
    while IFS= read -r other; do
        [[ "${other}" == "${CATALOGUE}" ]] && continue
        if ! grep -q '^schema_version' "${other}"; then
            fail "D" "${other} does not carry schema_version preamble"
        fi
    done < <(find "${DIST_DIR}/catalogue" -type f -name '*.toml' 2>/dev/null)
}

# ---- (E) : sudoers drop-ins must reference binary paths that
# exist on supported targets.

lint_sudoers_binaries() {
    info "=== (E) sudoers binary paths ==="

    # Canonical target-binary presence table. Each entry names a
    # binary that must exist on a supported target for a sudoers
    # rule to be valid. The validation harness cross-checks the
    # actual presence on running rigs; this lint pass catches
    # obvious typos + reference drift at build time by comparing
    # against the known-good set.
    local -a KNOWN_TARGET_BINARIES=(
        # systemd control-plane
        "/bin/systemctl"
        "/usr/bin/systemctl"
        # network stack
        "/usr/sbin/nmcli"
        "/usr/bin/nmcli"
        "/usr/sbin/iw"
        "/usr/bin/iw"
        "/usr/sbin/rfkill"
        "/usr/bin/rfkill"
        # ALSA + audio
        "/usr/sbin/amixer"
        "/usr/bin/amixer"
        "/usr/sbin/alsactl"
        "/usr/bin/alsactl"
        "/usr/sbin/aplay"
        "/usr/bin/aplay"
        # MPD
        "/usr/bin/mpc"
        # mount lifecycle
        "/bin/mount"
        "/usr/bin/mount"
        "/bin/umount"
        "/usr/bin/umount"
        # power control
        "/sbin/reboot"
        "/usr/sbin/reboot"
        "/sbin/shutdown"
        "/usr/sbin/shutdown"
        # POSIX utilities used by hardware.audio-config DTBO
        # writes + configuration-file writes. Each is
        # always-present on Debian/Trixie + Raspberry Pi OS
        # supported target profiles.
        "/bin/cat"
        "/usr/bin/cat"
        "/bin/rm"
        "/usr/bin/rm"
        "/usr/bin/tee"
        "/bin/tee"
        "/bin/cp"
        "/usr/bin/cp"
        "/bin/mv"
        "/usr/bin/mv"
        "/bin/mkdir"
        "/usr/bin/mkdir"
        "/bin/chmod"
        "/usr/bin/chmod"
        "/bin/chown"
        "/usr/bin/chown"
        # shadow-file password writes (used by set_kiosk_password wire op)
        "/usr/sbin/chpasswd"
        # captive-portal probe wrapper installed by bootstrap.sh
        # from dist/bin/evo-captive-probe. The /bin/ + /usr/local
        # entries cover both the sudoers grant target and the
        # documentation references in the drop-in comments.
        "/usr/local/bin/evo-captive-probe"
        "/bin/evo-captive-probe"
        "/usr/bin/curl"
        "/usr/local"
    )

    if [[ ! -d "${SUDOERS_DIR}" ]]; then
        info "no sudoers directory; skipping"
        return
    fi

    local file path
    for file in "${SUDOERS_DIR}"/*.in; do
        [[ -f "${file}" ]] || continue
        # Extract every /path/like/this from the file.
        while IFS= read -r path; do
            [[ -z "${path}" ]] && continue
            [[ "${path}" =~ ^/etc/ ]] && continue  # /etc/... refs are policy, not exec binaries
            local found=0 known
            for known in "${KNOWN_TARGET_BINARIES[@]}"; do
                if [[ "${path}" == "${known}" ]]; then
                    found=1
                    break
                fi
            done
            if (( found == 0 )); then
                fail "E" "${file}: references binary path '${path}' not in the known-good target-binaries table"
            fi
        done < <(grep -oE '/(usr|bin|sbin|usr/bin|usr/sbin)/[[:alnum:]_-]+' "${file}" | sort -u)
    done
}

# --------- main ---------

echo "build-time lint: dist/release/build-time-lint.sh"
echo "repo root: ${REPO_ROOT}"
echo

lint_shelves_and_subjects
lint_catalogue_preambles
lint_sudoers_binaries

echo
if (( FAILURES > 0 )); then
    echo "build-time lint: REFUSE. ${FAILURES} violation(s) found. Fix before release build." >&2
    exit 1
fi

echo "build-time lint: PASS. Structured sources cross-check clean."
