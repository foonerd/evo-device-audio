# The in-app updater

The device updates its own plugins, and the host files they rely on, from
the artefacts plane, from the operator's surface. Nothing is installed
that a signed document and a hash did not name.

## What the operator can do

The `system.updater` shelf (plugin `org.evoframework.system.updater`)
answers six verbs, each with `v: 1`:

| Verb | Gate | What it does |
|---|---|---|
| `updater.check` | `write:system_admin` | Starts a check of the recorded channel's registry. |
| `updater.apply { component, version }` | `write:system_admin` | Applies one offered component: a plugin, or `evo-host-files`. |
| `updater.channel.set { channel }` | `write:system_admin` | Records `alpha`, `test` or `production`. |
| `updater.status` | read | Channel, state, last run, last inventory. |
| `updater.history { limit? }` | read | The last runs, newest first (at most 50). |
| `updater.channel.get` | read | The recorded channel (`production` until set). |

A verb that starts work answers at once with `accepted`. Poll
`updater.status`: `starting`, then `checked`, `applying`, `applied`,
`refused` or `rolled_back`. A refusal carries `error.code`.

A check offers a plugin that is installed and listed when the listed
version is newer, or the same version with different bytes than the
last bundle this updater applied (`reason` `newer`, `rebuild`; a plugin it
never applied is offered once as `unknown_bytes`). Host files are offered
with no installed record, a newer release, a changed carrier, or a missing
or different wrapper. Downgrades are never offered. A plugin that is not
installed is not offered.

## What an apply does

1. Fetches the bundle named by the signed registry, from under the
   configured base URL only, and checks its SHA-256.
2. Unpacks it into a root-owned scratch directory, refusing paths that
   leave the tree and any link, and checks the manifest names the requested
   plugin and version.
3. Copies the installed bundle to `/opt/evo/rollback/<plugin>-<utc>/`
   (the newest two are kept).
4. Replaces `/opt/evo/plugins/<plugin>` by rename and restarts `evo`.
5. Waits for the plugin to be admitted again. If it is not, the previous
   bundle is put back, `evo` is restarted, and the run ends `rolled_back`.

Nothing under `/var/lib/evo/plugins/<plugin>` (plugin `state/` and
`credentials/`) is read, written, moved or removed.

Applying the host files installs the wrappers (`/usr/local/bin/evo-*`) and
sudoers drop-ins (`/etc/sudoers.d/evo-*`) named by the signed
`host-files.toml`, each only if its name is on the box's allow list, each
sudoers file only after `visudo -cf`. Then it writes
`/var/lib/evo/installed.toml` (`release`, `architecture`, `built_at_utc`)
from that signed document. Replaced files are kept in
`/opt/evo/rollback/hostfiles-<utc>/` (the newest two are kept).

## How it is built

- **`evo-apply-update`** (`/usr/local/bin`, root) is the only thing the
  service user may run as root. It accepts `check`,
  `apply <component> <version>` and `channel <alpha|test|production>`,
  refuses anything else, writes nothing, and starts one instance of the
  `evo-apply-update@.service` unit.
- **The worker** (`/usr/libexec/evo-apply-update/`) is that unit's process.
  It runs as root outside the steward's sandbox, so a restart of `evo`
  does not stop it. It is not in any sudoers grant.
- **State** is `/var/lib/evo-updater/` (`root:root`, world-readable):
  `channel`, `status.json`, `inventory.json`, `history.jsonl`,
  `applied.toml`.
- **Configuration and trust** are under `/etc/evo-updater/`:
  `updater.toml` (`base_url`, optional `ca_file`), `allow.toml` and
  `trust.d/` (Ed25519 keys with a `.meta.toml` naming the plugin names each
  may sign for). The worker takes nothing from `/etc/evo` or `/var/lib/evo`,
  and refuses to run unless every directory it relies on, and every parent
  of it, is owned by root and not writable by group or other.

A registry or host-files document is accepted only if its detached Ed25519
signature verifies under a key in `trust.d`, that key may sign for the
names in it, the key's fingerprint is the one the document pins, and the
document has not expired.

## Installing the helper

`bootstrap.sh` places the helper (step 1j; skip with
`EVO_INSTALL_UPDATER=0`). On a box installed before the updater, run as
root:

    dist/scripts/install-updater.sh --dist dist --bin-dir <dir with the two
        binaries> --service-user <service user> [--fix-opt-ownership]

`--fix-opt-ownership` makes `/opt/evo`, `/opt/evo/bin` and
`/opt/evo/plugins` root-owned, which the worker requires. The helper cannot
update itself: its trust, allow list and unit are placed only by the
installer. A new shelf (a plugin that stocks a shelf the box's catalogue
does not have) also comes from the installer, which places the catalogue.

## Publishing

`scripts/release/generate-host-files.py` writes the host-files carrier and
`host-files.toml` for one release and triple from the files
`dist/etc-updater/allow.toml` names, byte-reproducibly. The registry is
`scripts/release/generate-registry.py`. Both documents are signed with the
plugin signing key and published beside the bundles.
