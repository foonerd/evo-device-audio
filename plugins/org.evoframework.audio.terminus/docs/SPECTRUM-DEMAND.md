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

## 4. Producer gate

The terminus plugin's `CaptureGate` opens when ALL FOUR of the following are true:

1. `TransportGate::should_emit()` — the current transport state is `Playing` (from the `audio_playback_now_playing` subject subscription).
2. `demand.enabled == true` — the operator has PERMITTED the visualiser via settings (mirrored through the runtime bridge into the demand subject). Necessary condition; no longer sufficient — see §4b on the demand-as-permission semantic.
3. `LocalRole::should_emit()` — the local device is `Source` or `Auto` (not `Receiver` — followers of an active multi-room group do not emit a parallel spectrum subject).
4. `interest > 0` — at least one WS subscriber is live on the framework's projection surface for `audio_playback_spectrum_frame`. The produce-iff-consumed signal: no consumer means no compute.

When the gate closes for ANY reason (transport off, operator disable, role becomes Receiver, interest → 0), the capture task:

- **Emits one final `audio_playback_spectrum_frame` envelope** at the current demand's `bins`/`channels` shape with `at_ms = 0` (the "parked" sentinel — matches the empty-envelope semantics used for a fresh-connect on a quiet transport). Subscribers observing the stream see this arrive and know the silence that follows is deliberate production-side quiet, not a transient drop.
- Exits its inner FFT loop.
- Drops the ALSA PCM handle (verifiable rig-side via `lsof -p <pid>` — the loopback capture device disappears from the FD list).
- Sleeps on the transport + demand + interest watch channels (whichever transitions first wakes the outer loop).

When the gate reopens, the outer loop opens a fresh PCM and re-enters the FFT loop. The first live frame IS the unpark signal — its `at_ms` is non-zero and the envelope carries current magnitudes. No dedicated unpark event required.

### 4b. Demand as permission (not gate)

`demand.enabled` is the operator's persistent opt-in — a PERMISSION, not a run-signal:

- `enabled = false`: the producer stays parked regardless of every other condition. Operator's "I never want this device to capture" gesture. Persists across reboots.
- `enabled = true`: the producer MAY capture — necessary but not sufficient. Actual capture starts only when interest > 0 AND transport is playing AND role is source/auto.

Setting-based demand alone MUST NOT start capture. A device with the visualiser permitted but no subscriber stays parked. This is the produce-iff-consumed invariant — the run-signal is the presence of a consumer, not the operator's persistent permission.

The interest count comes from the framework-owned `system_subscription_interest` subject (addressing `evo.system:subscription_interest`). The steward tracks the WS subscriber count per subject_type and publishes state updates `{ subject_type, count, at_ms }` on every transition. Terminus subscribes via the standard `SubjectStateSubscriber` primitive and filters for its own subject_type.

**Rig acceptance (F2A + F3 combined):**

- Playing + `enabled=true` + `interest > 0`: baseline CPU + PCM FD present.
- Playing + toggle to `enabled=false`: subscribers receive one final envelope with `at_ms = 0` at the current shape, CPU drops, PCM FD disappears within one poll cycle.
- Playing + `enabled=true` + last subscriber disconnects → `interest = 0`: same final envelope + park, same FD/CPU drop.
- Playing + toggle back to `enabled=true` (or a subscriber reconnects): CPU returns, spectrum subject resumes with a non-zero `at_ms` frame — subscribers see the unpark signal without waiting for a dedicated event.

The metrics that matter are BOTH:

- CPU + FD change on park (silence-alone is not the win — the compute path must actually drop);
- the parked-envelope arrival on the wire (silence-alone is not the signal — subscribers must be able to distinguish deliberate quiet from a drop).

---

## 5. Landings

| Landing | Status | Scope |
| --- | --- | --- |
| F1 | Realised | `audio_playback_spectrum_demand` subject + `audio.spectrum.set_demand` verb + settings-patch bridge in the UI runtime. |
| F2A | Realised | `CaptureGate` extends with `demand.enabled`; ALSA PCM released on disable (rig-verified 0 FDs across fleet). |
| F2B | Realised | Variable analyser `SpectrumAnalyser::new(sample_rate_hz, bins, channels)`; `PerceptualFrame` carries payload-truth `bins`/`channels`; mono collapses at mel stage with zero-length `correlation`; rebuild on demand-change mid-play within one to two frames. |
| F2C | Realised | 30 Hz emit throttle decoupled from ALSA hop rate — ideal-target wall-clock governor in `emit_throttle::EmitThrottle`; wire `rate_hz` field carries the governor target; rig-measured 30.0–30.40 Hz across the shape enum on the 47 Hz ALSA chain. |
| F3 | Realised | (a) Volatile emission — no durable `subject_states` mirror (via `SubjectAnnouncer::update_state_volatile`). (b) Parked-state wire visibility — one final envelope with `at_ms = 0` on every park transition. (c) Subscription-interest signal via the framework-owned `system_subscription_interest` subject — subscriber count per subject_type published as state on transitions; terminus consumes via standard `SubjectStateSubscriber`; `interest > 0` becomes the fourth CaptureGate condition, `demand.enabled` demotes to operator permission. |
| F4 | Realised | Rig A/B evidence on the aarch64 (Pi 5) and x86_64 (NUC) targets across enable/disable + shape-enum cycle; producer park + shape mirror + emit rate all verified. |

F3 lands in one commit against the framework's runtime (interest-signal primitive) + one commit against this plugin (terminus consumer + parked-envelope emission).

---

## References

- `src/demand.rs` — demand module (subject + verb + watch broadcast).
- `src/capture.rs` — capture loop (outer CaptureGate + inner emit + PCM lifecycle).
- `src/lib.rs` — verb dispatcher + demand-store lifecycle.
- `manifest.toml` / `manifest.oop.toml` — request_types include `audio.spectrum.set_demand`.
- `dist/catalogue/audio-rack.toml` — `audio_playback_spectrum_demand` subject declaration.
- Pattern siblings: `plugins/org.evoframework.network.smb-server/docs/SAMBA-SHARES.md`, `plugins/org.evoframework.storage.usb/docs/USB-STORAGE.md`, `plugins/org.evoframework.playback.mpd/docs/LIBRARY-TRIAGE.md`.
