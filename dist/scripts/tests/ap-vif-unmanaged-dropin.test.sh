#!/usr/bin/env bash
# ap-vif-unmanaged-dropin.test.sh — the access point's virtual
# interface must be unmanaged before it exists.
#
# The steward creates the vif `type __ap` and then marks it
# unmanaged, but `nmcli` can only name an interface that is
# already there. In the gap between the netlink event and that
# call NetworkManager claims the device, wpa_supplicant sets it
# up as a station, and a one-station phy refuses the second
# interface as "device or resource busy" — after which the vif
# keeps the wrong type and every apply declines on it.
#
# A rule already in place when the interface appears has no gap
# to lose. This pins that the rule ships, that it names the
# interface the steward actually creates, and that bootstrap
# installs it.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
DROP_IN="$REPO_ROOT/dist/networkmanager.conf.d/evo-ap-vif.conf"
BOOTSTRAP="$REPO_ROOT/dist/scripts/bootstrap.sh"
PLUGIN_SRC="$REPO_ROOT/plugins/org.evoframework.network/src/lib.rs"

PASS=0
FAIL=0

assert_ok() {
    local name="$1"
    if eval "$2"; then
        echo "PASS  $name"
        PASS=$((PASS + 1))
    else
        echo "FAIL  $name"
        FAIL=$((FAIL + 1))
    fi
}

assert_ok "the drop-in ships" "[[ -f \"\$DROP_IN\" ]]"
assert_ok "it is a per-device section" \
    "grep -qE '^\[device-' \"\$DROP_IN\""
assert_ok "it matches the AP vif by interface name" \
    "grep -qE '^match-device=interface-name:ap0\$' \"\$DROP_IN\""
assert_ok "it marks that device unmanaged" \
    "grep -qE '^managed=0\$' \"\$DROP_IN\""

# The whole point is a rule the steward can lift again moments
# later. NetworkManager.conf(5): a device unmanaged via
# keyfile.unmanaged-devices "is strictly unmanaged and cannot be
# overruled by using the API like nmcli device set $IFNAME managed
# yes", and it names device*.managed as the better choice. Used
# here it kept the access point down for good.
assert_ok "it does not use the strict, unliftable setting" \
    "! grep -qE '^unmanaged-devices=' \"\$DROP_IN\""
assert_ok "it does not declare a keyfile section" \
    "! grep -qE '^\[keyfile\]' \"\$DROP_IN\""

# The rule and the steward have to name the same interface. If
# the steward's default vif name ever moves, a drop-in still
# naming ap0 silently stops covering anything — the gap reopens
# and nothing else notices.
assert_ok "the steward still defaults its AP vif to ap0" \
    "grep -qF 'unwrap_or_else(|| \"ap0\".to_string())' \"\$PLUGIN_SRC\""

# Not a udev rule: radio policy stays next to the configuration
# NetworkManager already owns.
# (this file names the string itself, so the tests dir is excluded)
assert_ok "no udev rule is shipped for the vif" \
    "! grep -rqs --exclude-dir=tests 'NM_UNMANAGED' \"\$REPO_ROOT/dist\""

assert_ok "bootstrap installs it under /etc/NetworkManager/conf.d" \
    "grep -qF 'NETWORK_AP_VIF_CONF_DIR=\"/etc/NetworkManager/conf.d\"' \"\$BOOTSTRAP\""
assert_ok "bootstrap installs the file itself" \
    "grep -qF 'NETWORK_AP_VIF_CONF_FILE=\"\$NETWORK_AP_VIF_CONF_DIR/evo-ap-vif.conf\"' \"\$BOOTSTRAP\""
assert_ok "bootstrap refuses when the template is absent" \
    "grep -qF 'AP vif NetworkManager drop-in not found at' \"\$BOOTSTRAP\""
assert_ok "the install is root-owned and world-readable" \
    "grep -A1 'install -m 0644 -o root -g root' \"\$BOOTSTRAP\" \
       | grep -qF 'NETWORK_AP_VIF_CONF_FILE'"
assert_ok "operators can skip it" \
    "grep -qF 'EVO_INSTALL_NETWORK_AP_VIF_CONF:-1' \"\$BOOTSTRAP\""

# The drop-in must not become a way to keep the access point
# down. The steward hands the vif back at runtime, and that
# hand-back is what makes this safe rather than fatal.
assert_ok "the steward still hands the vif back before the raise" \
    "grep -qF '\"device\", \"set\", name, \"managed\", \"yes\"' \"\$PLUGIN_SRC\""

echo
echo "${PASS} passed, ${FAIL} failed"
[[ "${FAIL}" -eq 0 ]]
