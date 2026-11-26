#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Just a Nerd
#
# test-generate-host-files.sh — the host-files generator.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
GEN="$HERE/generate-host-files.py"
W="$(mktemp -d)"; trap 'rm -rf "$W"' EXIT
PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "PASS  $1"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL  $1"; }

gen() { python3 "$GEN" --release 0.1.13.3 --triple aarch64-unknown-linux-gnu \
    --carrier-out "$1/c.tar.gz" --carrier-url https://x.example/c.tar.gz --out "$1/h.toml" \
    --now 2026-11-27T10:00:00Z --built-at 2026-11-27T09:00:00Z "${@:2}"; }

mkdir -p "$W/a" "$W/b"; gen "$W/a"; gen "$W/b"
cmp -s "$W/a/c.tar.gz" "$W/b/c.tar.gz" && cmp -s "$W/a/h.toml" "$W/b/h.toml" && ok "two runs write identical bytes" || bad "two runs write identical bytes"

python3 - "$W/a" "$REPO" <<'PY' && ok "document, carrier and allow list agree" || bad "document, carrier and allow list agree"
import sys, tomllib, tarfile, hashlib
d, repo = sys.argv[1], sys.argv[2]
doc = tomllib.load(open(f"{d}/h.toml", "rb"))
allow = tomllib.load(open(f"{repo}/dist/etc-updater/allow.toml", "rb"))["file"]
assert doc["host"]["release"] == "0.1.13.3" and doc["host"]["architecture"] == "aarch64"
assert doc["host"]["sha256"] == hashlib.sha256(open(f"{d}/c.tar.gz", "rb").read()).hexdigest()
assert doc["host"]["expires_at"] == "2027-02-25T10:00:00Z", doc["host"]["expires_at"]
want = sorted((f["kind"], f["name"]) for f in allow)
got = sorted((f["kind"], f["name"]) for f in doc["files"])
assert want == got, (want, got)
tf = tarfile.open(f"{d}/c.tar.gz")
names = sorted(m.name for m in tf.getmembers())
assert len(names) == len(allow), names
for f in doc["files"]:
    rel = f"bin/{f['name']}" if f["kind"] == "bin" else f"sudoers.d/{f['name']}.in"
    assert hashlib.sha256(tf.extractfile(rel).read()).hexdigest() == f["sha256"], rel
for m in tf.getmembers():
    assert m.mtime == 0 and m.uid == 0 and m.isreg(), m.name
PY

# An allowed name with no source refuses the whole run and writes nothing.
mkdir -p "$W/fake/dist/bin" "$W/fake/dist/sudoers.d" "$W/c"
cp -r "$REPO/dist/bin/." "$W/fake/dist/bin/"; cp -r "$REPO/dist/sudoers.d/." "$W/fake/dist/sudoers.d/"
rm "$W/fake/dist/bin/evo-rtc-wake"
if gen "$W/c" --root "$W/fake" 2>/dev/null; then bad "an allowed file with no source is refused"; else ok "an allowed file with no source is refused"; fi
[[ ! -e "$W/c/h.toml" ]] && ok "nothing is written on refusal" || bad "nothing is written on refusal"

if gen "$W/c" --fingerprint sha256:zz 2>/dev/null; then bad "a malformed fingerprint is refused"; else ok "a malformed fingerprint is refused"; fi
if python3 "$GEN" --release 0.1.13.3 --triple aarch64-unknown-linux-gnu --carrier-out "$W/c/x" --carrier-url http://x/c --out "$W/c/y" 2>/dev/null; then bad "an http carrier url is refused"; else ok "an http carrier url is refused"; fi
echo "generate-host-files: $PASS passed, $FAIL failed"; [[ $FAIL -eq 0 ]]
