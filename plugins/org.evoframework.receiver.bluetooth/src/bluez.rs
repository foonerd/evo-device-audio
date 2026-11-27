// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The platform's Bluetooth stack, as the plugin needs it.
//!
//! [`Bluez`] is the whole of what the receiver asks of the host stack: read
//! and set the adapter's flags, register the sink endpoint and the pairing
//! agent, list and remove devices, and be told when something changes. The
//! real implementation talks to the stack over the system message bus (see
//! [`crate::dbus`]); the tests use an in-memory fake of it.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::policy::PairingGate;

/// The A2DP sink profile.
pub const SINK_UUID: &str = "0000110b-0000-1000-8000-00805f9b34fb";

/// What the sink endpoint offers a source, as the four SBC capability bytes:
/// 44.1 and 48 kHz; mono, dual channel, stereo and joint stereo; 4, 8, 12 and
/// 16 blocks; 4 and 8 subbands; loudness and SNR allocation; bitpool 2 to 53.
pub const SBC_CAPABILITIES: [u8; 4] = [0x3f, 0xff, 2, 53];

/// The adapter as the stack reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    /// The adapter's address.
    pub address: String,
    /// The name a phone shows.
    pub alias: String,
    /// Whether the radio is on.
    pub powered: bool,
    /// Whether the adapter answers inquiries.
    pub discoverable: bool,
    /// Whether the adapter accepts pairing.
    pub pairable: bool,
    /// Seconds the stack leaves the adapter discoverable (0 = no limit).
    pub discoverable_timeout: u32,
    /// Seconds the stack leaves the adapter pairable (0 = no limit).
    pub pairable_timeout: u32,
}

/// A device the stack knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceInfo {
    /// The device's address.
    pub address: String,
    /// The name it gave.
    pub name: String,
    /// Whether it is paired with this adapter.
    pub paired: bool,
    /// Whether it is connected now.
    pub connected: bool,
}

/// Why a call to the stack failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BtError {
    /// The stack answers but there is no adapter.
    #[error("no Bluetooth adapter")]
    NoAdapter,
    /// The stack is not running or cannot be reached.
    #[error("the Bluetooth stack is unavailable: {0}")]
    Unavailable(String),
    /// The radio is blocked (rfkill) and the stack will not power it.
    #[error("the Bluetooth adapter is blocked")]
    Blocked,
    /// The stack refused a request.
    #[error("the Bluetooth stack refused: {0}")]
    Refused(String),
    /// A device it was asked about is not known.
    #[error("no such device")]
    NotFound,
}

/// Something changed that the plugin should look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A device or the adapter changed: read them again.
    Changed,
    /// A device finished pairing (address).
    Paired(String),
    /// The stack went away.
    Gone,
    /// The stack came back.
    Back,
}

/// The host Bluetooth stack.
#[async_trait]
pub trait Bluez: Send + Sync {
    /// The adapter now.
    async fn adapter(&self) -> Result<AdapterInfo, BtError>;
    /// Switch the radio.
    async fn set_powered(&self, on: bool) -> Result<(), BtError>;
    /// Allow or forbid inquiries.
    async fn set_discoverable(&self, on: bool) -> Result<(), BtError>;
    /// Allow or forbid pairing.
    async fn set_pairable(&self, on: bool) -> Result<(), BtError>;
    /// Set how long the stack keeps the adapter discoverable and pairable.
    async fn set_timeouts(
        &self,
        discoverable: u32,
        pairable: u32,
    ) -> Result<(), BtError>;
    /// Register the sink endpoint and the pairing agent. The agent consults
    /// `gate` for every request it is asked about.
    async fn register(&self, gate: Arc<PairingGate>) -> Result<(), BtError>;
    /// Take both registrations back. Safe to call when none is held.
    async fn unregister(&self);
    /// Every device the stack knows.
    async fn devices(&self) -> Result<Vec<DeviceInfo>, BtError>;
    /// Forget a device: disconnect it and drop its pairing.
    async fn remove_device(&self, address: &str) -> Result<(), BtError>;
    /// Be told of changes.
    fn events(&self) -> broadcast::Receiver<Event>;
}

