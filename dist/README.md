# `dist/` — distribution-tier provisioning artefacts

Reference materials for an evo-device-audio deployment.
The framework's steward (`evo-device-audio` binary, built
from `crates/evo-device-audio-distribution`) provides the
plugin admission + dispatch + custody substrate. The
host's system services (MPD, ALSA) provide the audio path.
This directory holds the materials that bridge them on a
target host.

## Contents

- `catalogue/audio-rack.toml` — catalogue extension
  declaring the `audio` rack with `composition` + `playback`
  shelves, plus the subject types (`track`, `album`) and
  relation predicates (`album_of`, `tracks_of`) the audio
  playback warden announces on every song change. Vendor
  distributions include this fragment in their full
  catalogue alongside any other racks they admit.
- `mpd/evo-fragment.conf` — boot-time MPD fragment. Carries
  the AAMPP-compliant `device "evo"` pointing at the
  `pcm.evo` pipeline (see `alsa/99-evo.conf`). The
  fragment-writer worker rewrites `/etc/evo/mpd.conf` at
  every route-change from the framework-negotiated
  WriteEndpoint once a topology is published; this static
  form keeps MPD operational at boot before reconciliation
  has run.
- `alsa/asound.conf` — AAMPP modular ALSA pipeline.
  Defines `pcm.evo` as the single entry point every
  audio-producing plugin writes to: `plug` (handles format
  negotiation) → `hw:CARD=DAC,DEV=0` (hardware terminus,
  kernel-stable card name). The bootstrap script installs
  this file at `/etc/asound.conf` (system-wide; ALSA reads
  it at every PCM open). Future AAMPP modules (soxr
  resampler, EQ, room correction, peppyalsa visualisation)
  are layered in via the same file under operator control,
  or via the `delivery.alsa` plugin's authority once it
  lands. Existing `/etc/asound.conf` content is backed up
  to `/etc/asound.conf.pre-evo.<timestamp>` before
  overwrite; idempotent on re-run.
- `systemd/evo.service.d/state-dir-mode.conf` — drop-in
  override widening `StateDirectoryMode` to `0755` so MPD
  can traverse `/var/lib/evo/` to reach the music tree.
  Required only when the music library lives under
  `/var/lib/evo/music/`; vendor distributions that route
  the music tree elsewhere can omit it.
- `systemd/evo.service.d/mpd-restart-privileges.conf` —
  drop-in that relaxes the framework's reference hardening
  to permit the playback.mpd plugin's fragment-write +
  sudo-systemctl-restart legs. Adds
  `ReadWritePaths=/etc/evo`, `NoNewPrivileges=no`, and
  `Environment=EVO_SYSTEMCTL=/usr/bin/systemctl`. Vendor
  distributions that prefer to run the steward as root, or
  that park the playback.mpd restart leg, can omit this
  drop-in.
- `sudoers.d/evo-mpd-restart.in` — narrow NOPASSWD sudoers
  template granting the steward service user the exact
  command `/usr/bin/systemctl restart mpd` (only). The
  bootstrap script substitutes `@EVO_SERVICE_USER@` and
  installs at `/etc/sudoers.d/evo-mpd-restart` after
  validating syntax via `visudo -c`.
- `scripts/bootstrap.sh` — idempotent installer that lays
  down all of the above artefacts, resolves the appliance-
  class service user, chowns `/etc/evo/mpd.conf` to that
  user, and runs a verification preflight at the end. The
  framework's admission-time Privilege Preflight Admission
  Gate consumes the resulting state; the bootstrap is the
  dual that creates it.

## Bring-up procedure (reference target)

The steps captured during the release verification:

1. Install MPD: `apt install mpd alsa-utils mpc`.
2. Set up the music-library layout at
   `/var/lib/evo/music/{INTERNAL,USB,NAS}` with appropriate
   ownership / perms (`evo:audio` owner, `0755` dirs,
   `0644` files for the reference; vendor distributions
   choose their own service user).
3. Edit `/etc/mpd.conf`: set `music_directory` to
   `/var/lib/evo/music`; append
   `include_optional /etc/evo/mpd.conf`.
4. Compose the distribution's catalogue from
   `dist/catalogue/audio-rack.toml` plus any other racks
   the distribution admits, drop the result at
   `/opt/evo/catalogue/default.toml`.
5. Run the bootstrap script as root. It resolves the
   appliance-class service user, installs the systemd
   drop-ins, installs the sudoers drop-in (validated via
   `visudo -c`), seeds `/etc/evo/mpd.conf` owned by the
   service user, and runs a preflight that verifies the
   install:

   ```bash
   sudo dist/scripts/bootstrap.sh
   ```

   Or, with an explicit service user:

   ```bash
   sudo dist/scripts/bootstrap.sh --service-user evo
   ```

   Toggle individual steps with
   `EVO_INSTALL_MPD_SUDOERS=0`,
   `EVO_INSTALL_SYSTEMD_DROP_INS=0`,
   `EVO_INSTALL_MPD_FRAGMENT=0`.
6. Cross-build + deploy the steward binary
   (`scripts/cross-build.sh aarch64-unknown-linux-gnu
   --release --features alsa-substrate -p
   evo-device-audio-distribution` then `scp` to
   `/opt/evo/bin/evo`).
7. Restart `evo.service`; verify both plugins admit
   (`journalctl -u evo.service | grep "plugin admitted"`)
   and that the playback.mpd plugin resolved its restart
   strategy (`journalctl -u evo.service | grep "MPD
   restart strategy resolved"` — log line names the
   strategy: `direct` for root, `sudo` for the service
   user, or `no_op_disabled` when the framework's
   preflight refused).

## Verification

`evo-plugin-tool plan fire <plan_id>` against a plan
referencing an `mpd-path:...` URI dispatches via the
framework's source-verb dispatcher → warden custody
bootstrap → playback warden's request handler → MPD
load+play sequence → ALSA output to the configured DAC.
