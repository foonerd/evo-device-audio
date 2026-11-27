// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The host Bluetooth stack over the system message bus.
//!
//! One connection to the bus. Over it the plugin reads and sets the adapter's
//! flags, registers an A2DP sink endpoint and a pairing agent as objects of
//! its own, lists and removes devices, and listens for changes. The
//! registrations belong to the bus connection: if the plugin dies they vanish
//! with it, so nothing stale is left in the stack.
//!
//! This release registers the endpoint so a source can connect and configure
//! a stream, but it never asks for the stream's transport: no audio is taken.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection, MatchRule, MessageStream};

use crate::bluez::{
    AdapterInfo, Bluez, BtError, Connector, DeviceInfo, Event,
    SBC_CAPABILITIES, SINK_UUID,
};
use crate::policy::{allow_pairing, allow_service, PairingGate};

const BLUEZ: &str = "org.bluez";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const AGENT_PATH: &str = "/org/evoframework/bluetooth/agent";
const ENDPOINT_PATH: &str = "/org/evoframework/bluetooth/endpoint";

type Managed =
    HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

// ----- pure helpers -----

/// The device address in a stack object path
/// (`/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF`), if it is a device path.
pub(crate) fn address_of_path(path: &str) -> Option<String> {
    let last = path.rsplit('/').next()?;
    let hex = last.strip_prefix("dev_")?;
    let addr = hex.replace('_', ":");
    crate::policy::is_address(&addr).then_some(addr)
}

/// What the stack's error says, as one of ours.
pub(crate) fn classify(text: &str) -> BtError {
    let t = text.to_ascii_lowercase();
    if t.contains("rfkill") || t.contains("blocked") {
        BtError::Blocked
    } else if t.contains("serviceunknown")
        || t.contains("noreply")
        || t.contains("disconnected")
        || t.contains("not provided by any .service")
        || t.contains("timed out")
    {
        BtError::Unavailable(text.to_string())
    } else if t.contains("doesnotexist") || t.contains("does not exist") {
        BtError::NoAdapter
    } else {
        BtError::Refused(text.to_string())
    }
}

fn err(e: zbus::Error) -> BtError {
    classify(&e.to_string())
}

/// The configuration to ask for out of what a source offers: 44.1 kHz if it
/// can (the player's rate), the richest channel mode, 16 blocks, 8 subbands,
/// loudness allocation, and the highest bitpool up to ours. `None` when the
/// offer is not SBC or shares nothing with ours.
pub(crate) fn select_sbc_configuration(offer: &[u8]) -> Option<[u8; 4]> {
    let [o0, o1, omin, omax] = <[u8; 4]>::try_from(offer).ok()?;
    let [c0, c1, cmin, cmax] = SBC_CAPABILITIES;
    let (f, b) = (o0 & c0, o1 & c1);
    let freq = [0x20u8, 0x10].into_iter().find(|m| f & m != 0)?;
    let mode = [0x01u8, 0x02, 0x04, 0x08]
        .into_iter()
        .find(|m| f & m != 0)?;
    let blocks = [0x10u8, 0x20, 0x40, 0x80]
        .into_iter()
        .find(|m| b & m != 0)?;
    let sub = [0x04u8, 0x08].into_iter().find(|m| b & m != 0)?;
    let alloc = [0x01u8, 0x02].into_iter().find(|m| b & m != 0)?;
    let lo = omin.max(cmin);
    let hi = omax.min(cmax);
    if lo > hi {
        return None;
    }
    Some([freq | mode, blocks | sub | alloc, hi, hi])
}

fn string_prop(m: &HashMap<String, OwnedValue>, k: &str) -> Option<String> {
    m.get(k)?.downcast_ref::<&str>().ok().map(str::to_string)
}

fn bool_prop(m: &HashMap<String, OwnedValue>, k: &str) -> Option<bool> {
    m.get(k)?.downcast_ref::<bool>().ok()
}

fn u32_prop(m: &HashMap<String, OwnedValue>, k: &str) -> Option<u32> {
    m.get(k)?.downcast_ref::<u32>().ok()
}

