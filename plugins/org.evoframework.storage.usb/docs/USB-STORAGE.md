# USB storage inventory — normative for the audio distribution

This document is the sole source of truth for the USB-storage
values below (thin-doc + living-inventory pattern). Implementation,
bootstrap, catalogue, and UI copy MUST match this file.
Contradictions between code and this file are defects to be
closed against this file.

**Scope:** removable USB mass-storage as a music source
(discovery / mount lifecycle / hardware acceptance). Not
in scope: system-disk-as-media (banned by construction —
see §1); CD-ROM; USB DACs; USB Wi-Fi; per-drive Samba
sections.

---

## 1 System-disk classification (R1 — hard-refuse rule)

A block device is classified **system player disk** when ANY
of the following holds:

1. The device (or its parent disk) is the block backing for a
   mount at `/`, `/boot`, `/boot/firmware`, `/imgpart`, or the
   distribution's state root `/var/lib/evo` (or configured
   equivalent).
2. Any partition on the same physical disk (per `lsblk`
   `PKNAME` correlation) as a mount from rule 1.
3. Union match, not intersection: if EITHER rule 1 or rule 2
   applies to any partition on the disk, the whole disk (and
   every partition on it) is system.

Detection substrate: `/proc/self/mountinfo` (source of truth
per `NETWORK-SHARES-MOUNT-TRUTH-HANDOFF.md` pattern) +
`lsblk -J -o NAME,PKNAME,MOUNTPOINT,TRAN,TYPE,UUID,LABEL,FSTYPE`
+ `findmnt -no SOURCE <path>` to walk mount → block device.

**System-disk hard-refuse invariant** — for every classifier
output `system-disk`:

| Verb / behaviour | Response |
|---|---|
| Auto-mount on hotplug | **NEVER** — the classifier prunes system-disk before enumeration. |
| `storage.usb.list_drives` | Include with `class: "system-disk"` and no actionable verbs. |
| `storage.usb.safe_remove` | Refuse with `SystemDiskRefused { device }` structured error. |
| `storage.usb.repair_filesystem` | Refuse with `SystemDiskRefused { device }`. |
| `storage.usb.mount` | Refuse with `SystemDiskRefused { device }`. |
| Samba parent share export | Not affected — Samba serves `/var/lib/evo/music/USB/*` and system disks are never mounted there. |

The classifier's fact-tree is fixture-testable — every union
rule has a unit test with a synthetic `mountinfo` + `lsblk`
output. See § "Test fixtures" below.

---

## 2 Filesystem support matrix (P0)

| FS family | Max volume size (this plugin) | Mount option string | Dirty detection | Repair tool | Package (Debian Trixie) |
|---|---|---|---|---|---|
| `vfat` (FAT16 / FAT32) | **2 TiB** (FAT32 on-disk cap; refuse at mount when device size > 2 TiB with a copy-string surfaced to the operator recommending exFAT / ext4 reformat) | `noatime,dmask=0000,fmask=0000,iocharset=utf8,uid=<SERVICE_UID>,gid=<SERVICE_GID>` | `fsck.vfat -n <dev>` exit code 1 = dirty | `fsck.vfat -a <dev>` | `dosfstools` |
| `exfat` | no practical limit (128 PiB spec ceiling; plugin does not cap) | `noatime,dmask=0000,fmask=0000,iocharset=utf8,uid=<SERVICE_UID>,gid=<SERVICE_GID>` | `fsck.exfat -n <dev>` exit code non-zero = dirty | `fsck.exfat -a <dev>` | `exfatprogs` |
| `ntfs` | no practical limit (256 TiB per volume; plugin does not cap) | `noatime,dmask=0000,fmask=0000,uid=<SERVICE_UID>,gid=<SERVICE_GID>,windows_names,big_writes` | `ntfsfix --no-action <dev>` reports dirty / hiberfile | `ntfsfix <dev>` (accepts dirty + hiberfile per policy) | `ntfs-3g` |
| `ext2` / `ext3` / `ext4` | no practical limit (1 EiB on ext4; plugin does not cap) | `noatime` | `dumpe2fs -h <dev>` needs_recovery flag OR feature-flag inspection | `e2fsck -p <dev>` (auto-repair; escalate to `-y` on operator confirm) | `e2fsprogs` |

