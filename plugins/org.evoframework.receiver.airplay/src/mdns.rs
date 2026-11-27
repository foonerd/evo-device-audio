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
use futures::StreamExt;
use tokio::task::JoinHandle;
use zbus::message::Type as MessageType;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, MatchRule, MessageStream};

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

/// Why a standing registration ended without being withdrawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loss {
    /// Another device on the network is using the name.
    Collision,
    /// The mDNS daemon went away (stopped or restarted); its entries went
    /// with it.
    DaemonGone,
}

/// A registration that stands, and the way to hear that it stops standing.
pub struct Registered {
    /// The publication. Withdraw it to end it.
    pub registration: Box<dyn Registration>,
    /// Resolves if the registration is lost without being withdrawn.
    pub lost: tokio::sync::oneshot::Receiver<Loss>,
}

/// Something that can publish the device.
#[async_trait]
pub trait Advertiser: Send + Sync {
    /// Publish `records`. The registration keeps them published until it is
    /// withdrawn; `lost` says if it stopped standing first.
    async fn register(
        &self,
        records: &ServiceRecords,
    ) -> Result<Registered, AdvertiseError>;
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
    watcher: JoinHandle<()>,
}

fn fail(what: &str, e: impl std::fmt::Display) -> AdvertiseError {
    AdvertiseError(format!("{what}: {e}"))
}

#[async_trait]
impl Advertiser for AvahiAdvertiser {
    async fn register(
        &self,
        records: &ServiceRecords,
    ) -> Result<Registered, AdvertiseError> {
        match tokio::time::timeout(REGISTER_TIMEOUT, publish(records)).await {
            Ok(result) => result,
            Err(_) => Err(AdvertiseError(
                "the mDNS daemon did not answer in time".into(),
            )),
        }
    }
}

/// The group's own state changes, as the daemon sends them.
const GROUP_COLLISION: i32 = 3;
const GROUP_FAILURE: i32 = 4;

async fn publish(
    records: &ServiceRecords,
) -> Result<Registered, AdvertiseError> {
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

    // Listen before committing, so no change of state is missed: the daemon
    // probes the name for a moment after the commit and may find another
    // device using it, or itself go away.
    let state_rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(AVAHI_DEST)
        .map_err(|e| fail("match rule", e))?
        .interface(GROUP_IFACE)
        .map_err(|e| fail("match rule", e))?
        .member("StateChanged")
        .map_err(|e| fail("match rule", e))?
        .path(group.as_ref())
        .map_err(|e| fail("match rule", e))?
        .build();
    let state = MessageStream::for_match_rule(state_rule, &connection, None)
        .await
        .map_err(|e| fail("state subscription", e))?;
    let owner_rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.freedesktop.DBus")
        .map_err(|e| fail("match rule", e))?
        .interface("org.freedesktop.DBus")
        .map_err(|e| fail("match rule", e))?
        .member("NameOwnerChanged")
        .map_err(|e| fail("match rule", e))?
        .add_arg(AVAHI_DEST)
        .map_err(|e| fail("match rule", e))?
        .build();
    let owner = MessageStream::for_match_rule(owner_rule, &connection, None)
        .await
        .map_err(|e| fail("owner subscription", e))?;

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

    let (tx, lost) = tokio::sync::oneshot::channel();
    let watcher = tokio::spawn(watch(state, owner, tx));
    Ok(Registered {
        registration: Box::new(AvahiRegistration {
            connection,
            group,
            watcher,
        }),
        lost,
    })
}

/// Say, once, that the registration stopped standing.
async fn watch(
    mut state: MessageStream,
    mut owner: MessageStream,
    tx: tokio::sync::oneshot::Sender<Loss>,
) {
    let loss = loop {
        tokio::select! {
            msg = state.next() => {
                let Some(Ok(msg)) = msg else { break Loss::DaemonGone };
                if let Ok((code, _reason)) =
                    msg.body().deserialize::<(i32, String)>()
                {
                    if code == GROUP_COLLISION || code == GROUP_FAILURE {
                        break Loss::Collision;
                    }
                }
            }
            msg = owner.next() => {
                let Some(Ok(msg)) = msg else { break Loss::DaemonGone };
                if let Ok((_name, _old, new)) =
                    msg.body().deserialize::<(String, String, String)>()
                {
                    if new.is_empty() {
                        break Loss::DaemonGone;
                    }
                }
            }
        }
    };
    let _ = tx.send(loss);
}

#[async_trait]
impl Registration for AvahiRegistration {
    async fn withdraw(self: Box<Self>) {
        self.watcher.abort();
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

impl Drop for AvahiRegistration {
    fn drop(&mut self) {
        self.watcher.abort();
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
        /// The loss channel of every registration, oldest first.
        lose: Mutex<Vec<Option<tokio::sync::oneshot::Sender<Loss>>>>,
    }

    impl FakeAdvertiser {
        /// The daemon says the newest registration is lost.
        pub fn lose(&self, loss: Loss) {
            let last = self.lose.lock().unwrap().len().saturating_sub(1);
            self.lose_nth(last, loss);
        }

        /// The daemon says the `n`th registration ever made is lost, which
        /// for an earlier one is a report arriving late.
        pub fn lose_nth(&self, n: usize, loss: Loss) {
            if let Some(Some(tx)) =
                self.lose.lock().unwrap().get_mut(n).map(Option::take)
            {
                let _ = tx.send(loss);
            }
        }

        /// The names of the newest publication.
        pub fn newest(&self) -> Option<ServiceRecords> {
            self.published.lock().unwrap().last().cloned()
        }
    }

    struct FakeRegistration(Arc<AtomicUsize>);

    #[async_trait]
    impl Advertiser for FakeAdvertiser {
        async fn register(
            &self,
            records: &ServiceRecords,
        ) -> Result<Registered, AdvertiseError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(AdvertiseError("no mDNS daemon".into()));
            }
            self.published.lock().unwrap().push(records.clone());
            self.live.fetch_add(1, Ordering::SeqCst);
            let (tx, lost) = tokio::sync::oneshot::channel();
            self.lose.lock().unwrap().push(Some(tx));
            Ok(Registered {
                registration: Box::new(FakeRegistration(self.live.clone())),
                lost,
            })
        }
    }

    #[async_trait]
    impl Registration for FakeRegistration {
        async fn withdraw(self: Box<Self>) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
