// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use futures::StreamExt;

use async_utils::hanging_get::client::HangingGetStream;

use fidl_fuchsia_hardware_usb_policy as fpolicy;
use fpolicy::{ControllerProxy, DeviceState};

use anyhow::Error;
use fuchsia_async::{MonotonicInstant, Timer};
use futures::channel::mpsc as fmpsc;
use zx::MonotonicDuration;

use log::{error, info};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsbState {
    pub device_state: DeviceState,
    pub address: u8,
}

pub struct ControllerState {
    proxy: ControllerProxy,
    usb_state: Arc<Mutex<UsbState>>,
    watchers: Arc<Mutex<Vec<fmpsc::UnboundedSender<UsbState>>>>,
}

impl ControllerState {
    pub fn new(
        proxy: ControllerProxy,
        device_state: DeviceState, // initial state value
        address: u8,
    ) -> Self {
        Self {
            proxy,
            usb_state: Arc::new(Mutex::new(UsbState { device_state, address })),
            watchers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn subscribe(&self) -> (UsbState, fmpsc::UnboundedReceiver<UsbState>) {
        let (tx, rx) = fmpsc::unbounded();
        let state = {
            let mut w = self.watchers.lock().unwrap_or_else(|e| e.into_inner());
            w.push(tx);
            self.get_state()
        };
        (state, rx)
    }

    pub async fn monitor_device_state(&self) -> Result<(), Error> {
        // Clone the proxy to give the stream its own handle.
        let stream_proxy = self.proxy.clone();

        let mut stream = HangingGetStream::new(stream_proxy, |p| p.watch_device_state());

        while let Some(result) = stream.next().await {
            match result {
                Ok(Ok(update)) => {
                    let new_state = update.state.unwrap_or_else(DeviceState::unknown);
                    let new_address = update.address.unwrap_or(0);
                    info!("Received new state: {:?} address: {}", new_state, new_address);

                    {
                        let mut state_guard =
                            self.usb_state.lock().unwrap_or_else(|e| e.into_inner());
                        state_guard.device_state = new_state;
                        state_guard.address = new_address;
                    }

                    info!("State updated, waiting for next...");

                    let mut watchers = self.watchers.lock().unwrap_or_else(|e| e.into_inner());
                    watchers.retain(|tx| {
                        tx.unbounded_send(UsbState {
                            device_state: new_state,
                            address: new_address,
                        })
                        .is_ok()
                    });
                }
                Ok(Err(e)) => {
                    error!("Monitor loop failed! System Error: {:?}", e);
                    Timer::new(MonotonicInstant::after(MonotonicDuration::from_seconds(1))).await;
                }
                Err(e) => {
                    error!("Monitor loop failed! FIDL Error: {:?}", e);

                    Timer::new(MonotonicInstant::after(MonotonicDuration::from_seconds(1))).await;
                }
            }
        }
        error!("monitor_device_state() loop exited because the server closed the channel.");
        Ok(())
    }

    pub fn get_state(&self) -> UsbState {
        let state_guard = self.usb_state.lock().unwrap_or_else(|e| e.into_inner());
        *state_guard
    }
}
