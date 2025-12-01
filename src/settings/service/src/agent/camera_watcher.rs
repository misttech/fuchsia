// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use fuchsia_async as fasync;
use futures::channel::mpsc::UnboundedSender;
use settings_camera::connect_to_camera;
use settings_common::inspect::event::ExternalEventPublisher;
use settings_common::service_context::ServiceContext;
use settings_common::trace;

pub struct CameraWatcherAgent {
    /// Sends an event whenever camera muted state changes. The `bool`
    /// represents whether the camera is muted or not.
    muted_txs: Vec<UnboundedSender<bool>>,
    external_publisher: ExternalEventPublisher,
}

impl CameraWatcherAgent {
    pub fn new(
        muted_txs: Vec<UnboundedSender<bool>>,
        external_publisher: ExternalEventPublisher,
    ) -> Self {
        Self { muted_txs, external_publisher }
    }

    pub async fn spawn(self, service_context: &ServiceContext) -> Result<(), Error> {
        let camera_device_client =
            connect_to_camera(service_context, self.external_publisher.clone())
                .await
                .context("connecting to camera")?;
        let mut event_handler = EventHandler { muted_txs: self.muted_txs.clone(), sw_muted: false };
        fasync::Task::local(async move {
            let id = fuchsia_trace::Id::new();
            // Here we don't care about hw_muted state because the input service would pick
            // up mute changes directly from the switch. We care about sw changes because
            // other clients of the camera3 service could change the sw mute state but not
            // notify the settings service.
            trace!(id, c"camera_watcher_agent_handler");
            while let Ok((sw_muted, _hw_muted)) = camera_device_client.watch_mute_state().await {
                trace!(id, c"event");
                event_handler.handle_event(sw_muted);
            }
        })
        .detach();

        Ok(())
    }
}

struct EventHandler {
    muted_txs: Vec<UnboundedSender<bool>>,
    sw_muted: bool,
}

impl EventHandler {
    fn handle_event(&mut self, sw_muted: bool) {
        if self.sw_muted != sw_muted {
            self.sw_muted = sw_muted;
            self.send_event(sw_muted);
        }
    }

    fn send_event(&self, muted: bool) {
        for muted_tx in &self.muted_txs {
            let _ = muted_tx.unbounded_send(muted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use futures::channel::mpsc;
    use settings_test_common::fakes::service::ServiceRegistry;

    // Tests that the agent cannot start without a camera3 service.
    #[fuchsia::test(allow_stalls = false)]
    async fn when_camera3_inaccessible_returns_err() {
        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);

        let agent = CameraWatcherAgent { muted_txs: vec![], external_publisher };
        let service_context =
            ServiceContext::new(Some(ServiceRegistry::serve(ServiceRegistry::create())));

        // Try to initiate the Service lifespan without providing the camera3 fidl interface.
        let result = agent.spawn(&service_context).await;
        assert!(matches!(result, Err(_)));
    }

    // Tests that events can be sent to the intended recipients.
    #[fuchsia::test(allow_stalls = false)]
    async fn event_handler_proxies_event() {
        let (tx1, mut rx1) = mpsc::unbounded();
        let (tx2, mut rx2) = mpsc::unbounded();

        let mut event_handler =
            EventHandler { muted_txs: vec![tx1.clone(), tx2.clone()], sw_muted: false };

        // Send the events.
        event_handler.handle_event(true);

        let mut channel_received = 0;

        let mut next_rx1 = rx1.next();
        let mut next_rx2 = rx2.next();

        // Loop over the select so we can handle the messages as they come in. When all messages
        // have been handled, the senders a closed to ensure the complete case can be hit below.
        loop {
            futures::select! {
                event = next_rx1 => {
                    let Some(muted) = event else {
                        continue;
                    };
                    assert!(muted);
                    // Close channel so we can exit select loop.
                    tx1.close_channel();
                    channel_received += 1;
                }
                event = next_rx2 => {
                    let Some(muted) = event else {
                        continue;
                    };
                    assert!(muted);
                    // Close channel so we can exit select loop.
                    tx2.close_channel();
                    channel_received += 1;
                }
                complete => break,
            }
        }

        assert_eq!(channel_received, 2);
    }
}
