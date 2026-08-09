# Spectrum demand — evo-device-audio (audio.terminus)

**Normative** contract for the spectrum-producer control
plane. Module code (`demand.rs`), capture-loop gate
(`capture.rs`), subject envelope shape, verb payload, and
the runtime-bridge apply path MUST match this file. Update
this document and the code constants in the same change.

Bound by a companion design record in the internal decision
tree, which cites this file by repo path and does not
duplicate the tables below.

Lineage (do not re-derive from chat):

- Original spectrum wire contract set the canonical maximum
  wire shape (256×stereo) as immutable and delegated
  operator-facing bin/channel choice to renderer-only
  downsample. That contract is superseded by the
  demand-driven-producer contract this document pins.
- Audit source: `.24` on 2026-08-09 — `ui.visualizer.
  enabled=false` unsubscribed the browser but the terminus
  producer continued to FFT + emit at 256×stereo.
- Cross-source pattern: thin-ADR + living-inventory pattern
  already proven by `SAMBA-SHARES.md`, `USB-STORAGE.md`,
  `LIBRARY-TRIAGE.md`.

This document is the audio-distribution inventory for the
spectrum-demand control plane. Framework primitives (subject
mirror, admission, dispatch) are named where relevant but
not restated here.

---

## 1. Demand subject — `audio_playback_spectrum_demand`

The single production-truth surface for the terminus
producer. Announced at plugin load with the disabled-default
envelope; republished on every `audio.spectrum.set_demand`
verb dispatch.

| Field | Type | Meaning |
|-------|------|---------|
| `v` | `u32` | Envelope version. `1` today. |
| `enabled` | `bool` | Producer gate. `false` → PCM released + no FFT + no emit. `true` → capture opens (subject to transport + role gates). |
| `bins` | `u32` | Mel-bin count. Enum: `32 \| 64 \| 128 \| 256`. Refused with Permanent outside the enum. |
| `channels` | `u32` | Channel mode. Enum: `1 \| 2` (`1` = mono; `2` = stereo). Refused with Permanent outside the enum. |
| `rate_hz_target` | `u32` | Emit throttle target in Hz. Clamped to `[1, 60]` at the plugin; typical value `30`. |
| `updated_at_ms` | `u64` | Wall-clock ms of last apply. Zero before the first apply. |

**Addressing:** `evo.audio.playback:spectrum_demand`.
**Cardinality:** singleton per device.

**Disabled default (plugin load):** `{ enabled: false, bins: 64, channels: 1, rate_hz_target: 30, updated_at_ms: 0 }` — matches the pre-supersession baseline (no spectrum activity on a device where the operator has never touched `ui.visualizer.*`).

---

## 2. Write verb — `audio.spectrum.set_demand`

The framework-side wire op that mutates the demand subject.
Dispatched via the plugin's respondent surface on the
`audio.terminus` shelf.

**Payload** (v1):

```json
{
  "v":              1,
  "enabled":        true,
  "bins":           64,
  "channels":       1,
  "rate_hz_target": 30
}
```

**Response** (v1):

```json
{
  "v":       1,
  "applied": {
    "v":              1,
    "enabled":        true,
    "bins":           64,
    "channels":       1,
    "rate_hz_target": 30,
    "updated_at_ms":  1720000000000
  }
}
```

**`deny_unknown_fields`** on the parsed struct catches typos synchronously — a client passing `preset` or `palette` receives `invalid_payload` synchronously rather than silently discarding the unknown parameter.

---

## 3. Apply path (F1-A runtime bridge)

The operator changes `ui.visualizer.{enabled, bin_count, channel_mode}` through any settings surface (Settings screen, Designer Visualizer Studio, future clients). The settings-patch write reaches the runtime's settings store at `/opt/evo/ui/data/settings.json`. `evo-ui-runtime`'s patch bridge derives the demand-payload from the changed keys and calls `audio.spectrum.set_demand` in the same request cycle. The runtime bridge is the single write path — every settings origin inherits the demand-write automatically.

**Mapping:**

| Settings key | Demand field | Transformation |
|---|---|---|
| `ui.visualizer.enabled` (bool) | `enabled` | direct copy (with `preset=off` → `enabled=false` collapse — see below) |
| `ui.visualizer.bin_count` (`32` / `64` / `128` / `256`) | `bins` | direct copy |
| `ui.visualizer.channel_mode` (`mono` / `stereo`) | `channels` | `mono` → `1`; `stereo` → `2` |
| — | `rate_hz_target` | fixed to `30` at the runtime bridge (or read from an optional operator setting when one exists) |

