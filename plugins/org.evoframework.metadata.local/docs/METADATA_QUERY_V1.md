# `metadata.query` — response schema (v1)

**Plugin:** `org.evoframework.metadata.local`  
**Shelf:** `metadata.providers`  
**Encoding:** JSON, UTF-8, `Content-Type: application/json` (as chosen by the steward transport).

This document is the **authoritative field catalogue** for the success and error payload. First-class fields are read from audio tags via [lofty](https://github.com/Serial-ATA/lofty-rs) and from the file’s `FileProperties`. Omitted keys mean “absent / unknown” (serde `skip_serializing_if`).

---

## Metadata profiles (operator config)

The **request** JSON does not select a profile. The operator sets a single global mode in the plugin config file (same path pattern as other plugins, e.g. `/etc/evo/plugins.d/org.evoframework.metadata.local.toml`).

```toml
[metadata]
# Optional. Default: "standard" (case-insensitive).
profile = "standard"
# profile = "extended"
```

| Profile   | Purpose | Success response |
|-----------|---------|------------------|
| `standard` | Default. Small payloads for typical UIs: core flat tag fields and top-level `duration_ms`, without large or specialist blocks. | Omits nested groups, `lyrics`, technical `file`, and unmapped `extras`. |
| `extended` | Full readout for cataloguing, classical/jazz credits, MusicBrainz IDs, replay gain, container bitrates, and vendor `TXXX` / unknown frames. | All fields the file supports, subject to size caps (e.g. lyrics). |

### What each profile includes (`status: "ok"`)

| Area | `standard` | `extended` |
|------|------------|------------|
| Top-level `v`, `status`, `detail` | yes | yes |
| `active_profile` | yes (`"standard"`) | yes (`"extended"`) |
| Flat tag fields: `title`, `artist`, `album`, `album_artist`, `artists`, `genre`, `track` / `track_total`, `disc` / `disc_total`, `year`, `duration_ms` | yes | yes |
| `subtitle`, `language`, `script`, `comment`, `mood`, `initial_key`, `bpm` | yes | yes |
| `compilation`, `podcast` | yes | yes |
| `lyrics` | **no** | yes (capped) |
| `sort` | **no** | yes |
| `credits` | **no** | yes |
| `classical` | **no** | yes |
| `original` | **no** | yes |
| `dates` | **no** | yes |
| `identifiers` (ISRC, MusicBrainz, …) | **no** | yes |
| `replay_gain` | **no** | yes |
| `file` (sample rate, bit depth, bitrates, …) | **no** | yes |
| `extras` (unmapped / vendor frames) | **no** | yes |

### How to switch profile (for a UI or operator tool)

1. **Read** the effective mode from successful `metadata.query` responses: field `active_profile` is `"standard"` or `"extended"`. Error responses do not include it.
2. **Change** the mode by editing the plugin TOML: set `[metadata] profile` to the desired value and save the file.
3. **Apply** the new config: reload the plugin or restart the component that calls `Plugin::load` with `LoadContext::config` (your distribution documents the exact action; often a service restart or plugin hot-reload if supported).
4. **Re-fetch** metadata; new responses use the updated profile. The UI can refresh when it detects a config change or on next query.

There is no per-request override in v1; a future request version could add one.

---

## Request (v1)

| Field   | Type   | Required | Description |
|---------|--------|----------|-------------|
| `v`     | number | yes      | Must be `1`. |
| `target`| object | yes      | Subject selector (same shape as `artwork.resolve`). |
| `target.scheme` | string | yes | `mpd-path` (MPD `file` relative to `[library] roots` or absolute) or `mpd-album` (see below). |
| `target.value`  | string | yes | MPD `file` path, or for `mpd-album` the compound `artist|album` (only the first pipe splits; the album part may contain further pipes). |

**Example**

```json
{ "v": 1, "target": { "scheme": "mpd-path", "value": "Classical/Bach/01.flac" } }
```

**`mpd-album`**

- **Value** matches `org.evoframework.playback.mpd` album subject addressing: `"{artist}|{album}"`, with `unknown` for missing/empty `Artist` (per the playback warden).
- **Resolution:** depth-first search under each `[library] root` in config order, directory entries sorted by name; the **first** local audio file whose **primary** tag `artist` and `album` match the request (after trim; missing artist in the file is treated as `unknown`) is used. The scan is bounded (at most 100,000 file reads per request); over the limit returns `not_found` with a `detail` string.
- **Not MPD:** this path does not query MPD; it is suitable for offline/standalone libraries.

---

## Top-level response (all outcomes)

| Field     | Type   | Description |
|-----------|--------|-------------|
| `v`       | number | Always `1`. |
| `status`  | string | `ok` · `not_found` · `unsupported` · `bad_request` |
| `detail`  | string? | Human-readable reason (errors; optional on `ok`). |
| `active_profile` | string? | On **`ok` only:** `"standard"` or `"extended"` — mirrors operator `[metadata] profile` after filtering. |

On non-`ok` outcomes, only `v`, `status`, and usually `detail` are set; all other fields are absent.

---

## `status: "ok"` — flat fields (convenience for UIs)

| Field            | Type        | Source / notes |
|------------------|-------------|----------------|
| `title`          | string?     | Track title. |
| `artist`         | string?     | Primary track artist (first line). |
| `artists`        | string[]?   | All `ARTIST` / `ARTISTS` style lines. |
| `album`          | string?     | Album title. |
| `album_artist`   | string?     | e.g. TPE2, `ALBUMARTIST`. |
| `genre`          | string?     | |
| `track`          | number?     | 1-based track index (u32). |
| `track_total`    | number?     | |
| `disc`           | number?     | 1-based disc (u32). |
| `disc_total`     | number?     | |
| `year`           | number?     | Four-digit year when parseable. |
| `duration_ms`    | number?     | From **container** (not a tag). |
| `subtitle`       | string?     | e.g. `TIT3` / `TrackSubtitle`. |
| `language`       | string?     | |
| `script`         | string?     | |
| `comment`        | string?     | First `Comment` line (ID3 `COMM` / Vorbis `COMMENT` style). |
| `mood`           | string?     | |
| `initial_key`    | string?     | Musical key. |
| `bpm`            | string?     | As in file (integer or decimal string). |
| `lyrics`         | string?     | Unsynchronised lyrics; **capped** at 512_000 bytes (UTF-8). **`extended` profile only** (omitted in `standard`). |
| `compilation`    | bool?       | When tag encodes a compilation flag. |
| `podcast`        | bool?       | When present in tags. |

---

## `status: "ok"` — nested objects

**Present only when `active_profile` is `"extended"`** (omitted entirely under `standard`).

Omitted if every field inside would be null.

### `sort`

Picard-style sort keys (TSO* / Vorbis `…sort`).

| Field            | Type    |
|------------------|---------|
| `track_title`    | string? |
| `album`          | string? |
| `track_artist`   | string? |
| `album_artist`   | string? |
| `composer`       | string? |

### `credits`

Classical / jazz / soundtrack credits.

| Field            | Type        |
|------------------|-------------|
| `composer`       | string?     |
| `conductor`      | string?     |
| `lyricist`       | string?     |
| `arranger`       | string?     |
| `writer`         | string?     |
| `performer`      | string?     | First ensemble / soloist line. |
| `performers`     | string[]?   | All `PERFORMER` (or equivalent) lines. |
| `producer`       | string?     |
| `mix_engineer`   | string?     |
| `engineer`       | string?     |
| `label`          | string?     |
| `publisher`      | string?     |
| `remixer`        | string?     | Often ID3 TPE4. |
| `director`       | string?     |

### `classical`

Large works, opera, multi-movement pieces.

| Field               | Type    |
|---------------------|---------|
| `work`              | string? |
| `movement`          | string? | Movement *name* (e.g. “Allegro”). |
| `movement_number`   | number? | u32 |
| `movement_total`    | number? | u32 |
| `show_name`         | string? |
| `content_group`     | string? |

### `original`

Reissue / remaster “original” metadata when tagged.

| Field            | Type    |
|------------------|---------|
| `album`          | string? |
| `artist`         | string? |
| `lyricist`       | string? |
| `release_date`   | string? |

### `dates`

Full date **strings** as in tags (often `YYYY-MM-DD` from MusicBrainz / Picard).

| Field                 | Type    |
|-----------------------|---------|
| `recording`           | string? |
| `release`             | string? |
| `original_release`    | string? |

### `identifiers`

| Field                        | Type    | Notes |
|------------------------------|---------|--------|
| `isrc`                       | string? | |
| `catalog_number`            | string? | |
| `barcode`                    | string? | |
| `musicbrainz_recording_id`  | string? | UUID text |
| `musicbrainz_track_id`      | string? | |
| `musicbrainz_release_id`   | string? | |
| `musicbrainz_release_group_id` | string? | |
| `musicbrainz_artist_id`     | string? | |
| `musicbrainz_release_artist_id` | string? | |
| `musicbrainz_work_id`       | string? | |

### `replay_gain`

Raw strings as stored in tags (e.g. `-7.24 dB`).

| Field         | Type    |
|---------------|---------|
| `track_gain`  | string? |
| `track_peak`  | string? |
| `album_gain`  | string? |
| `album_peak`  | string? |

### `file`

From **lofty** `FileProperties` (measured, not from tags).

| Field                 | Type    | Description |
|-----------------------|---------|-------------|
| `duration_ms`         | number? | Redundant with top-level `duration_ms`; same source. |
| `sample_rate_hz`      | number? | |
| `bit_depth`           | number? | u8 |
| `channels`            | number? | u8 |
| `channel_mask_bits`   | number? | `lofty` channel mask `bits()`. |
| `overall_bitrate_kbps` | number? | Container bitrate (kbit/s). |
| `audio_bitrate_kbps`  | number? | Audio stream (kbit/s). |

### `extras`

`object` (string → string), sorted by key in JSON (BTreeMap): **vendor and unmapped** tag entries whose [`ItemKey`](https://docs.rs/lofty) is `Unknown("…")` (e.g. custom ID3 `TXXX` keys). Text is truncated with the same cap as `lyrics`. **Binary** payloads appear as the literal `"<binary: N bytes>"`. If the same name appears multiple times, keys are `NAME`, `NAME@1`, `NAME@2`, …

Known first-class `ItemKey` values are **not** duplicated in `extras`; they appear only in the structured fields above.

---

## Example — success (abbreviated)

```json
{
  "v": 1,
  "status": "ok",
  "active_profile": "extended",
  "title": "Gigue",
  "artist": "Arthur Grumiaux",
  "album_artist": "Grumiaux, Arthur",
  "album": "Bach: Sonatas and Partitas",
  "year": 1997,
  "duration_ms": 210123,
  "credits": {
    "composer": "Johann Sebastian Bach",
    "conductor": "—",
    "performer": "Chamber Orchestra",
    "label": "Philips"
  },
  "classical": {
    "work": "BWV 1001–1006",
    "movement": "Gigue"
  },
  "dates": {
    "release": "1997-10-20"
  },
  "identifiers": {
    "musicbrainz_work_id": "4e6c5e8d-…"
  },
  "file": {
    "sample_rate_hz": 44100,
    "bit_depth": 16,
    "channels": 2
  }
}
```

---

## Example — error

```json
{
  "v": 1,
  "status": "not_found",
  "detail": "audio file not found for mpd_path"
}
```

---

## Configuration

Optional TOML under the plugin config path, e.g. `plugins.d/org.evoframework.metadata.local.toml` (see manifest `prerequisites`):

```toml
[library]
roots = ["/data/media/Music", "/media/usb0/Music"]
```

Paths must be **absolute**; they resolve relative `mpd-path` `target.value` the same way as `org.evoframework.artwork.local` and the MPD warden’s `file` field.

---

## Versioning

This document tracks **`metadata.query` v1** (`"v": 1` in the request). A future `v: 2` request may add `fields` / `include_extras` style toggles; until then, consumers should ignore unknown JSON keys on the response (forward compatibility).
