# Kernel codec table imports (build-time only)

These files are verbatim copies of Linux kernel source carrying the
HDA + AC97 codec identification tables the framework scrapes at
build time to produce the `[[hda_codecs]]` and `[[ac97_codecs]]`
sections in `data/alsa-cards.toml`.

The runtime DOES NOT read these `.c` files — only the importer
under `src/import.rs` does, and only when explicitly invoked via
`cargo run --example regen_codec_catalogues`. The runtime reads the
generated TOML.

## Source provenance

All files were fetched verbatim from `git.kernel.org`:

```
https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/plain/sound/pci/hda/patch_*.c?h=v6.6
https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/plain/sound/pci/ac97/ac97_codec.c?h=v6.6
```

Kernel tag: v6.6 (LTS). Bumping the tag is a deliberate maintainer
action — re-fetch the files, re-run the regen example, commit the
new TOML output. The regression-guard test
(`importer_output_matches_checked_in_catalogue`) pins the
deterministic byte-equal round-trip.

## Licensing

Every file in this directory ships with its original Linux kernel
header (`// SPDX-License-Identifier: GPL-2.0-or-later` or
equivalent). Inclusion in this repository is build-time use of
publicly-published kernel data tables to produce a structurally
distinct artefact (the TOML catalogue). The TOML catalogue carries
mechanical data extracted from these tables — codec IDs + chip
names — which are factual identifiers (not creative expression).

Distribution of these `.c` files alongside the framework respects
their GPL-2-or-later license: anyone receiving this repository
receives the kernel source verbatim under the kernel's license.
The framework does NOT statically or dynamically link against
this code; it parses it as text at build time only.

## Re-generating the catalogue

```bash
# From the delivery.alsa plugin directory:
cargo run --example regen_codec_catalogues
```

The example rewrites the `[[hda_codecs]]` and `[[ac97_codecs]]`
sections of `data/alsa-cards.toml` in place from the source files
in this directory. The per-card-name `[[cards]]` section is
preserved unchanged.
