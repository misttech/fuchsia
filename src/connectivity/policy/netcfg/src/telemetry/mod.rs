// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use fuchsia_inspect::Inspector;
use fuchsia_sync::Mutex;
use futures::Future;
use futures::channel::mpsc;
use log::{info, warn};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub enum TelemetryEvent {}

#[derive(Clone, Debug)]
pub struct TelemetrySender {
    sender: Arc<Mutex<mpsc::Sender<TelemetryEvent>>>,
    sender_is_blocked: Arc<AtomicBool>,
}

impl TelemetrySender {
    pub fn new(sender: mpsc::Sender<TelemetryEvent>) -> Self {
        Self {
            sender: Arc::new(Mutex::new(sender)),
            sender_is_blocked: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn send(&self, event: TelemetryEvent) {
        match self.sender.lock().try_send(event) {
            Ok(_) => {
                if self
                    .sender_is_blocked
                    .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    info!("TelemetrySender recovered and resumed sending");
                }
            }
            Err(_) => {
                if self
                    .sender_is_blocked
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    warn!(
                        "TelemetrySender dropped a msg: either buffer is full or no receiver is waiting"
                    );
                }
            }
        }
    }
}

const TELEMETRY_EVENT_BUFFER_SIZE: usize = 100;

pub fn serve_telemetry(
    inspector: &Inspector,
) -> (TelemetrySender, impl Future<Output = Result<(), Error>>) {
    let inspect_node = inspector.root();
    let telemetry_node = inspect_node.create_child("telemetry");

    inspect_node.record(telemetry_node);

    let (sender, receiver) = mpsc::channel::<TelemetryEvent>(TELEMETRY_EVENT_BUFFER_SIZE);
    let sender = TelemetrySender::new(sender);

    let fut = async move {
        let _receiver = receiver;
        // TODO(https://fxbug.dev/486892417): Add telemetry events for the network registry
        // and handle them here.
        let () = futures::future::pending().await;
        Ok(())
    };
    (sender, fut)
}
