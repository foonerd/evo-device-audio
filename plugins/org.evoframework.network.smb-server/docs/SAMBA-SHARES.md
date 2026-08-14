# SMB share inventory — evo-device-audio

**Normative** product contract for what this distribution exports
when the on-device SMB server is enabled. Implementation
(`org.evoframework.network.smb-server`), bootstrap, UI, and LAN
acceptance tests MUST match this file. Update this document and
the code constants in the same change.

Bound by a companion design record in the internal decision
tree, which cites this file by repo path and does not
duplicate the share table.

Lineage (do not re-derive from chat):

- Classic Volumio: `smb.conf.tmpl` — stock Internal / USB / NAS
- volumio-evo: `docs/SAMBA.md` + `crates/core/src/samba_conf.rs`
- Framework plugin stage: the `evo-plugins-stage` share
  convenience atop the framework's stage-watcher primitive
- Shelf / verbs: network file-source contract; wire shape:
  `shares.v1.toml`

This document is the **audio-distribution** inventory. Framework
ADRs own *admission* and *update channels*; they do not own the
LAN share list for this device.

---

## Ownership

| Rule | Detail |
|------|--------|
| Sole writer | `org.evoframework.network.smb-server` is the only writer of `/etc/samba/smb.conf` on this distribution. |
| No dual ownership | Do not install parallel shares via `smb.conf.d/` drop-ins that this plugin's apply would overwrite or ignore. Fold required shares into the rendered conf. |
| Apply replaces whole conf | Rendered text is authoritative. Distro default Samba persona MUST NOT remain visible on the LAN after a successful apply. |
| Disable | When SMB is disabled, stop `smbd` (and `nmbd` if installed). Do not leave a half-live server advertising junk shares. |

---

## Stock shares (always present when SMB enabled)

These are **not** `extra_shares`. The renderer MUST emit them
whenever `enabled = true`. Guest-friendly, read-write — same
product reason as Volumio / volumio-evo: LAN push into the music
library roots MPD already browses.

| SMB share name | Path | Guest | Read-only | Purpose |
|----------------|------|-------|-----------|---------|
| `Internal Storage` | `/var/lib/evo/music/INTERNAL` | yes | no | Local music library. Drop files here → appear under INTERNAL in My Music. |
| `USB` | `/var/lib/evo/music/USB` | yes | no | Removable-media library segment. |
| `NAS` | `/var/lib/evo/music/NAS` | yes | no | Parent of inbound network mounts (`…/NAS/<alias>`). |

Bootstrap MUST ensure the three directories exist (already does for
the music triad). Paths are the **evo music plane**, not classic
Volumio `/data/INTERNAL` / `/mnt/USB` / `/mnt/NAS`.

---

## Delivery shares

### Plugin packages — `evo-plugins-stage`

| Field | Value |
|-------|--------|
| SMB share name | `evo-plugins-stage` |
| Path | `/var/lib/evo/plugins/stage` |
| Guest | **no** (authenticated SMB user) |
| Read-only | no |
| Purpose | Operator drops signed plugin bundles (`.tar.gz` / `.zip`) for the framework stage watcher to admit. |
| Consumer | Steward `PluginStageWatcher` (`[plugins.stage]` default dir). Signature gate unchanged — SMB is convenience, not trust elevation. |

When SMB is enabled, this share MUST be present in the rendered
conf (product opt-in for File Sharing implies the delivery plane
is available). Bootstrap MUST create the stage directory with
ownership suitable for the steward service user + SMB write.

Retired: `evo-core-eng/scripts/install/setup-smb-stage.sh` as a
parallel owner of Samba config on this distribution. Behaviour
lands in this plugin's renderer instead.

### Uploads (stock when enabled)

| Field | Value |
|-------|--------|
| SMB share name | `Uploads` |
| Path | `/var/lib/evo/uploads` |
| Guest | yes |
| Read-only | no |
| Purpose | Designated upload root beyond the music triad (schema: “music library root + upload target”). |

Bootstrap MUST create `/var/lib/evo/uploads`.

### Core / framework binary via fileshare

**Open — not in this inventory yet.** Core updates land via
the HTTPS artefact channel to a local stage path. A Samba
share for core drops is **out of scope** until an explicit
product decision amends the update-channel contract or this
inventory. Do not invent a core share in code before that
decision.

---

## Extra shares (operator-defined)

`extra_shares` are **beyond** the stock + delivery set above.
Each entry: name + path + `guest_ok`. Validated on apply.

### Allowlist (path prefixes)

| Prefix | Purpose |
|--------|---------|
| `/var/lib/evo/music` | Music library tree (INTERNAL / USB / NAS and below). |
| `/var/lib/evo/uploads` | Designated upload root. |
| `/var/lib/evo/plugins/stage` | Plugin stage (normally covered by stock delivery share; allowlisted so extras cannot be the only way to reach it). |

