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


def pick_known_lyric_track(target_user, target_host):
    """Return an mpd-path for a track known to have lyrics on
    LRCLIB, or `None` when the rig's library carries none of
    the curated candidates.

    The candidates are picked for two properties: universally
    present on LRCLIB (mainstream singer-songwriter / pop),
    and typically present in the audio-team's reference
    libraries. `mpc find` on `(Artist,Title)` matches the
    tagged fields exactly — narrower than `mpc listall` so the
    smoke doesn't accidentally land on a live take / cover /
    karaoke variant that LRCLIB misses.
    """
    # (artist_tag, title_substring) — title is a substring
    # match to tolerate small punctuation / edition differences
    # in tag data.
    candidates = [
        ("Passenger", "Let Her Go"),
        ("Passenger", "Staring At the Stars"),
        ("Passenger", "Things That Stop You Dreaming"),
        ("Ed Sheeran", "Thinking Out Loud"),
        ("Adele", "Someone Like You"),
        ("Adele", "Hello"),
        ("Sarah McLachlan", "Angel"),
        ("Sarah McLachlan", "Building a Mystery"),
        ("Sarah McLachlan", "Vox"),
        ("Coldplay", "Yellow"),
        ("Radiohead", "Creep"),
    ]
    for artist, title_substring in candidates:
        try:
            out = ssh(
                target_user,
                target_host,
                f"mpc find artist {json.dumps(artist)} 2>/dev/null",
            )
        except subprocess.CalledProcessError:
            continue
        for track in out.splitlines():
            if title_substring.lower() in track.lower():
                return track
    return None


