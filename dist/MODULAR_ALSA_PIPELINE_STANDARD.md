# Modular ALSA Pipeline Standard (Device Reference)

## Scope

This document defines the canonical ALSA pipeline contract for
`evo-device-audio` targets. It is the reference implementation
and future extension baseline for device audio routing across
every hardware class the distribution admits.

The standard applies to:

- bootstrap-installed `/etc/asound.conf` variants
- runtime options rendering to `/etc/asound.d/evo-options.conf`
- source/receiver multiroom engagement wiring
- operator-visible diagnostics and failure semantics

Plugin-local companion contract:

- `plugins/org.evoframework.delivery.alsa/docs/MODULAR_ALSA_PLUGIN_CONTRACT.md`

## Design invariants

1. **One capability, one canonical path**
   - Producers write to one canonical ingress PCM.
   - Device renderers consume one canonical local-render PCM.
2. **No parallel truth**
   - Baseline ALSA graph is bootstrap-owned.
   - Runtime options graph is plugin-owned (drop-in only).
3. **Deterministic ownership**
   - Producer path and renderer path are distinct on source hosts.
   - No component silently writes outside its declared path.
4. **Explicit failure semantics**
   - Misconfiguration fails loudly (structured errors / probe warnings).
   - No silent fallback that masks topology breakage.

## Canonical topology contracts

### Receiver / standalone topology

- `pcm.evo` is the canonical producer ingress.
- Operator options may re-render `pcm.evo*` chain in drop-in.
- `ctl.evo` points to the active hardware card.

Flow:

`producer -> pcm.evo -> (optional rate/mixer) -> hw terminus`

### Source topology (multiroom source host)

- `pcm.evo` is producer-only and writes to loopback playback.
- `pcm.evo_loopback_capture` is capture target for source fan-out.
- `pcm.evo_local` is local DAC render target for source-local playback.
- Operator options on source hosts re-render **`pcm.evo_local*`**
  nodes, not `pcm.evo`, preserving producer-loopback invariants.

Flow:

`producer -> pcm.evo -> loopback playback`

`loopback capture -> multiroom fan-out scheduler -> remote receivers`

`loopback capture -> multiroom fan-out scheduler -> pcm.evo_local -> DAC`

## Ownership boundaries

- **Bootstrap (`dist/scripts/bootstrap.sh`) owns:**
  - baseline `/etc/asound.conf` install
  - role-specific template selection
  - source-role `snd-aloop` prerequisite
  - initial plugin config rendering
- **delivery.alsa owns:**
  - runtime drop-in rendering/writes
  - options-reactive pipeline projection
  - source-vs-main render-target selection
- **multiroom source plugin owns:**
  - source capture and fan-out
  - source-local DAC rendering via configured `alsa_pcm`

## Runtime options rendering contract

- Drop-in path: `/etc/asound.d/evo-options.conf`
- Render target selection:
  - if baseline graph contains `pcm.evo_local`, render source-local mode
  - else render main mode
- Main mode rewires:
  - `pcm.evo`
  - `pcm.evo_rate` / `pcm.evo_mixer` / `pcm.evo_terminus`
- Source-local mode rewires:
  - `pcm.evo_local`
  - `pcm.evo_local_rate` / `pcm.evo_local_mixer` / `pcm.evo_local_terminus`

## Bootstrap contract (source role)

Source role must render both fields in
`org.evoframework.multiroom.evo-native.toml`:

- `source_pcm` (loopback capture side)
- `alsa_pcm` (local DAC renderer target)

If source role omits `alsa_pcm`, bootstrap defaults to `evo_local`.

## Failure semantics

- Missing or unreadable `asound.conf` during render-target detection:
  - warn and default to main render target
- Drop-in write failure:
  - warn, retain prior on-disk pipeline
- Invalid operator setting domain:
  - structured refusal at responder boundary
- PCM open/probe failure:
  - explicit bootstrap verification warning with actionable hint

## Verification gates (release bar)

1. **Topology correctness**
   - source host: `pcm.evo` loopback-only, `pcm.evo_local` present
   - receiver host: `pcm.evo` terminates to active DAC chain
2. **Ownership correctness**
   - source producer opens loopback producer path only
   - source local renderer opens `alsa_pcm` target only
3. **No device-busy collisions**
   - no `Device or resource busy` on playback/test tone
4. **Options reactivity**
   - changing mixer/output updates drop-in and affects next PCM open
5. **Observable semantics**
   - warnings/happenings/logs map to real failure classes

## Extension standard

New processing stages must follow these rules:

- Additive, named ALSA nodes (no unnamed implicit side effects)
- One writer per config surface
- Backward-compatible defaults
- Explicit render-target behavior for source vs non-source
- Deterministic tests that pin rendered output shapes
