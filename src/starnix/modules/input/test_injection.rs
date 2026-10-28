// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::input_device::InputDevice;
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_input_injection::{
    InputDeviceRegistryRegisterAndGetDeviceInfoResponse, InputDeviceRegistryRequest,
    InputDeviceRegistryRequestStream,
};
use fidl_fuchsia_input_report as fir;
use fuchsia_async as fasync;
use futures::StreamExt;
use futures::future::{AbortHandle, Abortable};
use starnix_core::device::kobject::Device;
use starnix_core::task::Kernel;
use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
use starnix_logging::{log_debug, log_warn};
use starnix_modules_input_report_event_conversion::InputReportConverter;
use starnix_sync::Mutex;
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, error};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Weak};

const MAX_UNACKNOWLEDGED_REPORTS_LIMIT: u16 = 16;

/// Represents an injected device currently registered in the test injection registry.
#[derive(Clone)]
pub struct InjectedDevice {
    pub device_id: u32,
    pub minor: u32,
    pub input_device: InputDevice,
    pub(crate) starnix_device: Device,
    pub device_proxy: fir::InputDeviceProxy,
    pub(crate) reader_abort_handle: AbortHandle,
}

impl std::fmt::Debug for InjectedDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InjectedDevice")
            .field("device_id", &self.device_id)
            .field("minor", &self.minor)
            .field("starnix_device", &self.starnix_device)
            .finish()
    }
}

/// Internal state for `InputDeviceRegistry`.
#[derive(Default, Debug)]
pub(crate) struct RegistryState {
    pub(crate) devices: HashMap<u32, InjectedDevice>,
    pub(crate) reserved_minors: HashSet<u32>,
}

/// Server implementing `fuchsia.input.injection.InputDeviceRegistry`.
///
/// Enables test frameworks (such as `input-helper`) to dynamically inject input
/// devices and report streams into Starnix without hardcoded device configs.
pub struct InputDeviceRegistry {
    kernel: Weak<Kernel>,
    next_device_id: AtomicU32,
    pub(crate) state: Arc<Mutex<RegistryState>>,
    scope: fasync::Scope,
}

