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
| --- | --- | --- |
| `v` | `u32` | Envelope version. `1` today. |
| `enabled` | `bool` | Producer gate. `false` → PCM released + no FFT + no emit. `true` → capture opens (subject to transport + role gates). |
| `bins` | `u32` | Output bin count. Enum: `32 \| 64 \| 128 \| 256`. Refused with Permanent outside the enum. |
| `channels` | `u32` | Channel mode. Enum: `1 \| 2` (`1` = mono; `2` = stereo). Refused with Permanent outside the enum. |
| `rate_hz_target` | `u32` | Emit throttle target in Hz. Clamped to `[1, 60]` at the plugin; typical value `30`. |
| `frequency_scale` | `string` | Frequency-bin spacing across `[20, 20000]` Hz. Enum: `"log" \| "mel" \| "linear"`. Default `"log"` (music-analyser convention). Refused with Permanent outside the enum. Absent field (older UI runtime bridge that has not been upgraded) parses to `"log"`. |
| `updated_at_ms` | `u64` | Wall-clock ms of last apply. Zero before the first apply. |

**Addressing:** `evo.audio.playback:spectrum_demand`.
**Cardinality:** singleton per device.

**Disabled default (plugin load):** `{ enabled: false, bins: 64, channels: 1, rate_hz_target: 30, frequency_scale: "log", updated_at_ms: 0 }` — matches the pre-supersession baseline (no spectrum activity on a device where the operator has never touched `ui.visualizer.*`). `frequency_scale` defaults to `"log"` per the 2026-08-11 spectrum-frequency-scale ownership audit — the log axis is the music-analyser convention and the default the wire lands at when no prior demand has been persisted.

**Frequency scale semantics:**

- **`"log"`** (default) — equal log-ratio spacing across `[20, 20000]` Hz. Allocates ~37% of display width to 20–250 Hz. Matches the industry music-analyser default (audioMotion, TrueRTA, most SPL meters); recommended for every music-heavy device.
- **`"mel"`** — equal mel-scale spacing (perceptual-loudness bank the plugin originally shipped). Allocates ~8% of display width to 20–250 Hz. Better than linear for perception, but noticeably left-parked for music. Retained for operators who preferred the pre-2026-08-11 look.
- **`"linear"`** — equal Hz spacing (raw-FFT diagnostic layout). Allocates ~1% of display width to 20–250 Hz. Useful for engineering visualisation of raw spectrum energy; not recommended as a music-playback default.

The analyser rebuilds on `frequency_scale` change identically to a `bins` or `channels` change: the capture loop's inner FFT loop exits via `DemandShapeChanged`, and the outer loop constructs a new `SpectrumAnalyser` at the new scale within one or two frames. Peak-hold + onset history reset on rebuild — the small visual discontinuity is preferable to fabricating post-rebuild state.

---

## 2. Write verb — `audio.spectrum.set_demand`

The framework-side wire op that mutates the demand subject.
Dispatched via the plugin's respondent surface on the
`audio.terminus` shelf.

**Payload** (v1):

```json
{
  "v":               1,
  "enabled":         true,
  "bins":            64,
  "channels":        1,
  "rate_hz_target":  30,
  "frequency_scale": "log"
}
```

**Response** (v1):

```json
{
  "v":       1,
  "applied": {
    "v":               1,
    "enabled":         true,
    "bins":            64,
    "channels":        1,
    "rate_hz_target":  30,
    "frequency_scale": "log",
    "updated_at_ms":   1720000000000
  }
}
```

**`deny_unknown_fields`** on the parsed struct catches typos synchronously — a client passing `preset` or `palette` receives `invalid_payload` synchronously rather than silently discarding the unknown parameter. The same class of refusal applies to an unknown `frequency_scale` value (e.g. `"octave"`, `"logarithmic"`, `"fft"` — no aliases).

**Absent `frequency_scale` on the wire** defaults to `"log"` at the verb parse boundary — the compatibility hatch for an older UI runtime bridge that has not been upgraded. UI landing the setting in the same release train removes the hatch's steady-state role; absence remains supported for future forward compatibility.

---

## 3. Apply path — UI derives, terminus applies + remembers

