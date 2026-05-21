#!/usr/bin/env bash
# evo-install.sh — operator-facing online installer + reset
# primitives for evo-device-audio.
#
# The installer is the operator-visible entry point. It
# fetches a signed artefact bundle from the project's artefact
# channel, verifies the signature against a trust root pinned
# in this script, applies the bundle to the device as a
# single deterministic walk, and reports a structured
# post-condition. The operator runs ONE command; the existing
# bootstrap + deploy + reset walkers in `dist/scripts/`
# collapse into this surface for operator-facing flows.
#
# Four primitives, one per --mode value:
#
#   --mode=install
#       (default) Fresh device first install. Stages the
#       bundle, places /opt/evo, /etc/evo, sudoers, systemd
#       unit + drop-ins, ALSA + MPD reference configs,
#       creates the music library skeleton, starts the
#       steward, verifies the post-condition.
#
#   --mode=reinstall
#       Full wipe + reinstall. NOTHING survives the wipe —
#       /opt/evo, /etc/evo, sudoers entries, systemd unit
#       + drop-ins, /var/lib/evo (including the music
#       library), ALSA + MPD evo additions are all
#       removed. The host returns to its pre-evo state, then
#       the install flow runs. Post-condition identical to
#       --mode=install. Use when the operator wants a
#       guaranteed clean reset.
#
#   --mode=wipe-config
#       Wipe binaries + config + runtime state. PRESERVE the
#       music library. Re-fetches the bundle and re-applies
#       /opt/evo + /etc/evo + sudoers + drop-ins. Operator
#       config, admitted peers, groups, audio.options, and
#       every credential go back to installer-shipped
#       defaults. The music library content-hash list is
#       byte-equal before and after the operation.
#
#   --mode=wipe-user-data
#       Vacuum operator-generated state. PRESERVE binaries,
#       configs, sudoers, systemd, trust roots, asound,
#       mpd, AND the music library. The steward regenerates
#       its canonical_id, chain genesis, bootstrap token,
#       and signing keys on next start. Use when the
#       operator wants the device to behave as if first-
#       booting without re-installing.
#
# Channel selection:
#
#   EVO_BUNDLE_URL_BASE selects the artefact source. The
#   default points at the project's STABLE artefact channel.
#   Override the value to point at a developer-side HTTP
#   server hosting an unreleased bundle during release-cut
#   preparation.
#
# Usage:
#
#   curl -fsSL <URL>/install | sudo bash                       # default install
#   curl -fsSL <URL>/install -o evo-install.sh
#   sudo bash evo-install.sh --mode=install
#   sudo bash evo-install.sh --mode=reinstall
#   sudo bash evo-install.sh --mode=wipe-config
#   sudo bash evo-install.sh --mode=wipe-user-data
#
# Env tunables (apply across modes):
#   EVO_BUNDLE_URL_BASE         Channel-base URL.
#   EVO_BUNDLE_VERSION          Pin a specific bundle version.
#   EVO_BUNDLE_TRUST_ROOT_PEM   Override the pinned trust root
#                               (vendor-signed bundle).
#   EVO_SERVICE_USER            Service user (default: SUDO_USER
#                               or lowest non-system uid).
#   EVO_INSTALL_MUSIC_LIBRARY=0 Skip music-library skeleton at
#                               install time. Default: create.
#   EVO_ACCEPTANCE_SIGNING_KEY  Optional path to ed25519 PEM
#                               used to sign the emitted
#                               evidence record. Unsigned
#                               (placeholder) when absent.
#   EVO_INSTALL_EVIDENCE_OUT    Path to write the primitive's
#                               evidence record. Default:
#                               /var/lib/evo/evidence/
#                               <primitive>-<arch>.toml
#                               (created if absent).
#
# Exit codes:
#   0 — primitive succeeded; post-condition verified.
#   1 — operator error (wrong invocation, missing prerequisite,
#       no sudo).
#   2 — fetch error (bundle URL unreachable, signature file
#       missing).
#   3 — signature verification failed.
#   4 — apply error (a stage on the target failed).
#   5 — post-condition verification failed (service did not
#       become active, plugin failed to admit, music-library
#       hash diverged on a preserve-music primitive, etc.).

set -euo pipefail

# -------- Pinned trust root --------
#
# Public component of the ed25519 key the bundle was signed
# with. The bundle's signature must verify against this
# value or the install refuses. Operators who want to
# install a vendor-signed bundle override
# EVO_BUNDLE_TRUST_ROOT_PEM with their own pinned key.
EVO_BUNDLE_TRUST_ROOT_PEM_DEFAULT="-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAvJqIhluihUhLY435rJZnIjskDS9affTKSDUIYVIjVE0=
-----END PUBLIC KEY-----"
EVO_BUNDLE_TRUST_ROOT_PEM="${EVO_BUNDLE_TRUST_ROOT_PEM:-${EVO_BUNDLE_TRUST_ROOT_PEM_DEFAULT}}"

