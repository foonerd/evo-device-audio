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
| 10 | Watches rack (condition-driven plugins) | Implemented in evo-core v0.1.12 | Audio distribution decides whether it admits condition-driven plugins (auto-resume on network up, etc.). |
| 11 | Fast Path (latency-bounded wire channel) | Implemented in evo-core v0.1.12 | Audio distribution decides whether its frontends use Fast Path for transport ops (volume, pause, seek). |
| 12 | Steward Reconciliation Loop (compose-and-apply) | Implemented in evo-core v0.1.12 | Audio distribution gains the orchestration surface for `composition.alsa` → `delivery.alsa` flows. Distribution decides which composer/delivery pairs it admits. |
| 13 | Catalogue corruption resilience | Implemented in evo-core v0.1.12 | Audio distribution inherits the LKG + built-in fallback transparently. Distribution may pre-seed `catalogue.lkg.toml` from its own packaging if it wants to control the recovery state. |
| 14 | CBOR codec on the wire | Implemented in evo-core v0.1.12 | Audio distribution decides whether its frontends prefer CBOR over JSON. JSON remains supported. |
| 15 | Hot-reload `Live` mode | Implemented in evo-core v0.1.12 | Audio distribution decides whether any of its plugins author live-reload state-handover. Default: `Restart` mode is sufficient. |
| 16 | Happenings coalescing (per-subscriber rate limit) | Implemented in evo-core v0.1.12 | Audio distribution decides whether its consumers opt in to coalescing for high-rate variants. |
| 17 | Subject-grammar orphan migration verb | Implemented in evo-core v0.1.12 | Audio distribution decides whether it provides operator tooling that consumes the verb. |
| 18 | Reload-catalogue / reload-manifest operator verbs | Implemented in evo-core v0.1.12 | Audio distribution decides whether it surfaces these verbs in its frontend. |

Items 6 through 18 land in evo-core v0.1.12. Audio distributions consume each as it ships; the column above names the consumer-side decision each one forces, not whether the framework feature itself is delivered. Items 1 through 5 are permanent splits where the distribution owns the answer regardless of evo-core release cycle.

## Upgrading the evo-core pin

1.  Verify the new evo-core tag is green (`cargo test --workspace` in evo-core).
2.  Update `[workspace.dependencies].evo-plugin-sdk` in this repo's `Cargo.toml`: bump `tag = "..."` and `version = "..."` to match.
3.  Update `EVO_CORE_TAG` in `.github/workflows/continuous-dev.yml` and `.github/workflows/manual-build.yml`.
4.  Rerun `cargo build --workspace` and `cargo test --workspace`.
5.  Commit with a message naming the new evo-core version and any public-surface changes the bump forced.

## License

Apache 2.0. Each source file carries the SPDX identifier `Apache-2.0` in its header once code lands.
