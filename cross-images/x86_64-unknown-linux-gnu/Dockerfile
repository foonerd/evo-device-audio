# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Just a Nerd
#
# Cross-build image for the evo-device-audio steward + plugin
# wire binaries, targeting x86_64-unknown-linux-gnu.
#
# Base pinned to `debian:trixie` — the same OS + package set
# the target devices run — so build-time libasound2-dev links
# against the SAME symbol version and vendor patch level the
# runtime dynamic loader presents. Cross-rs's default
# `ghcr.io/cross-rs/*:main` images resolve to Ubuntu 20.04
# focal; the resulting binaries link against Ubuntu-vendored
# libraries at build time but load Debian-vendored libraries at
# run time. For mature ABIs (libasound2) that happens to work.
# For newer or vendor-sensitive ABIs it does not (see
# evo-kiosk-eng's Cross.toml rationale). We pick discipline
# over luck: build environment matches runtime environment
# exactly.
#
# Native build: this image runs on x86_64 hosts and produces
# x86_64 binaries. No cross-linker + no dpkg add-arch needed.
# Sibling images (aarch64 + armv7) fence a target arch and
# install its cross-linker.
#
# Compatible with cross-rs 0.2.5's `[target.<triple>] dockerfile`
# mechanism. Cross-rs mounts the workspace at /project and the
# host's cargo + rustup dirs at runtime; the image supplies base
# OS + native toolchain + dev libraries only.

FROM debian:trixie

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential \
      pkg-config \
      ca-certificates \
      libasound2-dev \
    && rm -rf /var/lib/apt/lists/*
