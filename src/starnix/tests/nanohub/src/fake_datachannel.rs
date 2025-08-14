// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;

use anyhow::{Context, Error};
use fidl_fuchsia_hardware_google_nanohub as fuchsia_nanohub;
use fuchsia_component::server::ServiceFs;
use fuchsia_component_test::LocalComponentHandles;
use futures::lock::Mutex;
use futures::{StreamExt, TryStreamExt};
use log::info;
use zx::AsHandleRef;

pub struct MockDataChannel {
    stat_flags: Arc<Mutex<u32>>,
    event: Arc<Mutex<Option<zx::Event>>>,
    channel_name: String,
    read_data: String,
}

enum Incoming {
    Svc(fuchsia_nanohub::DataChannelServiceRequest),
}

impl MockDataChannel {
    pub fn new(channel_name: String, read_data: String) -> Self {
        MockDataChannel {
            stat_flags: Arc::new(Mutex::new(fuchsia_nanohub::SIGNAL_WRITABLE)),
            event: Arc::new(Mutex::new(None)),
            channel_name,
            read_data,
        }
    }

    async fn handle_device_stream(
        mut stream: fuchsia_nanohub::DataChannelRequestStream,
        stat_flags: Arc<Mutex<u32>>,
        shared_event: Arc<Mutex<Option<zx::Event>>>,
        channel_name: String,
        read_data: String,
    ) -> Result<(), Error> {
        while let Some(request) = stream.try_next().await.context("failed request")? {
            match request {
                fuchsia_nanohub::DataChannelRequest::Read { payload: _, responder } => {
                    let response = fuchsia_nanohub::DataChannelReadResponse {
                        data: Some(read_data.as_bytes().to_vec()),
                        ..Default::default()
                    };
                    let _ = responder.send(Ok(response));
                }
                fuchsia_nanohub::DataChannelRequest::Write { payload: _, responder } => {
                    let mut stat_flags = stat_flags.lock().await;
                    *stat_flags |= fuchsia_nanohub::SIGNAL_READABLE;
                    if let Some(event) = &*shared_event.lock().await {
                        if let Err(e) = event.signal_handle(
                            zx::Signals::empty(),
                            zx::Signals::from_bits_truncate(*stat_flags),
                        ) {
                            info!("failed to signal event: {:?}", e);
                        }
                    }
                    let _ = responder.send(Ok(()));
                }
                fuchsia_nanohub::DataChannelRequest::Register { event, responder } => {
                    let stat_flags = stat_flags.lock().await;
                    if let Err(e) = event.signal_handle(
                        zx::Signals::empty(),
                        zx::Signals::from_bits_truncate(*stat_flags),
                    ) {
                        info!("failed to signal event: {:?}", e);
                    }
                    let mut event_lock = shared_event.lock().await;
                    *event_lock = Some(event);
                    let _ = responder.send(Ok(()));
                }
                fuchsia_nanohub::DataChannelRequest::GetIdentifier { responder } => {
                    let id = fuchsia_nanohub::DataChannelGetIdentifierResponse {
                        name: Some(channel_name.clone()),
                        ..Default::default()
                    };
                    let _ = responder.send(&id);
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub async fn mock_driverservice(
        channel_name: String,
        read_data: String,
        handles: LocalComponentHandles,
    ) -> Result<(), Error> {
        let mock = Self::new(channel_name, read_data);
        let mut fs = ServiceFs::new();
        let stat_flags = mock.stat_flags.clone();
        let shared_event = mock.event.clone();
        let channel_name = mock.channel_name.clone();
        let read_data = mock.read_data.clone();

        fs.dir("svc").add_fidl_service_instance("default", Incoming::Svc);

        fs.serve_connection(handles.outgoing_dir)?;

        fs.for_each_concurrent(0, move |request| {
            let stat_flags = stat_flags.clone();
            let shared_event = shared_event.clone();
            let channel_name = channel_name.clone();
            let read_data = read_data.clone();
            async move {
                match request {
                    Incoming::Svc(fuchsia_nanohub::DataChannelServiceRequest::Device(stream)) => {
                        if let Err(e) = Self::handle_device_stream(
                            stream,
                            stat_flags,
                            shared_event,
                            channel_name,
                            read_data,
                        )
                        .await
                        {
                            info!("Error handling device stream: {:?}", e);
                        }
                    }
                    _ => {}
                }
            }
        })
        .await;

        Ok(())
    }
}
