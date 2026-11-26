// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Just a Nerd

//! Safe extraction of a `.tar.gz` into a root-owned scratch directory. A
//! path with `..` or a root, a symlink, a hard link, a device, a file
//! whose size or count exceeds a bound: each is a refusal, and nothing is
//! installed from a partly extracted tree.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use tar::EntryType;

use crate::error::{io_err, Res, UpdError};

const MAX_FILES: usize = 4096;
const MAX_TOTAL: u64 = 256 * 1024 * 1024;

/// Extract `bytes` (a gzip tarball) under `dest`, which must already exist
/// and be empty. If every entry sits under one top directory called
/// `strip`, that directory is removed from the paths. Returns the relative
/// paths of the regular files written.
pub fn extract_tar_gz(
    bytes: &[u8],
    dest: &Path,
    strip: Option<&str>,
) -> Res<Vec<PathBuf>> {
    let mut ar = tar::Archive::new(GzDecoder::new(bytes));
    let entries = ar.entries().map_err(|e| {
        UpdError::new("archive_invalid", format!("not a tar.gz: {e}"))
    })?;
    let mut raw: Vec<(PathBuf, EntryType, u32, Vec<u8>)> = Vec::new();
    let mut total = 0u64;
    for entry in entries {
        let mut entry = entry
            .map_err(|e| UpdError::new("archive_invalid", e.to_string()))?;
        let ty = entry.header().entry_type();
        let path = entry
            .path()
            .map_err(|e| UpdError::new("archive_invalid", e.to_string()))?
            .into_owned();
        for c in path.components() {
            match c {
                Component::Normal(_) | Component::CurDir => {}
                _ => {
                    return Err(UpdError::new(
                        "archive_path_refused",
                        format!("{} escapes the tree", path.display()),
                    ))
                }
            }
        }
        match ty {
            EntryType::Directory => raw.push((path, ty, 0o755, Vec::new())),
            EntryType::Regular | EntryType::Continuous => {
                let mode = entry.header().mode().unwrap_or(0o644) & 0o755;
                let size = entry.header().size().unwrap_or(0);
                total += size;
                if total > MAX_TOTAL || raw.len() >= MAX_FILES {
                    return Err(UpdError::new(
                        "archive_too_large",
                        "bound exceeded",
                    ));
                }
                let mut buf = Vec::with_capacity(size as usize);
                entry
                    .by_ref()
                    .take(MAX_TOTAL)
                    .read_to_end(&mut buf)
                    .map_err(|e| {
                        UpdError::new("archive_invalid", e.to_string())
                    })?;
                raw.push((path, ty, mode, buf));
            }
            _ => {
                return Err(UpdError::new(
                    "archive_entry_refused",
                    format!(
                        "{}: only files and directories are accepted",
                        path.display()
                    ),
                ))
            }
        }
    }
    let prefix = strip.filter(|s| {
        !raw.is_empty()
            && raw.iter().all(|(p, ..)| {
                p.components()
                    .find(|c| matches!(c, Component::Normal(_)))
                    .is_some_and(|c| c.as_os_str() == std::ffi::OsStr::new(s))
            })
    });
    let mut files = Vec::new();
    for (path, ty, mode, data) in raw {
        let rel: PathBuf = path
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .skip(usize::from(prefix.is_some()))
            .collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(&rel);
        if ty == EntryType::Directory {
            fs::create_dir_all(&target)
                .map_err(|e| io_err("extract_failed", "mkdir", e))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| io_err("extract_failed", "mkdir", e))?;
        }
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| io_err("extract_failed", "create", e))?;
        std::io::Write::write_all(&mut f, &data)
            .map_err(|e| io_err("extract_failed", "write", e))?;
        fs::set_permissions(&target, fs::Permissions::from_mode(mode | 0o444))
            .map_err(|e| io_err("extract_failed", "chmod", e))?;
        files.push(rel);
    }
    Ok(files)
}

/// Build a `.tar.gz` for tests and for the lab minting tools.
#[cfg(test)]
pub fn build_tar_gz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, data, mode) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(*mode);
        h.set_cksum();
        b.append_data(&mut h, name, *data).unwrap();
    }
    let tar_bytes = b.into_inner().unwrap();
    let mut e =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut e, &tar_bytes).unwrap();
    e.finish().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packed_plugin_extracts_with_its_top_directory_removed() {
        let d = tempfile::tempdir().unwrap();
        let tgz = build_tar_gz(&[
            ("org.evoframework.x/manifest.toml", b"m", 0o644),
            ("org.evoframework.x/plugin.bin", b"bin", 0o755),
        ]);
        let files =
            extract_tar_gz(&tgz, d.path(), Some("org.evoframework.x")).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(fs::read(d.path().join("plugin.bin")).unwrap(), b"bin");
        let mode = fs::metadata(d.path().join("plugin.bin"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "an executable stays executable");
    }

    #[test]
    fn files_at_the_root_extract_as_they_are() {
        let d = tempfile::tempdir().unwrap();
        let tgz = build_tar_gz(&[("manifest.toml", b"m", 0o644)]);
        extract_tar_gz(&tgz, d.path(), Some("org.evoframework.x")).unwrap();
        assert!(d.path().join("manifest.toml").exists());
    }

    #[test]
    fn a_dotdot_or_absolute_path_is_refused() {
        // The tar builder will not write `..`, so the header is set by hand.
        for bad in ["../evil", "/abs/evil"] {
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            let name = h.as_old_mut().name.as_mut();
            name[..bad.len()].copy_from_slice(bad.as_bytes());
            h.set_cksum();
            let mut tar_bytes = h.as_bytes().to_vec();
            let mut block = vec![0u8; 512];
            block[0] = b'x';
            tar_bytes.extend(block);
            tar_bytes.extend(vec![0u8; 1024]);
            let mut e = flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::fast(),
            );
            std::io::Write::write_all(&mut e, &tar_bytes).unwrap();
            let tgz = e.finish().unwrap();
            let d = tempfile::tempdir().unwrap();
            let err = extract_tar_gz(&tgz, d.path(), None).unwrap_err();
            assert_eq!(err.code, "archive_path_refused", "{bad}");
        }
    }

    #[test]
    fn a_symlink_or_hard_link_entry_is_refused() {
        for ty in [tar::EntryType::Symlink, tar::EntryType::Link] {
            let mut b = tar::Builder::new(Vec::new());
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(ty);
            h.set_size(0);
            h.set_mode(0o777);
            h.set_link_name("/etc/passwd").unwrap();
            h.set_cksum();
            b.append_data(&mut h, "evil", std::io::empty()).unwrap();
            let tar_bytes = b.into_inner().unwrap();
            let mut e = flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::fast(),
            );
            std::io::Write::write_all(&mut e, &tar_bytes).unwrap();
            let d = tempfile::tempdir().unwrap();
            let err = extract_tar_gz(&e.finish().unwrap(), d.path(), None)
                .unwrap_err();
            assert_eq!(err.code, "archive_entry_refused");
        }
    }

    #[test]
    fn setuid_bits_are_dropped() {
        let d = tempfile::tempdir().unwrap();
        let tgz = build_tar_gz(&[("f", b"x", 0o4755)]);
        extract_tar_gz(&tgz, d.path(), None).unwrap();
        let mode = fs::metadata(d.path().join("f"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7000, 0);
    }

    #[test]
    fn not_a_tarball_is_refused() {
        let d = tempfile::tempdir().unwrap();
        assert!(extract_tar_gz(b"plain text", d.path(), None).is_err());
    }
}