/// Reaches the stack. Called at every start, so a stack that is not up yet is
/// found when it is.
#[async_trait]
pub trait Connector: Send + Sync {
    /// Connect to the stack.
    async fn connect(&self) -> Result<Arc<dyn Bluez>, BtError>;
}

#[cfg(test)]
pub(crate) mod fake {
    //! A stack in memory, and the rules of the real one that matter here.

    use super::*;
    use std::sync::Mutex;

    pub(crate) struct FakeState {
        pub(crate) adapter: Option<AdapterInfo>,
        pub(crate) devices: Vec<DeviceInfo>,
        pub(crate) registered: bool,
        pub(crate) gate: Option<Arc<PairingGate>>,
        /// Powering the radio fails as blocked.
        pub(crate) blocked: bool,
        /// Registering fails as refused.
        pub(crate) refuse_register: bool,
        /// Every state-changing call, in order.
        pub(crate) calls: Vec<String>,
        /// Every address `remove_device` was asked about, known or not.
        pub(crate) remove_asked: Vec<String>,
    }

    pub(crate) struct FakeBluez {
        pub(crate) st: Mutex<FakeState>,
        tx: broadcast::Sender<Event>,
    }

    pub(crate) fn adapter(powered: bool) -> AdapterInfo {
        AdapterInfo {
            address: "D8:3A:DD:B8:D6:75".into(),
            alias: "testspeaker".into(),
            powered,
            discoverable: false,
            pairable: false,
            discoverable_timeout: 180,
            pairable_timeout: 0,
        }
    }

    impl FakeBluez {
        pub(crate) fn new(adapter: Option<AdapterInfo>) -> Arc<Self> {
            let (tx, _) = broadcast::channel(64);
            Arc::new(Self {
                st: Mutex::new(FakeState {
                    adapter,
                    devices: Vec::new(),
                    registered: false,
                    gate: None,
                    blocked: false,
                    refuse_register: false,
                    calls: Vec::new(),
                    remove_asked: Vec::new(),
                }),
                tx,
            })
        }

        pub(crate) fn send(&self, e: Event) {
            let _ = self.tx.send(e);
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            self.st.lock().unwrap().calls.clone()
        }

        pub(crate) fn adapter_now(&self) -> AdapterInfo {
            self.st.lock().unwrap().adapter.clone().expect("adapter")
        }

        /// A remote device tries to pair. The stack refuses unless the adapter
        /// is pairable; the agent, if it is asked, refuses unless the window
        /// is open. Returns why it was refused.
        pub(crate) fn pair(
            &self,
            address: &str,
            name: &str,
        ) -> Result<(), &'static str> {
            {
                let mut s = self.st.lock().unwrap();
                let a = s.adapter.as_ref().ok_or("no adapter")?;
                if !a.pairable {
                    return Err("the adapter is not pairable");
                }
                if let Some(g) = &s.gate {
                    if !g.is_open() {
                        return Err("the agent rejected the request");
                    }
                }
                s.devices.push(DeviceInfo {
                    address: address.into(),
                    name: name.into(),
                    paired: true,
                    connected: false,
                });
            }
            self.send(Event::Paired(address.into()));
            Ok(())
        }

        pub(crate) fn set_connected(&self, address: &str, connected: bool) {
            for d in self.st.lock().unwrap().devices.iter_mut() {
                if d.address == address {
                    d.connected = connected;
                }
            }
            self.send(Event::Changed);
        }

