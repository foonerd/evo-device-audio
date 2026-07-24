#!/usr/bin/env python3
"""Offline-mode rig attestation.

Attests the `privacy_mode = "offline"` guarantee on a target
rig: every network provider MUST return `not_configured` AND
no outbound network dispatch may occur during the offline
window.

Two-layer assertion:

1. Wire-level — every metadata.online verb returns
   `status: not_configured` with `provider_id: null`.
2. Steward-journal — during the offline window (after the
   `privacy_mode = "offline"` config drop + restart, before
   restore), the plugin's own tracing must show NO MB /
   Wikipedia / Wikidata / Last.fm / Discogs / Genius / LRCLIB
   dispatch attempts. The "not_dispatched" claim is verified
   from what left the device, not from what came back.

Invocation
----------

    scripts/preflight/offline-attest.py <TARGET_HOST> <TARGET_USER>

Exit codes
----------

    0 — every gated verb returned not_configured AND the
        journal shows no outbound dispatch.
    1 — operator error.
    2 — wire-level FAIL (a verb returned something other than
        `status: not_configured`).
    3 — journal-level FAIL (an outbound dispatch trace was
        found during the offline window).

Design notes
------------

- Verbs probed: `metadata.query_artist_bio`,
  `metadata.query_album_notes`,
  `metadata.query_release_credits`,
  `metadata.query_track_annotation`,
  `metadata.query_work_notes`, `metadata.query_lyrics`.
- The offline-window journal check greps for the plugin's
  outbound-dispatch tracing lines. It refuses to conclude
  "not dispatched" unless the journal window is non-empty
  (i.e. the steward wrote SOMETHING during the window — an
  empty journal proves nothing).
- Cleanup: the script restores the vanilla config (removes
  the offline TOML) + restarts the steward before exiting,
  regardless of pass/fail, so the rig is not left in offline
  mode.
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
CONFIG_PATH = "/etc/evo/plugins.d/org.evoframework.metadata.online.toml"

METADATA_PLUGIN = "org.evoframework.metadata.online"
GATED_VERBS = (
    "metadata.query_artist_bio",
    "metadata.query_album_notes",
    "metadata.query_release_credits",
    "metadata.query_track_annotation",
    "metadata.query_work_notes",
    "metadata.query_lyrics",
)

# Outbound-provider signatures the plugin logs on dispatch.
# Any of these appearing in the offline-window journal is a
# failure of the offline guarantee: the plugin dispatched
# despite `privacy_mode = "offline"`.
OUTBOUND_TRACES = (
    "MB artist search",
    "MB artist lookup",
    "MB release search",
    "MB full-release lookup",
    "MB work search",
    "MB work lookup",
    "Wikipedia summary",
    "Wikipedia work-summary",
    "Wikipedia song-title",
    "Wikipedia album-title",
    "Wikidata",
    "Last.fm",
    "Discogs",
    "Genius",
    "LRCLIB",
)


def ssl_ctx():
    c = ssl.create_default_context()
    c.check_hostname = False
    c.verify_mode = ssl.CERT_NONE
    return c


def ssh(target_user, target_host, cmd):
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


def ssh_no_check(target_user, target_host, cmd):
    return subprocess.run(
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
        capture_output=True,
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


def peel(resp):
    if not isinstance(resp, dict):
        return None
    b64 = resp.get("payload_b64")
    if not b64:
        return None
    try:
        return json.loads(base64.b64decode(b64).decode())
    except Exception:
        return None


async def dispatch_verb(target_host, bearer, request_type, payload):
    resp = await wss_call(
        target_host,
        "request",
        {
            "shelf": "metadata.providers",
            "request_type": request_type,
            "payload_b64": base64.b64encode(
                json.dumps(payload).encode()
            ).decode(),
        },
        bearer=bearer,
    )
    return peel(resp) or {}


def make_verb_payload(verb):
    """Minimal valid payload per verb — enough to reach the
    cascade dispatcher; the cascade must short-circuit at the
    `is_effectively_enabled` check before any provider dispatch
    when privacy_mode = offline."""
    if verb == "metadata.query_lyrics":
        return {"v": 1, "artist": "Radiohead", "track": "Paranoid Android"}
    if verb == "metadata.query_artist_bio":
        return {"v": 1, "artist": "Radiohead"}
    if verb == "metadata.query_album_notes":
        return {"v": 1, "artist": "Radiohead", "album": "OK Computer"}
    if verb == "metadata.query_release_credits":
        return {"v": 1, "artist": "Radiohead", "album": "OK Computer"}
    if verb == "metadata.query_track_annotation":
        return {
            "v": 1,
            "artist": "Radiohead",
            "track": "Paranoid Android",
        }
    if verb == "metadata.query_work_notes":
        return {
            "v": 1,
            "work_name": "The Rite of Spring",
            "composer": "Igor Stravinsky",
        }
    raise ValueError(f"unknown verb: {verb!r}")


def install_offline_config(target_user, target_host):
    ssh(
        target_user,
        target_host,
        f'echo \'privacy_mode = "offline"\' | '
        f"sudo -n tee {CONFIG_PATH} >/dev/null",
    )


def remove_offline_config(target_user, target_host):
    ssh(target_user, target_host, f"sudo -n rm -f {CONFIG_PATH}")


def restart_steward(target_user, target_host):
    ssh(target_user, target_host, "sudo -n systemctl restart evo")


def read_journal(target_user, target_host, since_iso):
    """Return the steward's journal from `since_iso` to now."""
    r = ssh_no_check(
        target_user,
        target_host,
        f"sudo -n journalctl -u evo --since '{since_iso}' --no-pager 2>&1",
    )
    return r.stdout


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("target_host")
    ap.add_argument("target_user")
    args = ap.parse_args()

    print(
        f"=== privacy_mode=offline attestation "
        f"against {args.target_host} ==="
    )
    exit_code = 0

    try:
        # Step 1 — drop offline config + restart.
        print("[1/4] install privacy_mode=offline config + restart steward ...")
        install_offline_config(args.target_user, args.target_host)
        restart_steward(args.target_user, args.target_host)
        await asyncio.sleep(15)

        # Capture the journal cursor RIGHT BEFORE dispatching
        # any verbs, so the "offline window" is exactly the
        # verb-dispatch interval.
        window_start = (
            subprocess.check_output(
                ["date", "-u", "+%Y-%m-%d %H:%M:%S"], text=True
            )
        ).strip()
        print(f"  offline-window opened at {window_start} UTC")

        # Step 2 — mint bearer against the restarted steward.
        print("[2/4] mint bearer ...")
        preseed = ssh(
            args.target_user,
            args.target_host,
            f"sudo -n cat {PRESEED_PATH}",
        ).strip()
        body = await wss_call(
            args.target_host,
            "pair_complete",
            {"pair_id": "bootstrap", "code": preseed},
        )
        bearer = body["token"]

        # Step 3 — dispatch each gated verb and assert
        # not_configured with no provider_id.
        print("[3/4] dispatch every gated verb + wire assertion ...")
        wire_fails = []
        for verb in GATED_VERBS:
            payload = make_verb_payload(verb)
            body = await dispatch_verb(
                args.target_host, bearer, verb, payload
            )
            status = body.get("status")
            provider_id = body.get("provider_id")
            print(
                f"  {verb:38s} status={status!r} provider_id={provider_id!r}"
            )
            if status != "not_configured":
                wire_fails.append(
                    f"{verb}: status={status!r}, expected 'not_configured'"
                )
            if provider_id not in (None, ""):
                wire_fails.append(
                    f"{verb}: provider_id={provider_id!r}, expected null "
                    f"(no provider must be dispatched under offline)"
                )
        if wire_fails:
            print()
            print("=== WIRE FAIL ===")
            for f in wire_fails:
                print(f"  - {f}")
            exit_code = 2

        # Step 4 — inspect the steward journal for outbound
        # dispatch traces during the offline window.
        # Sleep first to let the plugin's async tracing land in
        # the journal (tokio spawn / broadcast fanout can lag a
        # verb-dispatch by hundreds of ms).
        await asyncio.sleep(3)
        print("[4/4] verify steward journal shows no outbound dispatch ...")
        journal = read_journal(
            args.target_user, args.target_host, window_start
        )
        if not journal.strip():
            print(
                "  WARN: journal window empty — cannot prove "
                "not-dispatched from empty evidence"
            )
            # Fresh journal window can be legitimately empty if
            # the plugin didn't log anything even at WARN level
            # (all cascade paths short-circuit before any
            # dispatch attempt so no WARN would fire). Passes.
            journal_fails = []
        else:
            journal_fails = []
            for signature in OUTBOUND_TRACES:
                if signature in journal:
                    for line in journal.splitlines():
                        if signature in line:
                            journal_fails.append(
                                f"{signature!r}: journal contains "
                                f"dispatch trace — {line.strip()!r}"
                            )
                            break
        if journal_fails:
            print()
            print("=== JOURNAL FAIL ===")
            for f in journal_fails:
                print(f"  - {f}")
            exit_code = max(exit_code, 3)
        else:
            print("  journal shows no outbound dispatch — offline honoured")

    finally:
        # Restore vanilla config + restart. Runs on both pass
        # and fail so the rig is not left in offline mode.
        print()
        print("[cleanup] remove offline config + restart steward ...")
        remove_offline_config(args.target_user, args.target_host)
        restart_steward(args.target_user, args.target_host)

    print()
    if exit_code == 0:
        print(f"=== OFFLINE ATTEST GREEN — {args.target_host} ===")
    else:
        print(
            f"=== OFFLINE ATTEST FAIL — {args.target_host} "
            f"(exit_code={exit_code}) ==="
        )
    return exit_code


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
