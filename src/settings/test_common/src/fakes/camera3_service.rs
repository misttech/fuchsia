// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fakes::service::Service;
use anyhow::{Error, format_err};
use fidl::endpoints::ServerEnd;
use fidl::prelude::*;
use fidl_fuchsia_camera3::{
    DeviceRequest, DeviceWatchMuteStateResponder, DeviceWatcherMarker, DeviceWatcherRequest,
    DeviceWatcherWatchDevicesResponder, WatchDevicesEvent,
};
use fuchsia_async::{self as fasync, DurationExt};
use futures::TryStreamExt;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::future::{AbortHandle, Abortable};
use settings_camera::CAMERA_WATCHER_TIMEOUT;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use zx::MonotonicDuration;

/// The last mute state sent on a camera device connection, if any.
type LastSentMuted = Rc<Cell<Option<bool>>>;

/// Software mute state shared by all camera device connections. `WatchMuteState` follows
/// hanging-get semantics: the first call on a connection returns immediately, and later calls
/// return once the state differs from the last value sent on that connection.
#[derive(Default)]
struct SwMuteState {
    muted: Cell<bool>,
    pending_watchers: RefCell<Vec<(LastSentMuted, DeviceWatchMuteStateResponder)>>,
    abort_handles: RefCell<Vec<AbortHandle>>,
}

impl SwMuteState {
    fn get(&self) -> bool {
        self.muted.get()
    }

    fn set(&self, muted: bool) {
        if self.muted.replace(muted) == muted {
            return;
        }
        for (last_sent, responder) in self.pending_watchers.borrow_mut().drain(..) {
            last_sent.set(Some(muted));
            let _ = responder.send(muted, false);
        }
    }

    fn watch(&self, last_sent: &LastSentMuted, responder: DeviceWatchMuteStateResponder) {
        let muted = self.muted.get();
        if last_sent.get() == Some(muted) {
            self.pending_watchers.borrow_mut().push((Rc::clone(last_sent), responder));
        } else {
            last_sent.set(Some(muted));
            let _ = responder.send(muted, false);
        }
    }

    fn disconnect(&self) {
        self.pending_watchers.borrow_mut().clear();
        for handle in self.abort_handles.borrow_mut().drain(..) {
            handle.abort();
        }
    }
}

/// Device changes shared by all device watcher connections. Once a connection has received a
/// nonempty device list, later `WatchDevices` calls on it return once devices are added or
/// removed.
#[derive(Default)]
struct DeviceChanges {
    connections: RefCell<Vec<std::rc::Weak<WatcherConnection>>>,
}

#[derive(Default)]
struct WatcherConnection {
    initial_devices_sent: Cell<bool>,
    queued: RefCell<Vec<WatchDevicesEvent>>,
    pending: RefCell<Option<DeviceWatcherWatchDevicesResponder>>,
}

impl WatcherConnection {
    fn flush(&self) {
        if self.queued.borrow().is_empty() {
            return;
        }
        if let Some(responder) = self.pending.borrow_mut().take() {
            let events: Vec<_> = self.queued.borrow_mut().drain(..).collect();
            let _ = responder.send(&events);
        }
    }
}

impl DeviceChanges {
    fn add(&self, event: WatchDevicesEvent) {
        self.connections.borrow_mut().retain(|connection| {
            let Some(connection) = connection.upgrade() else {
                return false;
            };
            if connection.initial_devices_sent.get() {
                connection.queued.borrow_mut().push(event);
                connection.flush();
            }
            true
        });
    }
}

/// Software mute state of each camera device, by id.
type Devices = Rc<RefCell<BTreeMap<u64, Rc<SwMuteState>>>>;

/// Returns the initial devices, with a single device with id 1.
fn initial_devices() -> Devices {
    Rc::new(RefCell::new(BTreeMap::from([(1, Rc::new(SwMuteState::default()))])))
}

pub struct Camera3Service {
    devices: Devices,
    device_changes: Rc<DeviceChanges>,
    mute_listeners: Rc<RefCell<Vec<UnboundedSender<bool>>>>,
    // If true, sends the camera device response immediately. If false, sends
    // an empty device list. If None, delay_camera_device should be provided.
    has_camera_device: Rc<AtomicBool>,
    // If true, first sends an empty device list on watch, then delays, then
    // sends the camera device response.
    delay_camera_device: Rc<AtomicBool>,
}

impl Camera3Service {
    pub fn new(has_camera_device: bool) -> Self {
        Self {
            devices: initial_devices(),
            device_changes: Rc::new(DeviceChanges::default()),
            mute_listeners: Rc::new(RefCell::new(Vec::new())),
            has_camera_device: Rc::new(AtomicBool::new(has_camera_device)),
            delay_camera_device: Rc::new(AtomicBool::new(false)),
        }
    }

    pub fn new_delayed_devices(delay_camera_device: bool) -> Self {
        Self {
            devices: initial_devices(),
            device_changes: Rc::new(DeviceChanges::default()),
            mute_listeners: Rc::new(RefCell::new(Vec::new())),
            has_camera_device: Rc::new(AtomicBool::new(false)),
            delay_camera_device: Rc::new(AtomicBool::new(delay_camera_device)),
        }
    }

    /// Returns whether all camera devices are software muted.
    pub fn camera_sw_muted(&self) -> bool {
        self.devices.borrow().values().all(|device| device.get())
    }

