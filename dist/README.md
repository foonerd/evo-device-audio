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
- `mpd/evo-fragment.conf` — MPD `audio_output` fragment
  declaring the device the steward's playback warden
  drives. Static form for verification; the
  substrate-aware shape lands as part of the playback
  warden's fragment-writer arc.
- `systemd/evo.service.d/state-dir-mode.conf` — drop-in
  override widening `StateDirectoryMode` to `0755` so MPD
  can traverse `/var/lib/evo/` to reach the music tree.
  Required only when the music library lives under
  `/var/lib/evo/music/`; vendor distributions that route
  the music tree elsewhere can omit it.

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
4. Drop `dist/mpd/evo-fragment.conf` to `/etc/evo/mpd.conf`
   and tune the `device` line for the target's DAC.
5. Drop `dist/systemd/evo.service.d/state-dir-mode.conf`
   to `/etc/systemd/system/evo.service.d/` and run
   `systemctl daemon-reload`.
6. Compose the distribution's catalogue from
   `dist/catalogue/audio-rack.toml` plus any other racks
   the distribution admits, drop the result at
   `/opt/evo/catalogue/default.toml`.
7. Cross-build + deploy the steward binary
   (`scripts/cross-build.sh aarch64-unknown-linux-gnu
   --release --features alsa-substrate -p
   evo-device-audio-distribution` then `scp` to
   `/opt/evo/bin/evo`).
8. Restart `evo.service`; verify both plugins admit
   (`journalctl -u evo.service | grep "plugin admitted"`).

## Verification

`evo-plugin-tool plan fire <plan_id>` against a plan
referencing an `mpd-path:...` URI dispatches via the
framework's source-verb dispatcher → warden custody
bootstrap → playback warden's request handler → MPD
load+play sequence → ALSA output to the configured DAC.
