//! Runtime ALSA drop-in lifecycle for multi-room source mode.
//!
//! When the plugin engages source role on a live group it writes
//! `/etc/asound.d/zz-evo-multiroom-source.conf`. The drop-in
//! redefines `pcm.evo` to route through `snd-aloop` (so any
//! pcm.evo writer's frames land on the loopback playback half
//! where the plugin's source-side capture task reads them) and
//! defines `pcm.evo_local` as a direct alias to the local DAC
//! (so the source-host's local rendering task can still play
//! audibly while fanning out to remote receivers).
//!
//! When the plugin disengages source role the drop-in is
//! truncated to empty rather than removed. The base
//! `/etc/asound.conf` includes this path explicitly (ALSA's
//! include syntax does not portably accept glob patterns), so
//! the file must always exist. ALSA's "last definition wins"
//! rule means an empty body has no effect on `pcm.evo`; the
//! base direct-to-DAC definition stays in force.
//!
//! Load order within `/etc/asound.conf`'s explicit include
//! list pins this drop-in after `evo-options.conf`, so
//! operator-options drop-ins compose upstream of the
//! multi-room override and source-mode wins when both are
//! present.
//!
//! Atomicity: writes go to a temp file in the same directory,
//! then `rename` into place. Concurrent readers (the MPD
//! plugin's asound-watcher, ALSA's PCM-open path, the operator
//! `cat`ing the file) see prior contents or next contents,
//! never a partial file.
//!
//! Card-name discovery: parsed once per write from the
//! bootstrap-installed `/etc/asound.conf`, which carries the
//! authoritative card name as `card "<NAME>"` inside the base
//! `pcm.evo` block. If the parse fails the drop-in omits the
//! `pcm.evo_local` definition; source-host fan-out still works
//! (the loopback wiring is independent of the local-DAC alias),
//! but the source-host's own local rendering is silent until
//! the next engage attempt re-parses successfully.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Drop-in file path the plugin owns. Pinned in
/// `/etc/asound.d/` ahead of any other naming convention so the
/// override wins via ALSA's last-definition-wins rule.
pub(crate) const DROP_IN_PATH: &str =
    "/etc/asound.d/zz-evo-multiroom-source.conf";

/// Path the plugin reads to discover the local DAC card name.
/// The bootstrap-installed file is the single source of truth.
const BASE_ASOUND_CONF: &str = "/etc/asound.conf";

/// Inert body written when source role disengages. The base
/// `/etc/asound.conf` includes this file by exact path; the
/// placeholder must exist so the include resolves. Empty
/// content (header comment only) has no effect on `pcm.evo`
/// per ALSA's last-definition-wins rule.
const DISENGAGED_PLACEHOLDER: &str =
    "# Multi-room source-mode ALSA drop-in for evo-device-audio.\n\
# Plugin-managed: org.evoframework.multiroom.evo-native overwrites\n\
# this file atomically when source role engages on a live group,\n\
# and truncates it back to this placeholder when the role\n\
# transitions out. The placeholder has no effect on pcm.evo; the\n\
# base direct-to-DAC definition stays in force.\n";

