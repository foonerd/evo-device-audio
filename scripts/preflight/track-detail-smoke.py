#!/usr/bin/env python3
"""Deploy gate: track_detail full-source smoke.

Rig-side smoke that fails a deploy the moment a rig's
`track_detail` composite returns `status: error` on any of the
first-tier sources.

Guarantees UI-team's non-negotiable: "A dropped payload_b64 must
fail CI, never a listening device." Half-landed wire protocol
work that drops `payload_b64` from any plugin's response
manifests here as `status: error` on `metadata_local` (or
similar). This gate catches it before the deploy publishes.

Invocation
----------

    scripts/preflight/track-detail-smoke.py <TARGET_HOST> <TARGET_USER>

Exit codes
----------

    0 — every named source returned non-error (`ok`,
        `not_found`, or `not_configured` — all legitimate).
    1 — operator misuse (bad args, ssh refused, MPD unhealthy,
        no bearer preseed on target).
    2 — track_detail endpoint returned non-200 or malformed
        body.
    3 — smoke FAIL: at least one source returned `error`. The
        caller (deploy-distribution.sh) MUST refuse to publish
        + restore `.prev` on this rig.

Design notes
------------

- Fixture track is auto-discovered via `mpc listall | head -1`
  on the target. A rig with no music is not a rig the smoke
  can honestly attest.
- Bearer is minted via the framework's `pair_complete`
  bootstrap-preseed handshake, the same path
  keyless-first-probe.py uses.
- Named sources checked: `metadata_local`, `reconciliation`,
  `artist_bio`, `album_notes`, `lyrics`. `artwork` is not in
  the gate list because artwork lives in its own plugin plane;
  the sources UI called out are the metadata / library plane.
- `not_configured` is legitimate (rig has no API keys). Only
  `status: error` fails the gate — that's the wire-dispatch
  failure the gate exists to catch.
"""
import argparse
import asyncio
import base64
import json
import ssl
import subprocess
import sys

import websockets

PRESEED_PATH = "/boot/evo/pair-preseed.txt"

GATED_SOURCES = (
    "metadata_local",
    "reconciliation",
    "artist_bio",
    "album_notes",
    "lyrics",
)


def ssl_ctx():
    c = ssl.create_default_context()
    c.check_hostname = False
    c.verify_mode = ssl.CERT_NONE
    return c


def ssh(target_user, target_host, cmd):
    """Run a command on the target rig, return stdout."""
    return subprocess.check_output(
        [
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            f"{target_user}@{target_host}",
            cmd,
        ],
        text=True,
    )


def pick_fixture_track(target_user, target_host):
    """Return an mpd-path that resolves on the rig, or raise.

    Prefers widely-decodable formats (mp3 / flac / m4a / ogg /
    opus / wav / aiff) so the gate exercises `metadata_local`'s
    happy path. Falls back to any indexed track if no supported
    format is found — the gate still catches wire-dispatch
    regressions on an unsupported-format track, just at the
    weaker "non-error" bar rather than the stronger "ok" bar.
    """
    try:
        out = ssh(target_user, target_host, "mpc listall 2>/dev/null")
    except subprocess.CalledProcessError as e:
        raise RuntimeError(
            f"mpc listall failed on {target_host} (rc={e.returncode}); "
            f"gate cannot pick a fixture track"
        )
    tracks = [t for t in out.splitlines() if t.strip()]
    if not tracks:
        raise RuntimeError(
            f"mpc listall returned no tracks on {target_host}; the gate "
            f"needs at least one indexed track to probe"
        )
    preferred_exts = (".mp3", ".flac", ".m4a", ".ogg", ".opus", ".wav",
                       ".aiff", ".aif", ".wma", ".alac")
    for t in tracks:
        lower = t.lower()
        if lower.endswith(preferred_exts):
            return t
    # No supported format found — take the first available and let
    # the gate probe against it. metadata_local will likely return
    # not_found on a DSD/unsupported track; still non-error, so
    # the gate still catches wire-dispatch regressions.
    return tracks[0]


def bootstrap_preseed(target_user, target_host):
    """Read the pair-preseed code from the rig."""
    try:
        return ssh(target_user, target_host, f"sudo -n cat {PRESEED_PATH}").strip()
    except subprocess.CalledProcessError as e:
        raise RuntimeError(
            f"cannot read pair preseed at {PRESEED_PATH} on "
            f"{target_host} (rc={e.returncode}); gate cannot mint a "
            f"bearer"
        )


