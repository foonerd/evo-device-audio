# `composition.select_mode` v1

`org.evoframework.composition.alsa` is a substrate-aware
respondent on the `audio.composition` shelf at shape 2. The
plugin consumes the framework's audio data plane through
`LoadContext::audio_routing`: it opens the
`CompositionEndpoints { input: ReadEndpoint, output: WriteEndpoint }`
pair the framework configured, drives audio bytes from input
to output, and reacts to topology rewires through a
registered `RouteChangeCallback`. The plugin exposes one
respondent surface — `composition.select_mode` — that the
framework calls when the reconciliation engine selects a new
composition mode for the active topology.

The request payload and the response payload are UTF-8 JSON.

## Declared modes

This build of the plugin declares one mode in
`[capabilities.composition].modes`:

| Mode | `preserves_bit_perfect` | Behaviour |
|---|---|---|
| `passthrough` | `true` | Byte-identical copy from input endpoint to output endpoint. No transformation. |

Subsequent commits layer further modes (`eq_only`,
`resampler`, `dsd_to_pcm`) onto this same plugin without
requiring a shape bump. The reconciliation engine picks one
mode per topology after intersecting source-produced format
with delivery-accepted format and applying operator policy.

## Request

```json
{
  "v": 1,
  "mode": "passthrough"
}
```

### Validation

- `v` must be `1`.
- `mode` must be a non-empty string.
- `mode` must match a `name` in the plugin's declared
  `[capabilities.composition].modes` list.

## Response

Success:

```json
{
  "v": 1,
  "status": "ok",
  "active_mode": "passthrough"
}
```

Refusal of an unknown mode:

```json
{
  "v": 1,
  "status": "bad_request",
  "error": "unknown mode 'eq_only'; declared modes: [passthrough]"
}
```

Refusal indicates a framework / catalogue drift — the
reconciliation engine MUST NOT have offered a mode the plugin
did not declare. Operators surface this as an admission /
catalogue inconsistency rather than a plugin-side fault.

## Audio byte flow

Audio bytes never appear in this request / response surface.
They flow exclusively through the OS-native primitive
identified by `composition_endpoints()`:

- `EndpointKind::AlsaPcm` — input is opened as ALSA capture
  on the framework-configured pcm name; output is opened as
  ALSA playback on the framework-configured pcm name; the
  worker drives a frame-aligned copy loop honouring the
  negotiated `AudioFormat`.
- Other endpoint kinds (`NamedPipe`, `SharedMemory`,
  `JackPort`) — supported by the SDK; not exercised by this
  build's worker. The plugin returns
  `endpoint_kind_unsupported` in `health_check` when the
  framework selects an endpoint kind beyond the worker's
  current implementation; future commits widen the worker
  to additional substrates.

## The zone EQ claim

`eq_only` is the one pipe and runs one cascade of 10 bands. The
bands it sounds come from one of two inputs: the operator's EQ on
`audio.options.settings` (`eq_engaged`, `eq_bands`), or the
delivery plugin's `evo.audio.zones` / `eq_claim` subject. The
claim is `{ v: 1, claimed: false }` or `{ v: 1, claimed: true,
zone_id, engaged: true, bands }` with exactly 10 bands. While the
claim is true the cascade sounds that zone's bands and treats the
EQ as engaged; the options EQ stays stored as it is and does not
supply the coefficients for that stretch. When the claim drops, or
the subject is missing or malformed, the options EQ sounds again.
A zone's bands are applied in no mode but `eq_only`. This plugin
does not read `zones.toml`.

## Where the EQ sits in the audio path

With `passthrough` selected the audio goes from the source straight
into the DAC's pcm (`evo`): the distribution publishes a two-stage
chain, source then delivery, and this plugin is not in the path. It
holds no sound device in that state, whatever endpoints it was last
given.

Selecting `eq_only` changes the chain. The plugin announces its mode
as the plugin event `composition.mode_changed` (payload
`{ "v": 1, "mode": "<mode>" }`) when it loads, on every mode change,
and when it takes a mode back (below). The distribution follows that
event and publishes a three-stage chain: the source writes
`evo_comp_in`, a fixed-format pcm on snd-aloop playback subdevice 3;
this plugin captures subdevice 3 of the other half
(`hw:Loopback,1,3`), runs its cascade, and plays the result into
`evo`; delivery reads `evo` as before. Choosing `passthrough` again
publishes the two-stage chain back, and the plugin closes its devices
as soon as the mode leaves `eq_only`. The chain carries one format,
44.1 kHz / 16-bit / stereo, which is what the source already renders
in either chain.

The cascade runs on the frames it moves when the mode is `eq_only`,
the EQ is engaged, and the format is one the DSP covers (s16le and
f32le, mono or stereo). Otherwise the frames pass byte-identical. The
bands come from the operator's EQ, or from the engaged zone's bands
while the delivery plugin's `eq_claim` names a zone (see above).

The pcm `evo_comp_in` is defined in the distribution's `asound.conf`.
Subdevices 0 and 7 of the loopback belong to multi-room source mode
and the terminus tap; the EQ chain uses 3.

### Opening the devices, and failing safe

A rewire hands the output pcm to this plugin while the source that
held it is still restarting, so the first open can find it busy. The
ALSA substrate retries the open for a bounded window (20 tries,
500 ms apart) and ends the wait at once if the chain changes. A
device that stays unopenable for the whole window, or a read or write
that breaks, puts the worker in `Failed`.

With the EQ in the path a failed worker reads nothing and the listener
hears silence, so the plugin takes the mode back: while the mode is
`eq_only` and the worker is `Failed`, the mode becomes `passthrough`,
is recorded so the next load does not repeat the failure, and is
announced and reported to the options as above. The distribution then
publishes the two-stage chain. `Unsupported` (a build that cannot
drive the substrate at all) is left to say so.

## Route changes

The plugin registers a `RouteChangeCallback` on `load`. On
every framework-fired route change (source switch, format
change, composition mode change, delivery hot-plug), the
plugin closes the old endpoints, calls
`composition_endpoints()` for the new pair, and reopens the
OS-native primitive at the new format before resuming byte
flow. The respondent surface is unaffected by route changes
beyond the worker's reopen cycle.
