// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::Peered;
use fuchsia_component::server::{ServiceFs, ServiceObjLocal};
use futures::StreamExt;
use log::warn;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use zx::HandleBased;
use {fidl_fuchsia_firmware_crash as ffc, fuchsia_async as fasync};

pub struct FirmwareCrashService {
    inner: Rc<RefCell<FirmwareCrashInner>>,
    scope: fasync::Scope,
}

struct FirmwareCrashInner {
    crash_count: HashMap<String, u32>,
    crashes: Vec<ffc::Crash>,
    watchers: Vec<Weak<RefCell<Watcher>>>,
}

pub struct Watcher {
    parent: Weak<RefCell<FirmwareCrashInner>>,
    crash_index: usize,
    completer: Option<ffc::WatcherGetCrashResponder>,
    event: Option<zx::EventPair>,
}

impl Default for FirmwareCrashService {
    fn default() -> Self {
        Self {
            inner: Rc::new(RefCell::new(FirmwareCrashInner {
                crash_count: HashMap::new(),
                crashes: Vec::new(),
                watchers: Vec::new(),
            })),
            scope: fasync::Scope::new_with_name("firmware_crash_service"),
        }
    }
}

impl FirmwareCrashService {
    pub fn publish(self: &Rc<Self>, fs: &mut ServiceFs<ServiceObjLocal<'_, ()>>) {
        let this = self.clone();
        fs.dir("svc").add_fidl_service(move |stream: ffc::ReporterRequestStream| {
            let this_clone1 = this.clone();
            let this_clone2 = this.clone();
            this_clone1.scope.spawn_local(async move {
                if let Err(e) = this_clone2.serve_reporter(stream).await {
                    warn!("Failed to serve fuchsia.firmware.crash.Reporter: {}", e);
                }
            });
        });

        let this = self.clone();
        fs.dir("svc").add_fidl_service(move |stream: ffc::WatcherRequestStream| {
            let this_clone1 = this.clone();
            let this_clone2 = this.clone();
            this_clone1.scope.spawn_local(async move {
                if let Err(e) = this_clone2.serve_watcher(stream).await {
                    warn!("Failed to serve fuchsia.firmware.crash.Watcher: {}", e);
                }
            });
        });
    }

    async fn serve_reporter(
        self: Rc<Self>,
        mut stream: ffc::ReporterRequestStream,
    ) -> Result<(), fidl::Error> {
        while let Some(request) = stream.next().await {
            match request? {
                ffc::ReporterRequest::Report { mut payload, .. } => {
                    self.report(&mut payload);
                }
                ffc::ReporterRequest::_UnknownMethod { ordinal, .. } => {
                    warn!("fuchsia.firmware.crash/Reporter received unknown method: {}", ordinal);
                }
            }
        }
        Ok(())
    }

    fn report(&self, crash: &mut ffc::Crash) {
        let watchers = {
            let mut inner = self.inner.borrow_mut();

            if let Some(subsystem) = &crash.subsystem_name {
                let count = inner.crash_count.entry(subsystem.clone()).or_insert(0);
                *count += 1;
                crash.count = Some(*count);
            }

            inner.crashes.push(clone_crash(crash));

            let mut active_watchers = Vec::new();
            inner.watchers.retain(|w| {
                if let Some(watcher) = w.upgrade() {
                    active_watchers.push(watcher);
                    true
                } else {
                    false
                }
            });
            active_watchers
        };

        for watcher in watchers {
            watcher.borrow_mut().new_crash_available();
        }
    }

    async fn serve_watcher(
        self: Rc<Self>,
        stream: ffc::WatcherRequestStream,
    ) -> Result<(), fidl::Error> {
        let watcher = Rc::new(RefCell::new(Watcher {
            parent: Rc::downgrade(&self.inner),
            crash_index: 0,
            completer: None,
            event: None,
        }));

        self.inner.borrow_mut().watchers.push(Rc::downgrade(&watcher));

        let mut stream = stream;
        while let Some(request) = stream.next().await {
            match request? {
                ffc::WatcherRequest::GetCrash { payload, responder } => {
                    watcher.borrow_mut().get_crash(payload, responder);
                }
                ffc::WatcherRequest::GetCrashEvent { responder } => {
                    watcher.borrow_mut().get_crash_event(responder);
                }
                ffc::WatcherRequest::_UnknownMethod { ordinal, .. } => {
                    warn!("fuchsia.firmware.crash/Watcher received unknown method: {}", ordinal);
                }
            }
        }
        Ok(())
    }
}

impl Watcher {
    fn new_crash_available(&mut self) {
        let Some(parent) = self.parent.upgrade() else {
            return;
        };
        let inner = parent.borrow();

        if let Some(responder) = self.completer.take() {
            let crash = clone_crash(&inner.crashes[self.crash_index]);
            let _ = responder.send(Ok(crash));
            self.crash_index += 1;
        } else if let Some(event) = &self.event {
            let _ = event.signal_peer(zx::Signals::NONE, zx::Signals::USER_0);
        }
    }

    fn get_crash(
        &mut self,
        request: ffc::WatcherGetCrashRequest,
        responder: ffc::WatcherGetCrashResponder,
    ) {
        if self.completer.is_some() {
            let _ = responder.send(Err(ffc::Error::AlreadyPending));
            return;
        }

        let Some(parent) = self.parent.upgrade() else {
            return;
        };

        let inner = parent.borrow();
        if inner.crashes.len() > self.crash_index {
            let crash = clone_crash(&inner.crashes[self.crash_index]);
            self.crash_index += 1;
            if let Some(event) = &self.event
                && self.crash_index == inner.crashes.len()
            {
                let _ = event.signal_peer(zx::Signals::USER_0, zx::Signals::NONE);
            }
            let _ = responder.send(Ok(crash));
            return;
        }

        if request.wait_for_crash.unwrap_or(true) {
            self.completer = Some(responder);
        } else {
            let _ = responder.send(Err(ffc::Error::NoCrashAvailable));
        }
    }