fn adapter_from(m: &HashMap<String, OwnedValue>) -> AdapterInfo {
    AdapterInfo {
        address: string_prop(m, "Address").unwrap_or_default(),
        alias: string_prop(m, "Alias").unwrap_or_default(),
        powered: bool_prop(m, "Powered").unwrap_or(false),
        discoverable: bool_prop(m, "Discoverable").unwrap_or(false),
        pairable: bool_prop(m, "Pairable").unwrap_or(false),
        discoverable_timeout: u32_prop(m, "DiscoverableTimeout").unwrap_or(0),
        pairable_timeout: u32_prop(m, "PairableTimeout").unwrap_or(0),
    }
}

fn device_from(m: &HashMap<String, OwnedValue>) -> Option<DeviceInfo> {
    let address = string_prop(m, "Address")?;
    Some(DeviceInfo {
        name: string_prop(m, "Alias")
            .or_else(|| string_prop(m, "Name"))
            .unwrap_or_else(|| address.clone()),
        address,
        paired: bool_prop(m, "Paired").unwrap_or(false),
        connected: bool_prop(m, "Connected").unwrap_or(false),
    })
}

// ----- the objects the stack calls back -----

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

/// The pairing agent. It answers for the window: nothing is accepted while it
/// is closed.
struct Agent {
    gate: Arc<PairingGate>,
    conn: Connection,
}

impl Agent {
    async fn device_is_paired(&self, device: &ObjectPath<'_>) -> bool {
        let reply = self
            .conn
            .call_method(
                Some(BLUEZ),
                device.as_str(),
                Some(PROPS_IFACE),
                "Get",
                &(DEVICE_IFACE, "Paired"),
            )
            .await;
        let Ok(reply) = reply else { return false };
        let Ok(value) = reply.body().deserialize::<OwnedValue>() else {
            return false;
        };
        value.downcast_ref::<bool>().unwrap_or(false)
    }
}

#[interface(name = "org.bluez.Agent1")]
impl Agent {
    async fn release(&self) {}

    async fn request_pin_code(
        &self,
        _device: ObjectPath<'_>,
    ) -> Result<String, AgentError> {
        Err(AgentError::Rejected("no PIN".into()))
    }

    async fn display_pin_code(
        &self,
        _device: ObjectPath<'_>,
        _pincode: String,
    ) {
    }

    async fn request_passkey(
        &self,
        _device: ObjectPath<'_>,
    ) -> Result<u32, AgentError> {
        Err(AgentError::Rejected("no passkey".into()))
    }

    async fn display_passkey(
        &self,
        _device: ObjectPath<'_>,
        _passkey: u32,
        _entered: u16,
    ) {
    }

    async fn request_confirmation(
        &self,
        _device: ObjectPath<'_>,
        _passkey: u32,
    ) -> Result<(), AgentError> {
        if allow_pairing(self.gate.is_open()) {
            Ok(())
        } else {
            Err(AgentError::Rejected("the pairing window is closed".into()))
        }
    }

    async fn request_authorization(
        &self,
        _device: ObjectPath<'_>,
    ) -> Result<(), AgentError> {
        if allow_pairing(self.gate.is_open()) {
            Ok(())
        } else {
            Err(AgentError::Rejected("the pairing window is closed".into()))
        }
    }

    async fn authorize_service(
        &self,
        device: ObjectPath<'_>,
        uuid: String,
    ) -> Result<(), AgentError> {
        let open = self.gate.is_open();
        let paired = if open {
            false
        } else {
            self.device_is_paired(&device).await
        };
        if allow_service(open, paired, &uuid) {
            Ok(())
        } else {
            Err(AgentError::Rejected(
                "not an audio service of a paired device".into(),
            ))
        }
    }

    async fn cancel(&self) {}
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum EndpointError {
    #[zbus(error)]
    ZBus(zbus::Error),
    InvalidArguments(String),
}

/// The A2DP sink endpoint. In this release it lets a source configure a
/// stream and goes no further: the transport is never acquired.
struct Endpoint {
    events: broadcast::Sender<Event>,
}

#[interface(name = "org.bluez.MediaEndpoint1")]
impl Endpoint {
    async fn select_configuration(
        &self,
        capabilities: Vec<u8>,
    ) -> Result<Vec<u8>, EndpointError> {
        select_sbc_configuration(&capabilities)
            .map(|c| c.to_vec())
            .ok_or_else(|| {
                EndpointError::InvalidArguments(
                    "no common SBC configuration".into(),
                )
            })
    }

