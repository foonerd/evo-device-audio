//! MPD daemon restart strategy.
//!
//! After re-writing the framework-controlled `audio_output`
//! fragment, MPD has to re-read its configuration to pick the
//! new device / format up. There is no in-band MPD command for
//! that — the operator-visible way is `systemctl restart mpd`.
//! The steward process (running as the distribution's service
//! user, not root) reaches systemctl through a passwordless
//! sudoers drop-in scoped specifically to `systemctl restart
//! mpd`. The drop-in is shipped by the distribution
//! (`evo-device-volumio` etc.) as part of bring-up; the plugin
//! invokes `sudo -n systemctl restart mpd` and surfaces a
//! structured failure if the drop-in is missing.
//!
//! The [`MpdRestarter`] trait abstracts the invocation so the
//! plugin's tests can substitute a deterministic counter
//! instead of touching real systemctl. Production constructs
//! [`SudoSystemctlRestarter`]; tests construct
//! [`NoOpRestarter`] and assert on its call count.

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

/// Future returned by [`MpdRestarter::restart`]. The trait is
/// object-safe at the cost of explicit pinning; the
/// [`tokio::process`] command path is `Send` and `'static` so
/// this fits.
pub type RestartFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Restart strategy for the MPD daemon. Implementations
/// signal the daemon to reload its configuration. Errors are
/// operator-readable strings (no structured taxonomy because
/// the plugin treats every failure mode equivalently — log,
/// publish to the worker status channel, keep the previous
/// state).
pub trait MpdRestarter: Send + Sync + Debug {
    /// Initiate the restart. Resolves on completion of the
    /// underlying signal (`Ok(())` on success; `Err(reason)`
    /// when systemctl rejected the request, sudo refused the
    /// drop-in, or the process failed to spawn).
    fn restart(&self) -> RestartFuture<'_>;
}

/// Production restart strategy: invokes `sudo -n systemctl
/// restart mpd`. Relies on the distribution's bring-up shipping
/// a sudoers drop-in that whitelists this exact command for the
/// steward's service user; absence surfaces as a sudo refusal
/// the plugin logs and remembers.
#[derive(Debug, Default)]
pub struct SudoSystemctlRestarter;

impl SudoSystemctlRestarter {
    /// Construct a fresh restarter. No internal state — the
    /// production strategy is stateless.
    pub fn new() -> Self {
        Self
    }
}

impl MpdRestarter for SudoSystemctlRestarter {
    fn restart(&self) -> RestartFuture<'_> {
        Box::pin(async move {
            let output = tokio::process::Command::new("sudo")
                .args(["-n", "systemctl", "restart", "mpd"])
                .output()
                .await
                .map_err(|e| format!("failed to spawn sudo systemctl: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "sudo -n systemctl restart mpd failed: exit={:?} \
                     stderr={}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Ok(())
        })
    }
}

/// Test-only restart strategy: counts invocations and returns
/// `Ok(())` without touching systemctl. The counter survives
/// arbitrary numbers of `restart` calls and is readable through
/// [`Self::call_count`].
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct NoOpRestarter {
    call_count: AtomicU64,
}

#[cfg(test)]
impl NoOpRestarter {
    /// Construct a fresh restarter with a zero counter.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Cumulative `restart` invocations on this counter.
    pub(crate) fn call_count(&self) -> u64 {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
impl MpdRestarter for NoOpRestarter {
    fn restart(&self) -> RestartFuture<'_> {
        Box::pin(async move {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

/// Test-only restart strategy that always reports a fixed
/// failure. Used to drive the worker's failure-path tests
/// (status channel publishes `Failed`; the worker keeps
/// running and reattempts on the next route change).
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FailingRestarter {
    reason: String,
    call_count: AtomicU64,
}

#[cfg(test)]
impl FailingRestarter {
    /// Construct a restarter that fails every call with the
    /// supplied reason.
    pub(crate) fn new<S: Into<String>>(reason: S) -> Self {
        Self {
            reason: reason.into(),
            call_count: AtomicU64::new(0),
        }
    }

    /// Cumulative `restart` invocations on this counter.
    pub(crate) fn call_count(&self) -> u64 {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
impl MpdRestarter for FailingRestarter {
    fn restart(&self) -> RestartFuture<'_> {
        let reason = self.reason.clone();
        Box::pin(async move {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Err(reason)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn noop_restarter_counts_invocations() {
        let r = NoOpRestarter::new();
        assert_eq!(r.call_count(), 0);
        r.restart().await.unwrap();
        assert_eq!(r.call_count(), 1);
        r.restart().await.unwrap();
        r.restart().await.unwrap();
        assert_eq!(r.call_count(), 3);
    }

    #[tokio::test]
    async fn failing_restarter_returns_configured_reason() {
        let r = FailingRestarter::new("sudoers drop-in missing");
        let err = r.restart().await.unwrap_err();
        assert_eq!(err, "sudoers drop-in missing");
        assert_eq!(r.call_count(), 1);
    }

    #[tokio::test]
    async fn restarter_is_object_safe_through_arc_dyn() {
        let r: Arc<dyn MpdRestarter> = Arc::new(NoOpRestarter::new());
        r.restart().await.unwrap();
    }
}
