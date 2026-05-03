"""Update a v0 release-plane channel pointer file in place.

Usage:
    promote_channel_pointer.py <path> <piece> <version> <channel> <updated_at>

Behaviour:
- If <path> does not exist, write a fresh channel file with one pointer.
- If <path> exists, read its existing (piece, version) pointer rows,
  replace or insert the (piece, version) entry passed on the command
  line, and write back a sorted pointer list.

The format is provisional (v0); see SECRETS.md and the framework's
release-plane contract documentation for the ongoing work in
evo-core that will reshape this in-place.
"""

from __future__ import annotations

import os
import sys
from typing import Dict


def parse_existing(path: str) -> Dict[str, str]:
    """Return a {piece -> version} dict from an existing channel file."""
    rows: Dict[str, str] = {}
    if not os.path.exists(path):
        return rows
    cur_piece: str | None = None
    cur_version: str | None = None
    with open(path) as f:
        for line in f:
            stripped = line.strip()
            if stripped.startswith("piece"):
                cur_piece = stripped.split("=", 1)[1].strip().strip('"')
            elif stripped.startswith("version"):
                cur_version = stripped.split("=", 1)[1].strip().strip('"')
                if cur_piece is not None:
                    rows[cur_piece] = cur_version
                    cur_piece = None
                    cur_version = None
    return rows


def write(path: str, channel: str, updated_at: str, rows: Dict[str, str]) -> None:
    out = [
        "schema_version = 0\n",
        'publisher = "org.evoframework"\n',
        f'channel = "{channel}"\n',
        f'updated_at = "{updated_at}"\n',
        "\n",
    ]
    for piece, version in sorted(rows.items()):
        out.append("[[pointers]]\n")
        out.append(f'piece = "{piece}"\n')
        out.append(f'version = "{version}"\n')
        out.append("\n")
    with open(path, "w") as f:
        f.writelines(out)


def main() -> None:
    if len(sys.argv) != 6:
        sys.stderr.write(
            "usage: promote_channel_pointer.py "
            "<path> <piece> <version> <channel> <updated_at>\n"
        )
        sys.exit(2)
    path, piece, version, channel, updated_at = sys.argv[1:]
    rows = parse_existing(path)
    rows[piece] = version
    write(path, channel, updated_at, rows)


if __name__ == "__main__":
    main()
