# SMB share inventory — evo-device-audio

**Normative** product contract for what this distribution exports
when the on-device SMB server is enabled. Implementation
(`org.evoframework.network.smb-server`), bootstrap, UI, and LAN
acceptance tests MUST match this file. Update this document and
the code constants in the same change.

Bound by a companion design record in the internal decision
tree, which cites this file by repo path and does not
duplicate the share table.

Lineage (do not re-derive from chat):

- Classic Volumio: `smb.conf.tmpl` — stock Internal / USB / NAS
- volumio-evo: `docs/SAMBA.md` + `crates/core/src/samba_conf.rs`
- Framework plugin stage: the `evo-plugins-stage` share
  convenience atop the framework's stage-watcher primitive
- Shelf / verbs: network file-source contract; wire shape:
  `shares.v1.toml`

This document is the **audio-distribution** inventory. Framework
ADRs own *admission* and *update channels*; they do not own the
LAN share list for this device.

---

## Ownership

| Rule | Detail |
|------|--------|
| Sole writer | `org.evoframework.network.smb-server` is the only writer of `/etc/samba/smb.conf` on this distribution. |
| No dual ownership | Do not install parallel shares via `smb.conf.d/` drop-ins that this plugin's apply would overwrite or ignore. Fold required shares into the rendered conf. |
| Apply replaces whole conf | Rendered text is authoritative. Distro default Samba persona MUST NOT remain visible on the LAN after a successful apply. |
| Disable | When SMB is disabled, stop `smbd` (and `nmbd` if installed). Do not leave a half-live server advertising junk shares. |

---

## Stock shares (always present when SMB enabled)

These are **not** `extra_shares`. The renderer MUST emit them
whenever `enabled = true`. Guest-friendly, read-write — same
product reason as Volumio / volumio-evo: LAN push into the music
library roots MPD already browses.

| SMB share name | Path | Guest | Read-only | Purpose |
|----------------|------|-------|-----------|---------|
| `Internal Storage` | `/var/lib/evo/music/INTERNAL` | yes | no | Local music library. Drop files here → appear under INTERNAL in My Music. |
| `USB` | `/var/lib/evo/music/USB` | yes | no | Removable-media library segment. |
| `NAS` | `/var/lib/evo/music/NAS` | yes | no | Parent of inbound network mounts (`…/NAS/<alias>`). |

Bootstrap MUST ensure the three directories exist (already does for
the music triad). Paths are the **evo music plane**, not classic
Volumio `/data/INTERNAL` / `/mnt/USB` / `/mnt/NAS`.

---

## Delivery shares

### Plugin packages — `evo-plugins-stage`

| Field | Value |
|-------|--------|
| SMB share name | `evo-plugins-stage` |
| Path | `/var/lib/evo/plugins/stage` |
| Guest | **no** (authenticated SMB user) |
| Read-only | no |
| Purpose | Operator drops signed plugin bundles (`.tar.gz` / `.zip`) for the framework stage watcher to admit. |
| Consumer | Steward `PluginStageWatcher` (`[plugins.stage]` default dir). Signature gate unchanged — SMB is convenience, not trust elevation. |

When SMB is enabled, this share MUST be present in the rendered
conf (product opt-in for File Sharing implies the delivery plane
is available). Bootstrap MUST create the stage directory with
ownership suitable for the steward service user + SMB write.

Retired: `evo-core-eng/scripts/install/setup-smb-stage.sh` as a
parallel owner of Samba config on this distribution. Behaviour
lands in this plugin's renderer instead.

### Uploads (stock when enabled)

| Field | Value |
|-------|--------|
| SMB share name | `Uploads` |
| Path | `/var/lib/evo/uploads` |
| Guest | yes |
| Read-only | no |
| Purpose | Designated upload root beyond the music triad (schema: “music library root + upload target”). |

Bootstrap MUST create `/var/lib/evo/uploads`.

### Core / framework binary via fileshare

