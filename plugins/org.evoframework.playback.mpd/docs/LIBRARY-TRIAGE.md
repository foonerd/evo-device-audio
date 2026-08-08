# Library triage — evo-device-audio (playback.mpd)

**Normative** contract for the library-plane drift-detection
substrate this plugin publishes. Module code
(`library_triage.rs`), classifier code (`gone_curation.rs`),
subject envelope shape, verb payloads, and the wipe-parity hook
in the installer MUST match this file. Update this document and
the code constants in the same change.

Bound by a companion design record in the internal decision
tree, which cites this file by repo path and does not duplicate
the tables below.

Lineage (do not re-derive from chat):

- 2026-08-07 fleet-wipe incident that surfaced the
  invisible-reconcile defect class.
- 2026-08-08 audits: library gone-curation parity gap;
  library-mountpoints continuous-deploy audit.
- Cross-source pattern: thin-ADR + living-inventory pattern
  already proven by `SAMBA-SHARES.md` and `USB-STORAGE.md`.

This document is the audio-distribution inventory for the
library-triage substrate. Framework primitives (subject
mirror, admission, wipe primitive shape) are named where
relevant but not restated here.

---

## 1. Dispositions — Offline vs Gone

The plugin recognises two mutually exclusive drift classes on
any single URI in the queue, favourites, or a stored playlist.
Overloading one for the other is a curation lie in either
direction; reviewers reject changes that conflate them.

| Class | Meaning | Byte-plane truth | Source state | Reconcile action |
|-------|---------|------------------|--------------|------------------|
| **Offline (retain)** | Temporary source unreachable. NAS unmounted, DLNA server down, network share not mounted yet. | Bytes may still exist upstream; the plugin cannot confirm. | `Offline` / `Retired` / `Probing` / any state other than `Online` or `Degraded`. | **KEEP** the URI. Project `available: false`. Skip at play. Do not prune from queue / favourites / stored playlists. |
| **Gone (prune)** | Permanent library loss. File deleted from disk, mass wipe, intentional `mpc update` removed the song. | Byte proven absent — URI is not in MPD's `listallinfo`. | Owning source resolves to `Online` or `Degraded` (the "reachable" set defined by `gone_curation::source_is_reachable`). | **REMOVE** the URI from queue, favourites, and every stored playlist. Refresh the affected shelf subjects. |

**Reachability matrix (normative).** Only these source states
authorise a Gone prune:

- `SourceState::Online` — source reachable and returning results.
- `SourceState::Degraded { .. }` — source reachable but slow; still authoritative on what it does and does not hold.

Every other source state (`Offline`, `Retired`, `Probing`,
unresolved-source-lookup) preserves the URI on the retain
side of the ledger. The classifier
(`gone_curation::classify_local_uri_gone`) checks
reachability BEFORE consulting `listallinfo` membership;
sequence matters.

**Remote-scheme opt-out (normative).** Any URI whose scheme is
`http://` / `https://` / `dlna:` / `DLNA:` / anything carrying
`://` other than a local MPD relative path is outside the
Gone-prune surface entirely. Remote streams and DLNA identity
URIs never prune — DLNA offline is closer to the Offline-retain
model above and is handled by the DLNA plugin's own probe
cadence.

---

## 2. Drift class inventory

Every classifier arm in `library_triage.rs` has one row here.
Adding a class without adding a row is a defect. Retiring a
class without deleting a row is a defect.

| Class (stable key) | What it means | Detection | Reconcile action | Auto-fires? |
|--------------------|---------------|-----------|------------------|-------------|
| `mpd_db_empty_but_music_present` | The music tree on disk has content but MPD's database reports zero songs. Typical trigger: post-wipe cold boot where the file plane was restored (NAS remounted, USB replugged) but no `mpc update` has run yet. | `music_directory` has ≥1 non-empty child directory under the triad AND `listallinfo("")` returns zero `File` entries. | `conn.update(None)` — fires a full-database rescan. | yes |
| `registry_count_diverges` | The source registry's persisted `track_count` for the floor source disagrees with MPD's `stats.songs` wire truth. Typical trigger: p2 wipe leaves stale thousands in `sources.toml` because a prior rehydrate did not persist the wire-truth apply. | Floor source (`local-internal`) `SourceRecord.track_count` ≠ `mpc stats` `songs`. | `apply_track_counts_with_scan_time` + `registry.persist()` + `library::publish_subjects` to refresh the `audio_library_state` envelope in the same sweep. | yes |
| `curation_carries_gone_uris` | The queue, favourites, or a stored playlist references URIs that no longer exist in MPD's database under a reachable source. Typical trigger: a file was deleted, a source was wiped, `library.remove_source` was fired. | ≥1 URI in `playlistinfo`, `listplaylistinfo(__favourites__)`, or any user playlist classifies as Gone under §1's rules. | `gone_curation::prune_gone_from_curation` — deletes highest-position first (MPD renumbers on delete), refreshes queue + favourites + playlist-index subjects. | yes |

**Extension rule.** A new class (per-source count divergence,
USB-plane stale mount, DLNA identity staleness, network-share
mount loss, sticker-store drift) lands as one row above, one
classifier arm in `library_triage.rs`, and — if the reconcile
action is not safe to auto-fire — an entry in the manual-verb
action table below with `auto` set to `no`. No wave-1 class
falls on the prompt-required side.

---

## 3. Wire shape

### Subject: `audio_library_triage`

Announced at plugin load with an empty envelope; republished at
the end of every triage sweep (both warm-start and idle
`Database`/`Update` bursts).

Envelope (payload `v = 1`):