    async fn set_configuration(
        &self,
        transport: ObjectPath<'_>,
        _properties: HashMap<String, OwnedValue>,
    ) {
        tracing::debug!(transport = %transport, "sink configured by a source; stream left unacquired");
        let _ = self.events.send(Event::Changed);
    }

    async fn clear_configuration(&self, _transport: ObjectPath<'_>) {
        let _ = self.events.send(Event::Changed);
    }

    async fn release(&self) {}
}

// ----- the stack -----

/// Connects to the stack over the system bus.
pub struct SystemConnector;

#[async_trait]
impl Connector for SystemConnector {
    async fn connect(&self) -> Result<Arc<dyn Bluez>, BtError> {
        Ok(Arc::new(SystemBluez::connect().await?))
    }
}

/// The stack, over one bus connection.
pub struct SystemBluez {
    conn: Connection,
    tx: broadcast::Sender<Event>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    registered: AtomicBool,
}

impl SystemBluez {
    /// Connect, and start listening for changes. Fails as unavailable when
    /// there is no bus or the stack does not answer.
    pub async fn connect() -> Result<Self, BtError> {
        let conn = Connection::system()
            .await
            .map_err(|e| BtError::Unavailable(e.to_string()))?;
        let this = Self {
            conn,
            tx: broadcast::channel(64).0,
            tasks: Mutex::new(Vec::new()),
            registered: AtomicBool::new(false),
        };
        // The stack must answer (this also starts it where it is activated by
        // the bus).
        this.managed().await.map_err(|e| match e {
            BtError::Refused(t) => BtError::Unavailable(t),
            other => other,
        })?;
        let task = this.spawn_watcher().await?;
        this.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(task);
        Ok(this)
    }

    async fn managed(&self) -> Result<Managed, BtError> {
        self.conn
            .call_method(
                Some(BLUEZ),
                "/",
                Some("org.freedesktop.DBus.ObjectManager"),
                "GetManagedObjects",
                &(),
            )
            .await
            .map_err(err)?
            .body()
            .deserialize::<Managed>()
            .map_err(|e| BtError::Refused(e.to_string()))
    }

    async fn adapter_path(&self) -> Result<String, BtError> {
        let managed = self.managed().await?;
        let mut paths: Vec<&OwnedObjectPath> = managed
            .iter()
            .filter(|(_, ifaces)| ifaces.contains_key(ADAPTER_IFACE))
            .map(|(p, _)| p)
            .collect();
        paths.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        paths
            .first()
            .map(|p| p.as_str().to_string())
            .ok_or(BtError::NoAdapter)
    }

