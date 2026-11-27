// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! What the worker asks of the host: the service user, a steward restart,
//! whether a plugin is admitted again, a sudoers syntax check, and the
//! start of a unit. Only root-owned binaries are run, by absolute path,
//! with a cleared environment.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::error::{Res, UpdError};

/// Host operations, faked in tests.
pub trait Host {
    /// The account the steward runs as.
    fn service_user(&self) -> Res<String>;
    /// Restart `evo.service` and wait for the restart to be requested.
    fn restart_evo(&self) -> Res<()>;
    /// Wait, bounded, until `evo` is active and `name`'s socket answers.
    fn wait_plugin_ready(&self, name: &str) -> bool;
    /// `visudo -cf <path>`.
    fn visudo_check(&self, path: &Path) -> Res<()>;
    /// Start the `evo-apply-update@<instance>.service` unit without waiting.
    fn start_unit(&self, instance: &str) -> Res<()>;
}

/// The real host.
pub struct SystemHost {
    /// `/run/evo/plugins`.
    pub socket_dir: PathBuf,
    /// How long to wait for admission after a restart.
    pub ready_timeout: Duration,
}

fn run(program: &str, args: &[&str]) -> Res<String> {
    let out = Command::new(program)
        .env_clear()
        .args(args)
        .output()
        .map_err(|e| {
            UpdError::new("host_command_failed", format!("{program}: {e}"))
        })?;
    if !out.status.success() {
        return Err(UpdError::new(
            "host_command_failed",
            format!(
                "{program} {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `^[a-z_][a-z0-9_-]{0,31}$`.
pub fn valid_user_name(u: &str) -> bool {
    let b = u.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b.iter().all(|c| {
            c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || *c == b'_'
                || *c == b'-'
        })
}

/// Is `user` an account in `passwd` text with a uid of at least 1000?
pub fn passwd_has_user(passwd: &str, user: &str) -> bool {
    passwd.lines().any(|l| {
        let f: Vec<&str> = l.split(':').collect();
        f.len() >= 3
            && f[0] == user
            && f[2].parse::<u32>().is_ok_and(|u| u >= 1000)
    })
}

impl Host for SystemHost {
    fn service_user(&self) -> Res<String> {
        let u = run(
            "/usr/bin/systemctl",
            &["show", "evo.service", "-p", "User", "--value"],
        )?;
        if !valid_user_name(&u) {
            return Err(UpdError::new(
                "service_user_invalid",
                "evo.service User= is not a plain name",
            ));
        }
        let passwd = std::fs::read_to_string("/etc/passwd").map_err(|e| {
            UpdError::new("service_user_invalid", format!("passwd: {e}"))
        })?;
        if !passwd_has_user(&passwd, &u) {
            return Err(UpdError::new(
                "service_user_invalid",
                "service user is not an account with uid >= 1000",
            ));
        }
        Ok(u)
    }

    fn restart_evo(&self) -> Res<()> {
        run("/usr/bin/systemctl", &["restart", "evo.service"]).map(|_| ())
    }

    fn wait_plugin_ready(&self, name: &str) -> bool {
        let sock = self.socket_dir.join(format!("{name}.sock"));
        let deadline = Instant::now() + self.ready_timeout;
        // Give the steward time to take the old socket down first.
        std::thread::sleep(Duration::from_secs(3));
        while Instant::now() < deadline {
            let active =
                run("/usr/bin/systemctl", &["is-active", "evo.service"])
                    .is_ok_and(|s| s == "active");
            if active && UnixStream::connect(&sock).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        false
    }

    fn visudo_check(&self, path: &Path) -> Res<()> {
        let p = path.to_string_lossy();
        run("/usr/sbin/visudo", &["-cf", &p])
            .map(|_| ())
            .map_err(|e| UpdError::new("sudoers_invalid", e.detail))
    }

    fn start_unit(&self, instance: &str) -> Res<()> {
        let unit = format!("evo-apply-update@{instance}.service");
        run("/usr/bin/systemctl", &["start", "--no-block", &unit]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_names_are_plain() {
        assert!(valid_user_name("evoproto"));
        assert!(valid_user_name("_svc-1"));
        for u in ["", "Root", "a b", "a;b", "1abc", "a$b", &"a".repeat(33)] {
            assert!(!valid_user_name(u), "{u:?}");
        }
    }

    #[test]
    fn the_service_user_must_be_a_real_unprivileged_account() {
        let p = "root:x:0:0::/root:/bin/bash\nevoproto:x:1000:1000::/home/evoproto:/bin/bash\nsvc:x:998:998::/:/usr/sbin/nologin\n";
        assert!(passwd_has_user(p, "evoproto"));
        assert!(!passwd_has_user(p, "root"));
        assert!(!passwd_has_user(p, "svc"));
        assert!(!passwd_has_user(p, "ghost"));
    }
}
