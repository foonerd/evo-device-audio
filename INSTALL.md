# Installing evo-device-audio

Operator install guide. Copy the commands as they appear.

The install runs on the target device itself. You do not need a build machine, a compiler, or a network share. All you need is a working internet connection on the device.

## Supported hosts

- **Raspberry Pi** running Raspberry Pi OS (Debian trixie image or newer).
- **Any x86_64 host** running Debian 13 (trixie) or newer.

The installer detects the architecture (`aarch64` or `x86_64`) and downloads the matching bundle automatically.

## Prerequisites (HARD REQUIREMENTS)

**`curl` and `sudo` MUST be installed on the device before you can run the installer.** They are not optional. The installer cannot bootstrap them itself — the online install one-liner is `curl … | sudo bash`, which fails at the very first step if either command is missing.

Check both are present:

```bash
command -v curl && command -v sudo
```

If that prints two paths (e.g. `/usr/bin/curl` and `/usr/bin/sudo`), you are ready — skip to [Install](#install).

If it prints nothing, or only one path, follow the path below that matches your OS image.

### Path A — Raspberry Pi OS

Both `curl` and `sudo` ship in the standard Raspberry Pi OS image. The check above should already pass. If for any reason it does not, run (as the `pi` user or your equivalent):

```bash
sudo apt-get update
sudo apt-get install -y curl sudo
```

### Path B — Minimal Debian install (neither is present)

A minimal Debian install ships without either command. You must install them before the installer will run.

Log in **at the console** as `root`, using the root password you set during the OS install. Then run:

```bash
apt-get update
apt-get install -y curl sudo
```

That is the whole prerequisite step. When it finishes, log out of the root console and log back in as your ordinary user account.

If your ordinary user account was created during the OS install with sudo-group membership (the default for Debian's guided install), you are done. If not, add it now (still as root):

```bash
usermod -aG sudo <your-user-account>
```

Log out and log back in as your ordinary user for the group change to take effect.

Re-run the prerequisite check before continuing:

```bash
command -v curl && command -v sudo
```

Both paths must print. Do not proceed to [Install](#install) until they do.

## Install

From your ordinary user account, run:

```bash
curl -fsSL https://raw.githubusercontent.com/foonerd/evo-device-audio/main/dist/scripts/evo-install.sh | sudo bash
```

The installer:

1. Downloads the signed bundle for your architecture.
2. Verifies the ed25519 signature against the shipped public key.
3. Installs binaries, systemd units, sudoers drop-ins, and the reference ALSA + MPD configuration.
4. Applies any missing runtime packages (`mpd`, `alsa-utils`, `samba`, and similar) automatically via `apt-get`.
5. Enables and starts `evo.service`.

Expect roughly **2 minutes** with a warm apt cache, **5 minutes** on a fresh minimal image where every runtime package is fetched for the first time.

When it finishes you should see:

```
=== evo-install.sh install complete ===
Service active. 19 plugins admitted.
```

## Verify

Check the service is up:

```bash
sudo systemctl status evo
```

Look for `Active: active (running)`. If it reads `Active: activating` for more than 30 seconds, something is wrong — see [Troubleshooting](#troubleshooting).

Tail the journal to watch the service behave:

```bash
sudo journalctl -u evo -f
```

Press `Ctrl+C` to stop tailing.

## Re-install and wipe modes

If a device is already installed and you want to start over:

| Mode | Command | What it does |
|---|---|---|
| `install` (default) | `curl -fsSL <URL> \| sudo bash` | First-time install. Refuses if already installed. |
| `reinstall` | `curl -fsSL <URL> \| sudo bash -s -- --mode=reinstall` | Full wipe and re-install. Deletes prior state. Preserves the music library at `/var/lib/evo/music`. |
| `wipe-config` | `curl -fsSL <URL> \| sudo bash -s -- --mode=wipe-config` | Wipes binaries and config only. Keeps the music library untouched. |
| `wipe-user-data` | `curl -fsSL <URL> \| sudo bash -s -- --mode=wipe-user-data` | Vacuums operator-generated state (queues, saved playlists, favourites). |

Replace `<URL>` with:

```
https://raw.githubusercontent.com/foonerd/evo-device-audio/main/dist/scripts/evo-install.sh
```

## Troubleshooting

**`curl: command not found`** — your host is missing `curl`. Go back to [Prerequisites](#prerequisites-hard-requirements).

**`sudo: command not found`** — your host is missing `sudo`. Go back to [Prerequisites](#prerequisites-hard-requirements).

**`FAIL: required tool missing: X`** — the installer refused because a base tool is not present. Install with (as root or via sudo):

```bash
sudo apt-get install -y tar gzip openssl coreutils findutils systemd
```

These are part of any working Debian install and should already be there.

**Service will not come up** — capture the last 100 journal lines and share them with the maintainer:

```bash
sudo journalctl -u evo --no-pager | tail -100 > /tmp/evo-install-fail.log
```

**Anything else** — capture the full install output and the journal:

```bash
curl -fsSL https://raw.githubusercontent.com/foonerd/evo-device-audio/main/dist/scripts/evo-install.sh | sudo bash 2>&1 | tee /tmp/evo-install.log
sudo journalctl -u evo --no-pager > /tmp/evo-journal.log
```

Attach both files to your report.