# -------- Defaults --------
# Default URL points at the public artefact channel for the
# stable distribution. Set EVO_BUNDLE_URL_BASE to override
# (e.g. point at a developer-side HTTP server hosting an
# unreleased bundle during release-cut preparation).
EVO_BUNDLE_URL_BASE="${EVO_BUNDLE_URL_BASE:-https://github.com/foonerd/evo-device-audio-artefacts/releases/latest/download}"
EVO_BUNDLE_VERSION="${EVO_BUNDLE_VERSION:-0.1.0}"
EVO_INSTALL_MUSIC_LIBRARY="${EVO_INSTALL_MUSIC_LIBRARY:-1}"
EVO_INSTALL_EVIDENCE_OUT="${EVO_INSTALL_EVIDENCE_OUT:-}"
EVO_ACCEPTANCE_SIGNING_KEY="${EVO_ACCEPTANCE_SIGNING_KEY:-}"

# -------- Argument parsing --------
MODE="install"
print_usage() {
    sed -n '2,90p' "$0" >&2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --mode=*)
            MODE="${1#--mode=}"
            shift
            ;;
        --mode)
            if [[ $# -lt 2 ]]; then
                echo "FAIL: --mode requires a value" >&2
                exit 1
            fi
            MODE="$2"
            shift 2
            ;;
        -h|--help)
            print_usage
            exit 0
            ;;
        *)
            echo "FAIL: unknown argument: $1" >&2
            print_usage
            exit 1
            ;;
    esac
done

case "${MODE}" in
    install|reinstall|wipe-config|wipe-user-data) ;;
    *)
        echo "FAIL: invalid --mode='${MODE}' (expected install|reinstall|wipe-config|wipe-user-data)" >&2
        exit 1
        ;;
esac

PRIMITIVE_ID=""
case "${MODE}" in
    install)         PRIMITIVE_ID="p1_full_initial_setup" ;;
    reinstall)       PRIMITIVE_ID="p2_full_wipe_and_reinstall" ;;
    wipe-config)     PRIMITIVE_ID="p3_config_wipe_preserving_music" ;;
    wipe-user-data)  PRIMITIVE_ID="p4_user_data_full_vacuum" ;;
esac

# -------- Pre-flight: root --------
if [[ "$(id -u)" -ne 0 ]]; then
    echo "FAIL: evo-install.sh must run as root (sudo bash $0)" >&2
    exit 1
fi

# -------- Pre-flight: required tools --------
need_tool() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "FAIL: required tool missing: $1" >&2
        exit 1
    fi
}
need_tool curl
need_tool tar
need_tool gzip
need_tool openssl
need_tool sha256sum
need_tool systemctl
need_tool install
need_tool find
need_tool sed

# -------- Resolve service user --------
SERVICE_USER="${EVO_SERVICE_USER:-}"
if [[ -z "${SERVICE_USER}" && -n "${SUDO_USER:-}" && "${SUDO_USER}" != "root" ]]; then
    SERVICE_USER="${SUDO_USER}"
fi
if [[ -z "${SERVICE_USER}" ]]; then
    SERVICE_USER="$(awk -F: '$3 >= 1000 && $3 < 65000 {print $1; exit}' /etc/passwd)"
fi
if [[ -z "${SERVICE_USER}" ]] || ! id "${SERVICE_USER}" >/dev/null 2>&1; then
    echo "FAIL: could not resolve service user (set EVO_SERVICE_USER=<name>)" >&2
    exit 1
fi

# -------- Detect architecture --------
case "$(uname -m)" in
    x86_64) ARCH="x86_64-unknown-linux-gnu" ;;
    aarch64) ARCH="aarch64-unknown-linux-gnu" ;;
    armv7l) ARCH="armv7-unknown-linux-gnueabihf" ;;
    *)
        echo "FAIL: unsupported architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

# -------- Bundle path resolution --------
BUNDLE_NAME="evo-device-audio-${ARCH}-${EVO_BUNDLE_VERSION}.tar.gz"
BUNDLE_URL="${EVO_BUNDLE_URL_BASE}/${BUNDLE_NAME}"
BUNDLE_SIG_URL="${BUNDLE_URL}.sig"

START_NS="$(date -u +%s%N)"
START_UTC="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

echo "=== evo-install.sh ==="
echo "Mode:          ${MODE} (${PRIMITIVE_ID})"
echo "Architecture:  ${ARCH}"
echo "Bundle URL:    ${BUNDLE_URL}"
echo "Service user:  ${SERVICE_USER}"
echo "Music library: $([[ ${EVO_INSTALL_MUSIC_LIBRARY} != 0 ]] && echo create-or-preserve || echo skip)"
echo ""

