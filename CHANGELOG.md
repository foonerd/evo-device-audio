# Changelog

All notable changes to evo-device-audio are recorded here.

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
