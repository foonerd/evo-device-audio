# `alsa.pipeline.compose` v1

`org.evoframework.composition.alsa` exposes one request type:

- `alsa.pipeline.compose`

The request payload and the response payload are UTF-8 JSON.

## Request

```json
{
  "v": 1,
  "output": {
    "pcm": "hw:0,0",
    "ctl": "0"
  },
  "modules": [
    {
      "plugin": "org.evoframework.resampler",
      "id": "soxr",
      "order": 10,
      "snippet_template": "pcm.soxr_out { ... \"{{input_pcm}}\" ... }",
      "output_pcm": "soxr_out"
    }
  ],
  "final_alias": "volumio_pipeline"
}
```

### Validation summary

- `v` must be `1`.
- `output.pcm` must be non-empty and contain only `[A-Za-z0-9_.:,-]`.
- `final_alias` (or default alias) must match `[A-Za-z0-9_.-]+`.
- Each module identity `(plugin,id)` must be unique.
- `module.id` and `module.output_pcm` must match `[A-Za-z0-9_.-]+`.
- `module.snippet_template` must contain `{{input_pcm}}`.

Modules are sorted by `(order, plugin, id)` before composition.

## Response

Success:

```json
{
  "v": 1,
  "status": "ok",
  "pipeline": {
    "signature": "base=hw:0,0;chain=org.evoframework.resampler:soxr@10;final=soxr_out",
    "final_pcm": "volumio_pipeline",
    "asound_conf": "...",
    "mpd_audio_output": "...",
    "modules_applied": [
      {
        "plugin": "org.evoframework.resampler",
        "id": "soxr",
        "order": 10,
        "output_pcm": "soxr_out"
      }
    ]
  }
}
```

Validation failure:

```json
{
  "v": 1,
  "status": "bad_request",
  "error": "..."
}
```