Two-sided responsibility. **UI** derives the operator's intent from the settings surface and pushes it via `audio.spectrum.set_demand`. **Terminus** applies the pushed value, publishes it on the demand subject, AND persists it so the intent survives reboot + terminus reload without a UI re-push.

### 3.1 UI-side derive (master ∧ preset)

The operator's on/off intent is the logical AND of two settings:

- `ui.visualizer.enabled` — the master switch. `false` means "off, everywhere, always".
- `ui.visualizer.preset` — the renderer choice. `"off"` is a preset value that also means "off".

`enabled` in the demand payload is `master && preset != "off"`. Either setting flipping to off produces `enabled: false`. Both must be on for the demand to carry `enabled: true`. `preset` itself is renderer-only and never crosses the wire to terminus.

Any UI settings surface (Settings screen, Designer Visualizer Studio, future clients) writes to `/opt/evo/ui/data/settings.json`. `evo-ui-runtime`'s patch bridge derives the demand payload from the derived on/off value plus the shape settings and calls `audio.spectrum.set_demand` in the same request cycle. The runtime bridge is the single write path — every settings origin inherits the demand-write automatically.

**Mapping:**

| Settings | Demand field | Transformation |
| --- | --- | --- |
| `ui.visualizer.enabled && ui.visualizer.preset != "off"` | `enabled` | logical AND at the UI |
| `ui.visualizer.bin_count` (`32` / `64` / `128` / `256`) | `bins` | direct copy |
| `ui.visualizer.channel_mode` (`mono` / `stereo`) | `channels` | `mono` → `1`; `stereo` → `2` |
| `ui.visualizer.frequency_scale` (`log` / `mel` / `linear`) | `frequency_scale` | direct copy; absent → `"log"` |
| — | `rate_hz_target` | fixed to `30` at the runtime bridge (or read from an optional operator setting when one exists) |

Renderer-only settings (`preset`, `palette`, `color_mode`, `sensitivity_db`) never appear on the demand — they change what the frame is drawn as, not what the producer emits. Visualizer.tsx MUST NOT remap bin indices for scale; the wire carries payload-truth bins in the operator-selected scale order.

### 3.2 Terminus-side apply + persistence

`SpectrumDemandStore::handle_set_demand` validates the payload (bins/channels enums, `rate_hz_target` clamp), updates the in-memory value via `send_replace` on the watch channel the capture loop consumes, and republishes the demand subject via `SubjectAnnouncer::update_state` (the durable path — not `update_state_volatile`, which is reserved for spectrum-frame emissions). Every successful apply writes through to the framework's `subject_states` durable mirror.

### 3.3 Rehydrate on plugin load

`SpectrumDemandStore::announce_initial` reads the framework's `subject_states` mirror on every plugin load. Prior applied demand → rehydrated + used as the initial store value. No prior state (first-ever boot on this device, or the row was cleared) → `disabled_default`. Corrupted / out-of-range persisted rows → `disabled_default` (silence beats a stale-envelope-driven FFT run). Terminus emits one of two structured log lines at plugin load — `source = "rehydrate-from-mirror"` or `source = "no-prior-state-use-default"` — so the journal shows which path was taken.

Consequence: **operator intent survives reboot + terminus reload without a UI re-push.** UI reassert stays valuable as drift-detection against concurrent operator override or a manual demand reset; it is no longer the sole memory of what the operator asked for.

### 3.4 Wire-honesty on `/api/v1/request`

The framework's HTTPS `/request` path returns a truthful HTTP status class:

- Verb capability denied / plugin not admitted / no responder / application error → non-2xx (4xx for caller-fault classes, 5xx / 429 for retryable pressure classes). The full framework error envelope (class, subclass, message, op id) rides in the body verbatim.
- Successful apply → HTTP 200 with `{ v, applied: { enabled, bins, channels, rate_hz_target, updated_at_ms } }`. The `applied` envelope carries the values terminus actually applied so a UI validated-apply helper can compare against what it sent.

The UI's originate can classify by HTTP status alone (retryable vs permanent) without parsing every 200's body to hunt for a hidden error envelope, and can validate that terminus applied what it sent by comparing the `applied` fields against its request.

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

