// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! What the host decides about senders: who may connect, whether a stream may
//! start, and how a running session is ended from outside.
//!
//! The library enforces nothing about *who* a sender is; that is the host's
//! to say. It does guarantee that a stream is never started without the
//! host's gate having said yes, that every yes is followed by exactly one
//! release, and that the release comes only after the audio sink has been
//! closed.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::oneshot;

/// The gate's answer when a sender asks to start a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Start the stream.
    Allow,
    /// Refuse it. The sender is answered `453` and nothing is created.
    Refuse,
}

/// The host's say over who connects and whether a stream starts.
pub trait SessionGate: Send + Sync + 'static {
    /// Whether to talk to this peer at all, asked as soon as a connection is
    /// accepted. `false` drops the connection with no reply. The address is
    /// canonical: an IPv4 sender on a dual-stack listener is reported as IPv4.
    fn accept_peer(&self, peer: IpAddr) -> bool {
        let _ = peer;
        true
    }

    /// Whether a stream from this peer may start, asked before anything for
    /// the stream is created. Every [`Admission::Allow`] is followed by one
    /// call to [`released`](Self::released), however the session ends.
    fn admit(&self, peer: IpAddr) -> Admission;

    /// The admitted session is over and its audio sink is closed.
    fn released(&self, peer: IpAddr);
}

/// The default gate: one stream at a time, from anyone. A second sender is
/// refused while a stream is running and admitted again after it ends.
#[derive(Debug, Default)]
pub struct SingleSession {
    busy: AtomicBool,
}

impl SingleSession {
    /// A gate with no stream running.
    pub fn new() -> SingleSession {
        SingleSession::default()
    }
}

impl SessionGate for SingleSession {
    fn admit(&self, _peer: IpAddr) -> Admission {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Admission::Allow
        } else {
            Admission::Refuse
        }
    }

    fn released(&self, _peer: IpAddr) {
        self.busy.store(false, Ordering::Release);
    }
}

/// A handle with which the host ends the running session from outside: when
/// its operator disables the receiver, or the player says stop.
#[derive(Clone, Default)]
pub struct SessionControl {
    slot: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl SessionControl {
    /// A handle with no session registered.
    pub fn new() -> SessionControl {
        SessionControl::default()
    }

    /// End the most recently started session: its control connection is
    /// closed, its sink is closed and the gate's release follows. Returns
    /// `true` if a session was running.
    pub fn end_session(&self) -> bool {
        let sender = self
            .slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        sender.is_some_and(|tx| tx.send(()).is_ok())
    }

    /// Register a started session; the returned receiver fires when the host
    /// asks for it to end.
    pub(crate) fn register(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
        rx
    }

    /// Forget a registration whose session has ended (its receiver is gone).
    pub(crate) fn forget_closed(&self) {
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.as_ref().is_some_and(oneshot::Sender::is_closed) {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> IpAddr {
        "192.168.1.9".parse().expect("address")
    }

    #[test]
    fn single_session_admits_one_and_refuses_the_second() {
        let gate = SingleSession::new();
        assert_eq!(gate.admit(peer()), Admission::Allow);
        assert_eq!(gate.admit(peer()), Admission::Refuse);
        gate.released(peer());
        assert_eq!(gate.admit(peer()), Admission::Allow);
    }

    #[tokio::test]
    async fn end_session_fires_the_registered_session_once() {
        let control = SessionControl::new();
        assert!(!control.end_session(), "nothing running");
        let rx = control.register();
        assert!(control.end_session());
        assert!(rx.await.is_ok(), "the session is told to end");
        assert!(!control.end_session(), "only once");
    }

    #[tokio::test]
    async fn a_finished_session_is_forgotten() {
        let control = SessionControl::new();
        let rx = control.register();
        drop(rx);
        control.forget_closed();
        assert!(!control.end_session());
    }
}
