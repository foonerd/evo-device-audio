# `network.nm` requests v1

`org.evoframework.network.nm` is a singleton respondent for the `networking.link`
shelf. Payloads are UTF-8 JSON.

## Request types

- `network.nm.status`
- `network.nm.scan`
- `network.nm.intent.get`
- `network.nm.intent.set`
- `network.nm.intent.apply`
- `network.nm.captive.status`
- `network.nm.captive.start`
- `network.nm.captive.submit`
- `network.nm.captive.complete`

## Intent model parity notes

The plugin accepts/uses the tested `volumio-evo`-style fields:

- `ethernet.enabled`
- `ethernet.device` (alias: `ethernet.ifname`)
- `ethernet.ipv4_mode` (`dhcp` or `static`, alias `manual` accepted)
- `ethernet.ipv4_address`, `ethernet.ipv4_gateway`, `ethernet.ipv4_dns[]`
- `wifi.ifname`, `wifi.role` (`sta|ap|disabled`)
- `wifi.sta_ssid`, `wifi.sta_open`
- `wifi.sta_ipv4_mode`, `wifi.sta_ipv4_address`, `wifi.sta_ipv4_gateway`, `wifi.sta_ipv4_dns[]`
- `wifi.sta_selection_mode` (`legacy|auto_stable|auto_performance|prefer_band|lock_bssid`)
- `wifi.sta_preferred_band` (`2.4ghz|5ghz|6ghz`) for `prefer_band`
- `wifi.sta_lock_bssid` (AA:BB:CC:DD:EE:FF)
- `wifi.ap_ssid`, `wifi.ap_channel`, `wifi.ap_band` (`bg|a|6GHz`)
- `fallback.hotspot_enabled`, `fallback.hotspot_connection_name`
- `fallback.hotspot_ifname`, `fallback.hotspot_fallback`

## Captive reliability policy config

Plugin config accepts optional reliability controls (either top-level or under `[captive]`):

- `credential_policy`:
  - `replay_allowed` (default)
  - `single_use_ticket`
  - `manual_after_failure`
- `retry_budget` (default `2`, minimum `1`)
- `replay_window_sec` (default `900`, minimum `1`)

These controls govern how aggressively the plugin can replay captive credentials
after drop/reboot/failure.

## Durability naming convention

Following steward conventions, this plugin uses:

- primary file (for example `network-intent.toml`)
- LKG shadow (`network-intent.lkg.toml`)
- atomic temp suffix on writes (`network-intent.toml.tmp`)

This mirrors the steward catalogue pattern (`catalogue.lkg.toml`, `<file>.tmp`) rather
than a `.bak` suffix.

State files are persisted in versioned envelopes with `schema_version`:

- `network-intent.toml` -> `{ schema_version, intent }`
- `captive-session.json` -> `{ schema_version, state }`

Legacy flat payloads are still accepted on load and migrated in-memory, then the
next save rewrites canonical envelope format.

### Secret at-rest hardening

Wi-Fi PSK files (`wifi-sta.psk`, `wifi-ap.psk`) support encrypted-at-rest storage:

- Set `EVO_NETWORK_SECRET_KEY` to enable encryption with `xchacha20poly1305`.
- Set `EVO_NETWORK_SECRET_REQUIRE=1` (or plugin config `secrets.require_encrypted = true`)
  to reject plaintext secret files.

Without a key, plugin behavior remains backward compatible (plaintext) unless
`require_encrypted` is enabled.

## Operational scenario coverage

Current apply/reconcile logic covers these branches:

- `Wi-Fi disabled`: bring down STA and hotspot profiles (best effort).
- `STA with hotspot disabled`: enforce STA profile and connectivity.
- `STA with hotspot enabled (same radio)`: retry AP bring-up and restore STA as needed.
- `STA with hotspot enabled (split iface)`: run STA and hotspot on distinct ifaces.
- `STA with hotspot enabled (concurrent vif)`: create AP vif (`ap0` or override), follow STA channel.
- `AP role`: enforce AP/hotspot profile as primary mode.
- `No LAN carrier + hotspot failure`: critical open-hotspot recovery path.
- `LAN carrier present`: prefer restoring STA on single-radio displacement.

## UI notifier contract (recommended)

The plugin returns structured state in responses; UI should surface toast/status
notifications so operators understand network transitions and reliability guards.

Suggested mapping:

- `info`
  - captive probe started
  - retry budget reset after replay window
- `success`
  - connected and authenticated (`captive.phase = authenticated`)
  - intent apply succeeded (`apply.ok = true`)
- `warning`
  - retry budget reached (`captive.requires_user_confirmation = true`)
  - hotspot recovery path activated
  - concurrent/split-radio fallback decisions
- `error`
  - apply failed (`apply.ok = false`)
  - captive submit failed (`captive.phase = failed`)
  - parse/fallback degradation (primary state invalid, LKG used)

Minimum UI fields to show in toast/details:

- current operation (`scan`, `apply`, `captive_submit`, `captive_probe`)
- phase/result
- human message (`last_error` or final step)
- whether explicit operator confirmation is required

Implementation note: captive/apply responses expose a machine-readable
`notices` array with `{level, code, message}` entries so UI can map toast
rendering without parsing freeform text. Stable code list:
`docs/NOTICE_CODES.md`.

Captive responses also expose `actions` for explicit UI affordances. Current IDs:

- `captive.start_probe`
- `captive.confirm_replay` (present when guarded replay confirmation is required)
- `captive.mark_complete_failed` (present during failed captive state)

## Observability contract

All successful `network.nm.*` responses include:

- `observability.request_type`
- `observability.correlation_id`
- `observability.requests_handled`
- `observability.secret_encryption`
- `observability.secret_encryption_required`

These are intended for operator telemetry stitching (API, steward logs, UI traces)
without parsing free-form step strings.

`network.nm.status` additionally exposes failure-domain fields:

- `degraded` (`true` when one or more backend checks fail)
- `domain_health.device_table`
- `domain_health.general_status`
- `domain_health.wifi_scan`

This keeps the status endpoint responsive even when one subsystem is degraded.

## Minimal request examples

### `network.nm.intent.set`

```json
{
  "intent": {
    "version": 1,
    "ethernet": { "enabled": true, "ipv4_mode": "dhcp" },
    "wifi": { "ifname": "wlan1", "role": "sta", "sta_ssid": "HotelWiFi", "sta_open": false },
    "fallback": {
      "hotspot_enabled": true,
      "hotspot_connection_name": "volumio-hotspot",
      "hotspot_ifname": "wlan0",
      "hotspot_fallback": true
    }
  },
  "sta_psk": "AbC123xY",
  "apply": true
}
```

### `network.nm.captive.start`

```json
{
  "url": "http://connectivitycheck.gstatic.com/generate_204"
}
```

### `network.nm.scan` response notes

`network.nm.scan` now returns:

- `available`: de-duplicated SSID view (UI list)
- `candidates`: BSSID-level records (`ssid`, `bssid`, `signal_pct`, `freq_mhz`, `band`, `active`)

Use `candidates` when presenting advanced roaming/debug screens or when allowing
operators to pin `sta_lock_bssid`.

### `network.nm.captive.submit`

```json
{
  "url": "http://portal.example/login",
  "method": "POST",
  "form": {
    "guestName": "McAllen",
    "roomNumber": "A10",
    "accessCode": "AbC123xY"
  },
  "confirm_replay": false
}
```

Set `confirm_replay=true` only when operator/UI confirms it is safe to retry
the same credential set (important for single-use or uncertain ticket semantics).
