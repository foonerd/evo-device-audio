# `network.nm` operations runbook

This runbook is for operators maintaining `org.evoframework.network.nm` in field
deployments (from constrained SBC targets to high-core servers).

## 1) Quick triage

Start with `network.nm.status` and inspect:

- `degraded`
- `domain_health.device_table`
- `domain_health.general_status`
- `domain_health.wifi_scan`
- `observability.correlation_id`

Guideline:

- `degraded = false`: continue with normal intent/captive workflow.
- `degraded = true`: investigate only failing domains first; avoid broad resets.

## 2) Captive portal stuck or flaky

1. Call `network.nm.captive.status` with probe enabled.
2. Inspect:
   - `captive.phase`
   - `captive.last_error`
   - `captive.requires_user_confirmation`
   - `actions[]` (`captive.confirm_replay` when guarded replay is needed)
3. If confirmation is required, issue `network.nm.captive.submit` with
   `confirm_replay = true` only after explicit operator/UI decision.
4. If portal flow remains failed, mark terminal state with
   `network.nm.captive.complete` and `success = false`.

## 3) Secret encryption and key handling

Secrets are stored in `wifi-sta.psk` / `wifi-ap.psk` with optional encryption.

- `EVO_NETWORK_SECRET_KEY` enables encrypted secret envelopes.
- `EVO_NETWORK_SECRET_REQUIRE=1` enforces encrypted-only secret reads/writes.

When `require` is enabled but key is missing, treat this as a deployment
misconfiguration (not a network fault).

## 4) Secret key rotation (no intent loss)

1. Ensure service is healthy and `network.nm.intent.get` succeeds.
2. Deploy new `EVO_NETWORK_SECRET_KEY`.
3. Re-submit current intent via `network.nm.intent.set` including current PSKs;
   this rewrites secret files with the new key material.
4. Verify `network.nm.intent.get` reports `*_psk_configured = true`.
5. Run `network.nm.intent.apply` and confirm `apply.ok = true`.

## 5) Recovery after reboot / power loss

The plugin uses atomic write + LKG shadow for intent and captive state.

If primary state is corrupted:

- plugin attempts LKG fallback automatically;
- fallback path is visible in logs and behavior.

Operator action:

1. Read `network.nm.intent.get`.
2. If intent is unexpected, push canonical intent via `network.nm.intent.set`.
3. Re-run `network.nm.intent.apply`.

## 6) Degraded status domain playbook

- `device_table` failed:
  - verify `nmcli` availability and permissions;
  - avoid captive retries until device inventory is stable.
- `general_status` failed:
  - inspect NetworkManager daemon state;
  - keep current connections unchanged while diagnosing.
- `wifi_scan` failed:
  - do not clear persisted credentials preemptively;
  - continue with known SSID/BSSID policy where possible.
- `radio` blocked:
  - check `flight_mode.block_reason` in `network.nm.status`;
  - for `software_blocked`, clear rfkill/NM flight mode then retry apply;
  - for `hardware_or_bios_blocked`, verify chassis switch/key or BIOS policy.

## 7) Zero-downtime operating posture

- Prefer targeted intent updates over broad teardown.
- Keep fallback hotspot policy explicit in intent.
- Use `network.nm.scan` `candidates` for deterministic STA selection.
- Use `observability.correlation_id` to correlate API/UI/log events per action.

## 8) Hardware auto-profiler policy (vendor devices)

For fleets with mixed hardware, keep a profile policy outside the plugin and
feed values through config management (UI provisioning, warden, image defaults).

Profile baseline:

- `balanced`: `nmcli_timeout_ms=8000`, `curl_timeout_ms=30000`,
  `scan_cache_ttl_ms=3000`
- `constrained`: `nmcli_timeout_ms=10000`, `curl_timeout_ms=30000-35000`,
  `scan_cache_ttl_ms=5000`
- `performance`: `nmcli_timeout_ms=6000`, `curl_timeout_ms=20000`,
  `scan_cache_ttl_ms=1500-3000`

Operational rules:

1. Hardware profiler chooses profile by board/vendor class.
2. Operator override always wins over profiler recommendation.
3. Unknown board class falls back to `balanced`.
4. Changes should be applied on controlled restart windows (avoid churn).
5. Record selected profile in device diagnostics for support triage.

## 9) Flight mode and rfkill behavior

The plugin is defensive for both software and hardware flight-mode cases:

- status reports radio lock state under `flight_mode.*` and `domain_health.radio`;
- apply does not panic on blocked Wi-Fi radio; it returns a controlled
  non-success report (`apply.ok=false`) with actionable warning steps.
- on startup, plugin enforces Wi-Fi radio `on` unless persisted flight mode is enabled.

Control API (Wi-Fi only):

- `network.nm.flight_mode.get`
- `network.nm.flight_mode.set` with `{ "enabled": true|false }`

Operator workflow:

1. Call `network.nm.status`.
2. If `flight_mode.wifi_blocked=true`, resolve the blocker first:
   - software path: unblock via NM/rfkill policy tooling;
   - hardware path: toggle physical key/switch or BIOS/EC policy.
3. Re-run `network.nm.intent.apply`.
