# delivery.alsa Modular Pipeline Contract

## Purpose

This document preserves the plugin-local contract for
`org.evoframework.delivery.alsa` and its relationship to the
device-tier modular ALSA standard.

Canonical device standard:

- `dist/MODULAR_ALSA_PIPELINE_STANDARD.md`

This plugin document exists so cross-reference from plugin work is
never lost when investigating routing/rendering behavior.

## Plugin responsibilities

`org.evoframework.delivery.alsa` is responsible for:

- observing `audio.options.settings`
- rendering ALSA drop-in definitions
- atomically writing `/etc/asound.d/evo-options.conf`
- exposing delivery inventory and endpoint views (`delivery.*` verbs)
- selecting render target based on active baseline topology

It is not responsible for:

- bootstrap ownership of baseline `/etc/asound.conf`
- source fan-out behavior
- multiroom role selection and source capture policy

## Render-target policy

The plugin supports two render targets:

- `Main`: rewires `pcm.evo*` nodes
- `SourceLocal`: rewires `pcm.evo_local*` nodes

Target selection rule:

- if active baseline graph includes `pcm.evo_local` -> `SourceLocal`
- else -> `Main`

This preserves source-host invariant:

- producer path (`pcm.evo`) remains loopback-only
- options changes affect source local renderer path only

## On-disk contract

- Baseline config: `/etc/asound.conf` (bootstrap-owned)
- Runtime options: `/etc/asound.d/evo-options.conf` (plugin-owned)

One writer per surface is mandatory.

## Failure semantics

- unreadable baseline while selecting target:
  - warn, default to `Main`
- drop-in write failure:
  - warn, prior pipeline remains active
- malformed settings payload:
  - refuse at responder boundary with structured error

## Verification expectations

For plugin-level verification, confirm:

1. drop-in rewrite occurs on options changes
2. source baseline triggers `SourceLocal` rendering
3. main baseline triggers `Main` rendering
4. no source-host rewrite of producer `pcm.evo`
5. deterministic render output (snapshot-stable tests)

## Cross-reference map

- Device-tier standard:
  - `dist/MODULAR_ALSA_PIPELINE_STANDARD.md`
- Plugin implementation:
  - `plugins/org.evoframework.delivery.alsa/src/lib.rs`
  - `plugins/org.evoframework.delivery.alsa/src/options_render.rs`