**Open — not in this inventory yet.** Core updates land via
the HTTPS artefact channel to a local stage path. A Samba
share for core drops is **out of scope** until an explicit
product decision amends the update-channel contract or this
inventory. Do not invent a core share in code before that
decision.

---

## Extra shares (operator-defined)

`extra_shares` are **beyond** the stock + delivery set above.
Each entry: name + path + `guest_ok`. Validated on apply.

### Allowlist (path prefixes)

| Prefix | Purpose |
|--------|---------|
| `/var/lib/evo/music` | Music library tree (INTERNAL / USB / NAS and below). |
| `/var/lib/evo/uploads` | Designated upload root. |
| `/var/lib/evo/plugins/stage` | Plugin stage (normally covered by stock delivery share; allowlisted so extras cannot be the only way to reach it). |

### Denylist (always reject, even under an allowed root)

| Prefix | Reason |
|--------|--------|
| `/var/lib/evo/settings` | Secrets and persisted credentials. |
| `/var/lib/evo/plugins/stage/rejected` | Rejected bundles; not an operator drop target for new installs. |

Classic Volumio prefixes (`/data/INTERNAL`, `/mnt/USB`, `/mnt/NAS`)
are **not** allowlisted on this distribution. Operators and
tests use the evo music plane only.

Code constants (default allowlist / denylist) live next to
`render_smb_conf` in the smb-server plugin runtime and MUST
match this table.

---

## Forbidden on the LAN (never advertise)

After a successful enable/apply, a browse of the device MUST NOT
show:

| Name / class | Why forbidden |
|--------------|----------------|
| `print$` | Debian/Samba printer-driver default. |
| `printers` | Distro printer share. |
| `homes` / per-user home shares | Distro `[homes]`; often surfaces as `nobody` when guest maps to the nobody account. |
| `nobody` | Guest/homes artefact — not a product share. |
| Unsolicited test names | e.g. ad-hoc extras left from bring-up (`WalkTest`). Only operator-chosen extras after stock/delivery. |

Rendered `[global]` MUST disable printer loading (e.g.
`load printers = no`, `printing = bsd`, `printcap name = /dev/null`
or equivalent) so stock Samba printer shares cannot reappear.

---

## LAN acceptance (rig)

With SMB enabled on a cold or warm apply:

1. Browse `\\<device>` / `smb://<device>/`.
2. **Must** list: `Internal Storage`, `USB`, `NAS`, `evo-plugins-stage`, `Uploads`.
3. **Must not** list: `print$`, `nobody`, `homes`, `printers`, or any share not in this inventory / operator extras.
4. Drop a signed plugin bundle on `evo-plugins-stage` → stage watcher admits (or rejects into `rejected/` with reason).
5. Drop a music file on `Internal Storage` → visible under INTERNAL in the library after MPD update.

---

## UI contract (brief)

| Surface | Behaviour |
|---------|-----------|
| Stock + delivery shares | Shown as fixed (not editable path/name); clarify guest vs auth for `evo-plugins-stage`. |
| Extra shares | List editor (name, path, guest_ok) per `shares.v1.toml`. |
| Enable / min_protocol / SMB users | Existing File Sharing controls. |

Copy that claims “share this device's library” is true only while
the stock music shares exist in the rendered conf.

---

## Boundary (do not conflate)

| Plugin | Role |
|--------|------|
| `org.evoframework.network.shares` | **Client** — mount remote NAS/NFS into `/var/lib/evo/music/NAS/<alias>`. |
| `org.evoframework.network.smb-server` | **Server** — export this device on the LAN per this inventory. |

---

## Change discipline

1. Edit this file.
2. Update renderer + allow/deny constants + bootstrap mkdirs in the same change set.
3. If the inventory gains/removes a stock or delivery share, update the companion design record only when the *invariant* changes; do not duplicate the table into that record.
4. Schema (`extra_shares` “beyond shipped defaults”) stays aligned with the stock/delivery sections above.
