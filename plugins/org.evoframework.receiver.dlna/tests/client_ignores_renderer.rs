// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The renderer and the `source.dlna` client are different UPnP device
//! classes, and the client must not pick the renderer up as a source.
//!
//! The client keeps a device only if its description lists a
//! ContentDirectory service. This test serves the renderer's real
//! description over HTTP and runs the client's own fetch-and-parse path,
//! `evo_dlna::fetch_media_server`, against it.

use std::sync::Arc;

use org_evoframework_receiver_dlna::bridge::NoPlayer;
use org_evoframework_receiver_dlna::control::Control;
use org_evoframework_receiver_dlna::description::{
    device_description, SERVICES,
};
use org_evoframework_receiver_dlna::events::Subscriptions;
use org_evoframework_receiver_dlna::http::{start, DeviceInfo, Services};

#[tokio::test]
async fn the_client_fetch_path_refuses_the_renderer() {
    let h = start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(Services {
            info: DeviceInfo {
                uuid: "11111111-2222-3333-4444-555555555555".into(),
                friendly_name: "Kitchen".into(),
            },
            control: Control::new(Arc::new(NoPlayer)),
            subs: Subscriptions::new(),
        }),
    )
    .await
    .unwrap();
    let url = format!("http://{}/description.xml", h.local_addr());
    let r = evo_dlna::fetch_media_server(&url).await;
    assert!(
        r.is_err(),
        "the DLNA client must not accept a MediaRenderer as a source: {r:?}"
    );
    h.stop().await;
}

#[test]
fn the_renderer_description_lists_no_content_directory() {
    let xml = device_description("u", "n");
    assert!(!xml.contains("ContentDirectory"));
    for s in SERVICES {
        assert!(!s.service_type.contains("ContentDirectory"));
    }
}
