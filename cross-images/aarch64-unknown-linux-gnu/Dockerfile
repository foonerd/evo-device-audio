# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Just a Nerd
#
# Cross-build image for the evo-device-audio steward + plugin
# wire binaries, targeting aarch64-unknown-linux-gnu. Base
# pinned to `debian:trixie` for runtime-parity ABI (see the
# x86_64 sibling for the rationale).
#
# Cross toolchain shape:
#
#   - Host arch (amd64) build-essential for build scripts.
#   - Cross-linker for aarch64: gcc-aarch64-linux-gnu.
#   - dpkg multi-arch: arm64 enabled so apt can install the
#     target-arch dev libraries alongside amd64 build tools.
#   - Target-arch dev library: libasound2-dev:arm64. Debian
#     trixie carries the same upstream + vendor patch level as
#     amd64, so cross-build and native x86_64 build produce
#     binaries that link against symbol-equivalent runtime libs.
#   - pkg-config env vars: cargo (via alsa-sys build script)
#     resolves pkg-config against target-arch .pc files under
#     /usr/lib/aarch64-linux-gnu/pkgconfig.
#
# Compatible with cross-rs 0.2.5's `[target.<triple>] dockerfile`
# mechanism.

FROM debian:trixie

ENV DEBIAN_FRONTEND=noninteractive

RUN dpkg --add-architecture arm64 && \
    apt-get update && apt-get install -y --no-install-recommends \
      build-essential \
      pkg-config \
      ca-certificates \
      gcc-aarch64-linux-gnu \
      libc6-dev:arm64 \
      libasound2-dev:arm64 \
    && rm -rf /var/lib/apt/lists/*
# libc6-dev:arm64 is required in addition to gcc-aarch64-linux-gnu's
# built-in cross libc. cc-rs-invoked cross builds (e.g. aws-lc-sys /
# ring / rustls native backends) include <stdlib.h> which resolves
# under Debian's multiarch layout to /usr/include/stdlib.h and
# indirects to bits/libc-header-start.h. That bits/ header lives in
# libc6-dev:arm64's /usr/include/aarch64-linux-gnu/bits/ tree; without
# the :arm64 install the cross-compile fails with
# `fatal error: bits/libc-header-start.h: No such file or directory`.

ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    PKG_CONFIG_ALLOW_CROSS_aarch64_unknown_linux_gnu=1 \
    PKG_CONFIG_PATH_aarch64_unknown_linux_gnu=/usr/lib/aarch64-linux-gnu/pkgconfig \
    PKG_CONFIG_SYSROOT_DIR_aarch64_unknown_linux_gnu=/
