# Developing evo-plugins-audio

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

Both must be green before any version bump. In Phase 1 scaffolding state the workspace contains only the `evo-plugins-audio-shared` anchor crate (an empty library that future plugins will share utilities through); `build` and `test` succeed trivially until plugin crates land.

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
6.  If the plugin needs utilities shared with other plugins (path normalisation, library scanning, common error types), depend on `evo-plugins-audio-shared = { workspace = true }` and add the helper to that crate. Do not duplicate across plugins.
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

## Upgrading the evo-core pin

1.  Verify the new evo-core tag is green (`cargo test --workspace` in evo-core).
2.  Update `[workspace.dependencies].evo-plugin-sdk` in this repo's `Cargo.toml`: bump `tag = "..."` and `version = "..."` to match.
3.  Update `EVO_CORE_TAG` in `.github/workflows/continuous-dev.yml` and `.github/workflows/manual-build.yml`.
4.  Rerun `cargo build --workspace` and `cargo test --workspace`.
5.  Commit with a message naming the new evo-core version and any public-surface changes the bump forced.

## Git

Claude (the assistant used during development) proposes file changes. The user commits, tags, and pushes. Claude does not run git commands.

## License

Apache 2.0. Each source file carries the SPDX identifier `Apache-2.0` in its header once code lands.