### Denylist (always reject, even under an allowed root)

| Prefix | Reason |
|--------|--------|
| `/var/lib/evo/settings` | Secrets and persisted credentials. |
| `/var/lib/evo/plugins/stage/rejected` | Rejected bundles; not an operator drop target for new installs. |

Classic Volumio prefixes (`/data/INTERNAL`, `/mnt/USB`, `/mnt/NAS`)
are **not** allowlisted on this distribution. Operators and
tests use the evo music plane only.

Code constants (default allowlist / denylist) live next to
`render_smb_conf` in the smb-server plugin runtime and MUST
match this table.

---

## SMB users (authenticated access)

Named SMB users are how an operator authenticates to
non-guest shares (today: `evo-plugins-stage`) without ever
using a shell on the device. Guest-ok stock shares
(`Internal Storage`, `USB`, `NAS`, `Uploads`) do not require
a named user; authenticated shares do.

The File Sharing UI (`network.smb_server.user_add` /
`user_revoke`) is the only supported management path.
Requiring `useradd` / `smbpasswd` by hand on the device is a
defect — the UI exists so the operator never needs a shell.

### Samba constraint (why a Unix name still appears)

This distribution runs Samba with `security = user` and the
default `tdbsam` passdb. Under that mode:

1. `smbpasswd -a <name>` (and `pdbedit -a`) requires `<name>`
   to resolve via NSS (`getpwnam`) **before** a passdb row can
   be created. Calling `smbpasswd -a` alone for a name that
   does not exist fails (`Failed to add entry for user …`).
2. Share sections set `force user` / `force group` (see
   renderer) so **file ownership on disk does not follow the
   SMB login**. Authenticated and guest writers land under the
   forced identity. The NSS entry exists for Samba's auth
   machinery, not to give the operator a login or a home
   directory.

Therefore: each named SMB user has a **non-login system
account** plus a Samba passdb entry. That is not a “shell
account” in the product sense — no interactive shell, no
home, no SSH grant. It is the appliance pattern volumio-evo
already ships (`useradd -r -s nologin -d /nonexistent`).

**Out of scope / rejected:** inventing a passdb-only username
with zero NSS entry while staying on stock `security = user`
+ `tdbsam`. That combination does not work on the Debian
Samba this distribution runs. Changing Samba security mode
or passdb backend is a separate design decision, not a silent
shortcut in `user_add`.

**Rejected:** adding the distribution-configured steward
service user, `root`, `nobody`, or other system identities as
SMB logins. Those accounts exist for the OS / steward; they
MUST NOT be promoted to LAN file-share credentials. The live
service-user name is picked up dynamically from
`EVO_SERVICE_USER` (or `USER`) at validation time so the
protection travels with every distribution regardless of the
name it chose.

### Provisioning lifecycle (normative)

All steps run elevated via a **narrow wrapper** (sudoers grants
the wrapper only — not free-form `useradd` / `userdel` argv).
Password bytes come from the credential vault and are piped on
stdin; they NEVER appear on argv or in plugin state TOML.

The plugin's `add_user` + `revoke_user` verbs straddle three
substrates: (a) plugin persisted state (`smb_server.toml`), (b)
the framework credential vault, (c) the OS NSS + Samba passdb
(reached via the wrapper). A partial commit across those three
leaves an operator-visible-inconsistent state that only manual
sudo recovers from, so both verbs implement atomic-commit
semantics with idempotent-recovery contracts stated here.

**Prerequisite: privileges drop-in.** The framework's reference
`evo.service` bakes `ProtectSystem=strict`, which mounts the
entire filesystem read-only for every descendant regardless of
euid. Samba's `security = user` + `tdbsam` passdb opens
`/var/lib/samba/private/{passdb,secrets}.tdb` for R+W on every
`smbpasswd -a` and returns EROFS without a distribution-scope
carve-out. The distribution ships
`dist/systemd/evo.service.d/samba-server-privileges.conf` with
the narrowest possible relaxation:

```ini
[Service]
ReadWritePaths=/var/lib/samba
```

installed by `dist/scripts/bootstrap.sh` alongside every other
`evo.service.d/*.conf`. Vendor distributions that admit the
smb-server plugin but prefer the unrelaxed posture drop the
file and accept that user provisioning surfaces `smbpasswd -a
failed`; share management (`network.smb_server.apply`) is
unaffected.

**Add** (`network.smb_server.user_add`):

1. Validate username (see below) and refuse blocklisted names.
2. Refuse `CredentialVaultUnavailable` if the plugin's
   `LoadContext.credential_vault` was `None` at load time
   (distinct class from `CredentialMissing` so the operator UI
   renders "vault not wired to this plugin" rather than
   "your password did not save").