    async fn set_prop(
        &self,
        path: &str,
        iface: &str,
        name: &str,
        value: Value<'_>,
    ) -> Result<(), BtError> {
        // The stack answers "busy" while the radio is changing state: ask again.
        let mut last = BtError::Refused("busy".into());
        for attempt in 0..4 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            match self
                .conn
                .call_method(
                    Some(BLUEZ),
                    path,
                    Some(PROPS_IFACE),
                    "Set",
                    &(iface, name, &value),
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let mapped = err(e);
                    if matches!(&mapped, BtError::Refused(t) if t.to_ascii_lowercase().contains("busy"))
                    {
                        last = mapped;
                        continue;
                    }
                    return Err(mapped);
                }
            }
        }
        Err(last)
    }

    async fn spawn_watcher(&self) -> Result<JoinHandle<()>, BtError> {
        let conn = self.conn.clone();
        let tx = self.tx.clone();
        let rule = |iface: &'static str,
                    member: &'static str,
                    sender: &'static str| {
            MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(sender)
                .and_then(|b| b.interface(iface))
                .and_then(|b| b.member(member))
                .map(|b| b.build())
                .map_err(|e| BtError::Unavailable(e.to_string()))
        };
        let mut props = MessageStream::for_match_rule(
            rule(
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                BLUEZ,
            )?,
            &conn,
            Some(256),
        )
        .await
        .map_err(err)?;
        let mut added = MessageStream::for_match_rule(
            rule(
                "org.freedesktop.DBus.ObjectManager",
                "InterfacesAdded",
                BLUEZ,
            )?,
            &conn,
            Some(64),
        )
        .await
        .map_err(err)?;
        let mut removed = MessageStream::for_match_rule(
            rule(
                "org.freedesktop.DBus.ObjectManager",
                "InterfacesRemoved",
                BLUEZ,
            )?,
            &conn,
            Some(64),
        )
        .await
        .map_err(err)?;
        let mut owner = MessageStream::for_match_rule(
            rule(
                "org.freedesktop.DBus",
                "NameOwnerChanged",
                "org.freedesktop.DBus",
            )?,
            &conn,
            Some(64),
        )
        .await
        .map_err(err)?;
        Ok(tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(Ok(m)) = props.next() => {
                        let path = m.header().path().map(|p| p.to_string()).unwrap_or_default();
                        let Ok((iface, changed, _)) =
                            m.body().deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                        else { continue };
                        if iface == DEVICE_IFACE && bool_prop(&changed, "Paired") == Some(true) {
                            if let Some(a) = address_of_path(&path) {
                                let _ = tx.send(Event::Paired(a));
                                continue;
                            }
                        }
                        if iface == DEVICE_IFACE || iface == ADAPTER_IFACE || iface.starts_with("org.bluez.Media") {
                            let _ = tx.send(Event::Changed);
                        }
                    }
                    Some(Ok(_)) = added.next() => { let _ = tx.send(Event::Changed); }
                    Some(Ok(_)) = removed.next() => { let _ = tx.send(Event::Changed); }
                    Some(Ok(m)) = owner.next() => {
                        if let Ok((name, _old, new)) = m.body().deserialize::<(String, String, String)>() {
                            if name == BLUEZ {
                                let _ = tx.send(if new.is_empty() { Event::Gone } else { Event::Back });
                            }
                        }
                    }
                    else => { let _ = tx.send(Event::Gone); break; }
                }
            }
        }))
    }
}

impl Drop for SystemBluez {
    fn drop(&mut self) {
        for t in self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            t.abort();
        }
    }
}

#[async_trait]
impl Bluez for SystemBluez {
    async fn adapter(&self) -> Result<AdapterInfo, BtError> {
        let path = self.adapter_path().await?;
        let all: HashMap<String, OwnedValue> = self
            .conn
            .call_method(
                Some(BLUEZ),
                path.as_str(),
                Some(PROPS_IFACE),
                "GetAll",
                &(ADAPTER_IFACE,),
            )
            .await
            .map_err(err)?
            .body()
            .deserialize()
            .map_err(|e| BtError::Refused(e.to_string()))?;
        Ok(adapter_from(&all))
    }

    async fn set_powered(&self, on: bool) -> Result<(), BtError> {
        let path = self.adapter_path().await?;
        self.set_prop(&path, ADAPTER_IFACE, "Powered", Value::from(on))
            .await
    }

    async fn set_discoverable(&self, on: bool) -> Result<(), BtError> {
        let path = self.adapter_path().await?;
        self.set_prop(&path, ADAPTER_IFACE, "Discoverable", Value::from(on))
            .await
    }

    async fn set_pairable(&self, on: bool) -> Result<(), BtError> {
        let path = self.adapter_path().await?;
        self.set_prop(&path, ADAPTER_IFACE, "Pairable", Value::from(on))
            .await
    }

    async fn set_timeouts(
        &self,
        discoverable: u32,
        pairable: u32,
    ) -> Result<(), BtError> {
        let path = self.adapter_path().await?;
        self.set_prop(
            &path,
            ADAPTER_IFACE,
            "DiscoverableTimeout",
            Value::from(discoverable),
        )
        .await?;
        self.set_prop(
            &path,
            ADAPTER_IFACE,
            "PairableTimeout",
            Value::from(pairable),
        )
        .await
    }

