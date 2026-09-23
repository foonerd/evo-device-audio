# shellcheck shell=bash
#
# chown_tenant_state_trees <user> [<state_root>]
#
# Tenant-own the StateDirectory trees the kiosk layer creates
# as root after the early chown_tree_same_fs pass:
#   <state_root>/settings
#   <state_root>/settings/kiosk
#   <state_root>/ui
# Then re-assert <state_root> itself as <user> mode 0755.
#
# Default <state_root> is /var/lib/evo. Missing trees are
# skipped. This is not a walk: uploads, music, and plugin
# stage keep the owners bootstrap already set. A second
# chown_tree_same_fs here would reparent those on purpose.

chown_tenant_state_trees() {
    local user="${1:-}"
    local root="${2:-/var/lib/evo}"
    local p

    if [[ -z "$user" ]]; then
        echo "chown_tenant_state_trees: user is required" >&2
        return 2
    fi

    for p in "${root}/settings" "${root}/settings/kiosk" "${root}/ui"; do
        if [[ -d "$p" ]]; then
            chown "$user:$user" "$p"
            chmod 0755 "$p"
        fi
    done

    if [[ -d "$root" ]]; then
        chown "$user:$user" "$root"
        chmod 0755 "$root"
    fi
}