**`preset=off` folds to `enabled=false`.** If the operator sets `ui.visualizer.preset = "off"`, the UI-side settings write ALSO patches `enabled = false` on the same write. The runtime bridge only sees the resulting `enabled` value; `preset` is a renderer-only concept that never touches the demand subject.

---

## 4. Producer gate (F2A — landed this cycle)

The terminus plugin's `CaptureGate` opens when ALL of the following are true:

1. `TransportGate::should_emit()` — the current transport state is `Playing` (from the `audio_playback_now_playing` subject subscription).
2. `demand.enabled == true` — the operator has enabled the visualiser via settings (mirrored through the runtime bridge into the demand subject).
3. `LocalRole::should_emit()` — the local device is `Source` or `Auto` (not `Receiver` — followers of an active multi-room group do not emit a parallel spectrum subject).
4. (F3 follow-up) `interest > 0` — at least one subscription permits `audio_playback_spectrum_frame`. Optional secondary gate; NOT required by F2A; the hard park on `enabled=false` MUST NOT depend on interest accounting.

When the gate closes for ANY reason (transport off, operator disable, role becomes Receiver, or future interest=0), the capture task:

- Exits its inner FFT loop.
- Drops the ALSA PCM handle (verifiable rig-side via `lsof -p <pid>` — the loopback capture device disappears from the FD list).
- Sleeps on the transport + demand watch channels (whichever transitions first wakes the outer loop).

When the gate reopens, the outer loop opens a fresh PCM and re-enters the FFT loop. First-frame latency is the ALSA open + first-hop-worth of samples — sub-100 ms on the reference chain.

**Rig acceptance for this landing:**

- Playing + `enabled=true`: baseline CPU + PCM FD present.
- Playing + toggle to `enabled=false`: CPU drops, spectrum subject stops updating, PCM FD disappears within one poll cycle (`lsof -p <pid>` shows the loopback device removed).
- Playing + toggle back to `enabled=true`: CPU returns, spectrum subject resumes, PCM FD reappears.

The metric that matters is the CPU + FD change, not just the subject silence. A silent subject with the FFT still spinning is exactly the class this landing exists to close.

---

## 5. Sequenced follow-up landings

| Wave | Scope |
|------|-------|
| F2B | Variable analyser: `SpectrumAnalyser::new(sample_rate_hz, bins, channels)`; `PerceptualFrame` uses `Vec<f32>` sized to `bins × channels`; rebuild on demand-change mid-play; frame payload's `bins`/`channels` fields report actual analyser state (payload-truth invariant — the frame is the shape authority, not the demand). |
| F2C | 30 Hz emit throttle decoupled from ALSA hop rate. Inner compute loop stays hop-rate for ring-buffer discipline; emit path governed by wall-clock throttle at `demand.rate_hz_target`. |
| F3 | Volatile emission (no `subject_states` durable mirror — spectrum frames are high-rate telemetry, not operator-visible mutations worth persisting) + optional subscription-interest signal exposed from projection-ws to terminus. |
| F4 | Rig A/B evidence on Pi 5 across enable/disable cycle. |

F2B, F2C, F3, F4 land as sequenced commits after this one. The wire shape stays 256×stereo across F2A (this landing) and only mutates at F2B; UI-side consumer (U2) lands in lockstep with F2B against the mutated wire.

---

## References

- `src/demand.rs` — demand module (subject + verb + watch broadcast).
- `src/capture.rs` — capture loop (outer CaptureGate + inner emit + PCM lifecycle).
- `src/lib.rs` — verb dispatcher + demand-store lifecycle.
- `manifest.toml` / `manifest.oop.toml` — request_types include `audio.spectrum.set_demand`.
- `dist/catalogue/audio-rack.toml` — `audio_playback_spectrum_demand` subject declaration.
- Pattern siblings: `plugins/org.evoframework.network.smb-server/docs/SAMBA-SHARES.md`, `plugins/org.evoframework.storage.usb/docs/USB-STORAGE.md`, `plugins/org.evoframework.playback.mpd/docs/LIBRARY-TRIAGE.md`.
