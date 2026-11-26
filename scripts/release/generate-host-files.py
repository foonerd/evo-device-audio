#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Just a Nerd
"""Write the host-files carrier and its document for one channel and triple.

Usage:
    generate-host-files.py --release R --triple TRIPLE --carrier-out FILE
                           --carrier-url URL --out FILE
                           [--built-at RFC3339] [--valid-days N]
                           [--fingerprint sha256:HEX] [--now RFC3339]

The carrier is a tar.gz of `bin/<name>` (from `dist/bin/`) and
`sudoers.d/<name>.in` (from `dist/sudoers.d/`), exactly the files
`dist/etc-updater/allow.toml` names and no others. The document is
`host-files.toml`: a `[host]` header (release, architecture,
built_at_utc, the pinned key, expiry, the carrier URL and sha256) and
one `[[files]]` entry per carried file with its sha256.

The worker refuses a carrier that names a file the box's own
allow list does not, so the generator reads the same list and refuses
when a listed file has no source. The tarball is byte-reproducible: sorted
names, zero mtime, root ownership, fixed modes.

Neither file is signed here; the caller signs the written document.
"""

from __future__ import annotations

import argparse
import datetime as dt
import gzip
import hashlib
import io
import re
import sys
import tarfile
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
ARCH = {
    "aarch64-unknown-linux-gnu": "aarch64",
    "x86_64-unknown-linux-gnu": "x86_64",
    "armv7-unknown-linux-gnueabihf": "armv7",
}
COMMONS = "sha256:9cd7d7381ee7c2b3bfa490b39077afdc925192299dda661ef94dddba71e574da"
PLAIN = re.compile(r"[0-9A-Za-z._+-]{1,64}")
RFC = "%Y-%m-%dT%H:%M:%SZ"


def fail(msg: str) -> "None":
    print(f"generate-host-files: {msg}", file=sys.stderr)
    sys.exit(1)


def sources(root: Path) -> list[tuple[str, str, str, Path]]:
    """(kind, name, path inside the carrier, source file) for every allowed file."""
    allow = tomllib.loads((root / "dist" / "etc-updater" / "allow.toml").read_text())
    out = []
    for f in allow.get("file", []):
        kind, name = f["kind"], f["name"]
        if kind == "bin":
            src, rel = root / "dist" / "bin" / name, f"bin/{name}"
        elif kind == "sudoers":
            src, rel = root / "dist" / "sudoers.d" / f"{name}.in", f"sudoers.d/{name}.in"
        else:
            fail(f"allow.toml names an unknown kind {kind!r}")
        if not src.is_file():
            fail(f"allow.toml names {kind} {name}, which has no source at {src}")
        out.append((kind, name, rel, src))
    out.sort(key=lambda t: t[2])
    return out


def build_carrier(files: list[tuple[str, str, str, Path]]) -> bytes:
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT) as tf:
        for _kind, _name, rel, src in files:
            data = src.read_bytes()
            ti = tarfile.TarInfo(rel)
            ti.size, ti.mtime, ti.mode = len(data), 0, 0o644
            ti.uid = ti.gid = 0
            ti.uname = ti.gname = ""
            tf.addfile(ti, io.BytesIO(data))
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=0, compresslevel=9) as gz:
        gz.write(raw.getvalue())
    return out.getvalue()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--release", required=True)
    ap.add_argument("--triple", required=True)
    ap.add_argument("--carrier-out", required=True)
    ap.add_argument("--carrier-url", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--built-at")
    ap.add_argument("--valid-days", type=int, default=90)
    ap.add_argument("--fingerprint", default=COMMONS)
    ap.add_argument("--now")
    ap.add_argument("--root", default=str(REPO_ROOT))
    a = ap.parse_args()

    if a.triple not in ARCH:
        fail(f"unknown triple {a.triple}")
    if not PLAIN.fullmatch(a.release):
        fail("release is not a plain token")
    if not a.carrier_url.startswith("https://"):
        fail("carrier url must be https")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", a.fingerprint):
        fail("fingerprint is not sha256:<64 hex>")
    now = dt.datetime.strptime(a.now, RFC) if a.now else dt.datetime.now(dt.timezone.utc).replace(tzinfo=None)
    built = a.built_at or now.strftime(RFC)
    dt.datetime.strptime(built, RFC)
    expires = (now + dt.timedelta(days=a.valid_days)).strftime(RFC)

    files = sources(Path(a.root))
    carrier = build_carrier(files)
    Path(a.carrier_out).parent.mkdir(parents=True, exist_ok=True)
    Path(a.carrier_out).write_bytes(carrier)

    lines = [
        "[host]",
        f'release = "{a.release}"',
        f'architecture = "{ARCH[a.triple]}"',
        f'built_at_utc = "{built}"',
        f'public_key_fingerprint = "{a.fingerprint}"',
        "schema_version = 1",
        f'expires_at = "{expires}"',
        f'url = "{a.carrier_url}"',
        f'sha256 = "{hashlib.sha256(carrier).hexdigest()}"',
        "",
    ]
    for kind, name, _rel, src in files:
        lines += [
            "[[files]]",
            f'kind = "{kind}"',
            f'name = "{name}"',
            f'sha256 = "{hashlib.sha256(src.read_bytes()).hexdigest()}"',
            "",
        ]
    Path(a.out).parent.mkdir(parents=True, exist_ok=True)
    Path(a.out).write_text("\n".join(lines))


if __name__ == "__main__":
    main()