/// Errors produced while writing or removing the drop-in.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DropInError {
    #[error("create temp file in {dir}: {source}")]
    CreateTemp {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("write drop-in temp file: {source}")]
    WriteTemp {
        #[source]
        source: std::io::Error,
    },
    #[error("rename {temp} into {final_path}: {source}")]
    Rename {
        temp: PathBuf,
        final_path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Atomically install the source-mode drop-in at
/// [`DROP_IN_PATH`]. Parses the local DAC card name from
/// `/etc/asound.conf` so the drop-in's `pcm.evo_local` resolves
/// to the right hardware. If the card name is unavailable the
/// drop-in still installs (without `pcm.evo_local`) so loopback
/// fan-out works.
///
/// Idempotent against repeated calls with the same card name:
/// the rendered file content is byte-for-byte stable, so
/// re-writing produces the same final inode contents. The
/// rename is atomic so concurrent ALSA opens see one of the
/// stable byte sequences.
pub(crate) fn install_source_drop_in() -> Result<(), DropInError> {
    let card = detect_audio_card();
    let body = render(card.as_deref());
    write_atomic(Path::new(DROP_IN_PATH), &body)
}

/// Collapse the source-mode drop-in to its inert form by
/// truncating the file to empty (or writing a header-only
/// body when the placeholder is missing). Called on every
/// transition out of engaged source so `pcm.evo` resolves to
/// the base direct-to-DAC definition on the next PCM open.
///
/// The file is preserved (not removed) because the base
/// `/etc/asound.conf` includes this exact path; ALSA's
/// include syntax does not portably tolerate missing files.
/// The placeholder body is the same one the bootstrap seeds
/// so an operator reading the file post-disengage sees an
/// honest "no source mode active" state.
pub(crate) fn remove_source_drop_in() -> Result<(), DropInError> {
    write_atomic(Path::new(DROP_IN_PATH), DISENGAGED_PLACEHOLDER)
}

/// Render the drop-in body. Public-within-crate for testing.
pub(crate) fn render(card: Option<&str>) -> String {
    let mut s = String::with_capacity(1024);
    s.push_str(HEADER);
    s.push_str(PCM_EVO_LOOPBACK);
    if let Some(card) = card {
        s.push_str(&render_pcm_evo_local(card));
    } else {
        s.push_str(PCM_EVO_LOCAL_PLACEHOLDER);
    }
    s.push_str(PCM_LOOPBACK_CAPTURE);
    s
}

fn render_pcm_evo_local(card: &str) -> String {
    format!(
        r#"
pcm.evo_local {{
    type plug
    slave.pcm "hw:CARD={card},DEV=0"
    hint {{
        show on
        description "evo: source-host local DAC renderer target"
    }}
}}
"#
    )
}

const HEADER: &str = "# Runtime ALSA drop-in written by \
org.evoframework.multiroom.evo-native while a live multi-room \
source-mode group is engaged on this device. Removed \
automatically when the role transitions out of engaged source. \
DO NOT EDIT BY HAND --- this file is regenerated on every \
engage transition.\n\n";

// pcm.evo composes a multi-slave tee writing simultaneously to
// the multi-room loopback (subdev 0, the source-mode capture
// the multi-room plugin reads + fans out) AND the audio-terminus
// loopback (subdev 7, the spectrum / post-mixer-signal tap).
// Both subdevs are independent capture buffers fed by the same
// pcm.evo write; spectrum stays alive during source-mode
// engagement. The multi-room source-mode capture continues to
// read from `pcm.evo_loopback_capture` (subdev 1, capture of
// subdev 0); the terminus plugin reads from subdev 7's capture
// half.
const PCM_EVO_LOOPBACK: &str = r#"pcm.evo {
    type plug
    slave.pcm "evo_source_mode_tee"
    hint {
        show on
        description "evo: multi-room source producer + terminus tap"
    }
}

pcm.evo_source_mode_tee {
    type multi
    slaves.multiroom.pcm "evo_source_mode_loopback_playback"
    slaves.multiroom.channels 2
    slaves.terminus.pcm "evo_terminus_tap"
    slaves.terminus.channels 2
    bindings.0.slave multiroom
    bindings.0.channel 0
    bindings.1.slave multiroom
    bindings.1.channel 1
    bindings.2.slave terminus
    bindings.2.channel 0
    bindings.3.slave terminus
    bindings.3.channel 1
}

pcm.evo_source_mode_loopback_playback {
    type hw
    card "Loopback"
    device 0
    subdevice 0
}

# pcm.evo_terminus_tap is also defined in /etc/asound.conf for
# the non-source-mode case (the multi-slave tee there writes to
# the DAC primary + terminus tap). Redefining the same alias
# here is a no-op since both definitions target the same subdev;
# the duplication is explicit so this drop-in stands alone
# (operators reading just this file see the full source-mode
# wiring without cross-referencing the base asound.conf).
pcm.evo_terminus_tap {
    type hw
    card "Loopback"
    device 0
    subdevice 7
}
"#;

const PCM_EVO_LOCAL_PLACEHOLDER: &str =
    "\n# pcm.evo_local omitted: local DAC card name could not be parsed \
from /etc/asound.conf at engage time. Source-host fan-out to \
remote receivers is unaffected; only the source-host's own \
local rendering is silent until the next engage attempt.\n";

const PCM_LOOPBACK_CAPTURE: &str = r#"
pcm.evo_loopback_capture {
    type hw
    card "Loopback"
    device 1
    subdevice 0
    hint {
        show on
        description "evo: source-host loopback capture target"
    }
}
"#;

