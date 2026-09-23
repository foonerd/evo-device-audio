use std::path::PathBuf;
use std::process::Command;

use evo::appointments::RtcWakeCallback;

const RTC_WAKEALARM_PATH: &str = "/sys/class/rtc/rtc0/wakealarm";
const RTC_WAKE_WRAPPER: &str = "/usr/local/bin/evo-rtc-wake";

pub(crate) struct SysfsRtcWake {
    path: PathBuf,
}

impl SysfsRtcWake {
    pub(crate) fn new() -> Self {
        Self {
            path: PathBuf::from(RTC_WAKEALARM_PATH),
        }
    }
}

impl RtcWakeCallback for SysfsRtcWake {
    fn program_wake(&self, at_ms_utc: Option<u64>) {
        let value = at_ms_utc
            .map(|at_ms| (at_ms / 1_000).to_string())
            .unwrap_or_else(|| "0".to_owned());
        let result = Command::new("sudo")
            .args(["-n", RTC_WAKE_WRAPPER, &value])
            .status();

        match result {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::error!(
                path = %self.path.display(),
                ?status,
                "failed to program RTC wake alarm"
            ),
            Err(error) => tracing::error!(
                path = %self.path.display(),
                ?error,
                "failed to invoke RTC wake wrapper"
            ),
        }
    }
}
