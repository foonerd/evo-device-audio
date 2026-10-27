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

## Route changes

The plugin registers a `RouteChangeCallback` on `load`. On
every framework-fired route change (source switch, format
change, composition mode change, delivery hot-plug), the
plugin closes the old endpoints, calls
`composition_endpoints()` for the new pair, and reopens the
OS-native primitive at the new format before resuming byte
flow. The respondent surface is unaffected by route changes
beyond the worker's reopen cycle.