3. Refuse `UserAlreadyExists` if the name is already in
   persisted `smb_users`.
4. **Speculatively persist** the record into `smb_users` and
   `state.save` under a single lock scope (closes the race
   window between the duplicate check and the mutation). If
   save fails: discard the in-memory push, surface
   `Persistence` — no side effect has run, operator retries
   cleanly.
5. Fetch password bytes from the vault via `credential_key`.
   On absent entry: roll back the speculative row +
   `state.save`, surface `CredentialMissing`. On rollback save
   failure: surface `AddRollbackFailed` carrying both errors.
6. Fire the wrapper: it **refuses ANY pre-existing NSS entry**
   (strict-refuse-any-NSS gate — the plugin must own every
   NSS entry it attaches Samba credentials to), then
   `useradd -r -s /usr/sbin/nologin -d /nonexistent <name>`
   (system UID range, no login shell, no home), then
   `smbpasswd -a -s <name>` with password piped once on stdin
   (wrapper doubles for `smbpasswd -s`). On wrapper failure or
   subprocess I/O error: roll back the speculative row +
   `state.save`, surface `UserSyncFailed` (or the underlying
   `SubprocessIo`). On rollback save failure: surface
   `AddRollbackFailed` carrying both stderr strings so the
   operator UI can render composite failure and point at
   `user_revoke` as the idempotent recovery gesture.
7. On success: republish `system_smb_server`.

A plugin crash between step 4's save-success and step 6's
wrapper-success leaves a phantom row in state. Recovery: the
operator's next `user_revoke` for that name converges
idempotently (wrapper delete is no-op-safe against an absent
NSS entry and an absent passdb row; vault delete is
idempotent per the vault contract; `state.retain` removes the
plugin row).

**Revoke** (`network.smb_server.user_revoke`):

1. Refuse `UserNotFound` if the name is not in persisted
   `smb_users`.
2. Snapshot `credential_key` from the record before mutating.
3. Fire the wrapper: `smbpasswd -x <name>` (ignore
   already-absent passdb) + `userdel <name>` ONLY when the NSS
   entry matches the provisioned shape (nologin + nonexistent
   home) — foreign NSS accounts are left in place to avoid
   nuking what the plugin does not own.
4. Retract the vault row via `credentials.delete_password`
   (idempotent per the vault contract). On failure surface
   `CredentialDeleteFailed`; the SMB user is already gone at
   this point so the operator UI renders "vault row dangling"
   and offers manual remediation via the credential admin
   surface.
5. In-memory `smb_users.retain(...)` + `state.save`. On save
   failure: re-hydrate the in-memory state from disk via
   `SmbServerState::load(&path)` so memory matches persistence
   (WARN log names the class); return `Persistence`. The
   operator's next `user_revoke` is idempotent across all
   three substrates and converges on the first successful save.
6. On success: republish `system_smb_server`.

An implementation that only calls `smbpasswd` and never
creates/removes the non-login NSS entry, or that skips the
vault-delete step, or that mutates in-memory state without
persisting the same delta, is **non-compliant** with this
inventory.

### Username rules

| Rule | Value |
|------|--------|
| Pattern | `^[a-z_][a-z0-9_-]{0,31}$` (same discipline as volumio-evo sync) |
| Case | Store and pass to Samba in lowercase |
| Blocklist | At minimum: `root`, `nobody`, `nfsnobody`, the configured steward service user, `smbd`, `sshd`, `www-data`. Extensible in code; must include the live service user at runtime. |

### Password rules

| Rule | Detail |
|------|--------|
| Wire | UI never sends password bytes on `user_add`; it sends `credential_key` only. |
| Device | Plugin fetches bytes from the vault at provision time; pipes to the wrapper / `smbpasswd -s`. |
| Persistence | Plugin state TOML holds usernames + vault key refs only — never password material. |

### File ownership (already decided on shares)

Stock, delivery, and extra share sections render
`force user` / `force group` so writes do not depend on a
per-SMB-user UID. Provisioned nologin accounts therefore do
not need group membership on the music / uploads / stage
trees for basic RW via `force user`. If a future share drops
`force user`, this section must be revisited before ship.

### Domain link (optional field)

`SmbUserRecord.mapped_domain_identity` may link an SMB user to
a domain member. Visitor accounts leave it unset. Revoking
domain membership MUST eventually revoke the linked SMB user
(cascade per the network file-source contract). Cascade wiring
is separate from the provision/revoke primitive above; both
must remain consistent.

### Privileges / sudoers