    /// Returns a receiver that is sent whether all camera devices are software muted whenever a
    /// `SetSoftwareMuteState` request is handled.
    pub fn watch_sw_mute_changes(&self) -> UnboundedReceiver<bool> {
        let (tx, rx) = mpsc::unbounded();
        self.mute_listeners.borrow_mut().push(tx);
        rx
    }

    /// Sets the software mute state of all camera devices as if it were changed by another
    /// client of the camera, and notifies pending `WatchMuteState` calls.
    pub fn set_camera_sw_muted(&self, muted: bool) {
        for device in self.devices.borrow().values() {
            device.set(muted);
        }
    }

    /// Adds a camera device with the given id and software mute state, and reports it as added
    /// to device watcher connections that have already received the initial device list.
    pub fn add_camera_device(&self, id: u64, sw_muted: bool) {
        let device = Rc::new(SwMuteState::default());
        device.set(sw_muted);
        if let Some(old_device) = self.devices.borrow_mut().insert(id, device) {
            old_device.disconnect();
        }
        self.device_changes.add(WatchDevicesEvent::Added(id));
    }

    /// Removes the camera device with the given id, closes its connections, and reports it as
    /// removed to device watcher connections that have already received the initial device list.
    pub fn remove_camera_device(&self, id: u64) {
        if let Some(device) = self.devices.borrow_mut().remove(&id) {
            device.disconnect();
            self.device_changes.add(WatchDevicesEvent::Removed(id));
        }
    }
}

impl Service for Camera3Service {
    fn can_handle_service(&self, service_name: &str) -> bool {
        service_name == DeviceWatcherMarker::PROTOCOL_NAME
    }

    fn process_stream(&mut self, service_name: &str, channel: zx::Channel) -> Result<(), Error> {
        if !self.can_handle_service(service_name) {
            return Err(format_err!("can't handle service"));
        }

        let mut device_watcher_stream =
            ServerEnd::<DeviceWatcherMarker>::new(channel).into_stream();

        let devices = Rc::clone(&self.devices);
        let mute_listeners = Rc::clone(&self.mute_listeners);
        let has_camera_device = Rc::clone(&self.has_camera_device);
        let delay_camera_device = Rc::clone(&self.delay_camera_device);
        let connection = Rc::new(WatcherConnection::default());
        self.device_changes.connections.borrow_mut().push(Rc::downgrade(&connection));
        let mut watch_count = 0;
        fasync::Task::local(async move {
            while let Some(req) = device_watcher_stream.try_next().await.unwrap() {
                // Support future expansion of FIDL.
                #[allow(unreachable_patterns)]
                match req {
                    DeviceWatcherRequest::WatchDevices { responder } => {
                        if connection.initial_devices_sent.get() {
                            let _ = connection.pending.borrow_mut().replace(responder);
                            connection.flush();
                            continue;
                        }
                        let camera_devices: Vec<_> = devices
                            .borrow()
                            .keys()
                            .map(|id| WatchDevicesEvent::Added(*id))
                            .collect();
                        let camera_devices = &camera_devices[..];
                        let empty_devices = &[][..];

                        let devices = if delay_camera_device.load(Ordering::Relaxed) {
                            if watch_count == 0 {
                                watch_count += 1;
                                empty_devices
                            } else {
                                let timer = fasync::Timer::new(
                                    MonotonicDuration::from_millis(CAMERA_WATCHER_TIMEOUT / 2)
                                        .after_now(),
                                );
                                timer.await;
                                camera_devices
                            }
                        } else if has_camera_device.load(Ordering::Relaxed) {
                            camera_devices
                        } else {
                            empty_devices
                        };
                        connection.initial_devices_sent.set(!devices.is_empty());
                        responder.send(devices).expect("Failed to send devices response");
                    }
                    DeviceWatcherRequest::ConnectToDevice {
                        id,
                        request, // ServerEnd<DeviceMarker>
                        control_handle: _,
                    } => {
                        let mut stream = request.into_stream();
                        let Some(camera_sw_muted) = devices.borrow().get(&id).cloned() else {
                            continue;
                        };
                        let devices = Rc::clone(&devices);
                        let mute_listeners = Rc::clone(&mute_listeners);
                        let (abort_handle, abort_registration) = AbortHandle::new_pair();
                        camera_sw_muted.abort_handles.borrow_mut().push(abort_handle);
                        fasync::Task::local(async move {
                            let _ = Abortable::new(
                                async move {
                                    let last_sent_muted = Rc::new(Cell::new(None));
                                    while let Some(req) = stream.try_next().await.unwrap() {
                                        // Support future expansion of FIDL.
                                        match req {
                                            DeviceRequest::SetSoftwareMuteState {
                                                muted,
                                                responder,
                                            } => {
                                                camera_sw_muted.set(muted);
                                                let _ = responder.send();
                                                let all_muted = devices
                                                    .borrow()
                                                    .values()
                                                    .all(|device| device.get());
                                                mute_listeners.borrow_mut().retain(|tx| {
                                                    tx.unbounded_send(all_muted).is_ok()
                                                });
                                            }
                                            DeviceRequest::WatchMuteState { responder } => {
                                                camera_sw_muted.watch(&last_sent_muted, responder);
                                            }
                                            _ => {}
                                        }
                                    }
                                },
                                abort_registration,
                            )
                            .await;
                        })
                        .detach();
                    }
                    _ => {}
                }
            }
        })
        .detach();

        Ok(())
    }
}
