# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Just a Nerd
#
# Cross-build image for the evo-device-audio steward + plugin
# wire binaries, targeting armv7-unknown-linux-gnueabihf. Base
# pinned to `debian:trixie` for runtime-parity ABI (see the
# x86_64 sibling for the rationale).
#
# Same shape as the aarch64 sibling with `armhf` instead of
# `arm64` as the target dpkg arch and `arm-linux-gnueabihf`
# instead of `aarch64-linux-gnu` as the cross-toolchain prefix.
#
# Compatible with cross-rs 0.2.5's `[target.<triple>] dockerfile`
# mechanism.

FROM debian:trixie

ENV DEBIAN_FRONTEND=noninteractive

RUN dpkg --add-architecture armhf && \
    apt-get update && apt-get install -y --no-install-recommends \
      build-essential \
      pkg-config \
      ca-certificates \
      gcc-arm-linux-gnueabihf \
      libc6-dev:armhf \
      libasound2-dev:armhf \
    && rm -rf /var/lib/apt/lists/*
# libc6-dev:armhf required for the same reason libc6-dev:arm64 is
# required in the aarch64 sibling — cc-rs cross builds need the
# multiarch bits/ headers.

ENV CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
    PKG_CONFIG_ALLOW_CROSS_armv7_unknown_linux_gnueabihf=1 \
    PKG_CONFIG_PATH_armv7_unknown_linux_gnueabihf=/usr/lib/arm-linux-gnueabihf/pkgconfig \
    PKG_CONFIG_SYSROOT_DIR_armv7_unknown_linux_gnueabihf=/