# -------- Pre-flight: install system packages --------
ensure_system_packages() {
    local pkgs_needed=()
    local pkg
    # network-manager: required by the network plugin's
    # wifi radio policy enforcement (`nmcli radio wifi on`).
    # Hosts without nmcli emit "spawn direct failed:
    # No such file or directory" at every steward start.
    for pkg in mpd alsa-utils mpc network-manager; do
        if ! dpkg -s "${pkg}" >/dev/null 2>&1; then
            pkgs_needed+=("${pkg}")
        fi
    done
    if [[ ${#pkgs_needed[@]} -gt 0 ]]; then
        echo "  installing: ${pkgs_needed[*]}"
        DEBIAN_FRONTEND=noninteractive apt-get update -qq
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${pkgs_needed[@]}"
    fi
}

# -------- Stage dir lifecycle --------
WORK_DIR=""
STAGE_DIR=""
init_work_dir() {
    WORK_DIR="$(mktemp -d -t evo-install.XXXXXX)"
    STAGE_DIR="${WORK_DIR}/stage"
    mkdir -p "${STAGE_DIR}"
    trap 'rm -rf "${WORK_DIR}"' EXIT
}

# -------- Fetch + verify the bundle --------
BUNDLE_PATH=""
BUNDLE_SIG_PATH=""
BUNDLE_SHA256=""
BUNDLE_SIZE=0

fetch_and_verify_bundle() {
    BUNDLE_PATH="${WORK_DIR}/${BUNDLE_NAME}"
    BUNDLE_SIG_PATH="${BUNDLE_PATH}.sig"
    local trust_root_path="${WORK_DIR}/trust-root.pem"
    if ! curl -fsSL --connect-timeout 10 --max-time 600 -o "${BUNDLE_PATH}" "${BUNDLE_URL}"; then
        echo "FAIL: fetch ${BUNDLE_URL} failed" >&2
        exit 2
    fi
    if ! curl -fsSL --connect-timeout 10 --max-time 60 -o "${BUNDLE_SIG_PATH}" "${BUNDLE_SIG_URL}"; then
        echo "FAIL: fetch ${BUNDLE_SIG_URL} failed" >&2
        exit 2
    fi
    BUNDLE_SHA256="$(sha256sum "${BUNDLE_PATH}" | awk '{print $1}')"
    BUNDLE_SIZE="$(stat -c %s "${BUNDLE_PATH}")"
    printf '%s\n' "${EVO_BUNDLE_TRUST_ROOT_PEM}" > "${trust_root_path}"
    if ! openssl pkeyutl -verify \
            -pubin -inkey "${trust_root_path}" \
            -rawin -in "${BUNDLE_PATH}" \
            -sigfile "${BUNDLE_SIG_PATH}" >/dev/null 2>&1; then
        echo "FAIL: signature does not verify against pinned trust root" >&2
        exit 3
    fi
}

extract_bundle() {
    tar -C "${STAGE_DIR}" -xzf "${BUNDLE_PATH}"
    if [[ ! -f "${STAGE_DIR}/bundle-manifest.toml" ]]; then
        echo "FAIL: bundle missing bundle-manifest.toml" >&2
        exit 4
    fi
    if [[ ! -x "${STAGE_DIR}/bin/evo-device-audio" ]]; then
        echo "FAIL: bundle missing bin/evo-device-audio" >&2
        exit 4
    fi
}

# -------- Music library hash discipline --------
snapshot_music_hashes() {
    if [[ -d /var/lib/evo/music ]]; then
        find /var/lib/evo/music -type f -print0 2>/dev/null \
            | sort -z | xargs -0 -r sha256sum 2>/dev/null \
            | sha256sum | awk '{print $1}'
    else
        echo "no_music_library"
    fi
}

# -------- Wipe primitives --------
restore_pre_evo_asound_conf() {
    # Restore the oldest pre-evo backup so the original
    # operator-set asound.conf survives the install round-trip.
    # Backups are named with a timestamp suffix (pre-evo.<YYYYmmddHHMMSS>),
    # so bash's default lexicographic glob order is chronological;
    # element 0 is the oldest.
    local oldest_backup=""
    shopt -s nullglob
    local backups=(/etc/asound.conf.pre-evo.*)
    shopt -u nullglob
    if (( ${#backups[@]} > 0 )); then
        oldest_backup="${backups[0]}"
    fi
    if [[ -n "${oldest_backup}" && -f "${oldest_backup}" ]]; then
        mv "${oldest_backup}" /etc/asound.conf
    else
        rm -f /etc/asound.conf
    fi
    # Clean up any remaining backups.
    shopt -s nullglob
    local remaining=(/etc/asound.conf.pre-evo.*)
    shopt -u nullglob
    if (( ${#remaining[@]} > 0 )); then
        rm -f "${remaining[@]}"
    fi
}

strip_evo_include_from_mpd_conf() {
    if [[ ! -f /etc/mpd.conf ]]; then
        return 0
    fi
    # Restore the pre-evo /etc/mpd.conf wholesale if a backup
    # from the original music_directory rewrite exists. The
    # backup is a single, fixed-name file created by
    # `sed -i.pre-evo-music`; restoring it reverses the
    # music_directory edit and any include lines added since.
    if [[ -f /etc/mpd.conf.pre-evo-music ]]; then
        mv /etc/mpd.conf.pre-evo-music /etc/mpd.conf
    fi
    # Even after the restore, additional include lines may
    # have been added by other paths (bootstrap.sh, earlier
    # evo-install.sh variants). Purge them too.
    purge_evo_mpd_includes
}

stop_prior_steward() {
    systemctl stop evo 2>/dev/null || true
    # Reset any auto-restart-pending state from a previously
    # broken unit (e.g. an earlier install attempt that left
    # the unit transitionally without ExecStart). Without
    # this, systemd keeps logging "Service has no ExecStart=,
    # ExecStop=, or SuccessAction=. Refusing." while it
    # auto-retries during the install transition.
    systemctl reset-failed evo 2>/dev/null || true
    # Kill any evo-device-audio process not under systemd's
    # control (manual sudo launches survive systemctl stop).
    pkill -KILL -f '/opt/evo/bin/evo-device-audio' 2>/dev/null || true
    pkill -KILL -f '/opt/evo/plugins/.*/plugin\.bin' 2>/dev/null || true
}

wipe_full() {
    stop_prior_steward
    rm -rf /opt/evo
    rm -rf /etc/evo
    rm -f /etc/sudoers.d/evo-* 2>/dev/null || true
    rm -f /etc/systemd/system/evo.service
    rm -rf /etc/systemd/system/evo.service.d
    rm -rf /var/lib/evo
    restore_pre_evo_asound_conf
    strip_evo_include_from_mpd_conf
    systemctl daemon-reload
}

wipe_config() {
    stop_prior_steward
    rm -rf /opt/evo
    rm -rf /etc/evo
    rm -f /etc/sudoers.d/evo-* 2>/dev/null || true
    rm -rf /etc/systemd/system/evo.service.d
    # Preserve /etc/systemd/system/evo.service (installer
    # re-applies it idempotently).
    # Wipe /var/lib/evo/* EXCEPT /var/lib/evo/music.
    if [[ -d /var/lib/evo ]]; then
        find /var/lib/evo -mindepth 1 -maxdepth 1 -not -name music -exec rm -rf -- {} +
    fi
    systemctl daemon-reload
}

wipe_user_data() {
    stop_prior_steward
    # Vacuum operator-generated state subdirs. Preserve
    # binaries, configs, sudoers, systemd, trust roots,
    # asound, mpd, music.
    rm -rf /var/lib/evo/state
    rm -rf /var/lib/evo/plugins
    rm -rf /var/lib/evo/https/credentials
    rm -rf /var/lib/evo/plans
    # The /etc/evo baseline re-application happens in
    # place_etc_evo() after the bundle is extracted.
    rm -rf /etc/evo
}

# -------- Apply (factored install stages) --------
place_opt_evo() {
    install -d -m 0755 -o root -g root /opt/evo /opt/evo/bin /opt/evo/plugins /opt/evo/catalogue
    install -m 0755 -o root -g root \
        "${STAGE_DIR}/bin/evo-device-audio" \
        /opt/evo/bin/evo-device-audio
    # Sweep stale plugin bundles + install fresh.
    local d p p_name
    for d in /opt/evo/plugins/*/; do
        if [[ -d "$d" ]]; then
            rm -rf "$d"
        fi
    done
    for p in "${STAGE_DIR}/plugins/"*/; do
        [[ -d "$p" ]] || continue
        p_name="$(basename "$p")"
        [[ -f "${p}/manifest.toml" ]] || continue
        install -d -m 0755 -o root -g root "/opt/evo/plugins/${p_name}"
        install -m 0644 -o root -g root "${p}/manifest.toml" "/opt/evo/plugins/${p_name}/manifest.toml"
        install -m 0644 -o root -g root "${p}/manifest.sig" "/opt/evo/plugins/${p_name}/manifest.sig"
        install -m 0755 -o root -g root "${p}/plugin.bin" "/opt/evo/plugins/${p_name}/plugin.bin"
        # Per-plugin data files (e.g. DAC catalogue source).
        if [[ -d "${p}/data" ]]; then
            install -d -m 0755 -o root -g root "/opt/evo/plugins/${p_name}/data"
            cp -a "${p}/data/." "/opt/evo/plugins/${p_name}/data/"
        fi
    done
    # Catalogue: compose schema_version preamble + fragment.
    local catalogue_path="/opt/evo/catalogue/default.toml"
    local tmp_cat
    tmp_cat="$(mktemp)"
    {
        echo "# Composed by evo-install.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "# Source fragment: dist/catalogue/audio-rack.toml"
        echo ""
        echo "schema_version = 1"
        echo ""
        cat "${STAGE_DIR}/dist/catalogue/audio-rack.toml"
    } > "${tmp_cat}"
    install -m 0644 -o root -g root "${tmp_cat}" "${catalogue_path}"
    rm -f "${tmp_cat}"
}

place_etc_evo() {
    install -d -m 0755 -o "${SERVICE_USER}" -g "${SERVICE_USER}" /etc/evo /etc/evo/plugins.d
    install -d -m 0755 -o root -g root /etc/evo/trust.d
    local pem
    for pem in "${STAGE_DIR}/dist/keys/"*.pem "${STAGE_DIR}/dist/keys/"*.meta.toml; do
        [[ -f "$pem" ]] || continue
        install -m 0644 -o root -g root "$pem" "/etc/evo/trust.d/$(basename "$pem")"
    done
    if [[ ! -f /etc/evo/mpd.conf ]]; then
        install -m 0644 -o "${SERVICE_USER}" -g "${SERVICE_USER}" \
            /dev/null /etc/evo/mpd.conf
    fi
    if [[ -f "${STAGE_DIR}/dist/etc-evo/client_acl.toml" ]]; then
        install -m 0644 -o root -g root \
            "${STAGE_DIR}/dist/etc-evo/client_acl.toml" \
            /etc/evo/client_acl.toml
    fi
}

install_systemd() {
    install -m 0644 -o root -g root \
        "${STAGE_DIR}/dist/systemd/evo.service" \
        /etc/systemd/system/evo.service
    install -d -m 0755 -o root -g root /etc/systemd/system/evo.service.d
    local conf
    for conf in "${STAGE_DIR}/dist/systemd/evo.service.d/"*.conf; do
        [[ -f "$conf" ]] || continue
        sed -e "s|@EVO_SERVICE_USER@|${SERVICE_USER}|g" "$conf" \
            > "/etc/systemd/system/evo.service.d/$(basename "$conf")"
        chmod 0644 "/etc/systemd/system/evo.service.d/$(basename "$conf")"
    done
}

install_sudoers() {
    local src base tmp_sudo systemctl_path nmcli_path curl_path
    systemctl_path="$(command -v systemctl)"
    nmcli_path="$(command -v nmcli || echo /usr/bin/nmcli)"
    curl_path="$(command -v curl || echo /usr/bin/curl)"
    for src in "${STAGE_DIR}/dist/sudoers.d/"*.in; do
        [[ -f "$src" ]] || continue
        base="$(basename "$src" .in)"
        tmp_sudo="$(mktemp)"
        sed -e "s|@EVO_SERVICE_USER@|${SERVICE_USER}|g" \
            -e "s|@SYSTEMCTL@|${systemctl_path}|g" \
            -e "s|@NMCLI@|${nmcli_path}|g" \
            -e "s|@CURL@|${curl_path}|g" \
            "$src" > "${tmp_sudo}"
        if ! visudo -c -f "${tmp_sudo}" >/dev/null 2>&1; then
            echo "  WARN: sudoers template ${base} failed visudo -c; skipping"
            rm -f "${tmp_sudo}"
            continue
        fi
        install -m 0440 -o root -g root "${tmp_sudo}" "/etc/sudoers.d/${base}"
        rm -f "${tmp_sudo}"
    done
}

place_music_library() {
    if [[ "${EVO_INSTALL_MUSIC_LIBRARY}" == "0" ]]; then
        return 0
    fi
    install -d -m 0755 -o root -g root /var/lib/evo
    if ! install -d -m 0755 -o "${SERVICE_USER}" -g audio \
            /var/lib/evo/music \
            /var/lib/evo/music/INTERNAL \
            /var/lib/evo/music/USB \
            /var/lib/evo/music/NAS 2>/dev/null; then
        install -d -m 0755 -o "${SERVICE_USER}" -g "${SERVICE_USER}" \
            /var/lib/evo/music \
            /var/lib/evo/music/INTERNAL \
            /var/lib/evo/music/USB \
            /var/lib/evo/music/NAS
    fi
}

purge_evo_mpd_includes() {
    # Strip ALL evo-related markers from /etc/mpd.conf so the
    # next inject lands on a clean baseline. Idempotent.
    # Targets:
    #   1. Lines `include "/etc/evo/mpd.conf"` (any whitespace).
    #   2. Lines `include_optional "/etc/evo/mpd.conf"` (any whitespace).
    #   3. Delimited block:
    #        # >>> evo-device-audio (...) — DO NOT EDIT >>>
    #        ...
    #        # <<< evo-device-audio (...) — DO NOT EDIT <<<
    #      Earlier bootstrap.sh and earlier evo-install.sh
    #      versions both wrap their canonical include in this
    #      block. Match by the `evo-device-audio` keyword in
    #      the start marker so future variants are caught.
    if [[ ! -f /etc/mpd.conf ]]; then
        return 0
    fi
    sed -i.pre-evo-purge \
        -e '/^[[:space:]]*include[[:space:]]\+"\/etc\/evo\/mpd\.conf"[[:space:]]*$/d' \
        -e '/^[[:space:]]*include_optional[[:space:]]\+"\/etc\/evo\/mpd\.conf"[[:space:]]*$/d' \
        -e '/^[[:space:]]*#[[:space:]]*>>>[[:space:]]*evo-device-audio.*>>>[[:space:]]*$/,/^[[:space:]]*#[[:space:]]*<<<[[:space:]]*evo-device-audio.*<<<[[:space:]]*$/d' \
        /etc/mpd.conf
    # Drop trailing blank lines left behind by the deletes.
    sed -i -e ':a' -e '/^$/{$d;N;ba' -e '}' /etc/mpd.conf
}

inject_mpd_include() {
    if [[ ! -f /etc/mpd.conf ]]; then
        return 0
    fi
    # Evict every prior evo marker first so the canonical
    # line lands exactly once, regardless of accumulated
    # cruft from earlier bootstrap.sh / evo-install.sh runs.
    purge_evo_mpd_includes
    # Add the canonical block (delimited so the next purge
    # finds it deterministically).
    {
        printf '\n# >>> evo-device-audio (evo-install.sh) — DO NOT EDIT >>>\n'
        printf 'include_optional "/etc/evo/mpd.conf"\n'
        printf '# <<< evo-device-audio (evo-install.sh) — DO NOT EDIT <<<\n'
    } >> /etc/mpd.conf
    if [[ "${EVO_INSTALL_MUSIC_LIBRARY}" != "0" ]] && \
       ! grep -qE '^\s*music_directory\s+"/var/lib/evo/music"' /etc/mpd.conf; then
        sed -i.pre-evo-music -E 's|^\s*music_directory\s+".*"|music_directory "/var/lib/evo/music"|' /etc/mpd.conf || true
    fi
}

install_asound_conf() {
    if [[ -f "${STAGE_DIR}/dist/alsa/asound.conf" ]]; then
        if [[ -f /etc/asound.conf ]] && \
           ! cmp -s "${STAGE_DIR}/dist/alsa/asound.conf" /etc/asound.conf; then
            cp /etc/asound.conf "/etc/asound.conf.pre-evo.$(date +%Y%m%d%H%M%S)"
        fi
        install -m 0644 -o root -g root \
            "${STAGE_DIR}/dist/alsa/asound.conf" \
            /etc/asound.conf
    fi
    install -d -m 0755 -o root -g root /etc/asound.d
    if [[ ! -f /etc/asound.d/evo-options.conf ]]; then
        : > /etc/asound.d/evo-options.conf
        chmod 0644 /etc/asound.d/evo-options.conf
    fi
}

start_steward() {
    systemctl daemon-reload
    systemctl enable evo.service >/dev/null 2>&1 || true
    systemctl restart evo.service
}

# -------- Post-condition verification --------
ACTIVE_STATE=""
PLUGINS_ADMITTED=0
ADMISSION_FAILURES=0
NOT_DECLARED=0
CATALOGUE_SOURCE=""

JOURNAL_FAIL_HITS=""
JOURNAL_FAIL_COUNT=0

verify_post_condition() {
    local deadline
    deadline=$(( $(date +%s) + 30 ))
    while [[ $(date +%s) -lt ${deadline} ]]; do
        if systemctl is-active evo >/dev/null 2>&1; then
            sleep 3   # let plugins admit
            break
        fi
        sleep 1
    done
    ACTIVE_STATE="$(systemctl is-active evo 2>/dev/null || echo unknown)"
    # Count admitted plugins by listing the per-plugin Unix
    # sockets the steward creates under /var/run/evo/plugins/.
    # Each successfully-admitted OOP plugin exposes its
    # request-socket here; this is observable substrate
    # independent of the steward's log-level filter (default
    # RUST_LOG=warn hides INFO-level "plugin admitted" lines).
    if [[ -d /var/run/evo/plugins ]]; then
        PLUGINS_ADMITTED=$(find /var/run/evo/plugins -maxdepth 1 -name '*.sock' 2>/dev/null | wc -l)
    else
        PLUGINS_ADMITTED=0
    fi
    ADMISSION_FAILURES=$(journalctl -u evo --since "60 seconds ago" --no-pager -o cat 2>/dev/null | grep -c '^skipping plugin: admission failed$' || true)
    NOT_DECLARED=$(journalctl -u evo --since "60 seconds ago" --no-pager 2>/dev/null | grep -c 'not declared in the catalogue' || true)
    CATALOGUE_SOURCE=$(journalctl -u evo --since "60 seconds ago" --no-pager -o json 2>/dev/null | grep 'catalogue loaded' 2>/dev/null | grep -oE '"F_SOURCE":"[a-z]+"' 2>/dev/null | head -1 | sed 's/.*:"//; s/"$//' || true)

    # Strict: any line containing "fail" (case-insensitive)
    # in the evo journal, OR in the journal of any service
    # the install touched (mpd), is treated as install
    # failure. The operator's engineering bar: zero "fail"
    # across every consumer of the install's output.
    local fail_evo fail_mpd
    fail_evo=$(journalctl -u evo --since "60 seconds ago" --no-pager 2>/dev/null | grep -iE 'fail(ed|ure)?\b' || true)
    fail_mpd=$(journalctl -u mpd --since "60 seconds ago" --no-pager 2>/dev/null | grep -iE 'fail(ed|ure)?\b' || true)
    JOURNAL_FAIL_HITS="${fail_evo}"
    if [[ -n "${fail_mpd}" ]]; then
        JOURNAL_FAIL_HITS="${JOURNAL_FAIL_HITS}${JOURNAL_FAIL_HITS:+$'\n'}${fail_mpd}"
    fi
    if [[ -n "${JOURNAL_FAIL_HITS}" ]]; then
        JOURNAL_FAIL_COUNT=$(printf '%s\n' "${JOURNAL_FAIL_HITS}" | grep -c . || true)
    else
        JOURNAL_FAIL_COUNT=0
    fi
}

# -------- Music-library hash verification --------
MUSIC_HASH_PRE=""
MUSIC_HASH_POST=""
MUSIC_HASH_PRESERVED="true"
MUSIC_HASH_CHANGED="false"

verify_music_hashes_preserved() {
    MUSIC_HASH_POST="$(snapshot_music_hashes)"
    if [[ "${MUSIC_HASH_PRE}" == "${MUSIC_HASH_POST}" ]]; then
        MUSIC_HASH_PRESERVED="true"
    else
        MUSIC_HASH_PRESERVED="false"
        echo "FAIL: music library hash diverged" >&2
        echo "      pre  = ${MUSIC_HASH_PRE}" >&2
        echo "      post = ${MUSIC_HASH_POST}" >&2
    fi
}

# -------- Evidence emission --------
emit_evidence() {
    local out_path="${EVO_INSTALL_EVIDENCE_OUT}"
    if [[ -z "${out_path}" ]]; then
        install -d -m 0755 -o root -g root /var/lib/evo/evidence
        out_path="/var/lib/evo/evidence/${PRIMITIVE_ID}-${ARCH}.toml"
    else
        install -d -m 0755 -o root -g root "$(dirname "${out_path}")"
    fi

    local end_ns end_utc elapsed_ms
    end_ns="$(date -u +%s%N)"
    end_utc="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    elapsed_ms=$(( (end_ns - START_NS) / 1000000 ))

    local service_active_bool="false"
    if [[ "${ACTIVE_STATE}" == "active" ]]; then
        service_active_bool="true"
    fi

    local music_hash_pre_field="\"\""
    if [[ -n "${MUSIC_HASH_PRE}" ]]; then
        music_hash_pre_field="\"${MUSIC_HASH_PRE}\""
    fi
    local music_hash_post_field="\"\""
    if [[ -n "${MUSIC_HASH_POST}" ]]; then
        music_hash_post_field="\"${MUSIC_HASH_POST}\""
    fi

    # Compose the unsigned evidence record. Signing happens
    # below if EVO_ACCEPTANCE_SIGNING_KEY is set.
    local body_path="${WORK_DIR}/evidence-body.toml"
    cat > "${body_path}" <<EOF
schema_version = 1
primitive = "${PRIMITIVE_ID}"
architecture = "${ARCH}"
ran_at_utc = "${end_utc}"
started_at_utc = "${START_UTC}"
elapsed_ms = ${elapsed_ms}
mode = "${MODE}"
bundle_url = "${BUNDLE_URL}"
bundle_sha256 = "${BUNDLE_SHA256:-}"
bundle_size_bytes = ${BUNDLE_SIZE}

[post_condition]
service_active = ${service_active_bool}
plugins_admitted_count = ${PLUGINS_ADMITTED}
admission_failures = ${ADMISSION_FAILURES}
subject_not_declared = ${NOT_DECLARED}
catalogue_source = "${CATALOGUE_SOURCE:-unknown}"
music_library_hash_pre = ${music_hash_pre_field}
music_library_hash_post = ${music_hash_post_field}
music_library_hash_preserved = ${MUSIC_HASH_PRESERVED}
music_library_hash_changed = ${MUSIC_HASH_CHANGED}

EOF

    # Signature block. Signed when EVO_ACCEPTANCE_SIGNING_KEY
    # is set; otherwise a placeholder records the run as
    # unsigned (the validation harness re-runs with a key for
    # the release-cut preflight).
    if [[ -n "${EVO_ACCEPTANCE_SIGNING_KEY}" && -r "${EVO_ACCEPTANCE_SIGNING_KEY}" ]]; then
        local sig_bin sig_b64
        sig_bin="${WORK_DIR}/evidence.sig"
        if openssl pkeyutl -sign \
                -inkey "${EVO_ACCEPTANCE_SIGNING_KEY}" \
                -rawin -in "${body_path}" \
                -out "${sig_bin}" >/dev/null 2>&1; then
            sig_b64="$(base64 -w0 < "${sig_bin}")"
            cat > "${out_path}" <<EOF
$(cat "${body_path}")
[signature]
key_id = "evo-acceptance-signing"
ed25519_b64 = "${sig_b64}"
EOF
        else
            echo "  WARN: evidence signing failed; writing unsigned record" >&2
            cat > "${out_path}" <<EOF
$(cat "${body_path}")
[signature]
key_id = "evo-acceptance-signing"
ed25519_b64 = "UNSIGNED_SIGNING_ERROR"
EOF
        fi
    else
        cat > "${out_path}" <<EOF
$(cat "${body_path}")
[signature]
key_id = "evo-acceptance-signing"
ed25519_b64 = "UNSIGNED_OPERATOR_RUN"
EOF
    fi
    chmod 0644 "${out_path}"
    echo "Evidence: ${out_path}"
}

# -------- Mode dispatch --------
init_work_dir

case "${MODE}" in
    install)
        echo "[1/9] system packages ..." ; ensure_system_packages ; echo "  ok"
        echo "[2/9] fetch bundle ..."     ; fetch_and_verify_bundle ; echo "  ok (sha256: ${BUNDLE_SHA256})"
        echo "[3/9] extract bundle ..."   ; extract_bundle          ; echo "  ok"
        echo "[4/9] stop prior steward ..." ; stop_prior_steward    ; echo "  ok"
        echo "[5/9] /opt/evo (binaries + plugins + catalogue) ..." ; place_opt_evo  ; echo "  ok"
        echo "[6/9] /etc/evo + sudoers + drop-ins + trust roots ..." ; place_etc_evo ; install_systemd ; install_sudoers ; echo "  ok"
        echo "[7/9] music library ..."    ; place_music_library     ; echo "  ok"
        echo "[8/9] mpd include + asound.conf ..." ; inject_mpd_include ; install_asound_conf ; echo "  ok"
        echo "[9/9] start + verify ..."   ; start_steward ; verify_post_condition
        ;;
    reinstall)
        echo "[1/10] system packages ..." ; ensure_system_packages ; echo "  ok"
        echo "[2/10] fetch bundle ..."    ; fetch_and_verify_bundle ; echo "  ok (sha256: ${BUNDLE_SHA256})"
        echo "[3/10] extract bundle ..."  ; extract_bundle          ; echo "  ok"
        echo "[4/10] FULL WIPE (binaries + config + state + music) ..."
        wipe_full ; echo "  ok"
        echo "[5/10] /opt/evo ..."        ; place_opt_evo           ; echo "  ok"
        echo "[6/10] /etc/evo + sudoers + drop-ins + trust roots ..." ; place_etc_evo ; install_systemd ; install_sudoers ; echo "  ok"
        echo "[7/10] music library skeleton ..." ; place_music_library ; echo "  ok"
        echo "[8/10] mpd include + asound.conf ..." ; inject_mpd_include ; install_asound_conf ; echo "  ok"
        echo "[9/10] start + verify ..."  ; start_steward ; verify_post_condition
        MUSIC_HASH_CHANGED="true"
        ;;
    wipe-config)
        echo "[1/10] snapshot music library hashes ..." ; MUSIC_HASH_PRE="$(snapshot_music_hashes)" ; echo "  ok (sha256: ${MUSIC_HASH_PRE})"
        echo "[2/10] system packages ..." ; ensure_system_packages ; echo "  ok"
        echo "[3/10] fetch bundle ..."    ; fetch_and_verify_bundle ; echo "  ok (sha256: ${BUNDLE_SHA256})"
        echo "[4/10] extract bundle ..."  ; extract_bundle          ; echo "  ok"
        echo "[5/10] CONFIG WIPE (binaries + config + state, music preserved) ..."
        wipe_config ; echo "  ok"
        echo "[6/10] /opt/evo ..."        ; place_opt_evo           ; echo "  ok"
        echo "[7/10] /etc/evo + sudoers + drop-ins + trust roots ..." ; place_etc_evo ; install_systemd ; install_sudoers ; echo "  ok"
        echo "[8/10] mpd include + asound.conf ..." ; inject_mpd_include ; install_asound_conf ; echo "  ok"
        echo "[9/10] start + verify ..."  ; start_steward ; verify_post_condition
        echo "[10/10] verify music library byte-equal ..." ; verify_music_hashes_preserved
        ;;
    wipe-user-data)
        echo "[1/8] snapshot music library hashes ..." ; MUSIC_HASH_PRE="$(snapshot_music_hashes)" ; echo "  ok (sha256: ${MUSIC_HASH_PRE})"
        echo "[2/8] fetch bundle (for /etc/evo baseline) ..." ; fetch_and_verify_bundle ; echo "  ok (sha256: ${BUNDLE_SHA256})"
        echo "[3/8] extract bundle ..."   ; extract_bundle          ; echo "  ok"
        echo "[4/8] USER-DATA VACUUM (operator-generated state, /etc/evo overrides reset; binaries + music preserved) ..."
        wipe_user_data ; echo "  ok"
        echo "[5/8] /etc/evo baseline (re-apply) + drop-ins + sudoers ..." ; place_etc_evo ; install_systemd ; install_sudoers ; echo "  ok"
        echo "[6/8] mpd include (idempotent) + asound.conf (idempotent) ..." ; inject_mpd_include ; install_asound_conf ; echo "  ok"
        echo "[7/8] start + verify ..."   ; start_steward ; verify_post_condition
        echo "[8/8] verify music library byte-equal ..." ; verify_music_hashes_preserved
        ;;
esac

echo ""
echo "  service:               ${ACTIVE_STATE}"
echo "  plugins admitted:      ${PLUGINS_ADMITTED}"
echo "  admission failures:    ${ADMISSION_FAILURES}"
echo "  not-declared warnings: ${NOT_DECLARED}"
echo "  catalogue source:      ${CATALOGUE_SOURCE:-unknown}"
echo "  journal fail hits:     ${JOURNAL_FAIL_COUNT}"
if [[ "${MODE}" == "wipe-config" || "${MODE}" == "wipe-user-data" ]]; then
    echo "  music library hash:    ${MUSIC_HASH_PRESERVED} (pre=${MUSIC_HASH_PRE} post=${MUSIC_HASH_POST})"
fi
if [[ "${JOURNAL_FAIL_COUNT}" -gt 0 ]]; then
    echo ""
    echo "  journal fail lines:"
    printf '%s\n' "${JOURNAL_FAIL_HITS}" | sed 's/^/    /'
fi
echo ""

POST_OK=1
if [[ "${ACTIVE_STATE}" != "active" ]]; then POST_OK=0; fi
if [[ "${PLUGINS_ADMITTED}" -lt 1 ]]; then POST_OK=0; fi
if [[ "${ADMISSION_FAILURES}" -ne 0 ]]; then POST_OK=0; fi
if [[ "${NOT_DECLARED}" -ne 0 ]]; then POST_OK=0; fi
if [[ "${JOURNAL_FAIL_COUNT}" -gt 0 ]]; then POST_OK=0; fi
if [[ "${MODE}" == "wipe-config" || "${MODE}" == "wipe-user-data" ]]; then
    if [[ "${MUSIC_HASH_PRESERVED}" != "true" ]]; then POST_OK=0; fi
fi

emit_evidence

if [[ "${POST_OK}" -eq 1 ]]; then
    echo "=== evo-install.sh ${MODE} complete ==="
    echo "Service active. ${PLUGINS_ADMITTED} plugins admitted."
    echo ""
    echo "Next steps:"
    echo "  - Inspect: systemctl status evo"
    echo "  - Operator wizard: evo-plugin-tool admin device identity show"
    if [[ "${MODE}" != "wipe-user-data" ]]; then
        echo "  - Music library: /var/lib/evo/music/{INTERNAL,USB,NAS}"
    fi
    exit 0
else
    echo "FAIL: post-condition verification failed" >&2
    echo "      Check: journalctl -u evo --no-pager -n 80" >&2
    exit 5
fi