/// Atomic write: temp file in the same directory, fsync, rename
/// over the target. Concurrent readers see prior or next, never
/// partial.
fn write_atomic(target: &Path, body: &str) -> Result<(), DropInError> {
    let dir = target.parent().unwrap_or(Path::new("."));
    let temp = dir.join(format!(
        ".{}.tmp.{}",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("evo-multiroom"),
        std::process::id()
    ));

    {
        let mut f = std::fs::File::create(&temp).map_err(|e| {
            DropInError::CreateTemp {
                dir: dir.to_path_buf(),
                source: e,
            }
        })?;
        f.write_all(body.as_bytes())
            .map_err(|e| DropInError::WriteTemp { source: e })?;
        f.sync_all()
            .map_err(|e| DropInError::WriteTemp { source: e })?;
    }

    std::fs::rename(&temp, target).map_err(|e| {
        // Best-effort temp cleanup; the persistent residue would
        // be operator-visible but harmless. Suppress the cleanup
        // error so the caller sees the underlying rename failure.
        let _ = std::fs::remove_file(&temp);
        DropInError::Rename {
            temp: temp.clone(),
            final_path: target.to_path_buf(),
            source: e,
        }
    })?;
    Ok(())
}

/// Parse the local DAC card name from
/// `/etc/asound.conf`. Returns the first `card "<NAME>"` value
/// found; the bootstrap-installed file declares the card name
/// in both `pcm.evo` and `ctl.evo` blocks, both referencing the
/// same authoritative name.
fn detect_audio_card() -> Option<String> {
    let contents = std::fs::read_to_string(BASE_ASOUND_CONF).ok()?;
    for line in contents.lines() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix("card") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        let end = rest.find('"')?;
        return Some(rest[..end].to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_with_card_includes_evo_local() {
        let body = render(Some("DAC"));
        assert!(body.contains("pcm.evo {"));
        // Source-mode redefines pcm.evo as a multi-slave tee
        // writing to BOTH the multi-room loopback (subdev 0)
        // AND the audio-terminus tap (subdev 7) — spectrum
        // stays alive during source-mode engagement.
        assert!(body.contains("evo_source_mode_tee"));
        assert!(body.contains("evo_source_mode_loopback_playback"));
        assert!(body.contains("evo_terminus_tap"));
        assert!(body.contains("card \"Loopback\""));
        assert!(body.contains("subdevice 0"));
        assert!(body.contains("subdevice 7"));
        assert!(body.contains("pcm.evo_local {"));
        assert!(body.contains("hw:CARD=DAC,DEV=0"));
        assert!(body.contains("pcm.evo_loopback_capture {"));
    }

    #[test]
    fn render_without_card_omits_evo_local_with_explanatory_comment() {
        let body = render(None);
        assert!(body.contains("pcm.evo {"));
        assert!(body.contains("evo_source_mode_tee"));
        assert!(body.contains("evo_terminus_tap"));
        assert!(!body.contains("pcm.evo_local {"));
        assert!(body.contains("pcm.evo_local omitted"));
        assert!(body.contains("pcm.evo_loopback_capture {"));
    }

    #[test]
    fn render_is_deterministic_for_identical_input() {
        let a = render(Some("PCH"));
        let b = render(Some("PCH"));
        assert_eq!(a, b);
    }

    #[test]
    fn write_then_collapse_to_placeholder_keeps_file_present() {
        let dir = tempdir();
        let target = dir.join("zz-evo-multiroom-source.conf");
        let body = render(Some("test-card"));
        write_atomic(&target, &body).unwrap();
        assert!(std::fs::read_to_string(&target)
            .unwrap()
            .contains("Loopback"));
        // Simulate the disengage path: write the placeholder
        // body over the engaged content. The file stays
        // present so /etc/asound.conf's explicit include still
        // resolves; the body collapses to the inert form.
        write_atomic(&target, DISENGAGED_PLACEHOLDER).unwrap();
        let post = std::fs::read_to_string(&target).unwrap();
        assert!(post.contains("Plugin-managed"));
        assert!(!post.contains("evo_source_mode_tee"));
        assert!(target.exists());
    }

    #[test]
    fn write_atomic_overwrites_existing_file() {
        let dir = tempdir();
        let target = dir.join("zz-evo-multiroom-source.conf");
        write_atomic(&target, "first").unwrap();
        write_atomic(&target, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");
    }

    fn tempdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "evo-multiroom-asound-dropin-test-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rand_suffix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0)
    }
}