    async fn register(&self, gate: Arc<PairingGate>) -> Result<(), BtError> {
        let adapter = self.adapter_path().await?;
        let server = self.conn.object_server();
        // A previous registration of ours on this connection is replaced.
        let _ = server.remove::<Agent, _>(AGENT_PATH).await;
        let _ = server.remove::<Endpoint, _>(ENDPOINT_PATH).await;
        server
            .at(
                AGENT_PATH,
                Agent {
                    gate,
                    conn: self.conn.clone(),
                },
            )
            .await
            .map_err(err)?;
        server
            .at(
                ENDPOINT_PATH,
                Endpoint {
                    events: self.tx.clone(),
                },
            )
            .await
            .map_err(err)?;
        let result = async {
            let mut props: HashMap<&str, Value> = HashMap::new();
            props.insert("UUID", Value::from(SINK_UUID));
            props.insert("Codec", Value::from(0u8));
            props
                .insert("Capabilities", Value::from(SBC_CAPABILITIES.to_vec()));
            self.conn
                .call_method(
                    Some(BLUEZ),
                    adapter.as_str(),
                    Some("org.bluez.Media1"),
                    "RegisterEndpoint",
                    &(
                        ObjectPath::try_from(ENDPOINT_PATH)
                            .map_err(|e| BtError::Refused(e.to_string()))?,
                        props,
                    ),
                )
                .await
                .map_err(err)?;
            let agent = ObjectPath::try_from(AGENT_PATH)
                .map_err(|e| BtError::Refused(e.to_string()))?;
            self.conn
                .call_method(
                    Some(BLUEZ),
                    "/org/bluez",
                    Some("org.bluez.AgentManager1"),
                    "RegisterAgent",
                    &(&agent, "NoInputNoOutput"),
                )
                .await
                .map_err(err)?;
            self.conn
                .call_method(
                    Some(BLUEZ),
                    "/org/bluez",
                    Some("org.bluez.AgentManager1"),
                    "RequestDefaultAgent",
                    &(&agent,),
                )
                .await
                .map_err(err)?;
            Ok::<(), BtError>(())
        }
        .await;
        match result {
            Ok(()) => {
                self.registered.store(true, Ordering::SeqCst);
                Ok(())
            }
            Err(e) => {
                self.unregister().await;
                Err(e)
            }
        }
    }

    async fn unregister(&self) {
        if let Ok(adapter) = self.adapter_path().await {
            if let Ok(ep) = ObjectPath::try_from(ENDPOINT_PATH) {
                let _ = self
                    .conn
                    .call_method(
                        Some(BLUEZ),
                        adapter.as_str(),
                        Some("org.bluez.Media1"),
                        "UnregisterEndpoint",
                        &(ep,),
                    )
                    .await;
            }
        }
        if let Ok(agent) = ObjectPath::try_from(AGENT_PATH) {
            let _ = self
                .conn
                .call_method(
                    Some(BLUEZ),
                    "/org/bluez",
                    Some("org.bluez.AgentManager1"),
                    "UnregisterAgent",
                    &(agent,),
                )
                .await;
        }
        let server = self.conn.object_server();
        let _ = server.remove::<Agent, _>(AGENT_PATH).await;
        let _ = server.remove::<Endpoint, _>(ENDPOINT_PATH).await;
        self.registered.store(false, Ordering::SeqCst);
    }

    async fn devices(&self) -> Result<Vec<DeviceInfo>, BtError> {
        let adapter = self.adapter_path().await?;
        let mut out: Vec<DeviceInfo> = self
            .managed()
            .await?
            .iter()
            .filter(|(p, _)| p.as_str().starts_with(&format!("{adapter}/dev_")))
            .filter_map(|(_, ifaces)| {
                ifaces.get(DEVICE_IFACE).and_then(device_from)
            })
            .collect();
        out.sort_by(|a, b| a.address.cmp(&b.address));
        Ok(out)
    }

