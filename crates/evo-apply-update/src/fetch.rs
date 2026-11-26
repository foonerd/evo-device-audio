// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Just a Nerd

//! Fetching over HTTPS. The worker shells out to `/usr/bin/curl` by
//! absolute path with a cleared environment: no TLS stack is linked into a
//! root program, and no redirect is followed.

use std::process::Command;

use crate::error::{Res, UpdError};

/// A source of bytes by URL.
pub trait Fetch {
    /// Fetch `url`, refusing more than `max_bytes`.
    fn get(&self, url: &str, max_bytes: u64) -> Res<Vec<u8>>;
}

/// `curl`-backed fetcher.
pub struct CurlFetch {
    /// Optional CA bundle for a lab origin.
    pub ca_file: Option<String>,
    /// Scratch directory curl writes into (root-owned).
    pub scratch: std::path::PathBuf,
}

/// Is this URL one the worker will fetch? `https` only, no userinfo,
/// no whitespace or control characters.
pub fn url_ok(url: &str) -> bool {
    url.starts_with("https://")
        && url.len() <= 2048
        && !url.bytes().any(|c| c <= b' ' || c == 0x7f)
        && !url["https://".len()..]
            .split('/')
            .next()
            .unwrap_or_default()
            .contains('@')
}

impl Fetch for CurlFetch {
    fn get(&self, url: &str, max_bytes: u64) -> Res<Vec<u8>> {
        if !url_ok(url) {
            return Err(UpdError::new(
                "url_refused",
                "only plain https URLs are fetched",
            ));
        }
        let out = self.scratch.join(format!(
            "fetch-{}-{}",
            std::process::id(),
            crate::timeutil::now_epoch()
        ));
        let mut cmd = Command::new("/usr/bin/curl");
        cmd.env_clear()
            .args([
                "--proto",
                "=https",
                "--tlsv1.2",
                "--fail",
                "--silent",
                "--show-error",
            ])
            .args(["--connect-timeout", "10", "--max-time", "180"])
            .args(["--max-filesize", &max_bytes.to_string()]);
        if let Some(ca) = &self.ca_file {
            cmd.args(["--cacert", ca]);
        }
        let status = cmd
            .arg("-o")
            .arg(&out)
            .arg("--")
            .arg(url)
            .output()
            .map_err(|e| UpdError::new("fetch_failed", format!("curl: {e}")))?;
        let result = if status.status.success() {
            std::fs::read(&out).map_err(|e| {
                UpdError::new("fetch_failed", format!("read: {e}"))
            })
        } else {
            Err(UpdError::new(
                "fetch_failed",
                String::from_utf8_lossy(&status.stderr).trim().to_string(),
            ))
        };
        let _ = std::fs::remove_file(&out);
        let bytes = result?;
        if bytes.len() as u64 > max_bytes {
            return Err(UpdError::new(
                "fetch_too_large",
                "body exceeds the bound",
            ));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_https_urls_are_fetched() {
        assert!(url_ok("https://raw.githubusercontent.com/o/r/main/x.toml"));
        for u in [
            "http://x.example/a",
            "file:///etc/passwd",
            "ftp://x/a",
            "https://user@x.example/a",
            "https://x.example/a b",
            "https://x.example/a\nb",
            "",
        ] {
            assert!(!url_ok(u), "{u:?}");
        }
    }
}