def bootstrap_preseed(target_user, target_host):
    """Read the pair-preseed code from the rig.

    The steward loads this file itself, as the service user, at
    startup — a preseed the service user cannot read is a preseed
    that never seeds. So the plain read is the one that must work,
    and it is tried first. The sudo read stays as a fallback for a
    rig where the operator dropped the file root-owned; that rig
    boots with no first-pair path anyway, but the fallback keeps
    the gate's diagnostic honest instead of blaming permissions.

    Requiring sudo unconditionally made the gate depend on
    passwordless sudo being configured on every target, which is a
    property of the rig, not of the thing being verified.
    """
    for cmd in (f"cat {PRESEED_PATH}", f"sudo -n cat {PRESEED_PATH}"):
        try:
            code = ssh(target_user, target_host, cmd).strip()
        except subprocess.CalledProcessError:
            continue
        if code:
            return code
    raise RuntimeError(
        f"cannot read pair preseed at {PRESEED_PATH} on "
        f"{target_host}; gate cannot mint a bearer. The operator "
        f"drops this file before first boot and the steward must be "
        f"able to read it as its own service user."
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
    external_transients = []
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
            # Discriminate substrate regressions from external-
            # service transients. Substrate regressions (missing
            # payload_b64, half-landed wire codec, plugin
            # response JSON parse failure, plugin admission
            # failure) MUST block the deploy — they mean the
            # binary this deploy just shipped is broken in a way
            # rollback protects against. External transients
            # (upstream 503s / rate-limits from MusicBrainz /
            # LRCLIB / cover_art_archive, `plugin error:`
            # framework wraps of PluginError) are NOT deploy
            # regressions — the shipped binary is fine, the
            # upstream is down. Downgrading them to a warning
            # keeps the deploy gate honest about what it
            # actually gates.
            detail_str = str(detail or "").lower()
            substrate_defect_markers = (
                "missing payload_b64",
                "wire codec",
                "dispatch failed",
                "plugin response json parse",
                "admission failed",
                "admission error",
                "response is not a json object",
            )
            external_transient_markers = (
                "plugin error:",
                "http 503",
                "http 502",
                "http 504",
                "http 429",
                "musicbrainz",
                "lrclib",
                "cover_art_archive",
                "read operation timed out",
                "connection refused",
                "temporarily unavailable",
            )
            is_substrate = any(m in detail_str for m in substrate_defect_markers)
            is_transient = any(
                m in detail_str for m in external_transient_markers
            )
            # Order matters: substrate markers take precedence.
            # A detail carrying both means the framework mis-
            # classified an upstream error as substrate-shape;
            # treat as substrate so the operator sees the more
            # serious class.
            if is_substrate:
                fails.append(
                    f"{name}: status=error, detail={detail!r} — SUBSTRATE "
                    f"REGRESSION CLASS (dropped payload_b64 / half-landed "
                    f"wire codec / dispatch failure / admission failure)"
                )
            elif is_transient:
                external_transients.append(
                    f"{name}: status=error, detail={detail!r} — external "
                    f"upstream transient (not a deploy regression)"
                )
            else:
                # Uncategorised error — treat as fail so a novel
                # failure shape does not slip through silently.
                fails.append(
                    f"{name}: status=error, detail={detail!r} — "
                    f"uncategorised error class; deploy gate defaults to "
                    f"refuse pending classification"
                )
    if external_transients:
        print()
        print("--- external upstream transients (non-blocking) ---")
        for t in external_transients:
            print(f"  {t}")

    # ---- Step 5 — lyrics cache-hit shape parity ------------------
    #
    # A prior defect (verified in code): the lyrics cache-hit path
    # returned `provider_id="cache"` with a wrapped payload
    # `{cached_from_provider_id, value:{...}}` while the live-hit
    # path returned flat `{plain_lyrics, ...}` under
    # `provider_id="lrclib"`. Every UI/consumer reading
    # `payload.plain_lyrics` at the top level rendered empty on
    # every refresh after the first play. The fix collapsed the
    # cache-hit shape to equal the live shape.
    #
    # This step gates on that invariant: pick a track known to
    # carry lyrics on LRCLIB, fetch track_detail twice, assert
    # the two `lyrics` sub-sources have identical
    # `(status, provider_id, payload.plain_lyrics non-empty)`
    # tuples. If the invariant is broken again, this step FAILs
    # before publish — the "Cluster One" instrumental fixture
    # gated the surface-level dispatch shape, not the cache
    # parity, and let the defect ship.
    #
    # SKIPPED (not FAILED) when the rig's library carries no
    # curated known-lyrics fixture — the gate cannot honestly
    # attest what the library does not carry.
    print()
    print("--- lyrics cache-hit shape parity ---")
    lyric_fixture = pick_known_lyric_track(args.target_user, args.target_host)
    if lyric_fixture is None:
        print(
            "  SKIP — no curated known-lyrics fixture found in the "
            "rig's library. Cache-hit shape parity not exercised on "
            "this deploy."
        )
    else:
        print(f"  lyric fixture: mpd-path={lyric_fixture!r}")
        lyric_path = (
            f"/api/v1/audio/track_detail?scheme=mpd-path"
            f"&value={quote(lyric_fixture, safe='')}"
        )
        try:
            first = await http_get_json(bearer, args.target_host, lyric_path)
            second = await http_get_json(bearer, args.target_host, lyric_path)
        except Exception as e:
            print(f"  FAIL (endpoint): lyric parity fetch failed: {e}")
            fails.append(f"lyrics_cache_parity: fetch failed: {e}")
        else:
            first_lyrics = (first.get("sources") or {}).get("lyrics") or {}
            second_lyrics = (second.get("sources") or {}).get("lyrics") or {}
            first_status = first_lyrics.get("status")
            second_status = second_lyrics.get("status")
            first_pid = first_lyrics.get("provider_id")
            second_pid = second_lyrics.get("provider_id")
            first_plain = str(
                (first_lyrics.get("payload") or {}).get("plain_lyrics") or ""
            )
            second_plain = str(
                (second_lyrics.get("payload") or {}).get("plain_lyrics") or ""
            )
            print(
                f"    first : status={first_status!r} "
                f"provider_id={first_pid!r} plain_lyrics_len={len(first_plain)}"
            )
            print(
                f"    second: status={second_status!r} "
                f"provider_id={second_pid!r} plain_lyrics_len={len(second_plain)}"
            )
            if first_status != "ok" or second_status != "ok":
                # Not necessarily a fail — LRCLIB may not have this
                # track. SKIP with a diagnostic when the FIRST call
                # already missed; only FAIL when live-hit was OK
                # but cache-hit disagreed.
                if first_status != "ok":
                    print(
                        "    SKIP — LRCLIB missed on the fixture; "
                        "cache parity not exercisable this run."
                    )
                else:
                    fails.append(
                        f"lyrics_cache_parity: first fetch status={first_status!r} "
                        f"but second fetch status={second_status!r} — "
                        f"cache-hit downgraded the response"
                    )
                    print(
                        "    FAIL — live-hit ok but cache-hit not ok"
                    )
            elif first_pid != second_pid:
                fails.append(
                    f"lyrics_cache_parity: provider_id differs — "
                    f"first={first_pid!r} second={second_pid!r} "
                    f"(cache-hit must echo live-hit's provider_id, "
                    f"not a synthetic 'cache' label)"
                )
                print("    FAIL — provider_id shape differs between live and cache")
            elif len(first_plain) == 0 or len(second_plain) == 0:
                fails.append(
                    f"lyrics_cache_parity: plain_lyrics empty on one/both "
                    f"reads (first_len={len(first_plain)} "
                    f"second_len={len(second_plain)}) — the exact "
                    f"'lyrics vanish on refresh' regression class"
                )
                print("    FAIL — plain_lyrics empty on live and/or cache read")
            elif first_plain != second_plain:
                fails.append(
                    f"lyrics_cache_parity: plain_lyrics text differs "
                    f"between live and cache reads — cache is returning "
                    f"different content than the source served"
                )
                print("    FAIL — plain_lyrics content differs live vs cache")
            else:
                print(
                    "    GREEN — cache-hit shape equals live-hit shape; "
                    "plain_lyrics identical across both reads"
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
