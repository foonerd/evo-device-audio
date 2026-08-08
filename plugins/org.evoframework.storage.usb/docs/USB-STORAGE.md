# USB storage inventory — normative for the audio distribution

**Bound by:** the companion decision record in evo-internal
(thin ADR + living inventory pattern — this file carries the
tables; the decision record binds them). This file is the sole
source of truth for the values below. Implementation,
bootstrap, catalogue, and UI copy MUST match this file.
Contradictions between code and this file are defects to be
closed against this file.

**Scope:** removable USB mass-storage as a music source
(P0.D2 / P0.G3 / P0.H4). Not in scope: system-disk-as-media
(banned by construction — see §1); CD-ROM; USB DACs; USB
Wi-Fi; per-drive Samba sections.

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

| FS family | Mount option string | Dirty detection | Repair tool | Package (Debian Trixie) |
|---|---|---|---|---|
| `vfat` (FAT12/16/32) | `noatime,dmask=0000,fmask=0000,iocharset=utf8,uid=<SERVICE_UID>,gid=<SERVICE_GID>` | `fsck.vfat -n <dev>` exit code 1 = dirty | `fsck.vfat -a <dev>` | `dosfstools` |
| `exfat` | `noatime,dmask=0000,fmask=0000,iocharset=utf8,uid=<SERVICE_UID>,gid=<SERVICE_GID>` | `fsck.exfat -n <dev>` exit code non-zero = dirty | `fsck.exfat -a <dev>` | `exfatprogs` |
| `ntfs` | `noatime,dmask=0000,fmask=0000,uid=<SERVICE_UID>,gid=<SERVICE_GID>,windows_names,big_writes` | `ntfsfix --no-action <dev>` reports dirty / hiberfile | `ntfsfix <dev>` (accepts dirty + hiberfile per policy) | `ntfs-3g` |
| `ext2` / `ext3` / `ext4` | `noatime` | `dumpe2fs -h <dev>` needs_recovery flag OR feature-flag inspection | `e2fsck -p <dev>` (auto-repair; escalate to `-y` on operator confirm) | `e2fsprogs` |

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

**Stable-id derivation** (first match wins):

1. Filesystem UUID via `/dev/disk/by-uuid/<uuid>` symlink →
   stable-id = `<uuid>` (lowercase, no braces).
2. Filesystem label (sanitised: `^[A-Za-z0-9_-]{1,32}$`; other
   chars → `_`) → stable-id = `label-<sanitised-label>`.
3. Synthesized fallback → stable-id =
   `unlabelled-<disk-serial>-<partition-index>`.

Rules:

- Stable-id MUST be stable across replug of the SAME volume.
- Stable-id MUST NOT collide across concurrent mounts. On
  collision (two volumes with the same label, no UUID), the
  fallback synthesizer disambiguates via `<disk-serial>`.
- The mount-point directory `create_dir_all` at mount time;
  `rm_dir` (empty-only) at unmount time. Empty check prevents
  destroying operator files if unmount races a mid-write.

---

## 4 Verb inventory (`storage.usb.v1` shelf)

| Verb | Payload | Response | Auth | Failure classes |
|---|---|---|---|---|
| `storage.usb.list_drives` | `{}` | `{ drives: [DriveRecord] }` — see §5 | none | never fails |
| `storage.usb.mount` | `{ stable_id }` | `{ mounted_at, class }` | operator | `system_disk_refused`, `unsupported_fs`, `mount_failed_dirty`, `subprocess_io` |
| `storage.usb.safe_remove` | `{ stable_id, force?: bool }` | `{ removed: true }` | operator | `system_disk_refused`, `busy`, `subprocess_io` |
| `storage.usb.repair_filesystem` | `{ stable_id, escalate?: bool }` | `{ repaired: true, before_class, after_class }` | operator + step-up | `system_disk_refused`, `unsupported_fs`, `repair_failed`, `subprocess_io` |

Read-only subject: `storage_usb_drives`, singleton addressing
scheme `evo.storage.usb.drives:local` — carries the same
`DriveRecord[]` payload as `list_drives`. Republished on every
hotplug attach, hotplug detach, mount, umount, repair-complete.
UI subscribes at Sources page mount.

---

## 5 `DriveRecord` shape (subject + list_drives response)

```
DriveRecord {
    stable_id:            string           // §3 derivation
    device_node:          string           // e.g. "/dev/sda1"
    parent_disk:          string           // e.g. "/dev/sda"
    label:                Option<string>   // fs label if present
    uuid:                 Option<string>   // fs uuid if present
    fs_type:              string           // "vfat" | "exfat" | "ntfs" | "ext4" | "unsupported"
    size_bytes:           u64
    class:                DriveClass       // enum below
    mount_root:           Option<string>   // "/var/lib/evo/music/USB/<stable_id>" when mounted
    library_source_id:   Option<string>   // library.add_source result when class=mounted-*
    last_transition_at:   i64              // wall-clock ms of last state change
}

DriveClass =
  | "system-disk"                          // §1 hard-refuse
  | "unsupported"                          // fs_type not in §2 matrix
  | "unmounted"                            // detected, not yet mounted
  | "mounted-clean"                        // mounted, no dirty flag
  | "mounted-dirty"                        // mounted, dirty flag on
  | "mounted-dirty-hiberfile"              // NTFS hiberfile present
  | "mount-failed-dirty"                   // mount refused due to dirty state
  | "mount-failed-other"                   // mount errno other than dirty
```