**Volume-size handling (large drives — > 2 TiB).** The
plugin queries `blockdev --getsize64 <device>` at classify
time and stores the byte size on `DriveRecord.size_bytes`
(§5). On mount attempt for FAT32 volumes reporting a device
size above 2 TiB, the mounter refuses with `class:
"mount-failed-oversized-vfat"` and surfaces an operator-visible
copy string ("This drive is larger than 2 TB and formatted as
FAT32; reformat as exFAT (Windows / macOS-compatible) or ext4
(Linux-native) to mount"). The refuse path is a normative
mount-decision; do not silently truncate, silently switch FS
driver, or silently fail without the operator hint. exFAT,
NTFS, and ext4 have no plugin-enforced ceiling — they mount
at whatever `blockdev` reports.

Support-matrix additions require an edit to this file. The
plugin's runtime uses the FS family key to select the option
string, dirty-check invocation, and repair invocation. Unknown
FS types surface as `class: "unsupported"` in the drive
listing with no actionable verbs.

**Mount uid/gid:** `<SERVICE_UID>` / `<SERVICE_GID>` derive
from the distribution's steward service user — resolved at
plugin load via `/proc/self/status` euid + `/etc/passwd`
(same `detect_service_user_from_procfs` pattern the smb-server
plugin uses for its blocklist). Ensures MPD (running as
`SERVICE_USER`) and Samba (which serves the stock `USB` share
under `force user = SERVICE_USER`) both read the tree.

**NTFS repair policy:** `ntfsfix` accepts dirty flag but
refuses volumes with an active Windows hiberfile
(`hiberfil.sys`). The plugin surfaces
`class: "mounted-dirty-hiberfile"` for that specific state
with an operator-visible copy string ("This drive was left
suspended by Windows — resume + shut down cleanly before
repair") and no repair verb offered.

---

## 3 Mount root + stable-id derivation

**Root:** `/var/lib/evo/music/USB/<stable-id>/`

Every media USB volume mounts here — no exceptions. This is the
same root the Samba parent `[USB]` share (per the smb-server
plugin's `SAMBA-SHARES.md` inventory) exports via
`force user = SERVICE_USER`, so mounts appear on the LAN
automatically without `smb.conf` churn.

**Stable-id derivation — user-friendly first, technical fallback last, deterministic enumeration on collision.**

The stable-id must be:

- **Readable on glass** — the operator sees the id in the
  Sources UI, in the Samba path (`\\<host>\USB\<stable-id>`),
  and in the mount path they may `cd` into over SSH. A hex
  UUID is not a name; a manufacturer + model is.
- **Stable across replug** of the same volume — same physical
  volume attached to any port must resolve to the same id.
- **Unique across concurrent mounts** — two volumes plugged
  in at the same time must never collide on the mount path.
- **Deterministic on repeated collision** — plugging the same
  two "Music" sticks in the same order must produce the same
  suffixes each session.

### Derivation ladder (first match wins for the base id)

Base id sourced from the first rule that yields a non-empty
sanitised token:

0. **Operator alias** (highest precedence). If the plugin's
   persisted state has a `(vendor, model, serial_short, partuuid)`
   → alias entry for this volume, base id = `<sanitised-alias>`.
   Set via the `storage.usb.rename` verb (§4); survives replug on
   the same rig; travels with the physical volume, not the port.
   See §7 ("Alias persistence") for the state file shape.
1. **Filesystem label** — udev `ID_FS_LABEL`. Sanitised per
   the token rule below. Base id = `<sanitised-label>`.
   Reflects the operator's or manufacturer's chosen name at
   format time; user-friendly when set.
2. **Vendor + model** — udev `ID_VENDOR` + `ID_MODEL`
   composite (e.g. `SanDisk-Cruzer-Blade`, `WD-Elements-25A2`,
   `Samsung-T7`). Sanitised. Base id =
   `<sanitised-vendor>-<sanitised-model>`. Reflects the
   manufacturer name printed on the enclosure; the operator's
   next-best mental handle after a label.
3. **Model-only** — `ID_MODEL` only when `ID_VENDOR` is
   empty or "USB" (common on white-label sticks). Base id =
   `<sanitised-model>`.
4. **Synthesized fallback** — `unlabelled-<sanitised-vendor-or-usb>-<serial-short-6>`
   where `serial-short-6` is the last 6 characters of
   `ID_SERIAL_SHORT` (lowercased, alphanumeric only). Only
   reached when a drive exposes neither label nor a model
   readable by udev — rare, mostly antique or intentionally
   generic devices.

### Sanitisation

Sanitised tokens match `^[A-Za-z0-9][A-Za-z0-9_-]{0,31}$`:
letters + digits + underscore + hyphen; first char alphanumeric;
1..=32 chars. Any other char → `-`; runs of `-` collapse; leading
`-` stripped. Empty result after sanitisation → skip the rule
and fall to the next one. Case preserved (Samba is
case-insensitive so `Music` and `music` resolve the same on
the LAN; the UI shows the case the operator chose).

### Partition suffix (when a base id needs multiple partitions)

Some sticks / SSDs carry multiple partitions the plugin would
mount. The base id names the DISK; each mounted partition gets
`-p<N>` where `N` is the partition number (`sda1` → `-p1`,
`sda2` → `-p2`). Skipped for the common single-partition case:
`SanDisk-Cruzer-Blade` (one partition) vs
`SanDisk-Cruzer-Blade-p2` (second partition on the same
physical stick).

### Collision handling — enumeration + disambiguation

For every mount, the plugin computes the candidate id then
resolves collision against existing mounts at
`/var/lib/evo/music/USB/*`:

- **Same base id, DIFFERENT physical volume** (two "Music"
  sticks plugged in simultaneously; two identical
  `SanDisk-Cruzer-Blade` sticks): append `-2`, `-3`, …
  Enumeration follows udev event order — first-arriving keeps
  the bare base id; second gets `-2`; third gets `-3`.
  Enumeration is DETERMINISTIC within a boot: cold-plug
  reconcile at plugin load sorts by `/sys/class/block/<dev>/dev`
  major:minor to give a stable ordering that survives across
  boots on the same rig with the same drives.
- **Same base id, SAME physical volume replugged** (drive
  unplugged then replugged): the same base id resolves again;
  no `-2` suffix. Detection: match on
  `(ID_VENDOR, ID_MODEL, ID_SERIAL_SHORT, PARTUUID)` tuple —
  if any prior mount session under this base id had the
  same tuple, reuse that id.
- **Base id would collide with a system-disk stub row** (§1
  §system-disk entries carry no mount but appear in
  `list_drives`): the media volume wins the base id; system
  rows are display-only.

### Stability contract summary

| Scenario | Behaviour |
|---|---|
| Same volume replugged (same port) | Same stable-id (label-source rules 1-3 stable per volume; enumeration re-derives to the same suffix if any) |
| Same volume replugged (different port) | Same stable-id (id is disk-property-derived, not port-property-derived) |
| Two identical drives simultaneously | Deterministic enumeration `-2`, `-3`, … in udev event order (or by major:minor at cold-plug) |
| Operator relabels the drive | New stable-id (intentional — the new label IS the new logical name) |
| Drive with no label + no vendor/model | `unlabelled-<vendor-or-usb>-<serial-short-6>` — stable across replug; hex-free enough to be identifiable |
| Volume > 2 TiB formatted FAT32 | No stable-id assigned (mount refuses at classify); row surfaces as `class: mount-failed-oversized-vfat` with the refuse-copy string from §2 |

### Mount-point lifecycle

- `create_dir_all(/var/lib/evo/music/USB/<stable-id>/)` at
  mount time with `SERVICE_USER` ownership.
- Empty-only `rmdir` at unmount time — prevents destroying
  operator files if the unmount races a mid-write.
- Enumeration suffix rows (`Music-2`, `SanDisk-Cruzer-Blade-3`)
  get the same lifecycle; no shared parent dir with the
  first-arriving id.

---

## 4 Verb inventory (`storage.usb.v1` shelf)

| Verb | Payload | Response | Auth | Failure classes |
|---|---|---|---|---|
| `storage.usb.list_drives` | `{}` | `{ drives: [DriveRecord] }` — see §5 | none | never fails |
| `storage.usb.mount` | `{ stable_id }` | `{ mounted_at, class }` | operator | `system_disk_refused`, `unsupported_fs`, `mount_failed_dirty`, `mount_failed_oversized_vfat`, `subprocess_io` |
| `storage.usb.safe_remove` | `{ stable_id, force?: bool }` | `{ removed: true }` | operator | `system_disk_refused`, `busy`, `subprocess_io` |
| `storage.usb.repair_filesystem` | `{ stable_id, escalate?: bool }` | `{ repaired: true, before_class, after_class }` | operator + step-up | `system_disk_refused`, `unsupported_fs`, `repair_failed`, `subprocess_io` |
| `storage.usb.rename` | `{ stable_id, alias }` — `alias` sanitised per §3 token rule (`^[A-Za-z0-9][A-Za-z0-9_-]{0,31}$`), empty alias clears the operator alias and falls back to the next rule in the derivation ladder | `{ new_stable_id, class }` — new_stable_id may equal current stable_id when the alias resolves to the same sanitised token after enumeration | operator | `system_disk_refused`, `invalid_alias`, `alias_would_collide` (only when the operator's requested alias collides with a foreign physical volume and enumeration cannot resolve — rare), `subprocess_io` (from the required unmount + remount cycle) |

**Rename semantics.** Alias write flows through a full
remount cycle because the mount path IS the friendly id
(operator `cd`s to it, Samba serves at it):

1. Alias sanitised + validated (empty → clear).
2. Persist `(vendor, model, serial_short, partuuid) → alias`
   to plugin state (`/var/lib/evo/plugins/org.evoframework.storage.usb/state/aliases.toml`).
3. Recompute stable-id per §3 with the new alias at rule 0.
4. Consumer-stop for the current `library_source_id`
   (identical to safe-remove step 3).
5. `sync` + clean `umount` from the OLD mount path.
6. Empty-only `rmdir` on the OLD `/var/lib/evo/music/USB/<old-id>`.
7. Mount at the NEW mount path.
8. `library.add_source local_usb` under the new id.
9. Republish subject with the new DriveRecord.

The operator experiences: "Rename" → brief spinner while
playback pauses if the drive is playing → new name on glass
→ Samba path reflects new name on next browse. Files never
touched.

Read-only subject: `storage_usb_drives`, singleton addressing
scheme `evo.storage.usb.drives:local` — carries the same
`DriveRecord[]` payload as `list_drives`. Republished on every
hotplug attach, hotplug detach, mount, umount, rename, and
repair-complete. UI subscribes at Sources page mount.

---

## 5 `DriveRecord` shape (subject + list_drives response)

```
DriveRecord {
    stable_id:            string           // §3 derivation (with alias precedence)
    display_name:         string           // sanitised display token = stable_id sans partition suffix
    id_source:            IdSource         // enum: which rule in §3 produced the base id
    device_node:          string           // e.g. "/dev/sda1"
    parent_disk:          string           // e.g. "/dev/sda"
    partition_index:      u32              // 1-based partition number within parent_disk
    partition_count:      u32              // total partitions on parent_disk
    label:                Option<string>   // fs label if present
    uuid:                 Option<string>   // fs uuid if present
    partuuid:             Option<string>   // GPT PARTUUID if present (alias-key component)
    vendor:               Option<string>   // udev ID_VENDOR (e.g. "SanDisk", "WD", "Samsung")
    model:                Option<string>   // udev ID_MODEL (e.g. "Cruzer-Blade", "Elements-25A2", "T7")
    serial_short:         Option<string>   // udev ID_SERIAL_SHORT (alias-key component)
    fs_type:              string           // "vfat" | "exfat" | "ntfs" | "ext4" | "unsupported"
    size_bytes:           u64              // blockdev --getsize64; drives the >2TiB FAT32 refuse (§2)
    class:                DriveClass       // enum below
    mount_root:           Option<string>   // "/var/lib/evo/music/USB/<stable_id>" when mounted
    library_source_id:    Option<string>   // library.add_source result when class=mounted-*
    alias_set:            bool             // true when this drive has an operator-set alias (rule 0 fired)
    last_transition_at:   i64              // wall-clock ms of last state change
}

IdSource =
  | "operator_alias"                       // rule 0 (§3)
  | "fs_label"                             // rule 1
  | "vendor_model"                         // rule 2
  | "model_only"                           // rule 3
  | "synthesized"                          // rule 4

DriveClass =
  | "system-disk"                          // §1 hard-refuse
  | "unsupported"                          // fs_type not in §2 matrix
  | "unmounted"                            // detected, not yet mounted
  | "mounted-clean"                        // mounted, no dirty flag
  | "mounted-dirty"                        // mounted, dirty flag on
  | "mounted-dirty-hiberfile"              // NTFS hiberfile present
  | "mount-failed-dirty"                   // mount refused due to dirty state
  | "mount-failed-oversized-vfat"          // >2TiB FAT32 refuse per §2
  | "mount-failed-other"                   // mount errno other than dirty / oversized
```

`class` transitions on the subject drive the UI state (Safe
remove offered when `mounted-*`; Repair offered when
`mounted-dirty` or `mount-failed-dirty`; Rename offered when
class is not `system-disk`; reformat-copy shown when class is
`mount-failed-oversized-vfat`; nothing actionable when
`system-disk` / `unsupported`). `id_source` drives the "how
did I get this name?" hint in the row's tooltip — operators
seeing `synthesized` know the drive has no label or model and
should consider renaming it.

---

## 6 Sudoers grants + wrapper (§ privilege model)

**Wrapper:** `/usr/lib/evo/evo-usb-mount`

Distribution-owned narrow root-only shell that takes an action
verb + stable-id and dispatches the correct `mount` / `umount`
/ `fsck.*` / `eject` invocation. Path-allowlisted to
`/var/lib/evo/music/USB/*` for mount targets; block-device
guard refuses any non-`/dev/sd*` / `/dev/nvme*n*p*` /
`/dev/mmcblk*p*` argv. Same trust-boundary discipline as
`/usr/local/bin/evo-smb-user-sync`.

Wrapper actions:

| Action | Invocation |
|---|---|
| `mount <stable-id> <fs-type> <device-node>` | `mount -t <fs> -o <options-per-§2> <device-node> /var/lib/evo/music/USB/<stable-id>` |
| `umount <stable-id>` | `umount /var/lib/evo/music/USB/<stable-id>` |
| `umount-force <stable-id>` | `umount -l /var/lib/evo/music/USB/<stable-id>` (only via `safe_remove force: true`) |
| `fsck <stable-id> <fs-type> <device-node>` | dispatches per §2 repair-tool matrix |
| `eject <parent-disk>` | `eject <parent-disk>` (best-effort; failure logged, not fatal) |

**Sudoers:** `/etc/sudoers.d/evo-storage-usb` installed by
`bootstrap.sh` Step 1g'. Template ships at
`plugins/org.evoframework.storage.usb/dist/sudoers.d/evo-storage-usb.in`:

```
Cmnd_Alias EVO_STORAGE_USB = /usr/lib/evo/evo-usb-mount
@EVO_SERVICE_USER@ ALL=(ALL) NOPASSWD: EVO_STORAGE_USB
```

`@EVO_SERVICE_USER@` substituted at install time by
`bootstrap.sh` (same pattern as `evo-network-shares`,
`evo-samba-server`). No other sudoers alias grants raw
`mount` / `umount` / `fsck` / `eject`.

---

## 7 Hotplug + coldplug lifecycle

**Hotplug:** udev userspace monitor via `nix::mount` / raw
netlink subscription (no `udisks2` runtime dependency for P0).
The plugin's warden owns the monitor; on every `add` event
matching `SUBSYSTEM=block`, `DEVTYPE=partition`, and
`ID_BUS=usb` (or `TRAN=usb` via `lsblk` lookup):

1. Classifier resolves the parent disk and every partition.
   Reads udev attributes `ID_VENDOR`, `ID_MODEL`,
   `ID_SERIAL_SHORT`, `ID_FS_LABEL`, `ID_FS_UUID`,
   `ID_PART_UUID` from `/run/udev/data/b<major>:<minor>` (or
   equivalent). Populates `DriveRecord` §5.
2. System-disk check per §1 — if hit, publish subject with
   `class: system-disk` and stop (no mount).
3. FS-type check per §2 — if unsupported, publish with
   `class: unsupported` and stop.
4. **Alias resolve.** Look up
   `(vendor, model, serial_short, partuuid)` in the plugin
   state file (§ "Alias persistence" below). If a hit,
   set `id_source = operator_alias` and use the alias as the
   base id (rule 0). Otherwise fall through §3 rules 1-4.
5. **Volume-size check.** For `vfat`, if `size_bytes` > 2 TiB,
   publish `class: mount-failed-oversized-vfat` and stop. Do
   not attempt the mount. See §2 for the operator copy string.
6. Compute stable-id per §3 (base id + partition suffix if
   parent_disk has multiple mountable partitions + enumeration
   `-2`/`-3`/… on collision). Persist the id-source enum on
   the DriveRecord for the UI tooltip hint.
7. Mount attempt via wrapper. On success, republish subject
   with `class: mounted-clean` / `mounted-dirty`. On failure,
   `mount-failed-dirty` / `mount-failed-other`.
8. When `mounted-*`: cross-plugin dispatch to
   `library.add_source` with `local_usb` record shape (per
   `library.v1.toml:92`). Record the returned
   `library_source_id` on the DriveRecord.

**Coldplug (at plugin load):** enumerate every `SUBSYSTEM=block
TRAN=usb` device via `lsblk -J -o NAME,PKNAME,MOUNTPOINT,TRAN,TYPE,UUID,LABEL,FSTYPE,PARTUUID,VENDOR,MODEL,SERIAL,SIZE`.
Sort by `/sys/class/block/<dev>/dev` major:minor before
processing — this gives DETERMINISTIC enumeration order
across boots (§3 stability contract). Then run the same
pipeline steps 1-8 per partition. Mount-truth reconcile per
`/proc/self/mountinfo`: if a volume is already mounted at
`/var/lib/evo/music/USB/<stable-id>/` (operator-mounted
before plugin load, or leftover from a previous plugin
instance), adopt without remounting — same adopt discipline
as `network.shares::adopt_existing_os_mount`.

**Detach:** on `remove` udev event, retract `library.remove_source`,
best-effort `umount`, republish subject with drive removed
from the list. Alias state is NOT retracted on detach — the
alias persists so replug of the same physical volume resolves
to the same operator-chosen name.

### Alias persistence

State file: `/var/lib/evo/plugins/org.evoframework.storage.usb/state/aliases.toml`

Mode `0600`, owned by `SERVICE_USER`. Same
`state.save`-with-mode-0600 discipline the smb-server plugin
uses for its `smb_server.toml`.

Shape (TOML):

```toml
schema_version = 1

[[alias]]
vendor        = "SanDisk"
model         = "Cruzer-Blade"
serial_short  = "4C530"
partuuid      = "a1b2c3d4-01"
alias         = "My-Vinyl-Rip"
set_at_ms     = 1786100000000

[[alias]]
vendor        = "WD"
model         = "Elements-25A2"
serial_short  = "WCC7K1"
partuuid      = "e5f6a7b8-02"
alias         = "Backup-2026"
set_at_ms     = 1786100005000
```

The `(vendor, model, serial_short, partuuid)` tuple is the
identity key. Match rules:

- **Exact tuple match** → alias applies.
- **Partial tuple match** (e.g. `partuuid` absent because the
  drive is MBR-partitioned) → the plugin degrades to
  `(vendor, model, serial_short, partition_index)` and matches
  on that. Documented in the alias-set flow so operators
  understand the identity is a bit coarser on MBR sticks.
- **No match** → no alias; derivation falls through to rule 1
  (fs label) and below.

The state file is written atomically (tmp + rename) on every
`storage.usb.rename` verb success. Ownership and mode enforced
per write.

Clearing an alias: `storage.usb.rename { stable_id, alias: "" }`
removes the matching entry. Drive re-mounts under the next
rule in the ladder (fs label / vendor-model / etc.).

---

## 8 Dirty repair path (R3 — normative)

1. UI operator gestures "Repair" on a `mounted-dirty` drive.
2. Plugin refuses if `class: system-disk` (§1 invariant).
3. Plugin dispatches consumer-stop to `playback.mpd`
   (`disposition::pause_and_clear_local_usb_source` — mirrors
   the network-shares MPD-stop-before-CIFS-mutation pattern).
4. Plugin dispatches `library.remove_source` for the drive's
   `library_source_id` (MPD update on library-side).
5. `sync` on the drive's parent disk.
6. Wrapper `umount <stable-id>`.
7. Wrapper `fsck <stable-id> <fs-type> <device-node>` per §2
   repair tool.
8. On repair success: wrapper `mount <stable-id> …` again;
   `library.add_source local_usb` again; republish subject with
   `class: mounted-clean`.
9. On repair failure: subject republished with
   `class: mount-failed-dirty` and a per-FS operator-visible
   copy string (e.g. NTFS hiberfile-refuse text).

Consumer-stop-before-mutation is normative — mirrors the
`network.shares` MPD-stop-before-CIFS-mutation pattern. No
`fsck` runs while MPD holds files open.

---

## 9 Safe-remove path (R4 — normative)

1. UI operator gestures "Safe remove" on a `mounted-*` drive.
2. Plugin refuses if `class: system-disk` (§1 invariant).
3. Consumer-stop: `library.remove_source` for the drive's
   `library_source_id` (MPD update).
4. `sync` on the drive's parent disk.
5. Wrapper `umount <stable-id>` (clean umount). On EBUSY:
   - If `force: false` (default), return `Busy { holders }`
     with the fuser-derived holder list. UI shows "Files are
     in use — stop playback and try again" or "Force" button.
   - If `force: true`, wrapper `umount-force <stable-id>` (lazy
     detach `-l`). Warn logged, `removed: true` returned.
6. Best-effort SCSI eject via wrapper `eject <parent-disk>`.
   Failure logged; not fatal (some drives ignore eject).
7. Retract from factory subject list.
8. UI shows "OK to unplug".

---

## 10 UI contract (Sources / Library — USB panel)

| Surface | Behaviour |
|---|---|
| Sources page | New "USB drives" section under existing SMB / NAS sections. Rows sourced from `storage_usb_drives` subject. |
| Row primary line | `display_name` (from `DriveRecord.display_name` — stable-id sans partition suffix; matches what the operator sees at `\\<host>\USB\<display_name>` on the LAN). |
| Row secondary line | `vendor + model + size` (e.g. "SanDisk Cruzer Blade · 32 GB"), plus `fs_type` (uppercased: FAT32 / exFAT / NTFS / ext4). Size shown human-friendly per SI unit (KB / MB / GB / TB). |
| Row tooltip / info popover | Shows `id_source` explanation: e.g. `operator_alias` → "Named by you", `fs_label` → "Named at format time", `vendor_model` → "Named from device model — Rename to give it a friendlier name", `synthesized` → "No label or model — Rename recommended". |
| Row affordances by class | `mounted-clean`: **Rename** + Safe remove. `mounted-dirty`: **Rename** + Safe remove + **Repair**. `mounted-dirty-hiberfile`: **Rename** + Safe remove + copy string ("resume + shut down cleanly before repair") + no repair. `mount-failed-dirty`: **Rename** + **Repair**. `mount-failed-oversized-vfat`: no actions + copy string ("This drive is larger than 2 TB and formatted as FAT32; reformat as exFAT or ext4 to mount"). `unsupported`: greyed row + copy string ("FS type <x> not supported"). `system-disk`: hidden by default; visible under a "System storage (read-only)" collapsed section; NO Rename (invariant). |
| Rename affordance | Inline text field (or modal on small screens). Client-side sanitises per §3 token rule as the operator types; disables submit on invalid input; shows preview of resulting `\\<host>\USB\<new-name>` path. On submit: fires `storage.usb.rename`, shows brief spinner while the plugin runs the remount cycle (§4), refreshes on subject republish. Empty input = "Reset name" (clears the operator alias — falls back to fs label / vendor+model). |
| Rename validation | Live inline: min 1 / max 32 chars; first char alphanumeric; subsequent chars alphanumeric / underscore / hyphen. Reserved names refused with copy string ("This name conflicts with another drive currently plugged in — pick another"): tests against current mount roots + system-disk stub rows. |
| Rename confirm | Only when the drive currently has a `library_source_id` (i.e. is mounted and in the library): "Renaming will briefly stop playback if this drive is playing. The name change reflects immediately on the network share and file browser. Continue?" |
| Repair confirm | Modal: "This will unmount and check <display_name>. Files must be unopened; playback stops. Continue?" |
| Force remove | Only offered on `Busy` response; second modal: "Files are still open. Force eject may cause data loss. Continue?" |
| `remount_usb` recovery hint | Wired to `storage.usb.mount` retry against the drive's stable-id. Consumed by disposition renderer per `playback.v1.toml:602`. |
| Multiple identical drives | Enumeration suffixes (`Music`, `Music-2`, `Music-3`) render as distinct rows with a "1 of 3" / "2 of 3" / "3 of 3" subscript when the operator has not renamed any of them. Rename encouraged via the tooltip hint. |
| Oversized-FAT32 copy | Modal (dismissable, non-actionable): "This drive is <size> — larger than the 2 TB FAT32 limit. To use it as a music source, reformat as exFAT (Windows / macOS compatible) or ext4 (Linux native). Formatting is not offered in the operator UI — use your desktop's disk utility."|

---

## 11 Boundary (do not conflate)

| Plugin | Role |
|---|---|
| `org.evoframework.storage.usb` | **Owner** — block hotplug, classify, mount, umount, fsck, eject; cross-plugin dispatch to library. |
| `org.evoframework.playback.mpd` | **Consumer** — `SourceKind::LocalUsb` on-mount-event trigger; MPD update on library-mutation; disposition-stop before mount-mutation. |
| `org.evoframework.network.smb-server` | **Consumer** — Samba parent `[USB]` share exports the tree. Storage.usb NEVER writes `smb.conf`. |
| `evo-core-eng` (framework) | **Nothing** — no storage / block / mount knowledge. Framework-hosted storage substrate was extracted to plugins on 2026-07-17 (`NetworkSharesRuntime` / `SambaServerRuntime`) and MUST NOT recur here. |

---

## 12 Test fixtures

**Unit-test fixtures** for the system-disk classifier — every
union rule from §1 has a synthetic `/proc/self/mountinfo` +
`lsblk -J` output check-in:

| Fixture | Description | Expected class |
|---|---|---|
| `pi5-nvme-boot` | Pi 5 booting from NVMe (root on /dev/nvme0n1p2); USB stick /dev/sda1 vfat plugged | sda: media, nvme0n1: system-disk union |
| `pi5-usb-boot` | Pi 5 booting from USB SSD (root on /dev/sda2); second USB stick /dev/sdb1 vfat plugged | sda: system-disk union (rules 1+2), sdb: media |
| `nuc-nvme-boot-usb-stick` | NUC on NVMe; USB stick vfat plugged | nvme0n1: system-disk, sda: media |
| `vm-virtio-root-usb-passthrough` | VM with root on /dev/vda; USB stick passthrough as /dev/sda | vda: system-disk, sda: media |
| `relabelled-boot-partition` | Rootfs partition relabelled as "USB_MUSIC" | Still system-disk (rule 1: parent-of-mount, not label) |
| `system-disk-with-unmounted-partition` | rootfs on /dev/sda2; /dev/sda3 is an unmounted vfat partition | Both partitions system-disk (rule 2: same parent disk) |

Fixtures live at
`plugins/org.evoframework.storage.usb/tests/fixtures/<fixture>/{mountinfo,lsblk.json,expected.json}`.

**Hardware acceptance:** across the supported target
triples (aarch64 Pi 5, x86_64 NUC, x86_64 VM). Each
target: plug FAT stick → row appears → browse → play →
dirty FAT stick → Repair → clean row → Safe remove → row
removed → Samba parent share shows the tree via
`smbclient` during mounted window. Evidence emitted at
`/var/lib/evo/evidence/storage-usb-hardware-acceptance-<triple>.toml`.

---

## 13 Change discipline

1. Edit this file first.
2. Update the plugin's classifier / mounter / verbs / subject
   in the SAME change set.
3. Update the sudoers template, wrapper, and bootstrap install
   stanza in the SAME change set.
4. Update the catalogue schema `storage.usb.v1` in the
   canonical schemas repository in the SAME change set (or the
   plugin fails admission).
5. Update the UI Sources / Library USB rows in the UI shell
   as a coordinated change (the UI consumes this file's
   subject + verb contract; not a plugin-side change).
6. If any invariant here changes (system-disk union expands;
   FS support matrix drops a type; mount root moves off
   `/var/lib/evo/music/USB/`), amend this file first, then
   code.

Reviewers refuse storage.usb code changes whose diffs do not
touch this file OR whose diffs contradict what this file says.