The interest count comes from a per-subject-type framework-owned subject. For a producer subject_type X, the framework owns a subject at addressing `evo.system:subscription_interest.X` and publishes state `{ subject_type, count, at_ms }` on it for every interest transition. Per-type isolation — one subject per subject_type, one state stream per subject — eliminates the last-write-wins race a single-subject shape would have when multiple types transition concurrently. Terminus subscribes to `evo.system:subscription_interest.audio_playback_spectrum_frame` via the standard `SubjectStateSubscriber` primitive.

**Boot-seed at count=0.** Producer plugins call `SubjectAnnouncer::seed_interest_zero(subject_type)` at load — idempotently creates the per-type interest subject at `{count:0, at_ms:0}` if not already present. Without this, the interest subject is announced lazily on the first `increment_interest` call, and the producer's `interest_subscriber` polls `resolve_addressing → None` every 500 ms until a consumer arrives. With the seed, the subject exists at plugin-load time and the subscriber resolves + attaches on first attempt. Idempotent w.r.t. the race where a consumer increments first: an existing count is not clobbered.

The framework's counter is fed from every subscription surface: WSS `subscribe_happenings` (per subject_type in the filter's allow-list), WSS `subscribe_subject` (canonical_id → subject_type lookup), Unix `subscribe_happenings`, Unix `subscribe_subject`. All four increment on subscribe, decrement on stream drop; multi-consumer devices see the sum. The `watch::Sender` inside the counter uses `send_replace` — `send` silently drops the value when there are zero receivers, which broke earlier iterations of the primitive.

Terminus resyncs the gate from the authoritative current_state on `SubjectStateStreamError::Lagged` and `::Closed` — a missed transition on the broadcast doesn't strand the gate at a stale value.

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
| F3 | Realised | (a) Volatile emission — no durable `subject_states` mirror (via `SubjectAnnouncer::update_state_volatile`). (b) Parked-state wire visibility — one final envelope with `at_ms = 0` on every park transition. (c) Subscription-interest signal — per-subject-type framework-owned interest subject with state published on transitions; terminus consumes via standard `SubjectStateSubscriber`; `interest > 0` becomes the fourth CaptureGate condition, `demand.enabled` demotes to operator permission. |
| F4 | Realised | Rig A/B evidence on the aarch64 (Pi 5) and x86_64 (NUC) targets across enable/disable + shape-enum cycle; producer park + shape mirror + emit rate all verified. |
| F5 | Realised | (a) Demand durability — terminus `SpectrumDemandStore` rehydrates from `subject_states` on plugin load, falls back to `disabled_default` only when no prior applied demand exists; operator intent survives reboot + terminus reload without UI re-push. (b) Interest boot-seed — producer plugins call `SubjectAnnouncer::seed_interest_zero` on load to eliminate the resolve-retry loop before the first consumer arrives. (c) Wire-honesty on `/api/v1/request` — refused dispatch surfaces as non-2xx with the framework error envelope preserved in the body; UI validated-apply helpers can classify by HTTP status and validate against the `applied` envelope without parsing every 200 for hidden errors. |

F3 lands in one commit against the framework's runtime (interest-signal primitive) + one commit against this plugin (terminus consumer + parked-envelope emission). F5 lands as three commits — durable-demand on this plugin (`SpectrumDemandStore::announce_initial` rehydrate), interest boot-seed as a shared framework SDK primitive plus a terminus caller-side commit, and wire-honesty as a framework-side commit on the `Dispatcher` trait + `Server::dispatch_http_wire_op` path.

---

## References

- `src/demand.rs` — demand module (subject + verb + watch broadcast).
- `src/capture.rs` — capture loop (outer CaptureGate + inner emit + PCM lifecycle).
- `src/lib.rs` — verb dispatcher + demand-store lifecycle.
- `manifest.toml` / `manifest.oop.toml` — request_types include `audio.spectrum.set_demand`.
- `dist/catalogue/audio-rack.toml` — `audio_playback_spectrum_demand` subject declaration.
- Pattern siblings: `plugins/org.evoframework.network.smb-server/docs/SAMBA-SHARES.md`, `plugins/org.evoframework.storage.usb/docs/USB-STORAGE.md`, `plugins/org.evoframework.playback.mpd/docs/LIBRARY-TRIAGE.md`.