impl std::fmt::Debug for InputDeviceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputDeviceRegistry")
            .field("devices", &self.state.lock().devices.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Drop for InputDeviceRegistry {
    fn drop(&mut self) {
        if let Some(kernel) = self.kernel.upgrade() {
            let devices = {
                let mut state = self.state.lock();
                let devices = std::mem::take(&mut state.devices);
                for (_, injected) in &devices {
                    state.reserved_minors.insert(injected.minor);
                }
                devices
            };
            for (_, injected) in devices {
                injected.reader_abort_handle.abort();
                unregister_device(
                    &kernel,
                    injected.starnix_device,
                    self.state.clone(),
                    injected.minor,
                );
            }
        }
    }
}

impl InputDeviceRegistry {
    /// Creates a new `InputDeviceRegistry`.
    pub fn new(kernel: Weak<Kernel>) -> Arc<Self> {
        Arc::new(Self {
            kernel,
            next_device_id: AtomicU32::new(1),
            state: Arc::new(Mutex::new(RegistryState::default())),
            scope: fasync::Scope::new(),
        })
    }

    /// Serves incoming requests from an `InputDeviceRegistryRequestStream`.
    pub async fn serve_stream(
        self: Arc<Self>,
        mut stream: InputDeviceRegistryRequestStream,
    ) -> Result<(), fidl::Error> {
        while let Some(request) = stream.next().await {
            match request? {
                InputDeviceRegistryRequest::Register { device, control_handle: _ } => {
                    if let Err(e) = self.register_device(device).await {
                        log_warn!("InputDeviceRegistry::Register failed: {:?}", e);
                    }
                }
                InputDeviceRegistryRequest::RegisterAndGetDeviceInfo { device, responder } => {
                    match self.register_device(device).await {
                        Ok(device_id) => {
                            let _ = responder.send(
                                InputDeviceRegistryRegisterAndGetDeviceInfoResponse {
                                    device_id: Some(device_id),
                                    ..Default::default()
                                },
                            );
                        }
                        Err(e) => {
                            log_warn!(
                                "InputDeviceRegistry::RegisterAndGetDeviceInfo failed: {:?}",
                                e
                            );
                            let _ = responder.send(
                                InputDeviceRegistryRegisterAndGetDeviceInfoResponse::default(),
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Registers an input device client, obtains its descriptor, instantiates an
    /// `InputDevice`, and spawns an asynchronous reader loop.
    pub async fn register_device(
        self: &Arc<Self>,
        device_client_end: ClientEnd<fir::InputDeviceMarker>,
    ) -> Result<u32, Errno> {
        let device_proxy = device_client_end.into_proxy();
        let descriptor = device_proxy.get_descriptor().await.map_err(|_| errno!(EIO))?;

        let (v2_client, v2_server) =
            fidl::endpoints::create_endpoints::<fir::InputReportsReaderV2Marker>();
        if let Err(e) = device_proxy
            .get_input_reports_reader_v2(v2_server, MAX_UNACKNOWLEDGED_REPORTS_LIMIT)
            .await
        {
            log_warn!("Failed to obtain InputReportsReaderV2: {:?}", e);
            return error!(EIO);
        }

        let device_id = self.next_device_id.fetch_add(1, Ordering::Relaxed);

        let kernel = self.kernel.upgrade().ok_or_else(|| errno!(ENODEV))?;
        let inspect_node = kernel.inspect_node.clone_weak();

        let input_device = InputDevice::from_device_descriptor(
            &descriptor,
            None,
            &format!("test_input_device_{device_id}"),
            &inspect_node,
        );

        let mut next_candidate = 0u32;
        let (starnix_device, minor) = loop {
            let candidate_minor = {
                let mut state = self.state.lock();
                while state.reserved_minors.contains(&next_candidate)
                    || state.devices.values().any(|d| d.minor == next_candidate)
                {
                    next_candidate += 1;
                }
                let cand = next_candidate;
                next_candidate += 1;
                state.reserved_minors.insert(cand);
                cand
            };

            match input_device.clone().register(&kernel, candidate_minor) {
                Ok(dev) => break (dev, candidate_minor),
                Err(e) => {
                    self.state.lock().reserved_minors.remove(&candidate_minor);
                    if e == errno!(EEXIST) {
                        continue;
                    }
                    return Err(e);
                }
            }
        };

        let (reader_abort_handle, reader_registration) = AbortHandle::new_pair();
        let injected = InjectedDevice {
            device_id,
            minor,
            input_device: input_device.clone(),
            starnix_device,
            device_proxy,
            reader_abort_handle,
        };
        {
            let mut state = self.state.lock();
            state.devices.insert(device_id, injected);
            state.reserved_minors.remove(&minor);
        }

        let reader_proxy = v2_client.into_proxy();
        let self_weak = Arc::downgrade(self);
        let reader_future = Abortable::new(
            Self::run_reports_reader_v2(reader_proxy, input_device),
            reader_registration,
        );
        self.scope.spawn(async move {
            let _ = reader_future.await;
            if let Some(this) = self_weak.upgrade() {
                this.remove_device(device_id);
            }
        });

        Ok(device_id)
    }

    /// Asynchronous report processing loop for `fuchsia.input.report.InputReportsReaderV2`.
    async fn run_reports_reader_v2(
        reader_proxy: fir::InputReportsReaderV2Proxy,
        input_device: InputDevice,
    ) {
        let mut converter = InputReportConverter::new();
        let mut event_stream = reader_proxy.take_event_stream();

        while let Some(event) = event_stream.next().await {
            match event {
                Ok(fir::InputReportsReaderV2Event::OnInputReports {
                    reports,
                    last_report_stamp,
                }) => {
                    for report in reports {
                        let events = converter.handle_report(&report);
                        input_device.dispatch_events(&events);
                    }
                    if let Err(e) = reader_proxy.acknowledge_reports(last_report_stamp) {
                        log_debug!("Failed to acknowledge input reports: {:?}", e);
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    log_debug!("InputReportsReaderV2 event error: {:?}", e);
                    break;
                }
            }
        }
    }

    /// Removes and unregisters an injected device.
    pub fn remove_device(&self, device_id: u32) {
        let removed = {
            let mut state = self.state.lock();
            let removed = state.devices.remove(&device_id);
            if let Some(ref injected) = removed {
                state.reserved_minors.insert(injected.minor);
            }
            removed
        };
        if let Some(injected) = removed {
            injected.reader_abort_handle.abort();
            if let Some(kernel) = self.kernel.upgrade() {
                unregister_device(
                    &kernel,
                    injected.starnix_device,
                    self.state.clone(),
                    injected.minor,
                );
            } else {
                self.state.lock().reserved_minors.remove(&injected.minor);
            }
        }
    }

    /// Returns an injected device if it is currently registered.
    pub fn device(&self, device_id: u32) -> Option<InjectedDevice> {
        self.state.lock().devices.get(&device_id).cloned()
    }
}

fn unregister_device(
    kernel: &Arc<Kernel>,
    event_device: Device,
    state: Arc<Mutex<RegistryState>>,
    minor: u32,
) {
    if !kernel.is_shutting_down() {
        let req = SpawnRequestBuilder::new()
            .with_sync_closure(move |current_task| {
                InputDevice::unregister(current_task, event_device);
                state.lock().reserved_minors.remove(&minor);
            })
            .build();
        kernel.kthreads.spawner().spawn_from_request(req);
        return;
    }
    state.lock().reserved_minors.remove(&minor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::{self, RequestStream, ServerEnd};
    use fidl_fuchsia_input_injection::InputDeviceRegistryMarker;
    use starnix_core::device::DeviceMode;
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_core::vfs::{FileHandle, VecOutputBuffer};
    use starnix_uapi::device_id::{DeviceId as StarnixDeviceId, INPUT_MAJOR};
    use starnix_uapi::errors::{EAGAIN, Errno};
    use zerocopy::FromBytes;

    const INPUT_EVENT_SIZE: usize = std::mem::size_of::<starnix_uapi::input_event>();

    fn read_uapi_events(
        file: &FileHandle,
        current_task: &starnix_core::task::CurrentTask,
    ) -> Vec<starnix_uapi::input_event> {
        std::iter::from_fn(|| {
            let mut event_bytes = VecOutputBuffer::new(INPUT_EVENT_SIZE);
            match file.read(current_task, &mut event_bytes) {
                Ok(INPUT_EVENT_SIZE) => Some(
                    starnix_uapi::input_event::read_from_bytes(Vec::from(event_bytes).as_slice())
                        .map_err(|_| anyhow::anyhow!("failed to read input_event from buffer")),
                ),
                Ok(other_size) => Some(Err(anyhow::anyhow!(
                    "got {} bytes (expected {})",
                    other_size,
                    INPUT_EVENT_SIZE
                ))),
                Err(Errno { code: EAGAIN, .. }) => None,
                Err(other_error) => Some(Err(anyhow::anyhow!("read failed: {:?}", other_error))),
            }
        })
        .enumerate()
        .map(|(i, read_res)| match read_res {
            Ok(event) => event,
            Err(e) => panic!("unexpected result {:?} on iteration {}", e, i),
        })
        .collect()
    }

    fn make_test_descriptor() -> fir::DeviceDescriptor {
        fir::DeviceDescriptor {
            device_information: Some(fir::DeviceInformation {
                vendor_id: Some(0x18d1),
                product_id: Some(0x2345),
                version: Some(1),
                product_name: Some("Test Touch".to_string()),
                ..Default::default()
            }),
            touch: Some(fir::TouchDescriptor {
                input: Some(fir::TouchInputDescriptor {
                    contacts: Some(vec![fir::ContactInputDescriptor {
                        position_x: Some(fir::Axis {
                            range: fir::Range { min: 0, max: 1080 },
                            unit: fir::Unit { type_: fir::UnitType::Meters, exponent: -3 },
                        }),
                        position_y: Some(fir::Axis {
                            range: fir::Range { min: 0, max: 2400 },
                            unit: fir::Unit { type_: fir::UnitType::Meters, exponent: -3 },
                        }),
                        ..Default::default()
                    }]),
                    max_contacts: Some(10),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[::fuchsia::test]
    async fn test_registration_and_event_dispatch_v2() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let registry = InputDeviceRegistry::new(Arc::downgrade(&kernel));
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<fir::InputReportsReaderV2ControlHandle>();
            let mut reader_sender_opt = Some(reader_sender);
            let (ack_sender, ack_receiver) = futures::channel::oneshot::channel::<()>();
            let mut ack_sender_opt = Some(ack_sender);

            // Mock InputDevice server
            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                let mut inner_tasks = Vec::new();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            let reader_stream = reader.into_stream();
                            let control_handle = reader_stream.control_handle();
                            if let Some(sender) = reader_sender_opt.take() {
                                let _ = sender.send(control_handle);
                            }
                            responder.send(16).unwrap();

                            let ack_sender_opt_inner = ack_sender_opt.take();
                            inner_tasks.push(fasync::Task::local(async move {
                                let mut ack_sender_opt_inner = ack_sender_opt_inner;
                                let mut reader_stream = reader_stream;
                                while let Some(req) = reader_stream.next().await {
                                    match req.unwrap() {
                                        fir::InputReportsReaderV2Request::AcknowledgeReports {
                                            ..
                                        } => {
                                            if let Some(sender) = ack_sender_opt_inner.take() {
                                                let _ = sender.send(());
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }));
                        }
                        _ => {}
                    }
                }
            }));

            let device_id = registry.register_device(device_client).await.unwrap();
            let injected = registry.device(device_id).expect("device should be registered");

            let file = injected.input_device.open_test(&current_task).unwrap();
            let control_handle = reader_receiver.await.unwrap();

            // Inject a touch report
            let touch_report = fir::InputReport {
                event_time: Some(100_000_000),
                touch: Some(fir::TouchInputReport {
                    contacts: Some(vec![fir::ContactInputReport {
                        contact_id: Some(1),
                        position_x: Some(500),
                        position_y: Some(600),
                        pressure: Some(50),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            };

            control_handle.send_on_input_reports(vec![touch_report], 1).unwrap();

            // Deterministic sync: wait until reader acknowledges report
            ack_receiver.await.unwrap();

            let events = read_uapi_events(&file, &current_task);
            assert!(!events.is_empty());
            assert!(events.iter().any(|e| e.type_ == starnix_uapi::EV_ABS as u16
                && e.code == starnix_uapi::ABS_MT_POSITION_X as u16
                && e.value == 500));
            assert!(events.iter().any(|e| e.type_ == starnix_uapi::EV_ABS as u16
                && e.code == starnix_uapi::ABS_MT_POSITION_Y as u16
                && e.value == 600));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pre_open_buffering() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let registry = InputDeviceRegistry::new(Arc::downgrade(&kernel));
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<fir::InputReportsReaderV2ControlHandle>();
            let mut reader_sender_opt = Some(reader_sender);
            let (ack_sender, ack_receiver) = futures::channel::oneshot::channel::<()>();
            let mut ack_sender_opt = Some(ack_sender);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                let mut inner_tasks = Vec::new();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            let reader_stream = reader.into_stream();
                            let control_handle = reader_stream.control_handle();
                            if let Some(sender) = reader_sender_opt.take() {
                                let _ = sender.send(control_handle);
                            }
                            responder.send(16).unwrap();

                            let ack_inner = ack_sender_opt.take();
                            inner_tasks.push(fasync::Task::local(async move {
                                let mut ack_inner = ack_inner;
                                let mut reader_stream = reader_stream;
                                while let Some(req) = reader_stream.next().await {
                                    match req.unwrap() {
                                        fir::InputReportsReaderV2Request::AcknowledgeReports {
                                            ..
                                        } => {
                                            if let Some(sender) = ack_inner.take() {
                                                let _ = sender.send(());
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }));
                        }
                        _ => {}
                    }
                }
            }));

            let device_id = registry.register_device(device_client).await.unwrap();
            let injected = registry.device(device_id).expect("device should be registered");

            let control_handle = reader_receiver.await.unwrap();

            // Inject a touch report BEFORE opening the file
            let touch_report = fir::InputReport {
                event_time: Some(150_000_000),
                touch: Some(fir::TouchInputReport {
                    contacts: Some(vec![fir::ContactInputReport {
                        contact_id: Some(1),
                        position_x: Some(700),
                        position_y: Some(800),
                        pressure: Some(40),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            };
            control_handle.send_on_input_reports(vec![touch_report], 1).unwrap();

            // Deterministic sync: wait until reader acknowledges report
            ack_receiver.await.unwrap();

            // Now open the device *after* the event has already been dispatched
            let file = injected.input_device.open_test(&current_task).unwrap();

            let events = read_uapi_events(&file, &current_task);
            assert!(!events.is_empty());
            assert!(events.iter().any(|e| e.type_ == starnix_uapi::EV_ABS as u16
                && e.code == starnix_uapi::ABS_MT_POSITION_X as u16
                && e.value == 700));
            assert!(events.iter().any(|e| e.type_ == starnix_uapi::EV_ABS as u16
                && e.code == starnix_uapi::ABS_MT_POSITION_Y as u16
                && e.value == 800));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_rollback_on_reader_failure() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let registry = InputDeviceRegistry::new(Arc::downgrade(&kernel));
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            // Mock InputDevice server that returns descriptor and immediately closes connection
            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                            // Close stream after descriptor so reader setup fails
                            break;
                        }
                        _ => {}
                    }
                }
            }));

            let result = registry.register_device(device_client).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), errno!(EIO));
            assert!(registry.device(1).is_none());
            assert!(registry.state.lock().reserved_minors.is_empty());
            let root = &kernel.device_registry.objects.root;
            assert!(root.lookup("devices/virtual/input/input0".into()).is_none());
            assert!(
                kernel
                    .device_registry
                    .get_device(StarnixDeviceId::new(INPUT_MAJOR, 0), DeviceMode::Char)
                    .is_err()
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_register_and_get_device_info_fidl() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let registry = InputDeviceRegistry::new(Arc::downgrade(&kernel));
            let mut _tasks = Vec::new();

            let (registry_client, registry_server) =
                endpoints::create_endpoints::<InputDeviceRegistryMarker>();

            let reg_clone = registry.clone();
            _tasks.push(fasync::Task::local(async move {
                let _ = reg_clone.serve_stream(registry_server.into_stream()).await;
            }));

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            let (_reader_sender, _reader_receiver) =
                futures::channel::oneshot::channel::<ServerEnd<fir::InputReportsReaderV2Marker>>();
            let mut reader_sender_opt = Some(_reader_sender);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            if let Some(sender) = reader_sender_opt.take() {
                                let _ = sender.send(reader);
                            }
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let proxy = registry_client.into_proxy();
            let res = proxy.register_and_get_device_info(device_client).await.unwrap();
            assert!(res.device_id.is_some());
            let id = res.device_id.unwrap();
            assert!(registry.device(id).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_register_one_way_fidl() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let registry = InputDeviceRegistry::new(Arc::downgrade(&kernel));
            let mut _tasks = Vec::new();

            let (registry_client, registry_server) =
                endpoints::create_endpoints::<InputDeviceRegistryMarker>();

            let reg_clone = registry.clone();
            _tasks.push(fasync::Task::local(async move {
                let _ = reg_clone.serve_stream(registry_server.into_stream()).await;
            }));

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            let (_reader_sender, _reader_receiver) =
                futures::channel::oneshot::channel::<ServerEnd<fir::InputReportsReaderV2Marker>>();
            let mut reader_sender_opt = Some(_reader_sender);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            if let Some(sender) = reader_sender_opt.take() {
                                let _ = sender.send(reader);
                            }
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let proxy = registry_client.into_proxy();
            // Call the one-way `register` FIDL method
            proxy.register(device_client).unwrap();

            // Wait deterministically for registration to complete
            while registry.device(1).is_none() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }
            assert!(registry.device(1).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_disconnect_teardown() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let registry = InputDeviceRegistry::new(Arc::downgrade(&kernel));
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            let (reader_sender, reader_receiver) =
                futures::channel::oneshot::channel::<ServerEnd<fir::InputReportsReaderV2Marker>>();
            let mut reader_sender_opt = Some(reader_sender);

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            if let Some(sender) = reader_sender_opt.take() {
                                let _ = sender.send(reader);
                            }
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let device_id = registry.register_device(device_client).await.unwrap();
            assert!(registry.device(device_id).is_some());

            // Receive reader server end and immediately drop it to simulate client disconnect.
            let reader_server = reader_receiver.await.unwrap();
            drop(reader_server);

            // Wait deterministically for disconnect handler to trigger
            while registry.device(device_id).is_some() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }

            assert!(registry.device(device_id).is_none());
        })
        .await;
    }

    fn make_keyboard_descriptor() -> fir::DeviceDescriptor {
        fir::DeviceDescriptor {
            device_information: Some(fir::DeviceInformation {
                vendor_id: Some(0x18d1),
                product_id: Some(0x1234),
                version: Some(1),
                product_name: Some("Test Keyboard".to_string()),
                ..Default::default()
            }),
            keyboard: Some(fir::KeyboardDescriptor {
                input: Some(fir::KeyboardInputDescriptor::default()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[::fuchsia::test]
    async fn test_multi_device_registration_and_minor_allocation() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let registry = InputDeviceRegistry::new(Arc::downgrade(&kernel));
            let mut _tasks = Vec::new();
            let readers = Arc::new(Mutex::new(Vec::new()));

            // Device 1: Touch
            let (device_client_1, device_server_1) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let readers_1 = readers.clone();
            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server_1.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            readers_1.lock().push(reader);
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            // Device 2: Keyboard
            let (device_client_2, device_server_2) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let readers_2 = readers.clone();
            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server_2.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_keyboard_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            readers_2.lock().push(reader);
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let (res_1, res_2) = futures::join!(
                registry.register_device(device_client_1),
                registry.register_device(device_client_2),
            );
            let id_1 = res_1.unwrap();
            let id_2 = res_2.unwrap();

            assert_ne!(id_1, id_2);

            let dev_1 = registry.device(id_1).expect("device 1 registered");
            let dev_2 = registry.device(id_2).expect("device 2 registered");

            assert_ne!(dev_1.minor, dev_2.minor);
            assert_eq!(registry.state.lock().devices.len(), 2);

            registry.remove_device(id_1);
            assert!(registry.device(id_1).is_none());
            assert!(registry.device(id_2).is_some());
            assert_eq!(registry.state.lock().devices.len(), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_registry_drop_unregisters_devices() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let registry = InputDeviceRegistry::new(Arc::downgrade(&kernel));
            let mut _tasks = Vec::new();
            let readers = Arc::new(Mutex::new(Vec::new()));

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let readers_clone = readers.clone();
            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            readers_clone.lock().push(reader);
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let id = registry.register_device(device_client).await.unwrap();
            assert!(registry.device(id).is_some());

            let state = registry.state.clone();
            // Dropping the registry must cleanly unregister devices
            drop(registry);

            while !state.lock().reserved_minors.is_empty() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }

            let root = &kernel.device_registry.objects.root;
            assert!(root.lookup("devices/virtual/input/input0".into()).is_none());
            assert!(
                kernel
                    .device_registry
                    .get_device(StarnixDeviceId::new(INPUT_MAJOR, 0), DeviceMode::Char)
                    .is_err()
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_minor_reserved_during_async_unregistration_and_reused_after() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let registry = InputDeviceRegistry::new(Arc::downgrade(&kernel));
            let mut _tasks = Vec::new();
            let readers = Arc::new(Mutex::new(Vec::new()));

            // Setup device 1
            let (device_client_1, device_server_1) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let readers_1 = readers.clone();
            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server_1.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            readers_1.lock().push(reader);
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let id_1 = registry.register_device(device_client_1).await.unwrap();
            let dev_1 = registry.device(id_1).unwrap();
            assert_eq!(dev_1.minor, 0);

            // Simulate an in-flight background unregistration where minor 0 remains in reserved_minors.
            registry.state.lock().reserved_minors.insert(0);

            // Register device 2 while minor 0 is reserved.
            let (device_client_2, device_server_2) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let readers_2 = readers.clone();
            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server_2.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_keyboard_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            readers_2.lock().push(reader);
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let id_2 = registry.register_device(device_client_2).await.unwrap();
            let dev_2 = registry.device(id_2).unwrap();

            // Device 2 must not collide with minor 0 because it was in reserved_minors.
            assert_ne!(dev_2.minor, 0);
            assert_eq!(dev_2.minor, 1);
            assert!(registry.device(id_2).is_some());

            // Release simulated in-flight reservation on minor 0.
            registry.state.lock().reserved_minors.remove(&0);

            registry.remove_device(id_1);
            registry.remove_device(id_2);

            while !registry.state.lock().reserved_minors.is_empty() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }

            // Now register device 3: minor 0 is free and should be reused.
            let (device_client_3, device_server_3) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            let readers_3 = readers.clone();
            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server_3.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            readers_3.lock().push(reader);
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let id_3 = registry.register_device(device_client_3).await.unwrap();
            let dev_3 = registry.device(id_3).unwrap();
            assert_eq!(dev_3.minor, 0);
            registry.remove_device(id_3);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_immediate_reader_termination_does_not_leak() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let registry = InputDeviceRegistry::new(Arc::downgrade(&kernel));
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            // Immediately drop reader endpoint so stream terminates
                            drop(reader);
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let id = registry.register_device(device_client).await.unwrap();

            // Wait deterministically for reader loop termination and remove_device to complete.
            while registry.device(id).is_some() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }

            assert!(registry.device(id).is_none());
            assert!(registry.state.lock().devices.is_empty());

            // Wait for kthread unregistration to release reserved_minors
            while !registry.state.lock().reserved_minors.is_empty() {
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    zx::MonotonicDuration::from_millis(2),
                ))
                .await;
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_register_and_get_device_info_failure_does_not_close_stream() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let registry = InputDeviceRegistry::new(Arc::downgrade(&kernel));
            let mut _tasks = Vec::new();

            let (registry_client, registry_server) =
                endpoints::create_endpoints::<InputDeviceRegistryMarker>();
            let registry_clone = registry.clone();
            _tasks.push(fasync::Task::local(async move {
                let _ = registry_clone.serve_stream(registry_server.into_stream()).await;
            }));

            let registry_proxy = registry_client.into_proxy();

            // 1. Send invalid device client whose channel is immediately closed.
            let (closed_client, closed_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            drop(closed_server);

            let res = registry_proxy
                .register_and_get_device_info(closed_client)
                .await
                .expect("request should not fail FIDL call");
            assert_eq!(res.device_id, None);

            // 2. Stream should remain open; a valid registration succeeds.
            let (valid_client, valid_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();
            _tasks.push(fasync::Task::local(async move {
                let mut stream = valid_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 { responder, .. } => {
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let res2 = registry_proxy
                .register_and_get_device_info(valid_client)
                .await
                .expect("stream should still be open and return Ok");
            assert!(res2.device_id.is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_remove_device_aborts_reader_task() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let registry = InputDeviceRegistry::new(Arc::downgrade(&kernel));
            let mut _tasks = Vec::new();

            let (device_client, device_server) =
                endpoints::create_endpoints::<fir::InputDeviceMarker>();

            _tasks.push(fasync::Task::local(async move {
                let mut stream = device_server.into_stream();
                while let Some(request) = stream.next().await {
                    match request.unwrap() {
                        fir::InputDeviceRequest::GetDescriptor { responder } => {
                            responder.send(&make_test_descriptor()).unwrap();
                        }
                        fir::InputDeviceRequest::GetInputReportsReaderV2 { responder, .. } => {
                            responder.send(16).unwrap();
                        }
                        _ => {}
                    }
                }
            }));

            let id = registry.register_device(device_client).await.unwrap();
            let dev = registry.device(id).expect("device should exist");
            assert!(!dev.reader_abort_handle.is_aborted());

            registry.remove_device(id);
            assert!(dev.reader_abort_handle.is_aborted());
        })
        .await;
    }
}
