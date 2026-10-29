// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use component_events::events::*;
use component_events::matcher::*;
use fuchsia_async as fasync;
use std::sync::{Arc, Mutex};

// Helper for getting information about the child component using event_stream.
pub struct AppMonitor {
    pub moniker: String,
    stopped: Arc<Mutex<bool>>,
    event_stream: Mutex<Option<EventStream>>,
    stop_task: Mutex<Option<fasync::Task<()>>>,
}

impl AppMonitor {
    pub async fn new(moniker: String) -> Self {
        let event_stream = EventStream::open().await.unwrap();
        Self {
            moniker,
            stopped: Arc::new(Mutex::new(false)),
            event_stream: Mutex::new(Some(event_stream)),
            stop_task: Mutex::new(None),
        }
    }

    // Non-blocking. Starts a separate task that waits for a Stopped event from the moniker.
    // Note: this is a best effort check, the stopped event is only being observed while the
    // system validation test is running.
    pub fn add_monitor_for_stop_event(&self) {
        *self.stopped.lock().unwrap() = false;
        let stopped_clone = self.stopped.clone();
        let moniker_clone = self.moniker.clone();
        let mut event_stream = self
            .event_stream
            .lock()
            .unwrap()
            .take()
            .expect("add_monitor_for_stop_event called without event stream");
        let task = fasync::Task::spawn(async move {
            EventMatcher::ok()
                .moniker(&moniker_clone)
                .wait::<Stopped>(&mut event_stream)
                .await
                .unwrap_or_else(|e| {
                    panic!("failed to observe {} stop event: {:?}", moniker_clone, e)
                });
            *stopped_clone.lock().unwrap() = true;
        });
        *self.stop_task.lock().unwrap() = Some(task);
    }

    pub fn has_seen_stop_event(&self) -> bool {
        *self.stopped.lock().unwrap()
    }

    // Blocking. Waits for event matcher to report that app is running.
    pub async fn wait_for_start_event(&self) {
        let mut event_stream = self
            .event_stream
            .lock()
            .unwrap()
            .take()
            .expect("wait_for_start_event called more than once");
        let moniker = self.moniker.clone();

        EventMatcher::ok()
            .moniker(&moniker)
            .wait::<Started>(&mut event_stream)
            .await
            .unwrap_or_else(|e| panic!("failed to observe {} start event: {:?}", moniker, e));
        *self.event_stream.lock().unwrap() = Some(event_stream);
    }
}
