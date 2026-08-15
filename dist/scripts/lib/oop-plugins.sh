# Shared OOP plugin tuple list for evo-device-audio.
#
# Sourced by:
#   - dist/scripts/build-bundle.sh      (online-installer artefact)
#   - dist/scripts/deploy-distribution.sh (dev/test redeploy path)
#
# Format per entry:
#   <plugin-name>:<plugin-crate>:<wire-binary-name>:<features>
#
# BOTH consumers MUST source this file. A stale copy in either
# script is what produced the 2026-08-07 fleet wipe incident:
# build-bundle shipped 13 plugins while deploy carried 18, so
# `--mode=reinstall` silently deleted shares / smb-server /
# notifications / terminus / metadata.online and left the UI
# shelves dead.
#
# shellcheck shell=bash

OOP_PLUGINS=(
    "org.evoframework.artwork.local:org-evoframework-artwork-local:artwork-local-wire:"
    "org.evoframework.artwork.online:org-evoframework-artwork-online:artwork-online-wire:"
    "org.evoframework.network:org-evoframework-network:network-wire:"
    "org.evoframework.metadata.local:org-evoframework-metadata-local:metadata-local-wire:"
    "org.evoframework.metadata.online:org-evoframework-metadata-online:metadata-online-wire:"
    "org.evoframework.hardware.audio-config:org-evoframework-hardware-audio-config:hardware-audio-config-wire:"
    "org.evoframework.playback.options:org-evoframework-playback-options:playback-options-wire:"
    "org.evoframework.composition.alsa:org-evoframework-composition-alsa:composition-alsa-wire:alsa-substrate"
    "org.evoframework.delivery.alsa:org-evoframework-delivery-alsa:delivery-alsa-wire:"
    "org.evoframework.playback.mpd:org-evoframework-playback-mpd:playback-mpd-wire:"
    "org.evoframework.multiroom.evo-native:org-evoframework-multiroom-evo-native:multiroom-evo-native-wire:alsa-substrate"
    "org.evoframework.audio.terminus:org-evoframework-audio-terminus:audio-terminus-wire:alsa-substrate"
    "org.evoframework.system.power:org-evoframework-system-power:system-power-wire:"
    "org.evoframework.network.shares:org-evoframework-network-shares:network-shares-wire:"
    "org.evoframework.network.smb-server:org-evoframework-network-smb-server:network-smb-server-wire:"
    "org.evoframework.system.notifications:org-evoframework-system-notifications:notifications-wire:"
    "org.evoframework.system.kiosk:org-evoframework-system-kiosk:system-kiosk-wire:"
    "org.evoframework.source.dlna:org-evoframework-source-dlna:source-dlna-wire:"
    "org.evoframework.storage.usb:org-evoframework-storage-usb:storage-usb-wire:"
)

# Expected admitted-plugin count after a clean install/reinstall.
# Kept as a named constant so evo-install.sh post-condition and
# INSTALL-REQUIREMENTS.md stay aligned with this list.
OOP_PLUGINS_EXPECTED_COUNT="${#OOP_PLUGINS[@]}"
