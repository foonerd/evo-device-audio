# Developing evo-device-audio

Contributor workflow for the audio-domain plugin commons.

## Related docs

-   [README.md](README.md) - landing page. Three-tier model, scope, namespace, trust posture.
-   [foonerd/evo-core docs/engineering/](https://github.com/foonerd/evo-core/tree/main/docs/engineering) - framework engineering layer (CONCEPT, BOUNDARY, CATALOGUE, SCHEMAS, PLUGIN_AUTHORING, PLUGIN_PACKAGING, VENDOR_CONTRACT). Read these for the contracts plugins implement.

## Prerequisites

-   Rust **1.85** or newer, matching the workspace `rust-version` (same MSRV as evo-core).
-   Network access for the SDK pin. `[workspace.dependencies]` resolves `evo-plugin-sdk` from `git = "https://github.com/foonerd/evo-core.git"` at `tag = "v0.1.9"`. No sibling clone required.
-   Cross-compile prerequisites if building for `aarch64-unknown-linux-gnu` locally: Docker, [`cross`](https://github.com/cross-rs/cross).

## Workspace conventions

Mirrors evo-core; any deviation is deliberate.

-   `#![forbid(unsafe_code)]` and `#![warn(missing_docs)]` as workspace lints.
-   `clippy::manual_async_fn` allowed at workspace level (see comment in `Cargo.toml`); the SDK contract uses `impl Future + Send + '_` rather than `async fn` in trait position.
-   Native async traits for plugin code, matching the SDK.
-   One pin for `evo-plugin-sdk` in `[workspace.dependencies]`. Plugin crates consume it via `evo-plugin-sdk = { workspace = true }`. There is exactly one place to change the version.
-   Shared crate metadata in `[workspace.package]`. Plugin crates set `package = { workspace = true }` and override only what they must.
-   Conventional-commit messages. Same style as evo-core.
-   Pre-1.0 versioning: patch for incremental work (including internal breaking changes), minor for public-surface breaking changes, major for milestones. Docs-only changes do not bump.
-   ASCII-only in source files and docs unless there is a concrete reason otherwise. No smart quotes, em dashes, or other non-ASCII punctuation.

## Build and test

From the workspace root:

```
cargo build --workspace
cargo test --workspace
```

Both must be green before any version bump. In Phase 1 scaffolding state the workspace contains only the `evo-device-audio-shared` anchor crate (an empty library that future plugins will share utilities through); `build` and `test` succeed trivially until plugin crates land.

## GitHub Actions

Workflows under [`.github/workflows/`](.github/workflows/):

-   **build** - on every `pull_request` and `push`: `cargo fmt`, `clippy` (`-D warnings`), `cargo test --workspace`. The SDK is fetched directly from the git tag; no sibling evo-core checkout.
-   **continuous-dev** - on `push` to `main` when code, CI, keys, or build config change: same checks, then `cross build` for `aarch64-unknown-linux-gnu` (when there are members), then optional `evo-plugin-tool` sign/verify against an OOP sample bundle (when one is present in `ci/oob-sign-smoke/`). Publishing to the artefacts repository is not wired yet.
-   **manual-build** - `workflow_dispatch` with a git `ref` and a `channel` input (for logging; same publish gap as above).
-   **promote** - placeholder for channel pointer moves on the artefacts repo (no rebuild).

## Repository secret PLUGIN_SIGNING_KEY_PEM

PKCS#8 PEM for the **private** key that pairs with the public key in [`keys/commons-plugin-signing-public.pem`](keys/commons-plugin-signing-public.pem) and its [`keys/commons-plugin-signing-public.meta.toml`](keys/commons-plugin-signing-public.meta.toml) sidecar.

When set, the continuous-dev and manual-build workflows sign and verify the OOP sign-smoke bundle. When unset, the sign step is skipped and CI remains green - the secret is required only for actually exercising the signing pipeline, not for build/test.

The private key never leaves the GitHub Actions runner. The public key fingerprint (SHA256 of the DER-encoded SubjectPublicKeyInfo) is recorded in the meta sidecar for verification on key rotation.

## Adding a new plugin

1.  Create `plugins/<full.dotted.name>/` (e.g. `plugins/org.evoframework.playback.mpd/`). The directory name matches the plugin's manifest name; this convention is shared with evo distribution repositories so plugins resolve by name on the filesystem directly.
2.  Add `Cargo.toml` with `name` set to the dotted name with dots replaced by hyphens (e.g. `org-evoframework-playback-mpd`) and `package = { workspace = true }` for shared metadata.
3.  Add `manifest.toml` with `name` set to the dotted form matching the directory name (e.g. `org.evoframework.playback.mpd`). The reverse-DNS namespace prefix is reserved for the plugin commons; do not publish under any other prefix from this repo.
4.  Add the new path to `[workspace].members` in the root `Cargo.toml`.
5.  Implement against the SDK trait that matches the slot the plugin will stock. See evo-core's [`PLUGIN_AUTHORING.md`](https://github.com/foonerd/evo-core/blob/main/docs/engineering/PLUGIN_AUTHORING.md).
6.  If the plugin needs utilities shared with other plugins (path normalisation, library scanning, common error types), depend on `evo-device-audio-shared = { workspace = true }` and add the helper to that crate. Do not duplicate across plugins.
7.  `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` all green before commit.

## Boundary discipline

This repository holds plugins, and only plugins.

It does not hold:

-   Catalogues. Authored by distributions.
-   Branding (product names, logos, colour palettes, splash screens).
-   Frontends or web UIs.
-   Vocabulary contracts (subject types, relation predicates) as separately-pinned things. Plugins encode the vocabulary their slot contracts require; the contract for "what subject types exist" lives in distribution catalogues.
-   Anything brand-specific. If a plugin needs vendor knowledge to implement, it belongs in `evo-device-<vendor>` instead.

If a change here seems to require modifying evo-core, re-read evo-core's `docs/engineering/BOUNDARY.md` section 5. The usual answer is "the contract it speaks is already declared in evo-core". If you genuinely find an evo-core gap, open an issue on `foonerd/evo-core`.

## Framework non-enforcement boundary

evo-core enforces the manifest contract surface that a userspace Rust process can apply portably across every supported OS family (Debian, Yocto, FreeBSD, macOS, Android AOSP, Buildroot, Alpine). It explicitly does NOT enforce the contract surface that requires kernel-level primitives, privilege escalation, or OS-specific orchestration. That second half is distribution-owned.

The split is documented normatively in `evo-core/docs/engineering/PLUGIN_PACKAGING.md` section 2 ("Enforcement scope"). The summary, restated here so plugin authors targeting the audio domain see the boundary at the right place:

**Enforced by evo-core (portable, every OS):**

-   `manifest.kind.interaction` (respondent / warden) — admission gate.
-   `manifest.target.shape` and `manifest.target.shape_supports` — admission gate.
-   `manifest.capabilities.respondent.request_types` — dispatch-time refusal of undeclared verbs.
-   `manifest.capabilities.respondent.response_budget_ms` — default request deadline.
-   `manifest.capabilities.warden.course_correction_budget_ms` and `custody_failure_mode` — warden dispatch enforcement.
-   `manifest.prerequisites.evo_min_version` and `os_family` — admission gate.
-   `trust.class` — admission gate; opt-in `[plugins.security]` UID/GID mapping for OOP spawns.

**NOT enforced by evo-core; distribution-owned:**

-   **OS-level isolation primitives** beyond `setuid` / `setgid`: seccomp profiles, Linux capability bounding sets, network namespaces, mount namespaces, SELinux / AppArmor / Smack policy, Android sandbox profiles, FreeBSD jails, macOS sandbox-exec profiles. The framework does not write to `/proc/self/seccomp_filter`, does not call `unshare(2)`, does not set `CAP_*` bits on its child processes. The distribution layer applies these.
-   **Manifest resource limits** for `prerequisites.outbound_network`, `prerequisites.filesystem_scopes`, `resources.max_memory_mb`, `resources.max_cpu_percent`. The framework parses these fields, preserves them on the manifest, and exposes them to the distribution's deployment tooling. It does not enforce them. The distribution layer applies cgroup limits, network sandboxing, and filesystem scope restrictions.

**What this means for an audio plugin author authoring against evo-device-audio:**

When you declare `resources.max_memory_mb = 64` in your plugin's manifest, evo-core will not kill your plugin if it allocates 1 GiB. The distribution that ships your plugin (the device that admits it) must apply a `MemoryMax=64M` (or equivalent) on the OOP plugin's cgroup. Your manifest is the contract; the distribution is the enforcement mechanism.

When you declare `prerequisites.outbound_network = "denied"`, evo-core will not block your plugin's `connect(2)` calls. The distribution must apply `RestrictAddressFamilies=AF_UNIX` (or equivalent) on the OOP plugin's systemd unit. Same contract; same split.

Plugins **MUST NOT** assume the distribution applies these enforcements. A plugin that crashes or misbehaves when a distribution chooses not to apply (e.g.) memory limits is the plugin author's bug, not the distribution's. Author plugins that respect their own declared limits voluntarily; the manifest is documentation for the distribution operator, not a sandbox the framework provides.

**What the audio reference distribution's deployment tooling implements:**

Audio-domain distributions building on evo-device-audio inherit this split. The reference systemd unit shipped at `evo-core/dist/systemd/evo.service.example` carries baseline hardening (`ProtectSystem=strict`, `ProtectHome=true`, `PrivateTmp=true`, `NoNewPrivileges=true`) for the steward process itself. Per-OOP-plugin hardening is the *distribution*'s responsibility — the steward spawns OOP plugins with the configured `[plugins.security]` UID/GID and inherits the steward's namespace; further restriction (per-plugin `MemoryMax=`, `RestrictAddressFamilies=`, etc.) is applied by whatever per-plugin systemd drop-in or cgroup orchestration the distribution authors. Vendor distributions targeting audio devices (e.g. `evo-device-volumio`) name the concrete primitives they apply in their own `DEVELOPING.md`.

If you author an audio plugin that materially depends on a specific enforcement (e.g. a metadata fetcher that requires no outbound network), document the dependency in your plugin's README and refuse to load with a clear error if the distribution has not granted the expected sandbox. The framework's `LoadContext` exposes the operator config; consult it at load and fail-fast rather than running degraded.

### Triage: what an audio distribution must assess

Every item below is **either** explicitly out of scope for evo-core (distribution-owned) **or** scheduled to land in a future evo-core release (distribution chooses whether/how to consume). Vendor distributions building on evo-device-audio walk this list per release and record their stance in their own `DEVELOPING.md`. Items left blank are unassessed; the canonical posture for an audio distribution is given so vendors have a starting point.

| # | Concern | Framework state | Reference audio-distribution posture |
| - | --- | --- | --- |
| 1 | Kernel-level sandboxing (seccomp, capabilities, namespaces, LSM) | Out of scope; distribution-owned | Apply per-trust-class systemd hardening (`SystemCallFilter=`, `CapabilityBoundingSet=`, `RestrictAddressFamilies=`, `PrivateNetwork=` for `Sandbox` trust). LSM profiles (AppArmor/SELinux) optional per-vendor. |
| 2 | Resource limits (`max_memory_mb`, `max_cpu_percent`) | Parsed, not enforced; distribution-owned | Apply manifest-derived `MemoryMax=` / `CPUQuota=` on per-plugin cgroup via systemd drop-in. |
| 3 | Network sandboxing (`outbound_network` manifest) | Parsed, not enforced; distribution-owned | Map `denied` → `RestrictAddressFamilies=AF_UNIX` on per-plugin drop-in. |
| 4 | Filesystem scopes (`filesystem_scopes` manifest) | Parsed, not enforced; distribution-owned | Map to `ReadWritePaths=` per plugin; rely on `ProtectSystem=strict` for the negative. |
| 5 | Empty-catalogue refusal at startup | Permanently out of scope (framework starts; logs the situation) | Optional packaging-time gate (postinst refuses install of an empty catalogue). Default: accept the framework's "starts anyway" behaviour. |
| 6 | Plugins administration operator verbs (enable / disable / uninstall / purge) | Implemented in evo-core v0.1.12 | Audio distribution decides whether its frontend surfaces these verbs as operator-facing controls. |
| 7 | Flight mode for hardware radios (Bluetooth / WiFi / FM / cellular) | Implemented in evo-core v0.1.12: framework provides signal bus + no-panic invariant; the device plugin owns the hardware switch | Audio distribution authors a per-distribution hardware-control plugin if its target devices ship controllable radios. Consumer plugins (streaming, library scanners) honour the framework's no-panic invariant on dependency loss. |
| 8 | User Interaction Routing (auth flow, credential prompts, etc.) | Implemented in evo-core v0.1.12 | Audio distribution authors a prompt-receiver surface (kiosk UI, frontend modal, remote bridge) if any admitted plugin issues `request_user_interaction`. |
| 9 | Appointments rack (time-driven plugins) | Implemented in evo-core v0.1.12 | Audio distribution decides whether it admits time-driven audio plugins (alarms, scheduled playback). |
| 10 | Watches rack (condition-driven plugins) | Implemented in evo-core v0.1.12 | Audio distribution authors sensor / hardware-event plugins (CEC ARC detection, BT peer manager, jack-insertion handler, USB DAC enumerator, CPU temp reader, motion sensor, etc.) per its target hardware; the framework provides the watch primitive that subscribes to whatever events the plugins emit. Audio-path-switching (BT peer connect / HDMI ARC active / headphone jack / USB DAC plug → switch output) is the canonical audio-domain use case. See "Watches — implications for plugins and UI" section below. |
| 11 | Fast Path (latency-bounded wire channel) | Implemented in evo-core v0.1.12 | Audio distribution decides whether its frontends use Fast Path for transport ops (volume, pause, seek). |
| 12 | Steward Reconciliation Loop (compose-and-apply) | Implemented in evo-core v0.1.12 | Audio distribution gains the orchestration surface for `composition.alsa` → `delivery.alsa` flows. Distribution decides which composer/delivery pairs it admits. |
| 13 | Catalogue corruption resilience | Implemented in evo-core v0.1.12 | Audio distribution inherits the LKG + built-in fallback transparently. Distribution may pre-seed `catalogue.lkg.toml` from its own packaging if it wants to control the recovery state. |
| 14 | CBOR codec on the wire | Implemented in evo-core v0.1.12 | Audio distribution decides whether its frontends prefer CBOR over JSON. JSON remains supported. |
| 15 | Hot-reload `Live` mode | Implemented in evo-core v0.1.12 — applies to both in-process and OOP plugins via the same SDK contract (`prepare_for_live_reload` + `load_with_state` callbacks); in-process sequential callback for state-preserving re-init; OOP cross-process state transfer for schema migration recovery | Audio distribution: in-process plugins opt in to Live for catalogue / config / runtime-context reload without dropping admission slot. OOP plugins use Restart by default (cold reload), Live for schema migrations between versions. Hardware-bound state preservation (ALSA pipeline, kernel-bound resources) uses the warden-architecture-pattern (resource owner separate from reloadable plugin code) — not framework-supported as a primitive. See "Hot reload — Live mode authoring" subsection below. |
| 16 | Happenings coalescing (per-subscriber rate limit) | Implemented in evo-core v0.1.12 — label-based keying via `CoalesceLabels` trait + derive macro; subscribers declare label lists on `subscribe_happenings.coalesce`; `describe_capabilities` advertises per-variant label sets; new `Happening::PluginEvent` variant lets sensor / hardware-event plugins emit structured events that participate in coalescing | Audio distribution: consumers (frontend, voice agent, MQTT bridge) declare coalesce label lists per subscription based on their use case (per-handle for custody bursts, per-subject for cross-variant collapse, per-watch / per-appointment / per-pair for the v0.1.12 trigger primitives). Sensor and hardware-event plugins emit through `PluginEvent` with structured payloads; coalescing keys on flattened payload fields like `sensor_id`. See "Happenings coalescing" subsection below for guidance. |
| 17 | Subject-grammar orphan migration verb | Implemented in evo-core v0.1.12 — three operator wire ops (`list_grammar_orphans`, `migrate_grammar_orphans` with `Rename` / `Map` / `Filter` strategies, `accept_grammar_orphans`); always-mint-new-IDs identity model reusing merge/split alias machinery; `pending_grammar_orphans` table; batched-commit + background-mode + dry-run for ARM-SBC operability | Audio distribution: catalogue authors planning subject-type renames or splits (e.g., `audio_track` → `track`, or `media_item` → `audio_track` + `video_track`) ship the catalogue change under a major version bump and document the migration call operators must issue. Distribution-side admin tooling consumes the three verbs and surfaces grammar-orphan state to the operator UI. See "Subject-grammar orphan migration — implications for catalogue authors and operators" subsection below. |
| 18 | Reload-catalogue / reload-manifest operator verbs | Implemented in evo-core v0.1.12 — two operator-trust-class wire ops (`reload_catalogue`, `reload_manifest`) distinct from `reload_plugin` (which reloads code); atomic-swap semantics with six-stage validation; idempotent-by-default with hash-skip; `dry_run` plan preview; five new Reserved happening subclasses for structured success/failure signaling; LKG updated only on success | Audio distribution: catalogue and manifest updates no longer require steward restart. Distribution-side admin tooling consumes both verbs to apply config rollouts live. CI/CD pipelines call the verbs sequentially for compose-and-apply distribution updates. See "Reload-catalogue / reload-manifest — implications for catalogue authors and operators" subsection below. |
| 19 | Time and Clock Trust — framework trust signal over OS-sync'd clock | Implemented in evo-core v0.1.12; framework consumes OS state, signals trust transitions, gates time-dependent subsystems. NTP / chrony / PTP daemon configuration remains distribution-owned (item 1-5 territory) | Audio distribution: configure an NTP daemon to keep clock fresh on cold start, reboot, network-up events, and periodically (recommended max staleness 24h). Declare `has_battery_rtc` in `evo.toml`. Author the distribution-side power warden's RTC-wake callback if hardware supports RTC. Document the chosen NTP source in this distribution's own `DEVELOPING.md`. |

Items 6 through 19 land in evo-core v0.1.12. Audio distributions consume each as it ships; the column above names the consumer-side decision each one forces, not whether the framework feature itself is delivered. Items 1 through 5 are permanent splits where the distribution owns the answer regardless of evo-core release cycle.

### User Interaction Routing — implications for plugins and UI

Item 8 above (User Interaction Routing) lands in evo-core v0.1.12 and has two distribution-side implications worth calling out explicitly: it shapes how plugins author auth / config flows, and it shapes how the consumer surface (frontend, voice agent, bridge) renders prompts. Authors planning new plugins or new UI surfaces against the audio reference should design against the contract below.

**For plugin authors (issuing prompts):**

A plugin needing the operator's input — credentials, server selection, an OAuth code, a confirmation, a static IP form — issues a prompt via the SDK's `request_user_interaction` (in-process) or its OOP wire-frame equivalent. The contract is plugin-orchestrated; the framework routes prompts but does not manage flow state. Multi-stage flows (WiFi configuration, OAuth code exchange) are chains of independent prompts the plugin issues based on prior answers, with a shared `session_id` field hinting to the consumer that the prompts belong to one wizard.

The closed prompt-type vocabulary for v0.1.12 (ten types):

| Type | Use case |
| --- | --- |
| `text` | Single-line free text (email, hostname, API key) |
| `password` | Masked single-line text |
| `select` | Pick one from a list (security type, output device, ambiguous-match disambiguation) |
| `select_with_other` | Pick from list OR enter your own (SSID list with "Hidden network" option) |
| `multi_select` | Pick multiple (enabled streaming services, allowed source kinds) |
| `confirm` | Yes/no |
| `multi_field` | Composite form (login = email + password + remember-me) |
| `external_redirect` | OAuth, captive-portal, browser-redirect flows |
| `datetime` | Date / time / datetime picker (Appointments / Watches consume) |
| `freeform` | Escape hatch: `{ mime_type, payload_b64 }` for unforeseen prompt shapes |

Author guidance:

-   **Validation is the plugin's responsibility.** The framework does not perform semantic validation on answers. The plugin validates after receipt and re-issues the prompt with `error_context: "<reason>"` and `previous_answer: <value>` set to surface the failure inline and let the user fix the wrong field without retyping.
-   **Persistence of secrets is the plugin's responsibility.** Tokens, credentials, and API keys land in the plugin's `credentials_dir` per `evo-core/docs/engineering/PLUGIN_PACKAGING.md` §3 contract. The framework routes the user's "remember me" choice via the `retention_hint` / `retain_for` fields but does not store the secret. The framework is not a credential store.
-   **Multi-stage flows compose simple types.** WiFi setup is a chain: `select_with_other` (network) → `select` (security if hidden) → `password`. OAuth is a single `external_redirect`. Static IP is `multi_field` with optional re-prompt-with-error after validation. No special multi-stage primitive; just chain prompts with a shared `session_id`.
-   **Plugin-declared timeout per prompt** (`timeout_ms`, default 60s, max 24h). Unattended devices with no consumer connected see the prompt time out; the plugin's logic decides how to handle that (retry on next operator action, fall back to default, surface degraded state).

**For consumer surfaces (rendering prompts):**

A consumer surface (frontend, voice agent, MQTT bridge, kiosk) holding the `user_interaction_responder` capability subscribes to open prompts via `op = "subscribe_user_interactions"`, renders them, and answers via `op = "answer_user_interaction"`. Only one connection at a time can hold the responder capability; first-claimer-wins; operator reconfigures precedence via `client_acl.toml`. Designers of consumer surfaces:

-   **MUST render every prompt type in the closed vocabulary.** A consumer that does not know how to render `external_redirect` cannot complete OAuth-shaped flows; a consumer that does not know `datetime` cannot serve Appointments / Watches workflows. Plan the renderer for all ten types.
-   **MUST render the unknown-type fallback.** New types add via framework ADR + non-breaking enum extension; consumers that observe a future type they don't recognise SHOULD render a "your client is out of date" fallback rather than crashing.
-   **SHOULD respect `session_id` grouping.** Prompts sharing a `session_id` belong to one user-visible flow; render them in a single wizard / modal stack rather than as independent dialogs.
-   **SHOULD respect `retention_hint`.** When a prompt declares `retention_hint = until_revoked`, the consumer surfaces a "remember me" affordance; the user's choice flows back as `retain_for: <enum>` on the answer. Without the hint, the consumer assumes single-use and does not surface a retention affordance.
-   **MUST handle `external_redirect` against whatever URL renderer the surface owns.** A web frontend opens the URL in a popup; a kiosk opens an embedded webview; a voice agent reads the URL aloud or skips the flow with a structured refusal. The framework hands the URL out; the consumer chooses the rendering strategy. Headless surfaces (no URL renderer) decline to handle `external_redirect` prompts and leave them open for whichever consumer can.
-   **MUST forward `error_context` and pre-fill `previous_answer`** when a re-prompt arrives. Re-typing the entire form after one wrong field is unacceptable UX.

Search and other consumer-initiated queries (browse, list, play, queue, library lookup) use the standard `op = "request"` against the relevant plugin's shelf — they are NOT prompts. The prompt-routing surface is for plugin → user questions only.

### Time and Clock Trust — distribution and plugin implications

evo-core v0.1.12 maintains a framework-side `TimeTrust` state that consumes OS-reported clock state and gates time-dependent subsystems. The framework does NOT run an NTP / PTP / GPS client — that is distribution / OS responsibility. This split has direct consequences for any audio distribution.

**Distribution responsibility (configure once per device):**

-   **Run an NTP / chrony / PTP daemon.** systemd-timesyncd (Debian/Ubuntu default), chrony (more configurable), or ptp4l (sub-microsecond precision; rare on consumer hardware) are all viable. Audio reference recommends chrony for its better drift handling and richer status surface.
-   **Configure sync triggers**: cold start, reboot, network-up events (NetworkManager dispatcher hooks), and periodically while running. Distribution's NTP-daemon configuration declares the sync interval; recommended worst-case staleness 24h.
-   **Use multiple network sources where available**: LAN if internal NTP server is present, public NTP pools as fallback (e.g., `2.pool.ntp.org`, vendor-specific stratum-1 servers if certified). Bluetooth-tethered network sync works if the daemon is configured to operate over the tether interface.
-   **Declare `has_battery_rtc` in `/etc/evo/evo.toml`.** Pi 3 / Pi 4 / many cheaper ARM SBCs have no battery-backed RTC and lose time on every power-off. reference target and most x86 boards do. Wrong declaration breaks the framework's no-RTC handling for `must_wake_device` appointments.
-   **Declare `max_acceptable_staleness_ms` in `evo.toml`** (default 24h is a reasonable starting point). Plugins may impose stricter per-plugin tolerances via their manifest's `synced_time_tolerance_ms`.
-   **For RTC-equipped devices, ensure the NTP daemon writes back to RTC** so cold-start trust is closer to correct. systemd-timesyncd does this automatically; chrony needs `rtcautotrim` configured.
-   **Document the chosen NTP source** in this distribution's own `DEVELOPING.md` so plugin authors and operators know what to expect. Stratum 1 is the realistic floor; public pools at Stratum 2-3 are normal; latency to the chosen pool affects sync precision (LAN sync ~1ms, public pool sync ~10-100ms).

**Plugin authoring (declare what you need):**

-   **Plugins requiring trustworthy time declare** `capabilities.requires_synced_time = true` in their manifest. The framework signals current trust state via `LoadContext.time_trust` plus subsequent `ClockTrustChanged` happenings; the plugin defers its real work until trust transitions to `Trusted`.
-   **Plugins with stricter tolerances declare** `capabilities.synced_time_tolerance_ms` (e.g., 1000 for "needs sync within 1s"; multi-device audio sync plugins may need much stricter). The framework respects the stricter value when signalling per-plugin trust state.
-   **Plugins managing their own time-dependent state subscribe to `Happening::ClockAdjusted`** and re-evaluate their internal schedules (cached future timestamps, OAuth refresh windows, etc.). The framework re-evaluates appointment + watch schedules automatically on clock adjustment; plugins managing their own future timestamps re-evaluate themselves.

**Consumer rendering:**

-   Frontends and other consumer surfaces SHOULD render the current `clock_trust` state visibly when it is not `Trusted` — operators need to see "device clock is untrustworthy; some features may not work" rather than experiencing silent failure.
-   Time-stamped wire frames carry a `clock_trust` annotation; consumer surfaces SHOULD distinguish "stamped during boot before sync" from "stamped during steady-state" when rendering historical data (audit logs, custody history, etc.).

**Test discipline:**

The reference target acceptance test exercises both RTC-equipped (reference target) and the no-RTC path (running on Pi 4 in dev contexts). Distributions should exercise their own target hardware mix during release acceptance.

### Appointments — implications for plugins and UI

evo-core v0.1.12 provides the appointments primitive — a runtime-created subject under the `evo-appointment` synthetic addressing scheme that fires a single request action at the declared time and recurrence. Multi-stage / multi-action coordination (alarm clock with brightness ramp + audio + snooze; scheduled-mode-transition with display + power + notification changes) is plugin-side orchestration. The framework provides the firing mechanism + the wake gate + persistence.

**For plugin authors (creating appointments):**

A plugin needing scheduled work — alarm clock, periodic library scan, daily backup, scheduled mode transitions, calendar-bridge translations — declares `capabilities.appointments = true` in its manifest and uses `LoadContext.appointments.create_appointment(...)` to schedule. The recurrence vocabulary covers the common cases:

| Recurrence | Use case |
| --- | --- |
| `OneShot { fire_at }` | Doctor reminder, package delivery alert, deadline notification |
| `Daily` | Every day, every time slot |
| `Weekdays` / `Weekends` | Mon-Fri / Sat-Sun shorthands |
| `Weekly { days }` | Per-day-of-week schedules (e.g., `["tue", "thu"]`) |
| `Monthly { day_of_month }` | "1st of every month" |
| `Yearly { month, day }` | "every 25 December" |
| `Cron { expr }` | Anything the structured variants can't express |

Author guidance:

-   **One action per appointment.** The framework dispatches ONE request to ONE shelf when the appointment fires. Multi-action orchestration is the receiving plugin's responsibility — chain multiple appointments with different `id` values + matching `session_id` if the plugin wants to render them as one logical alarm in the operator's UI.
-   **Wake control.** Set `must_wake_device = true` for alarm-clock-class appointments that must wake the device from suspend. Set `wake_pre_arm_ms` (default 0) to wake N ms before the fire to allow network and NTP sync to complete — critical on no-RTC devices where the device's clock is `Untrusted` immediately after wake.
-   **Pre-fire signalling.** Set `pre_fire_ms` to receive `Happening::AppointmentApproaching` N ms before the fire — useful for pre-warming (display brightness ramp, prefetching network resources) without authoring a separate appointment.
-   **Miss policy.** Set `miss_policy` per the appointment's intent: `Drop` for "fire only at the precise moment" (rare); `Catchup` for "fire anyway, even if hours late" (rare); `CatchupWithinGrace { grace_ms }` (default 5 min) for the natural alarm-clock behaviour.
-   **Time zone semantics.** `local` for alarms tied to wall-clock time (DST-aware); `utc` for events bound to absolute moments (e.g., "fire at exactly UTC midnight on the 1st"); `anchored { zone }` for events bound to a specific region's local time regardless of device location.
-   **Time-trust gating is automatic.** Appointments do not fire while `TimeTrust = Untrusted`; the framework queues them with `awaiting_clock_trust` reason. Plugin authors do not need to check trust state manually before scheduling — but DO need to handle the case where an alarm appointment's fire is delayed past the operator's expectation.
-   **Cross-restart resume is automatic.** Appointment subjects persist; the framework rehydrates pending appointments on every boot. A plugin re-issuing an appointment with the same content sees the existing subject; the plugin chooses to re-attach or supersede.

**For consumer surfaces (managing appointments via the wire):**

Frontends, mobile apps, voice agents, MQTT bridges interact with the appointments surface via four wire ops: `create_appointment`, `cancel_appointment`, `list_appointments`, `project_appointment`. Plus `subscribe_subject` against any `evo-appointment` subject for live updates. Consumer authors:

-   **MUST capability-negotiate `appointments_create`** before issuing `create_appointment` — default-allowed for same-UID Unix-socket peers; explicit ACL config for remote consumers.
-   **MUST handle the `appointments_admin` capability gate** for cross-claimant operations (cancelling another user's appointment, listing system-internal appointments). Default-denied; operator's `client_acl.toml` declares the granted set.
-   **SHOULD render `Happening::AppointmentFired` / `AppointmentMissed` / `AppointmentCancelled`** in the operator's history view so the operator sees what happened and when.
-   **SHOULD use `subscribe_subject`** on the `evo-appointment` subject for live state updates rather than polling `project_appointment`.

**Calendar integration is a bridge plugin (future direction).**

The calendar-bridge pattern (Google Calendar / Outlook / CalDAV / iCalendar): a plugin authenticates with the upstream calendar provider via User Interaction Routing (`external_redirect` for OAuth), polls or subscribes to upstream events, creates corresponding framework appointments via `create_appointment`. When events change in the upstream calendar, the bridge cancels / updates the framework-side appointments. No framework changes required; calendar-bridge plugins are version-pinned, signed, revocable like any other plugin. Distribution chooses whether and which calendar bridges to ship.

### Watches — implications for plugins and UI

evo-core v0.1.12 provides the watches primitive — a runtime-created subject under the `evo-watch` synthetic addressing scheme that fires a single request action when its declared condition matches. Watches are sibling to Appointments (sharing action / capability / persistence / quota infrastructure) but trigger on **conditions** rather than time.

**Audio-path switching is the canonical audio-domain use case.**

The audio reference's most common watch pattern: hardware events change which output the device should use, and a watch fires the appropriate `audio.delivery` switch. Concrete scenarios audio distributions ship support for:

| Hardware event | Source | Watch condition | Action |
| --- | --- | --- | --- |
| HDMI ARC becomes active (TV signals "send audio to me") | CEC plugin (distribution-owned, per-hardware) emits `Happening` on CEC state change | `HappeningMatch { variants: ["audio_output_available"], plugins: ["org.distribution.cec"] }` + `SubjectState { canonical_id: "<arc-port-uuid>", predicate: Equals { field: "state", value: "active" } }` | Dispatch `audio.delivery.set_output { output: "<arc-port-uuid>" }` |
| Bluetooth headphones connect | BT peer-manager plugin (distribution-owned) emits `Happening` on peer connect | `SubjectState` on the BT peer subject's `state` field, edge-triggered | Dispatch `audio.delivery.set_output { output: "<bt-peer-uuid>" }` |
| 3.5mm headphone jack inserted | Audio HAL plugin (distribution-owned) emits jack-insertion happening | `HappeningMatch` filter on the jack-insertion variant | Dispatch `audio.delivery.set_output { output: "headphone-3.5mm" }` and unmute internal amp |
| USB DAC plugged in | USB enumerator factory plugin (distribution-owned) admits a new instance subject | Watch on subject creation events for `evo-factory-instance` of `usb-dac-*` | Dispatch `audio.delivery.set_output` to the new DAC |
| TV powered off (HDMI ARC drops) | CEC plugin emits state-change | `SubjectState` predicate transitions to `state == "inactive"` | Dispatch `audio.delivery.revert_to_default_output` |

The framework provides watches; the **distribution provides the sensor / hardware-event plugins** that emit the events watches subscribe to. evo-core does NOT ship CEC parsing, BT peer detection, USB enumeration, or jack-insertion drivers. Per-target distributions (Volumio, vendor-specific audio firmware, etc.) author plugins for their hardware and emit structured happenings on the bus.

**For plugin authors (creating watches):**

A plugin needing condition-driven dispatch declares `capabilities.watches = true` and uses `LoadContext.watches.create_watch(...)`. The condition vocabulary covers three primitives that compose:

-   `HappeningMatch { filter }` — fire when a happening matches the existing `HappeningFilter` shape (variants / plugins / shelves dimensions).
-   `SubjectState { canonical_id, predicate, minimum_duration_ms? }` — fire when a subject's projection field matches a predicate (Equals, NotEquals, GreaterThan, LessThan, InRange, Hysteresis, Regex). Optional minimum-duration qualifier waits for the condition to hold this long before firing.
-   `Composite { op: All / Any / Not, terms: [...] }` — recursive AND / OR / NOT for compound conditions.

Author guidance:

-   **Edge-triggered by default.** A watch fires once when the condition transitions into match; re-arms automatically on transition out. Most audio-path-switching scenarios are edge-triggered (BT peer connects → switch once; doesn't fire again while connected).
-   **Level-triggered requires explicit cooldown.** Watches that fire while in match (CPU overheat → throttle every 30s while hot) declare `Level { cooldown_ms }` with mandatory cooldown ≥ 1s. Without cooldown, action storm under high event rates is the foot-gun.
-   **Hysteresis is first-class** for the canonical control-systems pattern. CPU throttle on temp > 75°C with don't-re-throttle until temp drops below 70°C uses `Hysteresis { upper: 75, lower: 70 }` rather than composite-encoded approximations (which oscillate in the 70-75°C band).
-   **Minimum duration for "stable" conditions.** "Stopped for 5 minutes" uses `SubjectState { predicate: Equals { state: "stopped" }, minimum_duration_ms: 300_000 }`. The framework tracks "when did the watch enter match state" and compares; transitions out before duration elapses reset the counter.
-   **One action per watch.** Multi-action orchestration is plugin-side. Chain multiple watches if a single hardware event should trigger multiple actions, or use a respondent plugin that dispatches the multi-action sequence in response to the watch's single fire.
-   **Time-trust gating is automatic.** Watches with duration-bearing conditions don't fire while `TimeTrust = Untrusted`. Pure event-match watches fire freely regardless.
-   **Sensor and hardware-event plugins must be distribution-authored** (see below). The framework provides the watch primitive that subscribes to events; the distribution authors plugins that emit the events.

**For consumer surfaces (managing watches via the wire):**

Frontends, mobile apps, voice agents, MQTT bridges interact with the watches surface via four wire ops: `create_watch`, `cancel_watch`, `list_watches`, `project_watch`. Plus `subscribe_subject` against any `evo-watch` subject for live updates. Capability gates parallel appointments — `watches_create` (default same-UID), `watches_admin` (default-denied).

**Sensor plugins are a distribution responsibility (canonical statement):**

Sensor and hardware-event plugins are distribution-owned, same posture as flight-mode hardware control (item 7 in the triage table) and NTP daemon configuration (item 19). evo-core does not ship per-hardware code; per-target distributions (Volumio, vendor-specific firmware, etc.) author the plugins their hardware needs:

| Sensor / event source | Where authored | Why distribution-owned |
| --- | --- | --- |
| HDMI CEC (ARC active / TV state) | Distribution plugin | Hardware-specific (which CEC chipset); kernel API may be vendor-specific |
| Bluetooth peer manager | Distribution plugin | BT stack is OS-specific (BlueZ on Linux, IOBluetoothFamily on macOS, etc.); pairing UI is per-vendor |
| 3.5mm jack insertion | Distribution plugin (uses ALSA jack-detect on Linux) | ALSA HAL is per-OS; jack-detect API varies |
| USB DAC enumeration (factory plugin) | Distribution plugin (uses udev on Linux) | udev / IOKit / Windows USB are per-OS |
| Motion sensor | Distribution plugin (driver-specific) | Sensor hardware varies wildly; no portable abstraction |
| Ambient light sensor | Distribution plugin | Same |
| CPU temperature | Distribution plugin (reads `/sys/class/thermal/` on Linux) | Per-OS thermal interface |
| Accelerometer (if present) | Distribution plugin (iio interface on Linux) | Per-hardware |

The framework's contract: **watches subscribe to whatever events the distribution emits**. The distribution's contract: **emit structured happenings with stable variant names + payload shapes** so watches authored against this audio reference work across vendor distributions.

The audio reference may ship a small set of brand-neutral baseline sensor plugins (e.g., `org.evoframework.sensor.cpu_temp` reading `/sys/class/thermal/` for Linux targets; portability is per-target). Vendor distributions ship the rest per their hardware (`com.volumio.cec`, `com.fiio.dac_enumerator`, etc.).

### Happenings coalescing — implications for plugins and consumer surfaces

evo-core v0.1.12 ships per-subscriber happenings coalescing — subscribers declare which fields ("labels") collapse a stream of high-rate events into a single representative event per window. The framework provides a `CoalesceLabels` trait on every `Happening` variant (auto-derived via a proc-macro from the variant's typed struct); each variant exposes its struct fields as labels; subscribers declare a label list on `subscribe_happenings.coalesce`; the framework computes a label tuple per matched happening and groups same-tuple happenings within a declared window.

**For plugin authors emitting events:**

A plugin that emits **structured events** the framework didn't anticipate (sensor readings, hardware state changes, plugin-specific notifications, periodic status reports) uses the new `Happening::PluginEvent { plugin, event_type, payload, at }` variant. The plugin's payload is opaque to the framework but flattened as labels for coalescing — top-level payload fields become labels under the same names. Author guidance:

-   **Document your `event_type` taxonomy.** Each plugin emitting `PluginEvent` defines a stable set of `event_type` strings; document these in your plugin's README so consumers can subscribe with the right filter and coalesce labels.
-   **Use stable payload field names.** Top-level payload fields become coalesce labels. A sensor plugin emitting `payload: { sensor_id, value, unit }` lets consumers coalesce by `["variant", "plugin", "sensor_id"]` cleanly. Field-name drift across releases breaks consumer coalesce configs.
-   **Sensor and hardware-event plugins emit via `PluginEvent`** — not via `CustodyStateReported`. Sensor data is producer-shaped, not custody-shaped; the `PluginEvent` variant fits the semantics correctly.

**For consumer surfaces (subscribing with coalesce):**

Consumers declare the coalesce label list per subscription based on their use case. Key scenarios for the audio domain:

| Consumer scenario | Coalesce labels |
| --- | --- |
| Per-handle position updates (one update per ~100ms per playback handle) | `["variant", "plugin", "shelf", "handle_id"]` filtered to `custody_state_reported` |
| Per-subject "current state" stream (any variant touching subject X) | `["primary_subject_id"]` (variant intentionally omitted) |
| Per-watch fire (level-triggered watch) | `["variant", "watch_id"]` filtered to `watch_fired` |
| Per-sensor reading (CPU temp at 1 Hz collapsed to 1/min) | `["variant", "plugin", "sensor_id"]` filtered to `plugin_event` with `event_type=reading` |
| Per-reconciliation pair update | `["variant", "pair"]` filtered to `reconciliation_applied` |

Consumer guidance:

-   **Query `describe_capabilities` once at connection time** to learn each variant's canonical label set. The response carries a `coalesce_labels` field listing labels per variant. Build coalesce configs against this authoritative source.
-   **Missing-label happenings pass through individually.** A coalesce config requesting `["handle_id"]` won't coalesce `SubjectForgotten` events (no handle_id field); they're delivered as-is. Not an error, just a no-op for that variant.
-   **`window_ms` defaults 100; max 5000.** Lower defeats coalescing; higher hides meaningful state changes. 100 ms is the sweet spot for moderate-burst smoothing on `CustodyStateReported`-class streams.
-   **`selection: latest` is default; `first` is available** for transition-event streams where the first-fire matters more than the most-recent-state.
-   **Cursor seq compresses; resume via `since` is consistent.** A subscriber reconnecting after disconnect with the same coalesce config replays the same coalesced view; no duplicate fires, no missed transitions.
-   **Use multiple subscriptions for multi-rule scenarios.** Different variants needing different keying = different subscriptions. The framework does NOT support multi-rule per single subscription.

The discoverability (via `describe_capabilities`) means consumer engineers don't have to read framework source to know what labels each variant exposes — runtime introspection IS the documentation.

### Hot reload — Live mode authoring

evo-core v0.1.12 ships hot-reload Live mode applying to both in-process and out-of-process plugins via the same SDK contract. The canonical use cases differ by transport:

-   **In-process plugins**: Live mode re-initializes the plugin's internal state machine without dropping the plugin's admission slot. Use cases: catalogue reload (new shelves / predicates without losing claim state); operator config reload (`/etc/evo/plugins.d/<plugin>.toml` change picked up); runtime context refresh (LoadContext re-built with current registry / bus / persistence handles).
-   **Out-of-process plugins**: Live mode is the schema-migration recovery path for binary updates. Default OOP reload remains Restart (cold start, no state handover); Live applies when the new plugin version's state format differs from the old version stored, and the plugin author wants in-flight state preserved across the swap.

**SDK contract — both transports:**

```rust
pub trait Plugin {
    // Existing methods.
    fn load(&self, ctx: LoadContext) -> /* ... */ {
        // Default: forward to load_with_state with no blob.
        self.load_with_state(ctx, None).await
    }
    fn unload(&self) -> /* ... */ { /* ... */ }

    // New optional callbacks for Live mode. Default impls
    // make Live equivalent to cold-load (Restart-shaped).
    fn prepare_for_live_reload(&self) -> Result<Option<StateBlob>, ReportError> {
        Ok(None)  // default: no state to preserve
    }
    fn load_with_state(&self, ctx: LoadContext, blob: Option<StateBlob>) -> /* ... */ {
        // Plugin author overrides this; default treats blob as cold-load.
        self.load(ctx).await  // discards blob
    }
}

pub struct StateBlob {
    pub schema_version: u32,
    pub payload: Bytes,
}
```

Plugin author guidance:

-   **Opt in to Live mode** by declaring `lifecycle.hot_reload = "live"` in the manifest AND implementing both callbacks. Without both, framework refuses Live with `unavailable / live_reload_not_implemented`; operator falls back to Restart.
-   **State blob is opaque to the framework** and is the plugin author's contract. Define a stable schema; bump `schema_version` on format changes; handle migration in `load_with_state`.
-   **Size limit is 16 MiB default, 64 MiB max.** Use the blob for transient in-flight state (buffer contents, partial OAuth code, EQ filter coefficients, library-scan progress); use durable persistence (subject store, `state_dir`) for long-lived data that already has a persistence path.
-   **`unload` must clean up resources** — file handles, threads, sockets, pending tasks. In-process Live reload runs `unload` then `load_with_state` in the same process; resource leaks accumulate across reload cycles in a way they don't accumulate across separate process lifetimes.
-   **Rollback semantics protect against bad migrations.** If `load_with_state` fails: in-process plugin gets cold-reloaded with `blob: None`; OOP plugin's old process keeps running and the new process is terminated. Plugin sees structured error; operator triages.

**Hardware-bound state preservation — the warden-architecture-pattern:**

Audio plugins managing hardware-bound state (ALSA pipeline, open device file handles, kernel-bound resources) cannot preserve that state through a plugin process restart — the kernel resources die with the process. Live mode preserves what's in the plugin's address space (or the emitted blob); it does NOT preserve kernel-side state via plugin's open handles.

The answer for distributions wanting **zero-disruption updates** to hardware-bound flows is the **warden-architecture-pattern**:

-   The warden plugin holds the resource only briefly during dispatch; the actual long-running resource owner (ALSA pipeline, kernel audio router, mixer engine) lives in a separate process or kernel thread the plugin code does NOT own.
-   Plugin code reload (Live mode) reconnects to the running resource owner; resource-side state is preserved because it never died.
-   Distribution authors the resource owner as a separate component (e.g., systemd-managed ALSA daemon; a kernel module; a separate vendor process) the plugin connects to.

This is a structural choice the distribution makes, not a framework gap. evo-core's Live mode primitive serves the in-process state path; hardware-bound preservation is the distribution's architectural decision.

**For audio-domain plugins specifically:**

| Plugin type | Live mode opt-in? | Why |
| --- | --- | --- |
| Library scanner / metadata fetcher | Yes (in-process) | Catalogue / config reload without losing scan progress |
| Streaming source (Spotify, Tidal, etc.) | Yes (OOP, Live) | Schema migration on plugin updates; OAuth refresh-token handover |
| MPD playback warden | Restart only (warden-arch holds ALSA state separately) | Hardware-bound state via the resource owner pattern |
| ALSA composition warden | Restart only (same reason) | Hardware-bound; resource owner is the actual ALSA pipeline daemon |
| Local artwork / file-tag respondents | Yes (in-process) | Cache state preserved across catalogue reload |
| Sensor plugins (CPU temp, BT peer, etc.) | Restart only | State is the kernel's; nothing for the plugin to hand over |
| Alarm clock plugin | Yes (in-process) | Pending-alarm state preserved across operator config reload |

The pattern: plugins with **plugin-owned state** opt in to Live; plugins with **kernel-owned or warden-owned state** stay on Restart and rely on the resource owner outliving the plugin code.

### Runtime capabilities dispatch + manifest-drift discipline + version-skew policy

evo-core v0.1.12 ships three related refinements to the existing dispatch-time capability gate:

**1. Warden-side capability gate** (parallel to the respondent gate shipped in v0.1.10):

Every warden's manifest now declares `capabilities.warden.course_correct_verbs: [...]` listing every verb the warden's `course_correct` accepts. The framework refuses any incoming dispatch whose verb is not in the list with a structured `StewardError::Dispatch` — the warden's `course_correct` body never sees undeclared verbs.

For audio-domain wardens, this means each warden enumerates its accepted course_correct verbs:

| Warden | Typical `course_correct_verbs` |
| --- | --- |
| `org.evoframework.playback.mpd` | `["set_volume", "pause", "resume", "seek_to", "set_output", "next_track", "previous_track", "set_playback_state"]` |
| `org.evoframework.composition.alsa` (future) | `["apply_pipeline", "add_processor", "remove_processor", "set_sample_rate", "set_channels"]` |
| Audio.delivery wardens | `["play_source", "stop", "set_output", "set_volume", "set_balance"]` |

The exact list is per-warden; the warden author declares it in the manifest, and the framework enforces. `fast_path_verbs` (per the Fast Path design) MUST be a subset of `course_correct_verbs` — the catalogue parser refuses on violation, admission refuses on validation failure.

**2. Three-tier manifest-drift detection** applies universally to both respondents and wardens. Drift = mismatch between the plugin's manifest declarations and the plugin's actual `describe()` output:

-   **Sign-time check via `evo-plugin-tool verify`**: at plugin sign / pack time, the tool extracts the plugin's `describe()` output and compares to the manifest. Mismatch refuses to sign with `VerifyError::ManifestDrift`. Catches drift at the plugin author's workstation before the bundle ships.
-   **Admission-time check by the framework**: at admit time, the framework calls `plugin.describe()`, compares to manifest, refuses with `AdmissionError::ManifestDrift` on mismatch (subject to the version-skew policy below). Catches drift the plugin author's CI missed.
-   **CI-time test harness pattern**: a new helper crate `evo-plugin-test` ships an `assert_manifest_matches_describe(&plugin)` helper plugin authors add to their unit tests. Catches drift before the plugin reaches sign.

The three tiers triangulate — drift caught at any one of them stops the bug before production. Plugin authors author all three (CI-time test, sign-time verify, admission-time enforcement passes through automatically).

**3. Kubernetes-style version-skew policy:**

Plugins admitted to a v0.1.12 framework with different `prerequisites.evo_min_version` declarations get different enforcement strictness based on the K8s-style skew window:

| Plugin's `evo_min_version` relative to running framework | Treatment |
| --- | --- |
| `> current` | Refused (existing `EvoVersionTooLow` check) |
| `current` or `current - 1` (in-window strict) | Strict enforcement: drift refuses on mismatch; mandatory new fields enforced |
| `current - 2` (warn-band) | Admitted; `Happening::PluginVersionSkewWarning` emitted; new fields treated as optional with backward-compat defaults; drift detection runs but warns rather than refuses |
| `current - 3` or older | Refused at admit with `permission_denied / version_skew_too_wide` |

Plus a time-decoupled refinement: plugins whose required minimum framework version's release date is more than **18 months** old are refused regardless of version-count window; 12-18 months are warn-band; under 12 months are strict (combined with the version-count window — plugin must be in-strict on BOTH dimensions to get strict enforcement).

For audio plugins specifically, this means:

-   Plugins built against v0.1.11 or v0.1.12: full strict enforcement under v0.1.12 framework. Drift caught and refused at admit.
-   Plugins built against v0.1.10: admitted with version-skew warning. Drift caught and warned about. Plugin author has a one-cycle window to refresh the plugin.
-   Plugins built against v0.1.9 or older: refused. Plugin author must rebuild against a newer framework before deployment.

This gives plugin authors **two minor-version cycles of grace** between framework release and forced plugin refresh. Operationally proven (Kubernetes pattern); ecosystem hygiene forced over time without breaking deployment headroom.

**For audio distributions:**

-   When this distribution publishes a new audio reference plugin set built against framework v0.1.12, set `evo_min_version = "0.1.12"` in each plugin's manifest. The framework will apply strict enforcement to these plugins.
-   When the framework moves to v0.1.13, these plugins will continue admitting under strict enforcement (one-version-behind is in-window).
-   When the framework moves to v0.1.14, these plugins will move into the warn-band — at which point the audio reference's release cycle should produce a refresh.
-   Plugin authors targeting older devices (long-life embedded systems that may not refresh quickly) should consider declaring the OLDEST `evo_min_version` their plugin actually requires — not the version they happen to be building against.

**Migration impact for v0.1.12:**

Audio reference plugins (org.evoframework.playback.mpd, metadata.local, artwork.local, future composition.alsa) gain their `course_correct_verbs` declarations in v0.1.12 implementation. Vendor distributions update their manifests in the same cycle — check this distribution's `Upgrading the evo-core pin` section for the migration steps when v0.1.12 lands.

### Subject-grammar orphan migration — implications for catalogue authors and operators

Subject types are part of a catalogue's public contract. The framework treats type stability as a major-version concern: renaming or removing a subject type is a breaking change that requires a catalogue major-version bump. Existing subjects of a removed type become **orphans** — they remain in the registry, no rack opines on them, and no plugin can announce a new subject of that type.

In v0.1.11 and earlier, the only signal the framework offered was a boot-time diagnostic warning. v0.1.12 ships the operator-callable structured migration surface that lets orphans join the new grammar without losing identity, relations, custody, or history.

**For audio distribution catalogue authors:**

When you publish a new audio reference catalogue version that renames or splits a subject type, you carry three responsibilities:

1.  **Bump the catalogue major version.** Subject-type changes are breaking. Distribution operators on the prior catalogue version must consciously upgrade.
2.  **Document the migration in the catalogue release notes.** State the operator command needed to migrate the orphans. Example for a rename:

    ```text
    Catalogue v3 renames subject type 'audio_track' to 'track'.
    Existing 'audio_track' subjects on deployed devices become orphans
    on first boot under v3. Operators migrate via:

      evo-plugin-tool admin grammar plan \
        --from-type=audio_track \
        --strategy=rename:to_type=track

      # review the dry-run output, then:

      evo-plugin-tool admin grammar migrate \
        --from-type=audio_track \
        --strategy=rename:to_type=track \
        --reason="catalogue v3: audio_track renamed to track"
    ```

3.  **Test the migration on the largest realistic library size before publishing the catalogue version.** Running 50,000-subject migrations on Pi Zero W is expected to take ~30 seconds; on reference target ~5 seconds. Catalogue authors validate this before shipping.

**Identity invariants the migration preserves:**

-   Each migrated subject receives a new canonical ID; the old ID retires as a `TypeMigrated` alias record.
-   Consumers holding stale references to the old ID receive a redirect via `describe_alias` (same as merge/split).
-   External addressings carry to the new ID (re-statement preserves identity).
-   Custody ledger ownership flows from old ID to new ID atomically.
-   Relations are rewritten via the merge cascade infrastructure.
-   Per-subject `Happening::SubjectMigrated` is emitted before the relation-graph rewrite (same emission ordering as `SubjectMerged` / `SubjectSplit`).

**The three migration strategies — when to use each:**

| Strategy | Use case | Catalogue change shape |
| --- | --- | --- |
| `Rename { to_type }` | A type was renamed. All orphans become a single new type. | Catalogue v3: `audio_track` → `track`. |
| `Map { discriminator_field, mapping }` | A type was split. Orphans route to multiple new types based on a payload field. | Catalogue v4: `media_item` splits into `audio_track` / `video_track` / `still_image` based on `mime_type`. |
| `Filter { predicate, to_type }` | Operator wants to migrate only some orphans (e.g., scope-limited rollout). Non-matching orphans remain. | Catalogue v3 with operator scoping the rollout to specific library roots. |

**For operator tooling consuming the verbs:**

A distribution's admin panel (whether shipped by the audio reference or a vendor distribution) typically consumes all three wire ops:

-   `list_grammar_orphans` — populate a "Pending grammar orphans" view: subject_type, count, first_observed_at, status.
-   `migrate_grammar_orphans` — surface a migration form per orphan type. Strategy selector (Rename / Map / Filter) drives the form fields. Always issue with `dry_run = true` first; show the operator the plan + duration estimate; require explicit confirmation before issuing the real call.
-   `accept_grammar_orphans` — surface as a "leave permanently orphaned" affordance with mandatory reason text.

The operator UI defaults the migration to `mode = "background"` for any plan with `would_migrate > 1000`. Foreground is the default for smaller migrations (operator gets immediate feedback).

**For ARM-SBC distribution operators:**

The framework's batched-commit, background-mode, and bounded-per-call execution model means migrations work on every target. Operators on slower SD-card storage (Pi Zero / Pi 3 with SD card) optionally chunk large migrations via `max_subjects = 1000`, running multiple calls overnight to avoid one long-running blocker. The verb is naturally idempotent against `from_type` — re-issuing after a partial migration resumes from the cursor.

**Per-subject vs per-batch happenings — what subscribers see:**

The framework emits both `Happening::GrammarMigrationProgress` (per batch) and `Happening::SubjectMigrated` (per subject). Consumer surfaces choose:

-   Ops dashboards: subscribe to `GrammarMigrationProgress` only; see ~50 events for a 5,000-subject migration.
-   Forensic / audit consumers: subscribe to `SubjectMigrated` without coalescing; see one event per subject.
-   Frontend "now-migrating" indicator: subscribe to `SubjectMigrated` with coalesce labels `["variant", "from_type", "to_type", "migration_id"]` — the framework's per-subscriber coalescing collapses these to one event per from_type/to_type pair.

The audio reference's admin-panel reference UI subscribes to `GrammarMigrationProgress` for the migration progress bar and to coalesced `SubjectMigrated` for the "completed N of M" counter.

### Reload-catalogue / reload-manifest — implications for catalogue authors and operators

The framework reads its catalogue and plugin manifests at steward boot. v0.1.12 ships two new operator-callable wire ops that update the loaded declarations without steward restart, decoupling operator-issued declaration updates from service interruption.

**Three reload verbs, three concerns:**

| Verb | Reloads | When to use |
| --- | --- | --- |
| `reload_plugin` | Plugin code (binary or in-process module) | Plugin author shipped a new code version |
| `reload_catalogue` | Catalogue declarations (rack, shelf, type, cardinality) | Distribution rolling out a new catalogue version |
| `reload_manifest` | A single plugin's manifest declarations (capabilities, verbs, lifecycle) | Plugin author bumped manifest declarations; code unchanged |

The three are independent. Operators combine them in sequence for code + declaration co-updates: `reload_plugin` first (manifest-drift window opens), then `reload_manifest` to update the framework's gate table.

**For audio distribution catalogue authors:**

When you publish a new audio reference catalogue version that adds racks, removes racks, or changes cardinality rules, the operator-facing rollout flow is:

1.  **Validate locally first.** Run `evo-plugin-tool admin reload plan catalogue --source=path:<new-catalogue.toml>` against a representative test deployment; review the dry-run output (added racks, removed racks, plugins that would be refused, cardinality violations). Catch breaking changes before they ship.
2.  **Document the rollout in catalogue release notes.** State the operator command verbatim:

    ```text
    Catalogue v3 adds the flight_mode rack.
    Operators apply the catalogue update without steward restart via:

      # Plan preview:
      evo-plugin-tool admin reload plan catalogue

      # Apply:
      evo-plugin-tool admin reload catalogue \
        --reason="catalogue v3: add flight_mode rack"
    ```

3.  **For breaking changes (rack removed, type removed, cardinality tightened):** document the `--allow-cardinality-divergence` flag if it applies, and document any required pre-cleanup (`migrate_grammar_orphans` for type removals, plugin admission cleanup for rack removals).

**Validation pipeline — what the verb checks before swapping:**

The new catalogue or manifest must pass a six-stage validation before any state mutation:

| Stage | What it checks | Refusal mode |
| --- | --- | --- |
| Parse | TOML syntax + schema | `catalogue_invalid` / `manifest_invalid` Reserved happening |
| Schema | Every field has a known shape | Structured error naming the offending field |
| Cardinality re-check | New rules vs current storage | `cardinality_violation` Reserved happening per offending shelf (refused unless `allow_cardinality_divergence = true`) |
| Admission re-evaluation | Currently-admitted plugins vs new catalogue | Plugins-to-be-refused listed in reply; forced-retract on commit |
| Drift re-check (manifest only) | New manifest vs live `describe()` | `manifest_drift` per the version-skew window |
| Atomic swap | Only after all checks pass | Old state preserved if any stage fails |

Failed reload is a no-op against framework state — old catalogue/manifest stays loaded; structured error names the failure mode.

**For operator tooling consuming the verbs:**

A distribution's admin panel typically wires both verbs:

-   `reload_catalogue` consumed for catalogue version rollouts. Default UI flow: dry-run → plan review → confirm → apply. Default `mode` is foreground (admission re-evaluation completes within the verb call; no progress polling needed).
-   `reload_manifest` consumed for per-plugin manifest updates. Often paired with `reload_plugin` (code reload) in a single operator-issued sequence; UI surfaces a single "Update plugin" button that issues both verbs in order.

The operator UI defaults `dry_run = true` for the first call and only issues the real reload after explicit confirmation.

**Idempotence and CI/CD:**

Both verbs are idempotent by default — calling with an unchanged catalogue/manifest is a no-op (hash-based skip; reply returns success with `from_version == to_version`, `duration_ms ≤ 5`). CI/CD pipelines deploying audio reference distribution updates can call the verbs unconditionally as part of every release without side effects.

When CI/CD ships a bundle (new catalogue + N updated plugin manifests), the recommended sequence is:

```text
1. Push catalogue file + manifest files to the device's filesystem.
2. evo-plugin-tool admin reload plan catalogue       # dry-run
3. evo-plugin-tool admin reload catalogue            # apply
4. For each updated plugin:
   evo-plugin-tool admin reload manifest --plugin=<id>
5. Verify outcome via list-of-admitted-plugins query.
```

Each step is independently failable; failure leaves prior steps committed and lets the operator (or CI/CD pipeline) fix-and-retry without rollback.

**LKG (last-known-good) interaction:**

A successful `reload_catalogue` writes the new catalogue to the LKG shadow file. Failed reload doesn't touch LKG — the catalogue corruption-resilience recovery path stays trustworthy across every reload event. Manifests are not LKG-cached; they live next to their plugin bundles.

**Reserved happening subclasses for consumer surfaces:**

Subscribe to these for structured success/failure signaling:

| Variant | Emitted when | Consumer use |
| --- | --- | --- |
| `Happening::Reserved::CatalogueInvalid` | Catalogue parse / schema check failed | Frontend shows the structured error fields |
| `Happening::Reserved::CardinalityViolation` | `allow_cardinality_divergence = true` accepted a violating state | Per-shelf operator alert: "shelf X has 12 subjects, declared max is 1" |
| `Happening::Reserved::ManifestInvalid` | Manifest reload failed validation | Per-plugin error in the admin panel |
| `Happening::CatalogueReloaded` | Successful catalogue reload | Notification: "Catalogue updated from v2 to v3; flight_mode rack added" |
| `Happening::ManifestReloaded` | Successful manifest reload | Per-plugin badge: "manifest updated to v0.4.2" |

Frontend admin panels render each subclass with field-level context, not free-text logs.

**Performance on ARM SBCs:**

Cost on Pi Zero W (smallest target) for typical operations:

| Operation | Cost |
| --- | --- |
| `reload_catalogue` with 20 plugins re-admitted | <200ms |
| `reload_manifest` (single plugin) | <10ms |
| Failed reload (validation refuses early) | <30ms |

Negligible on every target. Reload calls do not require the chunking / background-mode patterns that the orphan migration uses.

### Catalogue schemas — implications for plugin authors and distribution packagers

The framework's catalogue document declares racks, shelves, subject types, and cardinality. Each shelf carries a **shape** — the request types, payload shape, and acceptance criteria a plugin satisfies when claiming to implement the shelf. The schemas are versioned TOML files describing those shapes.

**Two schema surfaces:**

| Surface | Where it lives | Role |
| --- | --- | --- |
| In-tree skeleton | [evo-core's `dist/catalogue/schemas/`](https://github.com/foonerd/evo-core/tree/main/dist/catalogue/schemas) | Worked-example reference for the on-disk shape; carries `example/echo.v1.toml` |
| Brand-neutral framework-tier schemas | [`foonerd/evo-catalogue-schemas`](https://github.com/foonerd/evo-catalogue-schemas) | Canonical source for `org.evoframework.*` namespace shelves; versioned independently of evo-core |

The framework runtime does NOT consume the schemas at admission — the steward validates against the catalogue document, not per-shelf schemas. Schemas are author-time / CI-time artefacts only.

**For audio reference plugin authors:**

1.  **Reference the schemas repo by pinned version** in your plugin's CI configuration. Pin the `evo-catalogue-schemas` git tag matching the shelf shape you target (e.g., your plugin claims `org.evoframework.audio.delivery` shape v1; pin the schemas-repo tag that ships that shape).
2.  **Validate locally before shipping.** Once evo-core v0.1.12 ships:

    ```sh
    git clone -b v<X.Y.Z> https://github.com/foonerd/evo-catalogue-schemas /tmp/schemas
    evo-plugin-tool validate-shelf-schema \
      --schemas-path=/tmp/schemas/schemas \
      --plugin=path/to/your/plugin.toml
    ```

3.  **Read the schemas as your code contract.** When implementing a plugin claiming `target.shelf = "audio.delivery"`, `target.shape = 1`, the schema TOML at `schemas/org.evoframework/audio/delivery.v1.toml` documents:
    - The request types your plugin's `handle_request` must accept.
    - The payload shape (in-bytes / out-bytes layout) for each request type.
    - The acceptance criteria your plugin must satisfy (testable behaviours).
4.  **When a shelf shape needs to change**, open a PR against `foonerd/evo-catalogue-schemas` adding a new shape version (v2 alongside v1). Plugins targeting v1 keep working through the framework's `shape_supports` migration window. See the schemas repo's CONTRIBUTING.md for the review process.

**For audio-distribution packagers:**

1.  **Bundle the schemas.** The audio reference's distribution package (or a separate `evo-catalogue-schemas` package) installs the schemas to `/usr/share/evo-catalogue-schemas/` on the target device. Consumers (`evo-plugin-tool` and any other tool needing schemas) resolve via the cascade:
    - `--schemas-path` flag if passed
    - `$EVO_SCHEMAS_DIR` env var if set
    - `/usr/share/evo-catalogue-schemas/` if present
    - Error with guidance to install a schemas package or pass `--schemas-path`

2.  **Pin a specific schemas version** in your distribution's package manifest. Bumping the schemas pin is a deliberate distribution release decision, not an auto-update. The pinned version must be compatible with the evo-core version your distribution ships (the schemas repo's CHANGELOG.md documents per-version evo-core compatibility).
3.  **Track schemas-repo releases independently of evo-core.** A schemas-repo patch release (new shelf added, new acceptance criterion) doesn't require an evo-core version bump; a schemas-repo minor release (new shape version) typically aligns with framework-tier feature work the audio reference is shipping anyway.

**Distribution-specific shelves do NOT belong in `foonerd/evo-catalogue-schemas`.** Vendor distributions adopting the audio reference can author their own schemas under their own namespace (`com.<vendor>.*`) and publish them in their own schemas repository. The brand-neutral repo hosts contracts shared across the audio reference and any distribution adopting it.

**Validation tooling on the audio reference's CI:**

Once `evo-plugin-tool validate-shelf-schema` ships in evo-core v0.1.12, the audio reference's CI workflow adds a step that validates each plugin's manifest against the pinned schemas-repo version. The check runs on every PR and gates merge.

## Cross-compiling for the target hardware

reference target (`aarch64-unknown-linux-gnu`) and other non-host targets cross-compile via the [`cross`](https://github.com/cross-rs/cross) crate, configured by the workspace's `Cross.toml`. During v0.1.13 development the workspace pins `evo-plugin-sdk` via a path dep on the sibling `evo-core-eng` clone; cross-rs auto-mounts each path-dep crate directory but does not mount the path-dep's workspace root, so cargo's `edition.workspace = true` inheritance walk fails. The workaround is the `scripts/cross-build.sh` wrapper, which sets `CROSS_CONTAINER_OPTS` to mount the eng-line workspace root explicitly:

```sh
scripts/cross-build.sh aarch64-unknown-linux-gnu \
    --release \
    --features alsa-substrate \
    -p org-evoframework-composition-alsa
```

The `alsa-substrate` Cargo feature (off by default) pulls in the `alsa` crate; cross-builds enabling it install `libasound2-dev:$CROSS_DEB_ARCH` inside the container via `Cross.toml`'s `pre-build`. The dev-rig (Ubuntu 24.04 host) does not need libasound installed; native `cargo build` / `cargo test` runs without the feature and the `alsa-substrate` code is excluded from the build.

At v0.1.13 release-cut, the SDK pin flips back to a `git+tag` form, the path-dep workaround is no longer needed, and `scripts/cross-build.sh` becomes a thin convenience wrapper.

## Upgrading the evo-core pin

1.  Verify the new evo-core tag is green (`cargo test --workspace` in evo-core).
2.  Update `[workspace.dependencies].evo-plugin-sdk` in this repo's `Cargo.toml`: bump `tag = "..."` and `version = "..."` to match.
3.  Update `EVO_CORE_TAG` in `.github/workflows/continuous-dev.yml` and `.github/workflows/manual-build.yml`.
4.  Rerun `cargo build --workspace` and `cargo test --workspace`.
5.  Commit with a message naming the new evo-core version and any public-surface changes the bump forced.

## License

Apache 2.0. Each source file carries the SPDX identifier `Apache-2.0` in its header once code lands.