```json
{
  "v": 1,
  "findings": [
    {
      "class": "curation_carries_gone_uris",
      "severity": "warn",
      "description": "3 track reference(s) in the queue, favourites, or stored playlists no longer exist in the library. Pruned from curation.",
      "auto_reconciled": true,
      "reconciled_at_ms": 1720000000000,
      "evidence": {
        "queue_removed":            2,
        "favourites_removed":       1,
        "playlist_entries_removed": 0,
        "playlists_touched":        0
      }
    }
  ],
  "auto_reconciled_count":  1,
  "prompt_required_count":  0,
  "last_run_at_ms":         1720000000000
}
```

**Field discipline:**

- `v` — envelope version. Any shape change bumps this and
  edits this row in the same change.
- `findings[].class` — stable snake-case, one of the keys in
  §2. UI keys on this for localised copy.
- `findings[].severity` — `info` | `warn` | `critical`. Wave-1
  classes are all `warn`.
- `findings[].description` — one-line operator-visible
  summary. Contains counts / paths / source ids as
  appropriate; never leaks internal ADR / risk / decision ids.
- `findings[].auto_reconciled` — true when the sweep fired the
  reconcile action successfully; false when the class was
  detected but reconcile failed or is prompt-required.
- `findings[].reconciled_at_ms` — wall-clock ms of the
  reconcile completion. `null` when `auto_reconciled` is false.
- `findings[].evidence` — class-specific structured blob for
  diagnostics + audit trail. UIs surface it as an expandable
  "Details" section on the row.
- `auto_reconciled_count` / `prompt_required_count` — sum
  across `findings[]` for the operator-visible counters at the
  top of the panel.
- `last_run_at_ms` — wall-clock ms of the last sweep
  completion. Zero when no sweep has run yet.

### Verbs

| Verb | Request payload | Response | Side effect |
|------|-----------------|----------|-------------|
| `library.get_triage` | `{}` | Same envelope shape as the subject. | None (read-only snapshot of the last sweep). |
| `library.reconcile_triage` | `{}` | Same envelope shape as the subject (post-sweep). | Runs the full sweep, fires every reconcile action, republishes the subject. |

### When the sweep fires

- **Plugin warm-start**, after `library::rehydrate_from_mpd`
  in `ShelfBundle::rehydrate_all`. Boot into a wiped device
  publishes findings for whatever the wipe left drifted.
- **Every idle `Database` / `Update` burst**, after
  `library::rehydrate_from_mpd` in the idle observer's
  `Database` / `Update` arm. Captures drift from MPD-side
  scans and from operator-driven `library.update_source` calls.
- **Operator gesture** via `library.reconcile_triage`.

All three call paths route through `library_triage::run_triage`;
a process-wide gate serialises concurrent passes so two idle
bursts cannot interleave.

---

## 4. Discipline

### Wipe parity

A destructive wipe of the music plane MUST leave MPD-side
survivor state cleared in the same operation. The installer
function that guarantees this is `reset_mpd_curation_after_music_wipe`
in `dist/scripts/evo-install.sh`, called unconditionally from
`wipe_full`. It clears:

- `/var/lib/mpd/tag_cache`
- `/var/lib/mpd/state`
- `/var/lib/mpd/sticker.sql`, `sticker.sql-journal`,
  `sticker.sql-wal`, `sticker.sql-shm`
- Every entry under `/var/lib/mpd/playlists/` (including
  `__favourites__`)

Adding a new plugin-owned state file under `/var/lib/mpd`
(a new curation store, a new sticker namespace) MUST extend
that function in the same change.

### Registry persist

Every code site that learns wire truth via
`apply_track_counts_with_scan_time` MUST follow the apply with
`registry.persist()` in the same function. The current sites
are `library::rehydrate_from_mpd` and the
`registry_count_diverges` arm of `library_triage::run_triage`;
new sites MUST match.

### Never bypass the triage

Direct calls to `gone_curation::prune_gone_from_curation`
outside `library_triage::run_triage` are correct but silence
the operator-visible finding. Wave-1 has no such call site;
future callers MUST justify the bypass in the module docstring
of the calling code.

### Test scope

Deterministic layer (classifier arms, gate singletons,
envelope shape) is covered by unit tests in
`library_triage.rs` and `gone_curation.rs`. The end-to-end
sweep runs on every plugin warm-start against the live MPD
instance and republishes the `audio_library_triage` subject;
`library.get_triage` exposes the resulting envelope for
external verification. A shelf-level mock-MPD variant that
scripts the triage query set (`stats`, `listallinfo`,
`playlistinfo`, `listplaylistinfo`, `listplaylists`, `find`,
`deleteid`, `playlistdelete`, `update`) extends the existing
`playback_supervisor/test_mock.rs` `ConnBehaviour` set in the
same shape as the variants already there; the extension is a
self-contained addition to that module.

---

## References

- `src/library_triage.rs` — triage module (classifier arms +
  subject + verbs).
- `src/gone_curation.rs` — Offline-vs-Gone classifier +
  prune primitives.
- `src/library.rs` → `rehydrate_from_mpd` — wire-truth apply
  + persist site.
- `src/shelves.rs` → `ShelfBundle::rehydrate_all` — warm-start
  triage entry point.
- `src/idle_observer.rs` → `dispatch_refresh` `Database`/`Update`
  arm — reactive triage entry point.
- `dist/scripts/evo-install.sh` → `reset_mpd_curation_after_music_wipe`
  — wipe-parity function.
- Pattern siblings: `plugins/org.evoframework.network.smb-server/docs/SAMBA-SHARES.md`
  and `plugins/org.evoframework.storage.usb/docs/USB-STORAGE.md`.
