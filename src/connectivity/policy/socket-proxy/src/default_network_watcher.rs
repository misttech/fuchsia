// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implements the fuchsia.net.policy.properties.DefaultNetworkWatcher service.

use anyhow::{Context, Error};
use fidl::endpoints::{ControlHandle as _, RequestStream as _, Responder as _};
use fidl_fuchsia_net_policy_properties as fnp_properties;
use fuchsia_inspect_derive::{IValue, Inspect, Unit};
use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::{StreamExt, TryStreamExt};
use log::{info, warn};
use std::sync::Arc;

#[derive(Unit, Debug, Default)]
struct State {
    #[inspect(skip)]
    default_network_update: fnp_properties::DefaultNetworkUpdate,
    #[inspect(skip)]
    last_sent: Option<fnp_properties::DefaultNetworkUpdate>,
    #[inspect(skip)]
    queued_responder: Option<fnp_properties::DefaultNetworkWatcherWatchResponder>,

    updates_seen: u32,
    updates_sent: u32,
}

/// A wrapper around the fuchsia.net.policy.properties.DefaultNetworkWatcher
/// service that tracks when a DefaultNetworkUpdate needs to be sent.
#[derive(Inspect, Debug, Clone)]
pub(crate) struct Watcher {
    #[inspect(forward)]
    state: Arc<Mutex<IValue<State>>>,
    default_network_rx: Arc<Mutex<mpsc::Receiver<fnp_properties::DefaultNetworkUpdate>>>,
}

impl Watcher {
    /// Create a new Watcher.
    pub(crate) fn new(
        default_network_rx: Arc<Mutex<mpsc::Receiver<fnp_properties::DefaultNetworkUpdate>>>,
    ) -> Self {
        Self { default_network_rx, state: Default::default() }
    }

    /// Runs the fuchsia.net.policy.properties.DefaultNetworkWatcher service.
    pub(crate) async fn run<'a>(
        &self,
        stream: fnp_properties::DefaultNetworkWatcherRequestStream,
    ) -> Result<(), Error> {
        let mut state = match self.state.try_lock() {
            Some(o) => o,
            None => {
                warn!("Only one connection to DefaultNetworkWatcher is allowed at a time");
                stream.control_handle().shutdown_with_epitaph(fidl::Status::CONNECTION_ABORTED);
                return Ok(());
            }
        };
        let mut default_network_rx = self.default_network_rx.lock().await;
        info!("Starting fuchsia.net.policy.properties.DefaultNetworkWatcher server");
        let mut stream = stream.map(|result| result.context("failed request")).fuse();

        loop {
            futures::select! {
                request = stream.try_next() => match request? {
                    Some(fnp_properties::DefaultNetworkWatcherRequest::Watch { responder }) => {
                        let mut state = state.as_mut();
                        if state.queued_responder.is_some() {
                            warn!("Only one call to watch may be active at once");
                            responder.control_handle().shutdown_with_epitaph(
                                fidl::Status::CONNECTION_ABORTED
                            );
                        } else  {
                            state.queued_responder = Some(responder);
                            state.maybe_respond()?;
                        }
                    },
                    Some(_) | None => {}
                },
                default_network_update = default_network_rx.select_next_some() => {
                    log::trace!("Saw new update: {default_network_update:?}");
                    let mut state = state.as_mut();
                    state.updates_seen += 1;
                    state.default_network_update = default_network_update;
                    state.maybe_respond()?;
                }
            }
        }
    }
}

impl State {
    fn maybe_respond(&mut self) -> Result<(), Error> {
        if self.last_sent.as_ref() != Some(&self.default_network_update) {
            if let Some(responder) = self.queued_responder.take() {
                info!("Sending default network update to client");
                responder.send(&self.default_network_update)?;
                self.updates_sent += 1;
                self.last_sent = Some(self.default_network_update.clone());
            }
        }
        Ok(())
    }
}