`class` transitions on the subject drive the UI state (Safe
remove offered when `mounted-*`; Repair offered when
`mounted-dirty` or `mount-failed-dirty`; nothing actionable
when `system-disk` / `unsupported`).

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
2. System-disk check per §1 — if hit, publish subject with
   `class: system-disk` and stop (no mount).
3. FS-type check per §2 — if unsupported, publish with
   `class: unsupported` and stop.
4. Mount attempt via wrapper. On success, republish subject
   with `class: mounted-clean` / `mounted-dirty`. On failure,
   `mount-failed-dirty` / `mount-failed-other`.
5. When `mounted-*`: cross-plugin dispatch to
   `library.add_source` with `local_usb` record shape (per
   `library.v1.toml:92`). Record the returned
   `library_source_id` on the DriveRecord.

**Coldplug (at plugin load):** enumerate every `SUBSYSTEM=block
TRAN=usb` device via `lsblk`, run the same pipeline steps 1-5.
Mount-truth reconcile per `/proc/self/mountinfo`: if a volume
is already mounted at `/var/lib/evo/music/USB/<stable-id>/`
(operator-mounted before plugin load, or leftover from a
previous plugin instance), adopt without remounting — same
adopt discipline as `network.shares::adopt_existing_os_mount`.

**Detach:** on `remove` udev event, retract `library.remove_source`,
best-effort `umount`, republish subject with drive removed
from the list.

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
| Row display | Label / stable-id + FS type + size + `class`-driven affordances |
| Row affordances by class | `mounted-clean`: Safe remove. `mounted-dirty`: Safe remove + **Repair**. `mounted-dirty-hiberfile`: Safe remove + copy string ("resume + shut down cleanly before repair") + no repair. `mount-failed-dirty`: **Repair**. `unsupported`: greyed row + copy string ("FS type <x> not supported"). `system-disk`: hidden by default; visible under a "System storage (read-only)" collapsed section. |
| Repair confirm | Modal: "This will unmount and check <label>. Files must be unopened; playback stops. Continue?" |
| Force remove | Only offered on `Busy` response; second modal: "Files are still open. Force eject may cause data loss. Continue?" |
| `remount_usb` recovery hint | Wired to `storage.usb.mount` retry against the drive's stable-id. Consumed by disposition renderer per `playback.v1.toml:602`. |

---

## 11 Boundary (do not conflate)

| Plugin | Role |
|---|---|
| `org.evoframework.storage.usb` | **Owner** — block hotplug, classify, mount, umount, fsck, eject; cross-plugin dispatch to library. |
| `org.evoframework.playback.mpd` | **Consumer** — `SourceKind::LocalUsb` on-mount-event trigger; MPD update on library-mutation; disposition-stop before mount-mutation. |
| `org.evoframework.network.smb-server` | **Consumer** — Samba parent `[USB]` share exports the tree. Storage.usb NEVER writes `smb.conf`. |
| `evo-core-eng` (framework) | **Nothing** — no storage / block / mount knowledge. Repeats R-028 anti-precedent avoidance. |

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

**Rig acceptance (P0.H4):** Pi 5 + NUC + VM triangle. Each
target: plug FAT stick → row appears → browse → play → dirty
FAT stick → Repair → clean row → Safe remove → row removed
→ Samba parent share shows the tree via `smbclient` during
mounted window. Evidence emitted at
`/var/lib/evo/evidence/storage-usb-h4-<triple>.toml`.

---

## 13 Change discipline

1. Edit this file first.
2. Update the plugin's classifier / mounter / verbs / subject
   in the SAME change set.
3. Update the sudoers template, wrapper, and bootstrap install
   stanza in the SAME change set.
4. Update the catalogue schema `storage.usb.v1` in
   `evo-catalogue-schemas` in the SAME change set (or the
   plugin fails admission).
5. Update the UI Sources / Library USB rows in `evo-ui-eng` as
   a coordinated follow-up (UI team reads this file for the
   subject + verb contract; not a plugin-side change).
6. If any invariant here changes (system-disk union expands;
   FS support matrix drops a type; mount root moves off
   `/var/lib/evo/music/USB/`), amend the companion decision
   record in evo-internal first, then this file, then code.

Reviewers refuse storage.usb code changes whose diffs do not
touch this file OR whose diffs contradict what this file says.