        pub(crate) fn add_paired(&self, address: &str, name: &str) {
            self.st.lock().unwrap().devices.push(DeviceInfo {
                address: address.into(),
                name: name.into(),
                paired: true,
                connected: false,
            });
        }
    }

    #[async_trait]
    impl Bluez for FakeBluez {
        async fn adapter(&self) -> Result<AdapterInfo, BtError> {
            self.st
                .lock()
                .unwrap()
                .adapter
                .clone()
                .ok_or(BtError::NoAdapter)
        }
        async fn set_powered(&self, on: bool) -> Result<(), BtError> {
            let mut s = self.st.lock().unwrap();
            if on && s.blocked {
                return Err(BtError::Blocked);
            }
            s.calls.push(format!("powered={on}"));
            s.adapter.as_mut().ok_or(BtError::NoAdapter)?.powered = on;
            Ok(())
        }
        async fn set_discoverable(&self, on: bool) -> Result<(), BtError> {
            let mut s = self.st.lock().unwrap();
            s.calls.push(format!("discoverable={on}"));
            s.adapter.as_mut().ok_or(BtError::NoAdapter)?.discoverable = on;
            Ok(())
        }
        async fn set_pairable(&self, on: bool) -> Result<(), BtError> {
            let mut s = self.st.lock().unwrap();
            s.calls.push(format!("pairable={on}"));
            s.adapter.as_mut().ok_or(BtError::NoAdapter)?.pairable = on;
            Ok(())
        }
        async fn set_timeouts(
            &self,
            discoverable: u32,
            pairable: u32,
        ) -> Result<(), BtError> {
            let mut s = self.st.lock().unwrap();
            s.calls.push(format!("timeouts={discoverable},{pairable}"));
            let a = s.adapter.as_mut().ok_or(BtError::NoAdapter)?;
            a.discoverable_timeout = discoverable;
            a.pairable_timeout = pairable;
            Ok(())
        }
        async fn register(
            &self,
            gate: Arc<PairingGate>,
        ) -> Result<(), BtError> {
            let mut s = self.st.lock().unwrap();
            if s.refuse_register {
                // The stack takes the first registration and refuses the
                // next: half done, as the real one can be.
                s.registered = true;
                s.gate = Some(gate);
                return Err(BtError::Refused("agent".into()));
            }
            s.calls.push("register".into());
            s.registered = true;
            s.gate = Some(gate);
            Ok(())
        }
        async fn unregister(&self) {
            let mut s = self.st.lock().unwrap();
            if s.registered {
                s.calls.push("unregister".into());
            }
            s.registered = false;
            s.gate = None;
        }
        async fn devices(&self) -> Result<Vec<DeviceInfo>, BtError> {
            Ok(self.st.lock().unwrap().devices.clone())
        }
        async fn remove_device(&self, address: &str) -> Result<(), BtError> {
            let mut s = self.st.lock().unwrap();
            s.remove_asked.push(address.to_string());
            let before = s.devices.len();
            s.devices.retain(|d| d.address != address);
            if s.devices.len() == before {
                return Err(BtError::NotFound);
            }
            s.calls.push(format!("remove={address}"));
            Ok(())
        }
        fn events(&self) -> broadcast::Receiver<Event> {
            self.tx.subscribe()
        }
    }

    /// Hands out one stack, or an error.
    pub(crate) struct FakeConnector {
        pub(crate) target: Mutex<Result<Arc<FakeBluez>, BtError>>,
        pub(crate) connects: std::sync::atomic::AtomicUsize,
    }

    impl FakeConnector {
        pub(crate) fn to(bluez: &Arc<FakeBluez>) -> Arc<Self> {
            Arc::new(Self {
                target: Mutex::new(Ok(bluez.clone())),
                connects: Default::default(),
            })
        }
        pub(crate) fn set(&self, to: Result<Arc<FakeBluez>, BtError>) {
            *self.target.lock().unwrap() = to;
        }
    }

    #[async_trait]
    impl Connector for FakeConnector {
        async fn connect(&self) -> Result<Arc<dyn Bluez>, BtError> {
            self.connects
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match &*self.target.lock().unwrap() {
                Ok(b) => Ok(b.clone() as Arc<dyn Bluez>),
                Err(e) => Err(e.clone()),
            }
        }
    }
}