async def wss_call(target_host, op, payload, bearer=None):
    subs = [f"evo.bearer.{bearer}"] if bearer else None
    async with websockets.connect(
        f"wss://{target_host}:8443/api/v1/ws",
        ssl=ssl_ctx(),
        subprotocols=subs,
        close_timeout=2,
        ping_interval=15,
        ping_timeout=None,
        open_timeout=8,
    ) as ws:
        await ws.send(
            json.dumps(
                {
                    "frame_type": "request",
                    "request_id": 1,
                    "op": op,
                    "payload": payload,
                }
            )
        )
        async for m in ws:
            f = json.loads(m)
            if f.get("response_to") == 1:
                outer = f["outcome"]
                if outer.get("outcome") != "ok":
                    raise RuntimeError(f"wire error: {f}")
                val = outer.get("value", {})
                if isinstance(val, dict) and "error" in val:
                    raise RuntimeError(
                        f"handler error {op}: {val['error']}"
                    )
                return val


async def http_get_json(bearer, target_host, path):
    """Fetch a JSON body over HTTPS with a bearer token.

    Uses the standard-library `urllib` under `asyncio.to_thread`
    to avoid dragging in a third-party HTTP client. The bearer
    is passed as `Authorization: Bearer <token>`.
    """
    import urllib.request
    import urllib.error

    url = f"https://{target_host}:8443{path}"

    def _fetch():
        req = urllib.request.Request(
            url,
            headers={"Authorization": f"Bearer {bearer}"},
        )
        ctx = ssl_ctx()
        with urllib.request.urlopen(req, context=ctx, timeout=15) as resp:
            body = resp.read()
        return json.loads(body.decode())

    return await asyncio.to_thread(_fetch)


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("target_host")
    ap.add_argument("target_user")
    args = ap.parse_args()

    print(f"=== track_detail full-source smoke against {args.target_host} ===")

    # Step 1 — pick a fixture track from MPD.
    try:
        fixture = pick_fixture_track(args.target_user, args.target_host)
    except RuntimeError as e:
        print(f"FAIL (operator): {e}")
        return 1
    print(f"fixture: mpd-path={fixture!r}")

    # Step 2 — mint a bearer via bootstrap-preseed.
    try:
        preseed = bootstrap_preseed(args.target_user, args.target_host)
    except RuntimeError as e:
        print(f"FAIL (operator): {e}")
        return 1
    try:
        body = await wss_call(
            args.target_host,
            "pair_complete",
            {"pair_id": "bootstrap", "code": preseed},
        )
    except Exception as e:
        print(f"FAIL (operator): pair_complete refused: {e}")
        return 1
    bearer = body["token"]
    print(f"bearer minted")

    # Step 3 — hit track_detail for the fixture.
    from urllib.parse import quote
    path = (
        f"/api/v1/audio/track_detail?scheme=mpd-path"
        f"&value={quote(fixture, safe='')}"
    )
    try:
        body = await http_get_json(bearer, args.target_host, path)
    except Exception as e:
        print(f"FAIL (endpoint): track_detail request failed: {e}")
        return 2

    # Step 4 — inspect every named source.
    sources = body.get("sources") or {}
    if not sources:
        print(f"FAIL (endpoint): track_detail body has no `sources` key: {body}")
        return 2

    fails = []
    for name in GATED_SOURCES:
        sub = sources.get(name)
        if sub is None:
            fails.append(
                f"{name}: missing from response.sources — deploy delivered "
                f"a binary that dropped a first-tier source"
            )
            print(f"  {name:20s} MISSING")
            continue
        status = sub.get("status")
        detail = sub.get("detail")
        print(
            f"  {name:20s} status={status!r} detail={detail!r}"
        )
        if status == "error":
            fails.append(
                f"{name}: status=error, detail={detail!r} — this is the "
                f"exact regression class UI called out (dropped "
                f"payload_b64 / half-landed wire codec)"
            )

    print()
    if fails:
        print(f"=== SMOKE FAIL — {len(fails)} source(s) errored ===")
        for f in fails:
            print(f"  - {f}")
        print()
        print(
            "gate refuses the deploy. deploy-distribution.sh MUST restore "
            "`.prev` on this rig and exit non-zero. no half-landed protocol "
            "work reaches a listening device."
        )
        return 3

    print("=== SMOKE GREEN — every gated source non-error ===")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