    async fn remove_device(&self, address: &str) -> Result<(), BtError> {
        let adapter = self.adapter_path().await?;
        let device = format!(
            "{adapter}/dev_{}",
            address.replace(':', "_").to_ascii_uppercase()
        );
        let known = self
            .devices()
            .await?
            .iter()
            .any(|d| d.address.eq_ignore_ascii_case(address));
        if !known {
            return Err(BtError::NotFound);
        }
        let path = ObjectPath::try_from(device.as_str())
            .map_err(|e| BtError::Refused(e.to_string()))?;
        self.conn
            .call_method(
                Some(BLUEZ),
                adapter.as_str(),
                Some(ADAPTER_IFACE),
                "RemoveDevice",
                &(path,),
            )
            .await
            .map_err(err)?;
        Ok(())
    }

    fn events(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_path_gives_its_address() {
        assert_eq!(
            address_of_path("/org/bluez/hci0/dev_E0_8F_4C_9E_A7_72").as_deref(),
            Some("E0:8F:4C:9E:A7:72")
        );
        for not in [
            "/org/bluez/hci0",
            "/org/bluez/hci0/dev_E0_8F_4C_9E_A7_72/fd0",
            "/org/bluez/hci0/dev_E0_8F",
            "/org/bluez/hci0/dev_ZZ_8F_4C_9E_A7_72",
            "",
        ] {
            assert_eq!(address_of_path(not), None, "{not:?}");
        }
    }

    #[test]
    fn the_stacks_errors_become_ours() {
        assert_eq!(
            classify("org.bluez.Error.Failed: Blocked through rfkill"),
            BtError::Blocked
        );
        assert!(matches!(
            classify("org.freedesktop.DBus.Error.ServiceUnknown: The name org.bluez was not provided by any .service files"),
            BtError::Unavailable(_)
        ));
        assert!(matches!(
            classify(
                "org.freedesktop.DBus.Error.NoReply: did not receive a reply"
            ),
            BtError::Unavailable(_)
        ));
        assert_eq!(
            classify("org.bluez.Error.DoesNotExist: No such adapter"),
            BtError::NoAdapter
        );
        assert!(matches!(
            classify("org.bluez.Error.NotPermitted"),
            BtError::Refused(_)
        ));
    }

    #[test]
    fn the_configuration_chosen_is_in_what_both_sides_support() {
        // everything on offer: 44.1 kHz, joint stereo, 16 blocks, 8 subbands,
        // loudness allocation, bitpool 53
        assert_eq!(
            select_sbc_configuration(&[0xff, 0xff, 2, 250]),
            Some([0x21, 0x15, 53, 53])
        );
        // a source offering only 48 kHz stereo, 8 blocks, 4 subbands, SNR, bitpool 10..30
        assert_eq!(
            select_sbc_configuration(&[
                0x10 | 0x02,
                0x40 | 0x08 | 0x02,
                10,
                30
            ]),
            Some([0x12, 0x4A, 30, 30])
        );
        // nothing in common: a mode we do not offer cannot be asked for
        assert_eq!(select_sbc_configuration(&[0x20, 0x00, 2, 53]), None);
        // a bitpool range that does not meet ours
        assert_eq!(select_sbc_configuration(&[0xff, 0xff, 60, 100]), None);
        // not SBC at all
        assert_eq!(select_sbc_configuration(&[1, 2, 3]), None);
        assert_eq!(select_sbc_configuration(&[]), None);
    }

    #[test]
    fn what_we_offer_is_what_the_decoder_reads() {
        // both sampling rates, all four modes, every block count, both
        // subband counts, both allocation methods, bitpool 2 to 53
        let [b0, b1, lo, hi] = SBC_CAPABILITIES;
        assert_eq!(b0 & 0xf0, 0x30, "44.1 and 48 kHz");
        assert_eq!(b0 & 0x0f, 0x0f, "mono, dual, stereo, joint");
        assert_eq!(b1 & 0xf0, 0xf0, "4, 8, 12, 16 blocks");
        assert_eq!(b1 & 0x0c, 0x0c, "4 and 8 subbands");
        assert_eq!(b1 & 0x03, 0x03, "loudness and SNR");
        assert_eq!((lo, hi), (2, 53));
    }
}
