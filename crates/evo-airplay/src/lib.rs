// Copyright (c) 2026 Stefan Arentz; modifications (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0 AND MIT
//! An embeddable AirPlay 2 audio **receiver**: a real Mac/iPhone pairs with
//! it and streams to it; the host application gets decoded PCM and session
//! events. The library owns network → PCM (transient pairing, the encrypted
//! control channel, the buffered-audio channel, decrypt, AAC decode, and the
//! pause/seek/backpressure semantics); the host owns PCM → speaker via an
//! [`AudioSink`], mDNS registration (see [`Receiver::txt_records`] and
//! [`Receiver::raop_txt_records`]) and the policy over who may stream (see
//! [`SessionGate`] and [`SessionControl`]).
//!
//! Scope: AirPlay 2 **buffered** audio only (AAC-LC, 44.1 kHz stereo); a
//! realtime or AirPlay 1 stream is refused. One sender → one stream → one
//! output; the default gate refuses a second sender while one is streaming.
//! No PTP — for a single output, the sender's buffering plus this library's
//! backpressure suffice (blocking [`AudioSink::write`] paces playback). No
//! AirPlay wire concepts (plists, `shk`, sequence numbers) appear in this API.
//!
//! A stream is never accepted that cannot be played: if the sink cannot be
//! created, the sender is refused and nothing is announced.
//!
//! ```no_run
//! use evo_airplay::{AudioSink, Event, Receiver, SinkError};
//!
//! struct MySink;
//!
//! impl AudioSink for MySink {
//!     fn write(&mut self, pcm: &[i16]) { /* blocking write paces playback */ }
//!     fn flush(&mut self) { /* seek: drop device/prebuffer state */ }
//! }
//!
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     let receiver = Receiver::builder()
//!         .name("Office")
//!         .identity_path("/var/lib/myapp/airplay-identity")
//!         .build()?;
//!     // Register `receiver.txt_records()` / `raop_txt_records()` with mDNS.
//!     let control = receiver.session_control(); // control.end_session()
//!     let _ = control;
//!     let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
//!     tokio::spawn(async move {
//!         while let Some(event) = rx.recv().await {
//!             if let Event::Volume { db } = event { /* your gain path */ }
//!         }
//!     });
//!     receiver
//!         .run(
//!             |_rate, _channels| -> Result<Box<dyn AudioSink>, SinkError> {
//!                 Ok(Box::new(MySink))
//!             },
//!             events,
//!         )
//!         .await
//! }
//! ```

#![warn(missing_docs)]

mod apple_interop;
mod bplist;
mod buffered;
mod crypto_stream;
mod decode;
mod dmap;
mod events;
mod fairplay;
#[cfg(test)]
mod hostile;
mod http;
mod identity;
mod info;
mod limiter;
mod mac;
mod pairing;
mod player;
mod policy;
mod receiver;
mod session;
mod sink;

// Sender-side pieces the integration tests (and a future test-sender) drive
// the real server with. Public so the tests can reach them, but not part of
// the documented embedding API.
#[doc(hidden)]
pub mod cipher;
#[doc(hidden)]
pub mod sender;
#[doc(hidden)]
pub mod server;
#[doc(hidden)]
pub mod srp;
#[doc(hidden)]
pub mod tlv;

pub use events::{Event, EventSender};
pub use identity::Identity;
pub use info::txt_records;
pub use limiter::PairingLimiter;
pub use policy::{Admission, SessionControl, SessionGate, SingleSession};
pub use receiver::{Receiver, ReceiverBuilder};
pub use sink::{AudioSink, SinkError, SinkFactory};

/// Receiver-wide configuration, resolved by [`ReceiverBuilder::build`].
#[derive(Debug, Clone)]
pub struct Config {
    /// The receiver name senders see in the AirPlay picker.
    pub name: String,
    /// TCP port of the HTTP/RTSP control server.
    pub port: u16,
    /// The MAC address used as the AirPlay `deviceid`.
    pub mac: [u8; 6],
    /// Apple model string, e.g. `EvoAudio,1`.
    pub model: String,
    /// AirPlay source version, e.g. `366.0`.
    pub source_version: String,
    /// 64-bit AirPlay 2 capability bitmask.
    pub features: u64,
    /// AirPlay status flags.
    pub status_flags: u32,
    /// `Some` → require this password to pair: iOS and macOS show a
    /// "password" dialog and accept alphanumerics, not just digits. It is
    /// advertised as "password required" via status-flag bit 7 and entered by
    /// the sender during transient pairing. `None` → the receiver is **open**:
    /// pairing uses the protocol's fixed, public code, so any device that can
    /// reach it may stream. Never logged.
    pub password: Option<String>,
}

impl Config {
    /// MAC as colon-separated uppercase hex, e.g. `AA:BB:CC:DD:EE:FF`. This is
    /// the AirPlay `deviceid`.
    pub fn device_id(&self) -> String {
        self.mac
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// The `features` bitmask split into `(low32, high32)`, the form used in
    /// both the mDNS TXT record and `GET /info`.
    pub fn features_split(&self) -> (u32, u32) {
        (self.features as u32, (self.features >> 32) as u32)
    }
}
