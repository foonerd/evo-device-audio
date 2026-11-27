# NOTICE — evo-airplay

`evo-airplay` is the AirPlay 2 audio receiver library used by the AirPlay
receiver plugin. It receives buffered AirPlay 2 audio and hands the decoded
PCM and session events to its host. It is part of this repository and is
maintained here.

## Where it comes from

It is derived from **openairplay2 0.5.0** by Stefan Arentz, released under the
MIT License (`LICENSE-UPSTREAM-MIT` carries the upstream licence text and
copyright notice, as the licence requires).

| | |
| --- | --- |
| Upstream crate | `openairplay2` 0.5.0 on crates.io |
| Upstream repository | <https://github.com/st3fan/openairplay2> |
| Upstream archive sha256 | `eb3e2f41e0cb2f4c0519d7c1287abc751cb149de6bb7989a5e75f73c4ffc57a0` |
| Upstream licence | MIT, copyright 2026 Stefan Arentz |

Files that descend from upstream carry both notices in their SPDX line
(`Apache-2.0 AND MIT`): the upstream portions are MIT, our modifications are
released under the repository's Apache-2.0 licence.

### Changes made here

- Builds and tests on the repository's pinned Rust toolchain (1.85).
- Logging uses `tracing`, as the rest of the repository does.
- Public names, the default receiver name and the advertised model string are
  this project's.

Further changes are listed here as they land.

## Interoperability data derived from Apple's AirPlay

Apple devices only send audio to a receiver that answers a fixed FairPlay
`fp-setup` exchange. This library answers it with four 142-byte reply tables
and a 12-byte header. They are not original to this project or to openairplay2:
they are widely circulated interoperability constants derived from Apple's
AirPlay, also found in other open-source AirPlay receivers.

- They are used only so that Apple devices will send audio to this device.
- The MIT licence of this code does not grant rights in Apple's material, and
  this notice does not claim any.
- They are kept in a single source file, separate from all other code.
- The AirPlay receiver is off by default and is switched on by the operator.

This is a statement of what the material is, not legal advice.

## Third-party libraries

Rust dependencies carry their own licences (predominantly MIT or Apache-2.0).
One is not permissive: `symphonia` (AAC decoding) is MPL-2.0, a file-level
copyleft licence. None of its files are modified here.

## Protocol reference

The AirPlay 2 protocol handling in openairplay2 was written with reference to
shairport-sync (MIT, copyright James Laird and Mike Brady).