    fn get_crash_event(&mut self, responder: ffc::WatcherGetCrashEventResponder) {
        let (h1, h2) = zx::EventPair::create();
        self.event = Some(h1);
        let _ = responder.send(h2);
    }
}

fn clone_crash(crash: &ffc::Crash) -> ffc::Crash {
    ffc::Crash {
        subsystem_name: crash.subsystem_name.clone(),
        timestamp: crash.timestamp,
        reason: crash.reason.clone(),
        count: crash.count,
        firmware_version: crash.firmware_version.clone(),
        crash_dump: crash
            .crash_dump
            .as_ref()
            .and_then(|vmo| vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).ok()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;
    use futures::FutureExt;

    #[fasync::run_singlethreaded(test)]
    async fn test_report_and_watch() {
        let service = Rc::new(FirmwareCrashService::default());
        let (reporter, reporter_stream) = create_proxy_and_stream::<ffc::ReporterMarker>();
        let (watcher, watcher_stream) = create_proxy_and_stream::<ffc::WatcherMarker>();

        let service_clone1 = service.clone();
        let service_clone2 = service.clone();
        service_clone1.scope.spawn_local(async move {
            service_clone2.serve_reporter(reporter_stream).await.unwrap();
        });

        let service_clone1 = service.clone();
        let service_clone2 = service.clone();
        service_clone1.scope.spawn_local(async move {
            service_clone2.serve_watcher(watcher_stream).await.unwrap();
        });

        // 1. Report a crash
        let crash =
            ffc::Crash { subsystem_name: Some("test-subsystem".to_string()), ..Default::default() };
        reporter.report(crash).unwrap();

        // 2. Watch for crash
        let result = watcher
            .get_crash(&ffc::WatcherGetCrashRequest {
                wait_for_crash: Some(false),
                ..Default::default()
            })
            .await
            .unwrap();
        let received = result.unwrap();
        assert_eq!(received.subsystem_name.unwrap(), "test-subsystem");
        assert_eq!(received.count.unwrap(), 1);

        // 3. Report another crash for same subsystem
        let crash2 =
            ffc::Crash { subsystem_name: Some("test-subsystem".to_string()), ..Default::default() };
        reporter.report(crash2).unwrap();

        // 4. Watch again
        let result = watcher
            .get_crash(&ffc::WatcherGetCrashRequest {
                wait_for_crash: Some(false),
                ..Default::default()
            })
            .await
            .unwrap();
        let received2 = result.unwrap();
        assert_eq!(received2.subsystem_name.unwrap(), "test-subsystem");
        assert_eq!(received2.count.unwrap(), 2);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_wait_for_crash() {
        let service = Rc::new(FirmwareCrashService::default());
        let (reporter, reporter_stream) = create_proxy_and_stream::<ffc::ReporterMarker>();
        let (watcher, watcher_stream) = create_proxy_and_stream::<ffc::WatcherMarker>();

        let service_clone1 = service.clone();
        let service_clone2 = service.clone();
        service_clone1.scope.spawn_local(async move {
            service_clone2.serve_reporter(reporter_stream).await.unwrap();
        });

        let service_clone1 = service.clone();
        let service_clone2 = service.clone();
        service_clone1.scope.spawn_local(async move {
            service_clone2.serve_watcher(watcher_stream).await.unwrap();
        });

        // 1. Get crash (should hang)
        let get_fut = watcher.get_crash(&ffc::WatcherGetCrashRequest {
            wait_for_crash: Some(true),
            ..Default::default()
        });

        // 2. Report a crash
        let crash =
            ffc::Crash { subsystem_name: Some("test-subsystem".to_string()), ..Default::default() };
        reporter.report(crash).unwrap();

        // 3. Future should complete
        let result = get_fut.await.unwrap();
        let received = result.unwrap();
        assert_eq!(received.subsystem_name.unwrap(), "test-subsystem");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_get_crash_event() {
        let service = Rc::new(FirmwareCrashService::default());
        let (reporter, reporter_stream) = create_proxy_and_stream::<ffc::ReporterMarker>();
        let (watcher, watcher_stream) = create_proxy_and_stream::<ffc::WatcherMarker>();

        let service_clone1 = service.clone();
        let service_clone2 = service.clone();
        service_clone1.scope.spawn_local(async move {
            service_clone2.serve_reporter(reporter_stream).await.unwrap();
        });

        let service_clone1 = service.clone();
        let service_clone2 = service.clone();
        service_clone1.scope.spawn_local(async move {
            service_clone2.serve_watcher(watcher_stream).await.unwrap();
        });

        let event = watcher.get_crash_event().await.unwrap();

        // 1. Report a crash
        let crash =
            ffc::Crash { subsystem_name: Some("test-subsystem".to_string()), ..Default::default() };
        reporter.report(crash).unwrap();

        // 2. Event should be signaled
        fasync::OnSignals::new(&event, zx::Signals::USER_0).await.unwrap();

        // 3. Get the crash
        let _ = watcher
            .get_crash(&ffc::WatcherGetCrashRequest {
                wait_for_crash: Some(false),
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap();

        // 4. Signal should be cleared if no more crashes
        let timer =
            fasync::Timer::new(zx::MonotonicInstant::after(zx::MonotonicDuration::from_millis(10)))
                .fuse();
        let signals = fasync::OnSignals::new(&event, zx::Signals::USER_0).fuse();
        futures::pin_mut!(timer, signals);
        futures::select! {
            _ = signals => panic!("Signal should have been cleared"),
            _ = timer => (),
        }
    }
}
