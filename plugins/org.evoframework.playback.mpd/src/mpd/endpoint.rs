//! MPD server endpoint specification.
//!
//! Two transports are supported: TCP for remote MPD instances and the
//! classic localhost case, Unix domain sockets for the default modern
//! deployment (`/run/mpd/socket`). Both are validated at construction
//! time so configuration errors surface before any I/O is attempted.

use std::path::PathBuf;

use super::error::ConfigError;

/// Where to reach the MPD daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MpdEndpoint {
    /// Reach MPD over TCP.
    Tcp {
        /// Hostname or IP literal. Must be non-empty after trimming.
        host: String,
        /// TCP port (typically 6600).
        port: u16,
    },
    /// Reach MPD over a Unix domain socket.
    Unix {
        /// Absolute path to the socket. Must be non-empty.
        path: PathBuf,
    },
}

impl MpdEndpoint {
    /// Build a TCP endpoint, validating the host.
    ///
    /// Empty or whitespace-only hosts are refused: a silent default
    /// would let configuration mistakes reach the network stack and
    /// produce confusing failure modes.
    pub(crate) fn tcp(
        host: impl Into<String>,
        port: u16,
    ) -> Result<Self, ConfigError> {
        let host = host.into();
        if host.trim().is_empty() {
            return Err(ConfigError::EmptyHost);
        }
        Ok(Self::Tcp { host, port })
    }

    /// Build a Unix-socket endpoint, validating the path.
    ///
    /// Empty paths are refused. The file's existence is NOT checked
    /// here; that is a run-time concern handled at connect time.
    pub(crate) fn unix(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(ConfigError::EmptyPath);
        }
        Ok(Self::Unix { path })
    }
}

impl std::fmt::Display for MpdEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp { host, port } => write!(f, "tcp://{}:{}", host, port),
            Self::Unix { path } => write!(f, "unix://{}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_rejects_empty_host() {
        assert!(matches!(
            MpdEndpoint::tcp("", 6600),
            Err(ConfigError::EmptyHost)
        ));
    }

    #[test]
    fn tcp_rejects_whitespace_host() {
        assert!(matches!(
            MpdEndpoint::tcp("   ", 6600),
            Err(ConfigError::EmptyHost)
        ));
    }

    #[test]
    fn tcp_builds_with_valid_host() {
        let ep = MpdEndpoint::tcp("localhost", 6600).unwrap();
        assert_eq!(format!("{}", ep), "tcp://localhost:6600");
    }

    #[test]
    fn tcp_accepts_ip_literal() {
        let ep = MpdEndpoint::tcp("127.0.0.1", 6600).unwrap();
        assert_eq!(format!("{}", ep), "tcp://127.0.0.1:6600");
    }

    #[test]
    fn tcp_accepts_ipv6_literal() {
        // Note: this layer does not bracket the IPv6 literal; that is
        // the caller's responsibility for the host field. The Display
        // formats exactly what was configured.
        let ep = MpdEndpoint::tcp("::1", 6600).unwrap();
        assert_eq!(format!("{}", ep), "tcp://::1:6600");
    }

    #[test]
    fn unix_rejects_empty_path() {
        assert!(matches!(MpdEndpoint::unix(""), Err(ConfigError::EmptyPath)));
    }

    #[test]
    fn unix_builds_with_valid_path() {
        let ep = MpdEndpoint::unix("/run/mpd/socket").unwrap();
        assert_eq!(format!("{}", ep), "unix:///run/mpd/socket");
    }

    #[test]
    fn endpoints_equal_when_fields_match() {
        let a = MpdEndpoint::tcp("localhost", 6600).unwrap();
        let b = MpdEndpoint::tcp("localhost", 6600).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn endpoints_differ_by_port() {
        let a = MpdEndpoint::tcp("localhost", 6600).unwrap();
        let b = MpdEndpoint::tcp("localhost", 6601).unwrap();
        assert_ne!(a, b);
    }
}
