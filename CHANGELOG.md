# Changelog

All notable changes to evo-device-audio are recorded here.

## Unreleased

- Stored playlists: a playlist's view lists a network stream by the title,
  artist, album and composer the player last knew it by, where the server
  gave the playlist none (MPD reads no tag from an HTTP stream, so a saved
  playlist of DLNA tracks showed bare addresses). Only fields MPD left empty
  are filled; a library row is listed as MPD gives it, and a stream the
  player has never seen is still listed without a title.
  `org.evoframework.playback.mpd` is 0.1.4.
- Backup restore: the mixer (`mixer_type`, `mixer_device`, `mixer_control`)
  goes back whole or not at all. A hardware mixer is put back only if the
  device has that control on that card (read through `delivery.list_mixers`);
  otherwise the device keeps its own mixer and the restore reports one
  item, `mixer`, naming it. A backup taken on one device no longer points
  another device's volume control at a mixer it does not have.
- Ship list: the DLNA renderer and the AirPlay receiver
  (`org.evoframework.receiver.dlna`, `org.evoframework.receiver.airplay`)
  are on `dist/scripts/lib/oop-plugins.sh`, the one list the installer
  bundle, the redeploy script and the published pieces are built from. A
  redeploy no longer removes them, and `oop-list-parity.test.sh` fails when
  a plugin in the tree is missing from the list.
- In-app updater: the `org.evoframework.system.updater` plugin and a root
  helper (`evo-apply-update`) that checks a signed registry, applies one
  plugin at a time with the previous bundle kept and a restore if the new
  one is not admitted, delivers the host wrappers and sudoers grants, and
  writes the installed release record. See `dist/UPDATER.md`.

## [0.1.13] - 2026-09-01

First public release of the evo-device-audio plugin commons for
the audio domain: brand-neutral plugins signed by the evo project,
consumed by any audio distribution that admits them.

- Full plugin population for the audio domain (playback, metadata,
  artwork, ALSA composition + delivery, network shares, USB
  storage, kiosk system integration, notifications, power).
- Signed by the commons plugin key; verifier public half shipped
  at keys/commons-plugin-signing-public.pem.
- Cross-architecture bundles (x86_64-unknown-linux-gnu,
  aarch64-unknown-linux-gnu) published to
  foonerd/evo-device-audio-artefacts as v0.1.13 GH Release.
