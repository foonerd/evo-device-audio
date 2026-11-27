// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Registering the device with the local mDNS daemon.
//!
//! The daemon is the distribution's own Avahi. The plugin asks it over the
//! system message bus to publish two services for the one device,
//! `_airplay._tcp` and `_raop._tcp`, in a single entry group, and to withdraw
//! them. The registration is tied to the connection: if the plugin dies, the
//! daemon drops the entry group with it, so a crashed receiver cannot leave a
//! phantom device on the network.

use std::time::Duration;

use async_trait::async_trait;
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

const AVAHI_DEST: &str = "org.freedesktop.Avahi";
const SERVER_IFACE: &str = "org.freedesktop.Avahi.Server";
const GROUP_IFACE: &str = "org.freedesktop.Avahi.EntryGroup";
const IF_UNSPEC: i32 = -1;
const PROTO_UNSPEC: i32 = -1;
/// How long the daemon is given to answer before the registration is called
/// failed. A hung daemon must not hold an operator's verb past its budget.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);

/// What to publish for the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRecords {
    /// The `_airplay._tcp` instance name.
    pub airplay_name: String,
    /// The `_raop._tcp` instance name (`<device id>@<name>`).
    pub raop_name: String,
    /// The control port both services point at.
    pub port: u16,
    /// `_airplay._tcp` TXT records, as `key=value`.
    pub airplay_txt: Vec<String>,
    /// `_raop._tcp` TXT records, as `key=value`.
    pub raop_txt: Vec<String>,
}

/// Why the device could not be registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct AdvertiseError(pub String);

/// Something that can publish the device.
#[async_trait]
pub trait Advertiser: Send + Sync {
    /// Publish `records`. The returned registration keeps them published.
    async fn register(
        &self,
        records: &ServiceRecords,
    ) -> Result<Box<dyn Registration>, AdvertiseError>;
}

/// A live publication. Dropping it without calling `withdraw` still ends it,
/// only less promptly.
#[async_trait]
pub trait Registration: Send {
    /// Withdraw the services now and wait for the daemon to confirm.
    async fn withdraw(self: Box<Self>);
}

/// The system Avahi daemon, over the system message bus.
#[derive(Debug, Default)]
pub struct AvahiAdvertiser;

struct AvahiRegistration {
    connection: Connection,
    group: OwnedObjectPath,
}

fn fail(what: &str, e: impl std::fmt::Display) -> AdvertiseError {
    AdvertiseError(format!("{what}: {e}"))
}

#[async_trait]
impl Advertiser for AvahiAdvertiser {
    async fn register(
        &self,
        records: &ServiceRecords,
    ) -> Result<Box<dyn Registration>, AdvertiseError> {
        match tokio::time::timeout(REGISTER_TIMEOUT, publish(records)).await {
            Ok(result) => result,
            Err(_) => Err(AdvertiseError(
                "the mDNS daemon did not answer in time".into(),
            )),
        }
    }
}

async fn publish(
    records: &ServiceRecords,
) -> Result<Box<dyn Registration>, AdvertiseError> {
    let connection = Connection::system()
        .await
        .map_err(|e| fail("system bus", e))?;
    let group: OwnedObjectPath = connection
        .call_method(
            Some(AVAHI_DEST),
            "/",
            Some(SERVER_IFACE),
            "EntryGroupNew",
            &(),
        )
        .await
        .map_err(|e| fail("mDNS daemon", e))?
        .body()
        .deserialize()
        .map_err(|e| fail("mDNS daemon reply", e))?;

    for (name, service, txt) in [
        (&records.airplay_name, "_airplay._tcp", &records.airplay_txt),
        (&records.raop_name, "_raop._tcp", &records.raop_txt),
    ] {
        let txt: Vec<Vec<u8>> =
            txt.iter().map(|r| r.clone().into_bytes()).collect();
        connection
            .call_method(
                Some(AVAHI_DEST),
                &group,
                Some(GROUP_IFACE),
                "AddService",
                &(
                    IF_UNSPEC,
                    PROTO_UNSPEC,
                    0u32,
                    name.as_str(),
                    service,
                    "",
                    "",
                    records.port,
                    txt,
                ),
            )
            .await
            .map_err(|e| fail(service, e))?;
    }
    connection
        .call_method(Some(AVAHI_DEST), &group, Some(GROUP_IFACE), "Commit", &())
        .await
        .map_err(|e| fail("commit", e))?;
    Ok(Box::new(AvahiRegistration { connection, group }))
}

#[async_trait]
impl Registration for AvahiRegistration {
    async fn withdraw(self: Box<Self>) {
        let freed = self
            .connection
            .call_method(
                Some(AVAHI_DEST),
                &self.group,
                Some(GROUP_IFACE),
                "Free",
                &(),
            )
            .await;
        if let Err(e) = freed {
            tracing::debug!(error = %e, "mDNS entry group free failed; dropping the connection withdraws it");
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! An advertiser that publishes nowhere, for the runtime's tests.

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Records what was published and what is still live.
    #[derive(Default)]
    pub struct FakeAdvertiser {
        /// Make the next registrations fail.
        pub fail: AtomicBool,
        /// Every set of records ever published.
        pub published: Mutex<Vec<ServiceRecords>>,
        /// Registrations currently live.
        pub live: Arc<AtomicUsize>,
    }

    struct FakeRegistration(Arc<AtomicUsize>);

    #[async_trait]
    impl Advertiser for FakeAdvertiser {
        async fn register(
            &self,
            records: &ServiceRecords,
        ) -> Result<Box<dyn Registration>, AdvertiseError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(AdvertiseError("no mDNS daemon".into()));
            }
            self.published.lock().unwrap().push(records.clone());
            self.live.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeRegistration(self.live.clone())))
        }
    }

    #[async_trait]
    impl Registration for FakeRegistration {
        async fn withdraw(self: Box<Self>) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
