# evo-sbc

A decoder for SBC, the audio codec every Bluetooth A2DP device must support.
Frames in, 16-bit PCM out. Safe Rust, no dependencies, no `unsafe`.

The decoder is original code, written from the public codec specification. It
reproduces the reference fixed-point decoder's output **bit for bit** (the
conformance vectors in `tests/vectors` are compared sample for sample), including
the wrap-around of that decoder's 32-bit sums on frames whose scale factors and bit
counts no encoder produces from real audio.

## Provenance of the data it carries

* **Prototype filter.** The 40 and 80 coefficients of the synthesis prototype
  (`PROTO_4`, `PROTO_8` in `src/tables.rs`) are numeric data of the codec
  specification. They were transcribed from the table file of the `mini_sbc` crate,
  version 0.1.7 (`MIT OR Apache-2.0`; the `.crate` archive has the SHA-256
  `0522eb0199121b29e0ef8d3f3ff595914353173c116655c5f7b96ebb7458b830`), and are
  checked here by the conformance vectors rather than trusted. None of that
  crate's code is used. The integer tables the decoder runs on are derived from
  these coefficients by the single rule stated at the head of `src/tables.rs`; a
  test recomputes them from the rule and requires them to match.
* **Conformance vectors.** `tests/vectors/*.sbcv` are pairs of SBC frames and the
  PCM that the reference decoder (BlueZ's `libsbc`) makes of them, produced by
  `tests/vectors/generate.py`. The library is run on a development machine to make
  the files; it is not linked into, built with, or distributed with the decoder.
  One vector (`real_a2dp_…`) is a slice of a recording of a real A2DP source.

## Licence

Apache-2.0, like the rest of this repository.
