// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use driver_manager_bind::BindManagerHandle;
use fuchsia_async as fasync;
use fuchsia_async::DurationExt;
use futures::FutureExt;
use futures::channel::oneshot;
use log::{info, warn};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

const BOOTUP_TIMEOUT_DURATION: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);
const LAST_UPDATED_TIMEOUT_DURATION: zx::MonotonicDuration =
    zx::MonotonicDuration::from_seconds(10);

struct BootupTrackerInner {
    outstanding_start_requests: HashMap<String, String>,
    bootup_done: bool,
    waiters: Vec<oneshot::Sender<()>>,
    last_update_timestamp: zx::MonotonicInstant,
    bootup_timeout_task: Option<fasync::Task<()>>,
    bootup_timeout_sender: Option<oneshot::Sender<()>>,
}

pub struct BootupTracker {
    bind_manager: BindManagerHandle,
    inner: RefCell<BootupTrackerInner>,
    weak_self: Weak<Self>,
}

impl BootupTracker {
    pub fn new(bind_manager: BindManagerHandle) -> Rc<Self> {
        Rc::new_cyclic(|weak_self| Self {
            bind_manager,
            inner: RefCell::new(BootupTrackerInner {
                outstanding_start_requests: HashMap::new(),
                bootup_done: false,
                waiters: Vec::new(),
                last_update_timestamp: zx::MonotonicInstant::get(),
                bootup_timeout_task: None,
                bootup_timeout_sender: None,
            }),
            weak_self: weak_self.clone(),
        })
    }

    pub fn start(&self) {
        self.update_tracker_and_reset_timer();
    }

    pub async fn wait_for_bootup(&self) {
        if self.inner.borrow().bootup_done {
            return;
        }
        let (tx, rx) = oneshot::channel();
        self.inner.borrow_mut().waiters.push(tx);
        let _ = rx.await;
    }

    pub fn notify_new_start_request(&self, node_moniker: String, driver_url: String) {
        if self.inner.borrow().outstanding_start_requests.contains_key(&node_moniker) {
            warn!("Bootup tracker received conflicting start requests for node {}", node_moniker);
        }
        self
            .inner
            .borrow_mut()
            .outstanding_start_requests
            .insert(node_moniker, driver_url);
        self.update_tracker_and_reset_timer();
    }

    pub fn notify_start_complete(&self, node_moniker: &str) {
        if self
            .inner
            .borrow_mut()
            .outstanding_start_requests
            .remove(node_moniker)
            .is_none()
        {
            info!("Bootup tracker notified for an unknown start request for {}", node_moniker);
        }
        self.update_tracker_and_reset_timer();
    }

    pub fn notify_binding_changed(&self) {
        self.update_tracker_and_reset_timer();
    }

    fn check_bootup_done(&self) {
        if self.is_update_deadline_exceeded() {
            warn!("Deadline exceeded in the bootup tracker with:");
            warn!(
                "    {} unfinished start requests:",
                self.inner.borrow().outstanding_start_requests.len()
            );
            for (moniker, url) in self.inner.borrow().outstanding_start_requests.iter() {
                warn!("         - {} - {}", moniker, url);
            }
            if self.bind_manager.has_ongoing_bind() {
                warn!("    a hanging bind process in the bind manager");
            }
        }

        if !self.inner.borrow().outstanding_start_requests.is_empty()
            || self.bind_manager.has_ongoing_bind()
        {
            self.reset_bootup_timer();
            return;
        }

        info!("Bootup completed.");

        for sender in std::mem::take(&mut self.inner.borrow_mut().waiters) {
            let _ = sender.send(());
        }
        self.inner.borrow_mut().bootup_done = true;
        if let Some(sender) = self.inner.borrow_mut().bootup_timeout_sender.take() {
            let _ = sender.send(());
        }
        self.inner.borrow_mut().bootup_timeout_task = None;
    }

    fn update_tracker_and_reset_timer(&self) {
        self.inner.borrow_mut().last_update_timestamp = zx::MonotonicInstant::get();
        self.reset_bootup_timer();
    }

    fn on_bootup_timeout(weak_self: Weak<Self>) {
        if let Some(arc_self) = weak_self.upgrade() {
            arc_self.check_bootup_done();
        }
    }

    fn is_update_deadline_exceeded(&self) -> bool {
        zx::MonotonicInstant::get() - self.inner.borrow().last_update_timestamp
            >= LAST_UPDATED_TIMEOUT_DURATION
    }

    fn reset_bootup_timer(&self) {
        if self.inner.borrow().bootup_done {
            return;
        }

        if let Some(sender) = self.inner.borrow_mut().bootup_timeout_sender.take() {
            let _ = sender.send(());
        }

        let (sender, receiver) = oneshot::channel();
        self.inner.borrow_mut().bootup_timeout_sender = Some(sender);

        let weak_self = self.weak_self.clone();
        let timer = fasync::Timer::new(BOOTUP_TIMEOUT_DURATION.after_now());
        let task = fasync::Task::local(async move {
            futures::select! {
                _ = timer.fuse() => {
                    BootupTracker::on_bootup_timeout(weak_self);
                },
                _ = receiver.fuse() => {
                    // Task was cancelled.
                },
            }
        });
        self.inner.borrow_mut().bootup_timeout_task = Some(task);
    }
}
