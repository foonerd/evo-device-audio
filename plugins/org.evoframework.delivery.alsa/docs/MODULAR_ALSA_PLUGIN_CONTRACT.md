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
- steering one output at a time through the five output verbs
  (`delivery.enable_output`, `delivery.disable_output`,
  `delivery.set_output_volume`, `delivery.play_output`,
  `delivery.pause_output`)

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

## Output verbs

Each of the five output verbs names one output by the `hw:N,M`
id `delivery.list_outputs` gives it, and touches that output
only. An id `list_outputs` does not name is refused.

- `enable_output` / `disable_output`: whether the output may
  play. Disabling the playing output pauses it; no other output
  starts. The flag survives a restart.
- `set_output_volume { volume: 0..=100 }`: one `amixer` call
  against the output's own default mixer control. Refused when
  the output has no control, and when that control is the
  chain's hardware mixer: that is the global volume, which stays
  with `playback.options`. The last accepted level survives a
  restart.
- `play_output` / `pause_output`: the chain has one terminus, so
  an output is playing when the chain terminates at it. With no
  output verb spoken the chain follows the operator's
  `output_device`, and that output is the one playing. `play`
  binds the chain to the named output; a second `play` while
  another output is playing is refused and the first keeps
  playing. `pause` on the playing output renders a `null`
  terminus so it goes quiet and no other output starts. The
  binding is not persisted: the plugin releases it at unload,
  and the drop-in follows the operator's settings again.

The binding is rendered by the same renderer the options
observer uses, so an options change while an output is bound
keeps the bound terminus. `ctl.evo` and the softvol control stay
on the operator's card whatever the binding.

## Logical outputs

A logical output is a name the operator gives to one physical
`hw:N,M` output. It has a stable id the plugin mints
(`logical:<n>`, never reused), a name, and the physical output
it points at.

- `add_logical_output { name, output_id }`: a new row. The
  physical output must be one `delivery.list_outputs` names.
  The name is trimmed, non-empty, at most 64 characters, and
  unique among logical outputs case-insensitively. Two names
  may point at one physical output.
- `update_logical_output { id, name?, output_id? }`: a new
  name, a new physical output, or both; the id stays. An
  update that names neither is refused.
- `remove_logical_output { id }`: the row taken away; answers
  it. Refused, naming the zone, while a zone holds the id: the
  logical output, the zone and the physical output stay. Take
  the logical output out of the zone first.
- `list_logical_outputs {}`: every row, in id order; an empty
  set is an empty list. Reads only: no row, no file and no
  physical output changes.

A logical id is not a physical output: the five output verbs
refuse it. Names move no audio, no mixer and no drop-in. They
survive a restart in `<state_dir>/logical-outputs.toml`. No
subject is published and `delivery.list_outputs` is unchanged.

## Zones

A zone is a name over one or more logical outputs. It is not an
ALSA device and not an EQ. It has a stable id the plugin mints
(`zone:<n>`, never reused), a name, and the `logical:<n>` ids it
holds, in the order given.

- `zone.add { name, logical_output_ids }`: a new zone. The name
  is trimmed, non-empty, at most 64 characters, and unique among
  zones case-insensitively. The list holds at least one id; each
  id is a logical output the plugin has, named once, and in no
  other zone. A logical output sits in one zone at most.
- `zone.update { id, name?, logical_output_ids? }`: a new name,
  a new list, or both; the id stays. An update that names
  neither is refused. A refused update changes nothing.
- `zone.remove { id }`: the zone taken away; answers it. Its
  logical outputs and the physical outputs are untouched, and
  the freed logical outputs may join another zone.
- `zone.list {}`: every zone, in id order; an empty set is an
  empty list. Reads only.
