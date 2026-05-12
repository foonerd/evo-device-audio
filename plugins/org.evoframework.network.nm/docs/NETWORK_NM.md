# `org.evoframework.network.nm`

NetworkManager-backed link-control plugin for the evo framework.
Owns the `networking.link` shelf at shape 1; single-claimant per
device. The plugin is the operator-facing surface for network
intent (Ethernet + Wi-Fi STA + Wi-Fi AP + fallback hotspot +
per-radio policy), the privileged-binary dispatch layer
(`nmcli` + `iw` + `rfkill` + `curl`), and the runtime supervisor
that watches reachability and autonomously raises a critical-
recovery hotspot when the device loses every uplink.

This document is the operational contract. Updates to behaviour
land here in the same commit.

## Contents

- [Intent model](#intent-model)
- [Apply pipeline](#apply-pipeline)
- [Multi-radio role assignment](#multi-radio-role-assignment)
- [Privileged execution](#privileged-execution)
- [Runtime supervisor](#runtime-supervisor)
- [Wire-op surface](#wire-op-surface)
- [Reactive events](#reactive-events)
- [Configuration](#configuration)
- [Persistence](#persistence)
- [Captive portal handling](#captive-portal-handling)

## Intent model

Operator-declared network shape persists in `intent.toml` under
the plugin's state directory. Schema version `2`. Sections:

```toml
schema_version = 2

[intent]
version = 1

  [intent.ethernet]
  enabled = true
  device = ""              # empty: first ethernet device NM reports
  ipv4_mode = "dhcp"       # "dhcp" or "static"
  ipv4_address = ""        # CIDR when static, e.g. "192.168.1.10/24"
  ipv4_gateway = ""
  ipv4_dns = []

  [intent.wifi]
  ifname = "wlan0"
  role = "sta"             # "sta" | "ap" | "disabled"
  sta_ssid = ""
  sta_open = false
  sta_ipv4_mode = "dhcp"
  sta_ipv4_address = ""
  sta_ipv4_gateway = ""
  sta_ipv4_dns = []
  sta_selection_mode = "auto_stable"
  # "legacy" | "auto_stable" | "auto_performance"
  # | "prefer_band" | "lock_bssid"
  sta_preferred_band = ""  # "2.4ghz" | "5ghz" | "6ghz"
  sta_lock_bssid = ""      # used when sta_selection_mode = "lock_bssid"
  ap_ssid = ""             # derived from MAC suffix when empty
  ap_channel = 4
  ap_band = ""             # "bg" | "a" | "6GHz" (NM spelling)

  [intent.fallback]
  hotspot_enabled = true
  hotspot_connection_name = "evo-network-hotspot"
  hotspot_ifname = ""      # empty: same iface as STA (single-radio)
  hotspot_fallback = false

  [intent.radio_policy]
  flight_mode = false
  wifi_enabled_pref = true
  bluetooth_enabled_pref = true
  band_priority = ["6ghz", "5ghz", "2.4ghz"]
```

`radio_policy.flight_mode` is the device-wide kill that takes
every wireless family offline. `wifi_enabled_pref` /
`bluetooth_enabled_pref` are operator preferences applied when
flight mode is off and the kernel's `rfkill` is not blocking.
`band_priority` ranks bands for STA traffic assignment when more
than one Wi-Fi PHY is present (see
[Multi-radio role assignment](#multi-radio-role-assignment)).

PSK sidecars live next to the intent file:

- `wifi-sta.psk` — STA passphrase (`0o600`)
- `wifi-ap.psk` — AP passphrase (`0o600`)

Sidecars may be plaintext or XChaCha20-Poly1305 envelopes when
`EVO_NETWORK_SECRET_KEY` is configured (see
[Configuration](#configuration)). The
`network.nm.security.harden` verb migrates plaintext sidecars to
encrypted form and (optionally) flips the runtime require-flag.

Legacy `flight-mode.toml` and `wifi-preference.toml` state files
are subsumed by `intent.radio_policy`. On first plugin load at
schema version 2, the plugin reads any legacy files present,
folds their `enabled` bits into `radio_policy`, persists the
unified intent, and deletes the legacy files.

## Apply pipeline

`network.nm.intent.apply` exercises the apply orchestrator. The
pipeline serialises through an `NM_APPLY_LOCK` (one apply at a
time per plugin instance) and follows this sequence:

1. **Inventory + role assignment** (see
   [Multi-radio role assignment](#multi-radio-role-assignment))
   resolves `sta_ifname` + `ap_ifname` from the intent's explicit
   overrides + the live `iw dev` inventory + `radio_policy.band_priority`.
2. **Ethernet preflight**: `ensure_ethernet` modifies or creates
   the `evo-network-ethernet` profile per `intent.ethernet`. When
   `enabled = false`, the profile is brought down and skipped.
3. **Radio-block guard**: if `radio_policy.flight_mode` is on, or
   the kernel reports `rfkill` blocked, the Wi-Fi branch refuses
   with a structured warning. Operator must clear the block.
4. **Wi-Fi role branch**:
   - `Disabled` — bring STA and hotspot down; remove the virtual
     AP vif if it exists on a shared PHY.
   - `Sta` — pre-tear hotspot + STA profile (avoids brcmfmac AP
     channel pinning); attempt STA association; if PHY supports
     concurrent `managed + AP`, create the `ap0` virtual vif via
     `iw dev <sta> interface add ap0 type __ap`; adopt the STA's
     channel + band for the AP when same-PHY; ensure the hotspot
     profile; attempt `connection up` with retries; on retry
     exhaustion attempt critical recovery (see below);
     restore-after-hotspot on shared radio.
   - `Ap` — bring STA down; ensure the hotspot profile on the AP
     interface.
5. **Hotspot connection up with retries**: bounded retry loop
   around `nmcli connection up <hotspot>`. On exhaustion + no
   Ethernet carrier, the open critical-recovery path runs.
6. **Critical open hotspot recovery**: when AP `connection up`
   fails AND `intent.ethernet.enabled && !ethernet_carrier_up`,
   the apply pipeline strips `802-11-wireless-security` from the
   hotspot profile (open SSID) and forces it up. Operator can
   reach the device on the open SSID and resolve the underlying
   issue.

Every step appends a human-readable line to the apply report's
`steps` array. The report is returned on the wire-op response
and surfaces in the audit ledger via observability.

## Multi-radio role assignment

Devices may expose multiple Wi-Fi radios (on-SoC + PCIe + USB).
The plugin treats the inventory as the source of truth and
assigns roles based on the operator's intent plus PHY
capability.

Inventory rows (`WifiRadio`) are built from `iw dev` + per-PHY
`iw phy info` parsing. Each row carries:

- `ifname` (kernel netdev) + `phy` (backing PHY)
- `capability` — `supports_managed_plus_ap`, `max_channels`,
  supported interface modes (`managed`, `AP`, `IBSS`, …), per-band
  flags (2.4 / 5 / 6 GHz)
- `connection_class` — `onboard` or `usb` (sysfs
  `/sys/class/net/<if>/device/uevent` `DEVTYPE=usb_*`)
- `is_ap_vif` — `true` for rows that are virtual AP vifs created
  via `iw … type __ap`; excluded from the STA candidate pool

Role assignment (`wifi_roles::assign_wifi_roles`) resolves
`sta_ifname` + `ap_ifname` + `same_iface` flag + `had_explicit_ap`:

- **Explicit overrides** win: `intent.wifi.ifname` pins STA;
  `intent.fallback.hotspot_ifname` pins AP. Both pinned at once
  yields a split-radio assignment with no further inference.
- **Auto-pick STA**: highest-priority band (per
  `radio_policy.band_priority`) that any PHY supports drives the
  score; ties broken by connection class (onboard > USB for
  traffic-bearing stability) and by multi-band PHY flexibility
  (a tri-band PHY beats a single-band one on the same top band).
- **Auto-pick AP**: a different AP-capable radio when one
  exists; otherwise share the STA radio.

Downstream apply-pipeline layer applies the **concurrent-vif
promotion**: when `same_iface == true` AND the PHY supports
concurrent `managed + AP` AND the operator did not explicitly
pin AP, the AP ifname is promoted from `wlan0` to `ap0` (env
overridable via `EVO_NETWORK_AP_IFNAME`). The vif is created via
`iw dev <sta> interface add ap0 type __ap`. AP channel follows
STA on same-PHY because the kernel's `valid interface
combinations` reports `#channels <= 1`.

## Privileged execution

The plugin reaches the host through four privileged binaries:

| Tool | Used for |
|------|----------|
| `nmcli` | NetworkManager state — profiles, radio, connectivity, scan, captive helpers |
| `iw` | PHY capability detection + virtual AP vif lifecycle + STA link info |
| `rfkill` | Hardware/software radio block toggle (Wi-Fi + Bluetooth coordination under flight mode) |
| `curl` | Connectivity probes (RFC 8910 / HTTP 204) + captive-portal submission |

Each tool has a per-instance privilege dispatcher
(`AutoPrivilegedExec`) resolved at `Plugin::load` from the
framework's preflight result. Under root, the dispatcher
exec's the binary directly. Under a non-root service identity,
it dispatches `sudo -n <bin> ...` against narrow NOPASSWD
sudoers drop-ins the distribution's bootstrap ships.

The framework's Privilege Preflight Admission Gate (PPAG) probes
each tool independently against four capability intents:

- `nmcli_invocation`
- `iw_invocation`
- `rfkill_invocation`
- `curl_invocation`

The plugin's capabilities-watch reactor subscribes to PPAG's
re-probe channel; when the framework publishes a strategy
change (sudoers drop-in removed mid-operation, etc.), every
per-tool dispatcher re-resolves in lockstep without
re-admission. Operators see the rationale in the plugin log
line.

`privileges.yaml` declares the four capability intents plus
`required_binaries` entries with min-version guidance. `iw` and
`nmcli` are required; `rfkill` and `curl` degrade gracefully
when absent (flight-mode falls back to nmcli-only Wi-Fi toggle;
connectivity probes report `unknown`).

## Runtime supervisor

A background task spawned at `Plugin::load` watches link state
and publishes a reachability classification on a
`tokio::sync::watch` channel. Default cadence 10 s (overridable
via `EVO_NETWORK_SUPERVISOR_INTERVAL_MS`).

### Observations

Each tick composes a `SupervisorObservations` snapshot:

- `nm_connectivity` — `nmcli general connectivity` verdict
- `probe_http_code` + `probe_effective_url` — RFC-8910-style
  `curl` probe against `EVO_NETWORK_SUPERVISOR_PROBE_URL`
  (default `http://connectivitycheck.gstatic.com/generate_204`)
- `ethernet_carrier_up` — sysfs `/sys/class/net/<if>/carrier`
  across all non-`lo` non-`wl*` netdevs
- `wifi_associated` — `iw dev <managed-if> link` status across
  every managed-mode Wi-Fi netdev

Best-effort: a failed probe surfaces as `None` / `false` and the
tick continues.

### State machine

`classify_reachability` reduces the observations to one of five
states:

| State | Meaning |
|-------|---------|
| `Unknown` | No probe outcome yet (supervisor just started) |
| `Online` | Gateway reachable AND probe returned 204 |
| `Portal` | Gateway reachable AND probe redirected to a portal |
| `Limited` | Gateway reachable AND probe returned non-204 non-redirect |
| `Offline` | No usable uplink (no Wi-Fi association + no Ethernet carrier) |

`step(prev, obs, config)` advances the published view, counts
consecutive ticks in the current state, records portal info on
entry to `Portal`, and decides whether to trigger autonomous
recovery.

### Autonomous recovery

Two grace windows gate the recovery actions:

- `EVO_NETWORK_SUPERVISOR_CRITICAL_GRACE_MS` — default 30 000 ms.
  Once `Offline` persists for this long, the supervisor invokes
  `NmInner::autonomous_critical_recovery`: load the persisted
  intent, derive the hotspot connection name, force an open AP
  up. An operator can reach the device on the recovery hotspot.
- `EVO_NETWORK_SUPERVISOR_RESTORE_GRACE_MS` — default 15 000 ms.
  Once reachability returns to `Online` / `Limited` for this
  long after a recovery was active, the supervisor invokes
  `NmInner::autonomous_sta_restore`: load the persisted intent
  plus PSK sidecars, replay the apply pipeline. Operator's
  declared STA shape comes back.

Both actions emit log lines and reactive events (see
[Reactive events](#reactive-events)).

## Multi-source event-driven substrate

The supervisor is built on a `LinkEventSource` trait
abstraction: a stream of typed `LinkEvent`s drives the
supervisor's wakes, and each wake runs the existing
compose-observations → state-machine → publish pipeline
unchanged. Today's `spawn` mounts a single polling source as
the universal correctness floor; vendor distributions opting
into the multi-source mode mount additional typed sources
alongside the polling floor.

### Source set

Three event sources ship in the workspace:

- **Polling.** The universal correctness floor. Wakes every
  `interval_ms` (default 10 000 ms; bounded below by 5 000 ms
  per the adaptive-tick hard floor) regardless of what
  changed on the device. Cgf-free, no dependencies, every
  platform that can host a shell can host the polling source.
- **rtnetlink.** Linux kernel netlink subscriber. Observes
  layer-2 carrier (RTMGRP_LINK) and layer-3 address attach /
  detach (RTMGRP_IPV4_IFADDR + RTMGRP_IPV6_IFADDR). Cargo
  feature `source-rtnetlink`, gated on `target_os = "linux"`.
- **NetworkManager D-Bus.** zbus subscriber to
  `org.freedesktop.NetworkManager`'s `PropertiesChanged`
  signal. Surfaces NM's `Connectivity` verdict + the
  `PrimaryConnection` family of activation-lifecycle
  changes. Cargo feature `source-nm`, gated on
  `target_os = "linux"`.

The supervisor's `spawn_with_sources(config, actions, sources)`
consumes any `Vec<Box<dyn LinkEventSource>>`; each source is
pumped by its own fan-in task and feeds a shared `mpsc`. The
compose-observations callback runs after every wake regardless
of which source produced the event — events are timing,
observations are data.

### Cross-source reconciliation

The `reconcile::RULES` static slice is the framework's
explicit rule table for catching userspace-daemon confusion
modes:

| Rule | Detects |
| --- | --- |
| `carrier_down_but_daemon_full` | Kernel reports no carrier but NM advertises Full / Portal / Limited. Trust the kernel; flag the daemon. |
| `carrier_up_but_daemon_none` | Kernel reports carrier-up + IP attached but NM reports None. Daemon is mid-converge. |
| `daemons_disagree` | Two userspace daemons report contradictory verdicts. Surface without preferring either. |

Each rule produces a typed `Discrepancy` that surfaces as a
`LinkSourceDiscrepancy` observation when the rule fires. The
rule table is *data*, not code — operators read the active
rules via `describe_capabilities` without source-code access.

### Per-source health monitoring

Every active source runs a `health_probe()` on its own cadence
(default 60 s). A source that fails its probe transitions to
`SourceAdmissionState::Demoted` with an
exponentially-backed-off `next_attempt_at`. Demoted sources
no longer contribute events; the polling floor + remaining
sources carry the load. The backoff schedule climbs 30 s →
60 s → 2 min → 5 min → 15 min → 1 h, capped at 6 h. A
successful re-probe re-admits the source.

State transitions emit typed observations:
`LinkSourceDemoted { source, reason }` on demotion,
`LinkSourceAdmitted { source }` on re-admission.

### Adaptive safety tick

The polling source's tick interval is not a static config
constant. It is recomputed before every wake from the
supervisor's recent observations:

- Boot path → `tick_min` (default 10 s).
- Silence past `silence_threshold` (default 120 s) → shrink
  to `tick_min`.
- Otherwise → linear interpolation between `tick_min` and
  `tick_max` (default 5 min) driven by the
  healthy / active source ratio.

Hard floor: 5 000 ms. The framework refuses lower values at
config-validate time. Operators who genuinely need faster
cadence install another typed source rather than ratcheting
polling.

### Source-set presets

`presets::Preset` enumerates seven canonical presets:

| Preset | Source candidates |
| --- | --- |
| `linux-systemd-nm` | rtnetlink + NetworkManager + polling |
| `linux-systemd-networkd` | rtnetlink + (planned systemd-networkd) + polling |
| `linux-yocto-connman` | rtnetlink + (planned ConnMan) + polling |
| `linux-bare` | rtnetlink + polling |
| `bsd` | (planned devd) + polling |
| `polling-only` | polling alone |
| `embedded-rtos` | native platform source only |

`presets::default_preset()` selects per build target.
Operators override at config. `build_sources(preset,
polling_interval_ms)` is the async builder that walks the
preset's candidate list, attempts to mount each, and returns
the admitted sources alongside a per-candidate
`CandidateOutcome` (Admitted or Refused with diagnostic).
Per-source construction failure is never fatal — the polling
floor + whatever else admitted carries the load.

### Operational invariants

- The state machine (`classify_reachability` + `step`) is a
  pure function of `(SupervisorObservations, SupervisorView,
  SupervisorConfig)`. Multi-source code sits upstream of
  `step`, composing observations + detecting discrepancies +
  scoring source health. The state machine consumes the
  reconciled observation and decides recovery.
- The polling source is included in every shipping
  configuration on platforms where it is implementable.
  Removing it for the sake of "we have D-Bus, we don't need
  polling" recreates the silent-failure mode the design
  exists to eliminate.
- Source `health_probe` failures demote rather than fail the
  supervisor. A degraded supervisor running on the polling
  floor is correct; a crashed supervisor is not.
- Reconciliation rules are data. New rules append to the
  static `RULES` slice; the source code stays untouched.
- The `safety_tick_min_ms` hard floor of 5 000 ms is enforced
  at config-validate time.

## Wire-op surface

Single-claimant respondent on `networking.link`. Verbs:

| Request type | Read/write | Returns |
|--------------|------------|---------|
| `network.nm.status` | read | NM device table + active connections + connectivity + captive-portal phase + radio state |
| `network.nm.scan` | read | Wi-Fi scan rows + STA candidates + cache hit flag |
| `network.nm.intent.get` | read | Current persisted `NetworkIntent` + PSK-configured flags |
| `network.nm.intent.set` | write | Persists a new intent + optional PSKs; optional immediate apply |
| `network.nm.intent.apply` | write | Replays the apply pipeline; returns the steps + ok flag |
| `network.nm.captive.status` | read | Current captive-portal session state |
| `network.nm.captive.start` | write | Begin a captive-portal flow (probe + record portal URL) |
| `network.nm.captive.submit` | write | Submit credentials to the portal via curl POST |
| `network.nm.captive.complete` | write | Mark the captive session complete + verify connectivity |
| `network.nm.security.status` | read | Encryption state of the PSK sidecars + harden recommendation |
| `network.nm.security.harden` | write | Migrate plaintext sidecars to encrypted envelopes; optional runtime require flag |
| `network.nm.flight_mode.get` | read | Current flight-mode bit + radio state |
| `network.nm.flight_mode.set` | write | Flip flight-mode + push the Wi-Fi radio state through `nmcli` |
| `network.nm.radio.status` | read | Composed Wi-Fi radio view (preference + flight-mode + rfkill) |
| `network.nm.radio.set` | write | Operator Wi-Fi preference (enabled / disabled); attempts immediate apply when not flight-mode-blocked |
| `network.nm.supervisor.status` | read | Live supervisor view — reachability + last-observations + portal info + critical-recovery flag + state-tick count |
| `network.nm.wifi_devices` | read | Live multi-radio inventory — one row per kernel netdev with PHY capability + per-band support + connection class |

Per-verb payload contracts are documented in
`NETWORK_NM_REQUESTS_V1.md` (existing); the schema-repo
descriptor declares each verb with `payload_in = "tbd-review"`
pending the cross-plugin payload review.

## Reactive events

The supervisor's transition watcher emits typed
`Happening::PluginEvent` instances on the framework's durable
bus. Cross-plugin subscribers consume the same channel they
already subscribe to for everything else. Event types:

| Event type | Fires when | Payload |
|------------|-----------|---------|
| `network.reachability_changed` | Supervisor state transitions (any direction) | `{v, from, to, observations}` |
| `network.portal_detected` | Transition into `Portal` state | `{v, portal_url}` |
| `network.critical_recovery_raised` | Autonomous hotspot raise flips on | `{v, reachability, state_ticks}` |
| `network.sta_restored` | Autonomous STA restore completes | `{v, reachability, state_ticks}` |

Subscribers filter by `event_type` prefix `network.*`. A UI
client renders the reachability indicator and reacts to
`portal_detected` by surfacing a credential prompt; the
listening-plans engine can watch for `sta_restored` to resume
playback after a recovery; the audit ledger primitive sees every
critical-recovery raise / restore as a structured event.

## Configuration

`PluginConfig` parses from the per-plugin TOML config block.
Every field has an `EVO_NETWORK_*` env override at boot.

| TOML key | Env override | Default |
|----------|--------------|---------|
| `nmcli_path` | — | `/usr/bin/nmcli` |
| `iw_path` | `EVO_NETWORK_IW` | `/usr/sbin/iw` |
| `rfkill_path` | `EVO_NETWORK_RFKILL` | `/usr/sbin/rfkill` |
| `curl_path` | `EVO_NETWORK_CURL` | `/usr/bin/curl` |
| `wifi_iface` | — | `wlan0` |
| `nmcli_timeout_ms` | — | 8000 |
| `iw_timeout_ms` | `EVO_NETWORK_IW_TIMEOUT_MS` | 5000 |
| `rfkill_timeout_ms` | `EVO_NETWORK_RFKILL_TIMEOUT_MS` | 2000 |
| `curl_timeout_ms` | — | 30000 |
| `scan_cache_ttl_ms` | — | 3000 |

Captive-portal sub-config:

| TOML key | Default |
|----------|---------|
| `captive.credential_policy` | `replay_allowed` (`single_use_ticket` / `manual_after_failure` available) |
| `captive.retry_budget` | 3 |
| `captive.replay_window_sec` | 60 |

Supervisor sub-config (env-only):

| Env var | Default |
|---------|---------|
| `EVO_NETWORK_SUPERVISOR_INTERVAL_MS` | 10 000 |
| `EVO_NETWORK_SUPERVISOR_CRITICAL_GRACE_MS` | 30 000 |
| `EVO_NETWORK_SUPERVISOR_RESTORE_GRACE_MS` | 15 000 |
| `EVO_NETWORK_SUPERVISOR_PROBE_URL` | `http://connectivitycheck.gstatic.com/generate_204` |

Secret-encryption env vars:

| Env var | Effect |
|---------|--------|
| `EVO_NETWORK_SECRET_KEY` | Raw key material; SHA-256 derives the XChaCha20-Poly1305 key |
| `EVO_NETWORK_SECRET_REQUIRE` | `1` / `true` / `yes` / `on` — refuse to read plaintext sidecars |

Override for the auto-vif name in concurrent STA+AP mode:
`EVO_NETWORK_AP_IFNAME` (default `ap0`).

## Persistence

Files under the plugin's per-instance state directory
(`<state_dir>/`):

| File | Owner / format |
|------|----------------|
| `network-intent.toml` | The unified intent (schema v2 envelope) |
| `network-intent.lkg.toml` | Last-known-good shadow; primary parse failure falls back to this |
| `wifi-sta.psk` | STA PSK (plaintext or XChaCha20-Poly1305 envelope), `0o600` |
| `wifi-ap.psk` | AP PSK, same format |
| `captive-session.json` | Captive-portal session state + envelope |
| `captive-session.lkg.json` | LKG shadow for the captive state |

Writes are atomic (`tmp` + `rename`) and mirror to the LKG
shadow on success.

## Captive portal handling

`captive_detect` runs the connectivity probe (`curl` against the
configured probe URL) and classifies the outcome:

- HTTP 204 + same `url_effective` → online; clear any captive
  session state.
- Non-204 OR `url_effective` changed → portal detected; record
  the portal URL in the captive session state.

`captive_start` initiates the operator-facing flow. UI surfaces
the portal URL through the reactive event
(`network.portal_detected`) or polls
`network.nm.captive.status`. `captive_submit` posts credentials
to the portal via `curl --data-urlencode`. The credential
replay policy (per `PluginConfig.captive.credential_policy`)
gates re-submission of identical form payloads:

- `replay_allowed` — default; identical resubmits are fine.
- `single_use_ticket` — operator must explicitly
  `confirm_replay` after the first submission.
- `manual_after_failure` — operator must `confirm_replay` after
  a failed submission.

`captive_complete` marks the session terminal and re-probes
connectivity.

See `CAPTIVE_PORTAL_WORKFLOW.md` for the request/response
sequence operators consume.

## Notice codes

Diagnostic notices the plugin emits on wire responses are
catalogued in `NOTICE_CODES.md`. Operators reading the apply
report's `steps` field see structured codes for the load-bearing
states (radio blocked, hotspot retry exhaustion, captive
authentication required, etc.).

## Operator runbook

For step-by-step operational procedures — provisioning a fresh
device, recovering from a misconfigured intent, migrating PSK
sidecars to encrypted form — see `NETWORK_NM_RUNBOOK.md`.
