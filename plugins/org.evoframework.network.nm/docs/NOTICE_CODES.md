# `network.nm` notice codes

Machine-readable notices are returned as:

```json
{
  "level": "info|success|warning|error",
  "code": "stable_code_name",
  "message": "human-friendly text"
}
```

UI should key behavior/translations by `code`, not by `message`.

## Apply notices

- `network_apply_ok`
  - level: `success`
  - meaning: apply operation completed successfully.
- `network_apply_failed`
  - level: `error`
  - meaning: apply operation failed; inspect last step/error details.
- `network_apply_warning`
  - level: `warning`
  - meaning: non-fatal warning emitted during apply sequence.
- `network_apply_critical_recovery`
  - level: `warning`
  - meaning: critical fallback/recovery path was used (for example open-hotspot recovery).

## Captive notices

- `network_connectivity_full`
  - level: `success`
  - meaning: NM reports full connectivity.
- `network_connectivity_portal`
  - level: `info`
  - meaning: NM/probe indicates captive portal.
- `captive_submitting`
  - level: `info`
  - meaning: captive credential submit is in progress.
- `captive_authenticated`
  - level: `success`
  - meaning: captive authentication complete.
- `captive_failed`
  - level: `error`
  - meaning: captive submission/authentication failed.
- `captive_confirmation_required`
  - level: `warning`
  - meaning: guarded replay policy requires explicit confirmation before retry.

## UI recommendations

- Deduplicate identical `code` values in a short window (for example 30-60 seconds).
- Keep `captive_confirmation_required` visible/sticky until user acts.
- Localize by `code`; display plugin `message` as fallback detail.
- Preserve `level` mapping unless product has a stronger severity policy.