| Grant | Purpose |
|-------|---------|
| Wrapper script only (`/usr/local/bin/evo-smb-user-sync`) | `add` / `delete` actions; password on stdin for add |
| Not granted | Raw `useradd`, `userdel`, or unrestricted `smbpasswd` argv from the steward |

The wrapper is distribution-owned (ships at
`dist/bin/evo-smb-user-sync`, installed by bootstrap),
analogous to volumio-evo's `volumio-evo-smb-user-sync.sh`.
`privileges.yaml` capability `smb_user_provision` covers this
surface and names account create/delete, not `smbpasswd`
alone.

### Acceptance (users)

1. From File Sharing UI, add a **new** username that does not
   exist on the device → succeeds; `getent passwd <name>`
   shows nologin / nonexistent home; `pdbedit -L` lists the
   name; login to `evo-plugins-stage` with that password works.
2. Same name cannot be added twice (structured already-exists).
3. Blocklisted names (steward user, `root`, …) refuse with a
   clear error — no shell workaround suggested.
4. Revoke removes passdb + the nologin NSS entry + persisted
   record; SMB auth with that password fails.
5. Operator never needs SSH/`useradd` for the happy path.

---

## Forbidden on the LAN (never advertise)

After a successful enable/apply, a browse of the device MUST NOT
show:

| Name / class | Why forbidden |
|--------------|----------------|
| `print$` | Debian/Samba printer-driver default. |
| `printers` | Distro printer share. |
| `homes` / per-user home shares | Distro `[homes]`; often surfaces as `nobody` when guest maps to the nobody account. |
| `nobody` | Guest/homes artefact — not a product share. |
| Unsolicited test names | e.g. ad-hoc extras left from bring-up (`WalkTest`). Only operator-chosen extras after stock/delivery. |

Rendered `[global]` MUST disable printer loading (e.g.
`load printers = no`, `printing = bsd`, `printcap name = /dev/null`
or equivalent) so stock Samba printer shares cannot reappear.

---

## LAN acceptance (rig)

With SMB enabled on a cold or warm apply:

1. Browse `\\<device>` / `smb://<device>/`.
2. **Must** list: `Internal Storage`, `USB`, `NAS`, `evo-plugins-stage`, `Uploads`.
3. **Must not** list: `print$`, `nobody`, `homes`, `printers`, or any share not in this inventory / operator extras.
4. Drop a signed plugin bundle on `evo-plugins-stage` → stage watcher admits (or rejects into `rejected/` with reason). Authenticated drop requires an SMB user provisioned per **SMB users** above.
5. Drop a music file on `Internal Storage` → visible under INTERNAL in the library after MPD update.
6. SMB user add/revoke acceptance: see **SMB users → Acceptance**.

---

## UI contract (brief)

| Surface | Behaviour |
|---------|-----------|
| Stock + delivery shares | Shown as fixed (not editable path/name); clarify guest vs auth for `evo-plugins-stage`. |
| Extra shares | List editor (name, path, guest_ok) per `shares.v1.toml`. |
| Enable / min_protocol | Existing File Sharing controls. |
| Device name | The device's LAN identity IS the OS hostname. There is **no** separate netbios-name storage on the runtime or in the plugin state file. `network.smb_server.apply()` reads `/proc/sys/kernel/hostname` at render time and writes it into `smb.conf` as `netbios name`. Read `envelope.hostname` from `network.smb_server.get_state` on load; refresh on every `system_smb_server` subject update. Write via `network.smb_server.apply(system_hostname = <new>)` — that call runs `hostnamectl set-hostname <new>` and then re-renders `smb.conf`, so the next render's `netbios name` reflects the new hostname without a steward restart. Empty string on the envelope is a diagnostic signal (procfs I/O failure) — render the field placeholder, never the empty value. |
| SMB users | Add/list/revoke by username; password via device vault prompt only. UI does not create Unix accounts — the device provision path does. No need to "pick a running system user." |

Copy that claims "share this device's library" is true only while
the stock music shares exist in the rendered conf.

---

## Boundary (do not conflate)

| Plugin | Role |
|--------|------|
| `org.evoframework.network.shares` | **Client** — mount remote NAS/NFS into `/var/lib/evo/music/NAS/<alias>`. |
| `org.evoframework.network.smb-server` | **Server** — export this device on the LAN per this inventory. |

---

## Change discipline

1. Edit this file.
2. Update renderer + allow/deny constants + bootstrap mkdirs + user-provision wrapper/sudoers in the same change set.
3. If the inventory gains/removes a stock or delivery share, or changes the SMB-user provision model, update the companion design record only when the *invariant* changes; do not duplicate tables into that record.
4. Schema (`extra_shares` “beyond shipped defaults”; `smb_users`) stays aligned with the stock/delivery and **SMB users** sections above.
