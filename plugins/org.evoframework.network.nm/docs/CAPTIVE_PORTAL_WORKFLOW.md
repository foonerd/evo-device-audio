# Captive portal workflow

This document defines what `org.evoframework.network.nm` handles directly and
which cases require UI/browser support.

## Supported plugin-side flow

The plugin supports a backend-friendly captive lifecycle:

1. Associate with Wi-Fi (`network.nm.intent.set` / `network.nm.intent.apply`).
2. Detect captivity (`network.nm.captive.status` or `network.nm.captive.start`).
3. Submit portal credentials (`network.nm.captive.submit`).
4. Mark completion (`network.nm.captive.complete`) or continue retries.

Session state is persisted (`captive-session.json`) with phase:

- `idle`
- `probe_detected`
- `awaiting_credentials`
- `submitting`
- `authenticated`
- `failed`

Durability follows steward-style LKG mirroring and atomic rename:

- primary: `captive-session.json`
- LKG shadow: `captive-session.lkg.json`
- atomic write temp: `captive-session.json.tmp`

Persisted reliability fields include:

- `submit_attempts`
- `last_submit_fingerprint`
- `last_submit_at_epoch`
- `requires_user_confirmation`

## Reliability controls for drop/reboot environments

The plugin supports policy-driven replay behavior:

- `replay_allowed` (default): automatic retries allowed up to `retry_budget`.
- `single_use_ticket`: blocks automatic credential replay; retry requires `confirm_replay=true`.
- `manual_after_failure`: after failed submit, replay requires `confirm_replay=true`.

Additional guards:

- `retry_budget`: maximum automatic retries for same credential fingerprint.
- `replay_window_sec`: resets replay counter after cooldown window.

## What works well now

- Typical guest portal forms with key/value fields.
- Mixed-case and alphanumeric access codes.
- Room number + guest name style payloads.
- Redirect/probe based detection (`curl` effective URL + HTTP code + NM connectivity).

## Recovery scenarios

- **Network drop (short outage):** persisted state survives; plugin re-detects captivity and can resume per policy.
- **Power failure/reboot:** intent + captive state survive; replay behavior obeys `credential_policy` and retry budget.
- **Portal remembers device (MAC/session):** often transitions to `authenticated` without re-submit.
- **Portal does not remember device:** may require re-submit; policy decides if replay is automatic or user-confirmed.
- **Single-use ticket risk:** set `single_use_ticket`; plugin blocks replay unless UI/operator explicitly confirms.

## UI-required scenarios (must be called out to product/UI)

The plugin is intentionally non-browser. UI support is required for:

- Multi-page portal journeys where fields appear after JS execution.
- Click-through terms pages with dynamic hidden fields/tokens.
- One-time-password, SMS, or email confirmation pages.
- Human challenges (captcha, checkbox anti-bot, passkey prompts).
- Corporate SSO pages (OAuth/SAML) requiring full browser sessions.
- Portals that require rendering/inspection before field names are known.

In these cases, backend plugin methods can keep state and submit known fields,
but a UI/browser layer must collect dynamic form structure and user actions.

## Recommended UI integration contract

- Start with `network.nm.captive.start`.
- Display `captive.portal_url` when phase is `probe_detected` or `awaiting_credentials`.
- Collect fields in UI and post to `network.nm.captive.submit`.
- Poll `network.nm.captive.status` until connectivity is `full` or phase is `authenticated`.
- If plugin returns `failed`, show `captive.last_error` and allow retry/edit.
- When `captive.requires_user_confirmation=true`, UI must require explicit user/operator action before retry.
- Render backend-provided `actions[]` (for example `captive.confirm_replay`) as the primary UI action model, instead of inferring behavior from free-form text.

## Toast/status notification guidance

To avoid "silent" network behavior, UI should emit concise notifications:

- **Info**
  - "Checking captive portal..."
  - "Trying saved network profile..."
- **Success**
  - "Network connected"
  - "Captive portal authentication complete"
- **Warning**
  - "Retry limit reached; confirmation required"
  - "Hotspot fallback activated"
  - "Recovered using last-known-good network state"
- **Error**
  - "Portal authentication failed"
  - "Network apply failed"

Recommended dedupe:

- Collapse duplicate warnings within a short window (for example 30-60s).
- Keep one sticky warning when operator action is required.
- Always include actionable next step ("Retry with confirmation", "Open portal", "Edit credentials").

Notice `code` values are stable and documented in `docs/NOTICE_CODES.md`.

## Scenario checklist

- Open network + captive splash page.
- WPA2 network + captive web auth.
- Room/name form with mixed-case voucher.
- Invalid code retry loop.
- Redirect chain ending in success.
- Redirect chain still captive after submit.
- No internet and no captive (limited connectivity).
- One-time ticket consumed; replay blocked unless explicitly confirmed.
- Reboot mid-login followed by guarded replay decision.