- `zone.set_eq_engaged { id, engaged }` and
  `zone.set_eq_band { id, index, freq_hz, gain_db, q }`: the
  zone's own EQ, stored beside its name and membership. The
  flag starts false; there are exactly 10 bands, each
  `freq_hz` 20 through 20000, `gain_db` -15.0 through 15.0, `q`
  0.1 through 30.0 (the global EQ's ranges), starting at 1000
  Hz, 0 dB, Q 1.0. An unknown zone, an index outside 0 through
  9, and a field outside its range are refused before any
  write, naming the field. Every zone answer carries the EQ.
  A zones file written before the EQ existed loads with the
  default. The EQ is stored and answered only: nothing here
  renders it, and `eq_only` stays the one pipe.

`zone.add` and `zone.update` hold the logical-output read lock
from the membership check through the zone write, and
`remove_logical_output` holds the logical-output write lock from
its zone check through the removal; both take locks
logical-then-zones. A logical output therefore cannot be removed
between a zone's membership check and its write, and a zone
cannot take an id between the removal's check and its removal.

Zones move no audio, no mixer and no drop-in. A zone's EQ is
stored here, not rendered here; no scene is stored; `eq_only`
in composition.alsa stays the one pipe and the one cascade.

### Scenes

A scene is a name and a snapshot of every zone: id, name,
membership and EQ. It holds nothing else: not the playing
terminus, not the options EQ, not the logical outputs.

- `scene.save { name }`: the zones as they are, under the name.
  The name is trimmed, non-empty, at most 64 characters, and
  unique among scenes without regard to case; a scene of that
  name is replaced in place. At most 32 scenes: the 33rd is
  refused and nothing is written. Answers the scene.
- `scene.list {}`: the names, in the order first saved; an empty
  set is an empty list.
- `scene.recall { name }`: the live zones are replaced by the
  snapshot, ids kept; `next_id` becomes one past the highest
  present and never moves backward; an empty snapshot clears the
  zones. An unknown name, a snapshot naming a logical output the
  plugin does not have, one twice in a zone, or one in two
  zones is refused before any write, and the live zones stay.
  The logical outputs, the physical outputs and the options EQ
  stay. The EQ claim is published again. Answers the scene.
- `scene.remove { name }`: the scene taken away; the live zones
  stay. Answers it.

Scenes survive a restart in `<state_dir>/scenes.toml`. A missing
file is no scenes; a file that does not parse is no scenes, and
the live zones stay.

### The zone EQ claim

The plugin publishes one subject, `evo.audio.zones` /
`eq_claim` (type `audio_zone_eq_claim`): which zone's EQ the one
pipe should sound. The payload is `{ v: 1, claimed: false }`, or
`{ v: 1, claimed: true, zone_id, engaged: true, bands }` with
exactly 10 bands. The claim is true only while one physical
output is the chain terminus — the output `play_output` treats
as playing — and exactly one zone with `engaged` true holds a
logical output that points at that terminus. A silent terminus,
a zone with `engaged` false, and two engaged zones on the same
terminus all publish `claimed: false`. It is announced at load
and updated whenever the playing terminus, a zone's EQ or
membership, or a logical output's target changes; an unchanged
claim is not republished. The composition plugin subscribes and,
in `eq_only`, sounds the claimed bands instead of the options
bands; a missing subject leaves it on the options EQ. The
composition plugin does not read `zones.toml`.
Zones survive a restart in `<state_dir>/zones.toml`. No subject
is published. The four verbs are this plugin's `audio.zones`
stocking at shape 1; every other verb is its `audio.delivery`
stocking at shape 2. The manifest declares both as
`[[stockings]]`, and the two lists do not overlap.

## On-disk contract

- Baseline config: `/etc/asound.conf` (bootstrap-owned)
- Runtime options: `/etc/asound.d/evo-options.conf` (plugin-owned;
  rendered from the operator's settings plus the output binding)
- Output state: `<state_dir>/outputs-state.toml` (plugin-owned;
  enabled flags and last-set volumes, never the binding)
- Logical outputs: `<state_dir>/logical-outputs.toml`
  (plugin-owned; names, ids and the next id to mint)
- Zones: `<state_dir>/zones.toml` (plugin-owned; names, ids,
  logical output ids and the next id to mint)
- Scenes: `<state_dir>/scenes.toml` (plugin-owned; named
  snapshots of the zones)

One writer per surface is mandatory. The options observer and
the output verbs write the drop-in through one renderer under
one lock.

## Failure semantics

- unreadable baseline while selecting target:
  - warn, default to `Main`
- drop-in write failure:
  - warn, prior pipeline remains active
- malformed settings payload:
  - refuse at responder boundary with structured error
- unknown output, disabled output asked to play, second play,
  volume out of range, no mixer control, global mixer:
  - refuse at responder boundary; nothing moves
- `amixer` refuses a level:
  - the verb fails with the card's reason; no volume is recorded
- drop-in write failure after a rebind:
  - warn, verb fails transient, prior pipeline remains active
- unreadable or malformed `outputs-state.toml`:
  - warn, start with every output enabled and no volumes
- unknown logical output, empty or taken name, unknown
  physical output on add or update, empty update:
  - refuse at responder boundary; nothing moves
- unreadable or malformed `logical-outputs.toml`:
  - warn, start with no names
- a logical output a zone holds, asked to be removed:
  - refuse at responder boundary, naming the zone; nothing moves
- unknown zone, empty or taken name, no logical outputs, unknown
  or twice-named logical output, logical output in another
  zone, empty update:
  - refuse at responder boundary; nothing moves
- unreadable or malformed `zones.toml`:
  - warn, start with no zones
- unknown scene, empty or over-long scene name, a 33rd scene,
  a snapshot naming an unknown logical output or one in two
  zones:
  - refuse at responder boundary; the live zones stay
- unreadable or malformed `scenes.toml`:
  - warn, start with no scenes; the live zones stay

## Verification expectations

For plugin-level verification, confirm:

1. drop-in rewrite occurs on options changes
2. source baseline triggers `SourceLocal` rendering
3. main baseline triggers `Main` rendering
4. no source-host rewrite of producer `pcm.evo`
5. deterministic render output (snapshot-stable tests)
6. changing one output leaves the other as it was
7. an unknown output is refused by every output verb
8. the five output verbs, the four logical-output verbs and the
   four zone verbs are named only in this plugin
   (`dist/scripts/tests/output-verbs-live-in-delivery.test.sh`)
9. a logical id is refused by the five output verbs
10. `list_logical_outputs` lists what was added, drops what
    was removed, and leaves `list_outputs` unchanged
11. a zone is refused with no logical output; one logical
    output cannot sit in two zones; `zone.remove` drops no
    logical output and no physical output
12. a logical output a zone holds cannot be removed, and the
    refusal names the zone
13. a band write on one zone changes no other zone and reaches
    no mixer, drop-in, dispatcher or options verb
14. the claim follows the terminus and the one engaged zone;
    two engaged zones on one terminus do not claim; a band write
    on a zone without the claim publishes nothing new
15. a refused recall changes no zone; a recall changes no
    logical output and no options EQ; the 33rd scene is refused
    and nothing is written

## Cross-reference map

- Device-tier standard:
  - `dist/MODULAR_ALSA_PIPELINE_STANDARD.md`
- Plugin implementation:
  - `plugins/org.evoframework.delivery.alsa/src/lib.rs`
  - `plugins/org.evoframework.delivery.alsa/src/options_render.rs`
