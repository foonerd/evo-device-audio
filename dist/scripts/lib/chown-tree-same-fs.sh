# shellcheck shell=bash
#
# chown_tree_same_fs <tree> <user> [<group>]
#
# Recursively chown files and directories that live on the
# same filesystem as <tree>. USB and NAS adopts are other
# filesystems mounted under the music tree; walking them is
# the same class of defect as hashing through a live share.
#
# `find -xdev` does not descend, but it still emits the mount
# point itself. A read-only CIFS/tmpfs root then fails chown
# (EROFS) and, under `set -e`, aborts bootstrap before the
# asound render. Skip any path whose device number differs
# from the tree root. `chown -h` so a local symlink cannot
# retarget the walk onto a foreign inode.
#
# Missing <tree> is a no-op (same as the caller guarding
# `[[ -d /var/lib/evo ]]`). Failure on a same-filesystem
# inode is a real error and is not swallowed.

chown_tree_same_fs() {
    local tree="${1:-}"
    local user="${2:-}"
    local group="${3:-$user}"
    local rootdev path dev

    if [[ -z "$tree" || -z "$user" ]]; then
        echo "chown_tree_same_fs: tree and user are required" >&2
        return 2
    fi
    [[ -d "$tree" ]] || return 0

    rootdev="$(stat -c %d "$tree")"
    while IFS= read -r -d '' dev && IFS= read -r -d '' path; do
        if [[ "$dev" != "$rootdev" ]]; then
            continue
        fi
        chown -h "$user:$group" "$path"
    done < <(find "$tree" -xdev -printf '%D\0%p\0')
}
