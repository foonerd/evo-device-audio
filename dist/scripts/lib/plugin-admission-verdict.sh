# shellcheck shell=bash
#
# Pure decision: did every plugin a deploy shipped admit on this
# steward start. Prints one line per plugin and returns non-zero
# when any was refused or never admitted. Reads only its
# arguments, so the regression tests under dist/scripts/tests/
# can drive it with a journal and a socket list instead of a box.
#
# The two signals are the framework's own:
#
# - A refusal is logged at warn as
#   `skipping plugin <name>: admission failed (<reason>)`
#   (evo plugin_discovery). Any such line since the start fails
#   that plugin, whatever else is true of it.
# - An admitted out-of-process plugin binds
#   /run/evo/plugins/<name>.sock. No socket once admission has
#   settled fails that plugin. Admission is not logged at the
#   default warn level, so the socket is the admit signal, as it
#   is for evo-install.sh's post-condition.

# plugin_admission_verdict <journal> <admitted> <name>...
#   journal   `journalctl -u evo -o cat` since the steward started
#   admitted  plugin names with a socket, one per line
#   name      each plugin the deploy shipped
plugin_admission_verdict() {
    local journal="$1" admitted="$2"
    shift 2
    local name refusal failed=0
    for name in "$@"; do
        refusal="$(grep -F "skipping plugin ${name}: admission failed" <<<"${journal}" | tail -n 1 || true)"
        if [[ -n "${refusal}" ]]; then
            echo "  [FAIL] plugin ${name} refused at admission ${refusal#*: admission failed }"
            failed=1
        elif ! grep -qxF "${name}" <<<"${admitted}"; then
            echo "  [FAIL] plugin ${name} not admitted: no /run/evo/plugins/${name}.sock"
            failed=1
        else
            echo "  [ok]  plugin ${name} admitted"
        fi
    done
    return "${failed}"
}
