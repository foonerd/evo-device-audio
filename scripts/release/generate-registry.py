#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Just a Nerd
"""Write one plugin-registry manifest for a channel and a target triple.

Usage:
    generate-registry.py --artefacts PATH --channel dev|test|prod
                         --triple TRIPLE --base-url URL --out FILE
                         [--valid-days N] [--now RFC3339]

Reads the channel pointer file `channels/<channel>.toml` on an
artefacts checkout and writes the steward's plugin-registry manifest
(`[registry]` header plus one `[[plugins]]` entry per plugin) for that
channel and triple.

- A plugin pointer names a cut. The bundle listed for it is
  `bundles/<plugin>/<triple>/<plugin>-<cut>-<triple>.tar.gz`, and the
  listing's version is that bundle's own `[plugin].version`, read from
  the bundle.
- Only `org.evoframework.*` plugins on the out-of-process ship list
  (`dist/scripts/lib/oop-plugins.sh`) are listed. Other pieces the
  pointer file names, such as `evo-device-audio`, are not plugins and
  are left out.
- A plugin pointer with no cut, a named bundle that is not on the
  plane, a bundle whose own name differs, or a version that is not
  semver refuses the whole manifest. Nothing is written in part.

The manifest is not signed here; the caller signs the written bytes.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import re
import sys
import tarfile
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SHIP_LIST = REPO_ROOT / "dist" / "scripts" / "lib" / "oop-plugins.sh"
TRIPLES = (
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
    "armv7-unknown-linux-gnueabihf",
)
CHANNELS = ("dev", "test", "prod")
FINGERPRINT = (
    "sha256:9cd7d7381ee7c2b3bfa490b39077afdc925192299dda661ef94dddba71e574da"
)
SCHEMA_VERSION = 1
SEMVER = re.compile(
    r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?"
)


def fail(message: str) -> None:
    sys.stderr.write(f"generate-registry: {message}\n")
    sys.exit(1)


def ship_list() -> set[str]:
    """The plugin names in the out-of-process ship list."""
    names = set(re.findall(r'"(org\.evoframework\.[^:"]+):', SHIP_LIST.read_text()))
    if not names:
        fail(f"no plugin names found in {SHIP_LIST}")
    return names


def bundle_version(path: Path, plugin: str) -> str:
    """The `[plugin].version` the bundle carries, checked against its name."""
    with tarfile.open(path, "r:gz") as tar:
        members = [
            m for m in tar.getmembers()
            if m.isfile() and m.name.count("/") == 1 and m.name.endswith("/manifest.toml")
        ]
        if len(members) != 1:
            fail(f"{path}: expected one top-level manifest.toml, found {len(members)}")
        handle = tar.extractfile(members[0])
        manifest = tomllib.loads(handle.read().decode("utf-8"))
    section = manifest.get("plugin", {})
    if section.get("name") != plugin:
        fail(f"{path}: bundle names {section.get('name')!r}, not {plugin!r}")
    version = section.get("version", "")
    if not SEMVER.fullmatch(version):
        fail(f"{path}: version {version!r} is not semver")
    return version


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def toml_string(value: str) -> str:
    # JSON string escapes are valid TOML basic-string escapes.
    return json.dumps(value)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--artefacts", required=True, type=Path)
    parser.add_argument("--channel", required=True, choices=CHANNELS)
    parser.add_argument("--triple", required=True, choices=TRIPLES)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--valid-days", type=int, default=90)
    parser.add_argument("--now", default=None)
    args = parser.parse_args()

    base = args.base_url.rstrip("/")
    if not base.startswith("https://"):
        fail(f"base URL {base!r} is not HTTPS")
    now = (
        dt.datetime.fromisoformat(args.now.replace("Z", "+00:00"))
        if args.now
        else dt.datetime.now(dt.timezone.utc)
    ).replace(microsecond=0)
    if args.valid_days < 1:
        fail("--valid-days must be at least 1")
    expires = now + dt.timedelta(days=args.valid_days)

    pointers = tomllib.loads(
        (args.artefacts / "channels" / f"{args.channel}.toml").read_text()
    )
    if pointers.get("channel") != args.channel:
        fail(f"channels/{args.channel}.toml names channel {pointers.get('channel')!r}")
    shipped = ship_list()

    slug = f"evo-audio-{args.channel}"
    rel = f"registries/{slug}/{args.triple}"
    lines = [
        "[registry]",
        f"name = {toml_string(slug)}",
        f"maintainer = {toml_string('org.evoframework')}",
        f"public_key_fingerprint = {toml_string(FINGERPRINT)}",
        f"schema_version = {SCHEMA_VERSION}",
        f"generated_at = {toml_string(now.strftime('%Y-%m-%dT%H:%M:%SZ'))}",
        f"expires_at = {toml_string(expires.strftime('%Y-%m-%dT%H:%M:%SZ'))}",
        f"manifest_url = {toml_string(f'{base}/{rel}/registry.toml')}",
        f"signature_url = {toml_string(f'{base}/{rel}/registry.sig')}",
    ]

    listed = 0
    for pointer in sorted(pointers.get("pointers", []), key=lambda p: p["piece"]):
        plugin, cut = pointer["piece"], pointer["version"]
        if not plugin.startswith("org.evoframework."):
            continue
        if plugin not in shipped:
            fail(f"{plugin} is not on the out-of-process ship list")
        if not cut:
            fail(f"channels/{args.channel}.toml names no cut for {plugin}")
        bundle = f"bundles/{plugin}/{args.triple}/{plugin}-{cut}-{args.triple}.tar.gz"
        path = args.artefacts / bundle
        if not path.is_file():
            fail(f"{bundle} is not on the plane")
        lines += [
            "",
            "[[plugins]]",
            f"name = {toml_string(plugin)}",
            f"plugin_kind = {toml_string('functional')}",
            f"trust_class = {toml_string('platform')}",
            f"architectures = [{toml_string(args.triple)}]",
            "",
            "[[plugins.versions]]",
            f"version = {toml_string(bundle_version(path, plugin))}",
            f"url = {toml_string(f'{base}/{bundle}')}",
            f"sha256 = {toml_string(sha256(path))}",
        ]
        listed += 1
    if listed == 0:
        fail(f"channels/{args.channel}.toml names no plugin")

    body = "\n".join(lines) + "\n"
    tomllib.loads(body)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    tmp = args.out.with_suffix(args.out.suffix + ".tmp")
    tmp.write_text(body)
    tmp.replace(args.out)
    print(f"{args.out}: {listed} plugins, expires {expires:%Y-%m-%dT%H:%M:%SZ}")


if __name__ == "__main__":
    main()
