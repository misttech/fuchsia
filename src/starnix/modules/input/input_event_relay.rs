// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{
    InputDevice, InputDeviceInfo, InputDeviceInfoHandle, InputDeviceStatus, InputFile, uinput,
};

// Add a fuchsia-specific vendor ID. 0xfc1a is currently not allocated
// to any vendor in the USB spec.
//
// May not be zero, see below.
const FUCHSIA_VENDOR_ID: u16 = 0xfc1a;
const FUCHSIA_KEYBOARD_PRODUCT_ID: u16 = 0x1;
const FUCHSIA_TOUCH_PRODUCT_ID: u16 = 0x2;
const FUCHSIA_MOUSE_PRODUCT_ID: u16 = 0x3;

// Touch, keyboard, and mouse input IDs should be distinct.
// Per https://www.linuxjournal.com/article/6429, the bus type should be populated with a
// sensible value, but other fields may not be.
//
// While this may be the case for Linux itself, Android is not so relaxed.
// Devices with apparently-invalid vendor or product IDs don't get extra
// device configuration.  So we must make a minimum effort to present
// sensibly-looking product and vendor IDs.  Zero version only means that
// version-specific config files will not be applied.
//
// For background, see:
//
// * Allowable file locations:
//   https://source.android.com/docs/core/interaction/input/input-device-configuration-files#location
// * Android configuration selection code:
//   https://source.corp.google.com/h/googleplex-android/platform/superproject/main/+/main:frameworks/native/libs/input/InputDevice.cpp;l=60;drc=285211e60bff87fc5a9c9b4105a4b4ccb7edffaf
pub const TOUCH_INPUT_ID: uapi::input_id = uapi::input_id {
    bustype: uapi::BUS_VIRTUAL as u16,
    vendor: FUCHSIA_VENDOR_ID,
    product: FUCHSIA_TOUCH_PRODUCT_ID,
    version: 0,
};
pub const KEYBOARD_INPUT_ID: uapi::input_id = uapi::input_id {
    bustype: uapi::BUS_VIRTUAL as u16,
    vendor: FUCHSIA_VENDOR_ID,
    product: FUCHSIA_KEYBOARD_PRODUCT_ID,
    version: 1,
};
pub const MOUSE_INPUT_ID: uapi::input_id = uapi::input_id {
    bustype: uapi::BUS_VIRTUAL as u16,
    vendor: FUCHSIA_VENDOR_ID,
    product: FUCHSIA_MOUSE_PRODUCT_ID,
    version: 1,
};
use fidl::endpoints::{ClientEnd, RequestStream, ServerEnd};
use fidl_fuchsia_ui_input as fuiinput;
use fidl_fuchsia_ui_input::TouchDeviceInfo;
use fidl_fuchsia_ui_input3::{
    KeyEventStatus, KeyboardListenerMarker, KeyboardListenerRequest, KeyboardListenerRequestStream,
    KeyboardSynchronousProxy,
};
use fidl_fuchsia_ui_pointer::{
    MouseEvent as FidlMouseEvent, TouchEvent as FidlTouchEvent, TouchPointerSample,
    {self as fuipointer},
};
use fidl_fuchsia_ui_policy as fuipolicy;
use fidl_fuchsia_ui_views as fuiviews;
#[cfg(test)]
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
#[cfg(test)]
use futures::channel::oneshot;
#[cfg(test)]
use futures::channel::oneshot::Sender;
#[cfg(test)]
use futures::executor::block_on;
use futures::{FutureExt as _, StreamExt as _};
use sorted_vec_map::SortedVecMap;
use starnix_core::device::kobject::Device;
use starnix_core::power::{ContainerWakingStream, create_proxy_for_wake_events_counter};
use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_logging::{log_debug, log_error, log_warn};
use starnix_modules_input_app_event_conversion::button_fuchsia_to_linux::{
    new_touch_buttons_bitvec, parse_fidl_media_button_event, parse_fidl_touch_button_event,
};
use starnix_modules_input_app_event_conversion::key_fuchsia_to_linux::parse_fidl_keyboard_event_to_linux_input_event;
use starnix_modules_input_app_event_conversion::mouse_fuchsia_to_linux::FuchsiaMouseEventToLinuxMouseEventConverter;
use starnix_modules_input_app_event_conversion::touch_fuchsia_to_linux::FuchsiaTouchEventToLinuxTouchEventConverter;
use starnix_sync::{
    InputEventRelayOpenedFilesLock, InputRelayDeviceInfosLock, InputRelayDisplaySizeLock,
    LockDepMutex,
};
use starnix_uapi::errors::Errno;
use starnix_uapi::{error, uapi};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

const INPUT_RELAY_ROLE_NAME: &str = "fuchsia.starnix.kthread.input_relay";

#[derive(Clone, Copy)]
pub enum EventProxyMode {
    /// Don't proxy input events at all.
    None,

    /// Have the Starnix runner proxy events such that the container
    /// will wake up if events are received while the container is
    /// suspended.
    WakeContainer,
}

pub struct StartRelaysArgs {
    pub event_proxy_mode: EventProxyMode,
    pub touch_source_client_end: ClientEnd<fuipointer::TouchSourceV2Marker>,
    pub keyboard_proxy: KeyboardSynchronousProxy,
    pub mouse_source_client_end: ClientEnd<fuipointer::MouseSourceV2Marker>,
    pub view_ref: fuiviews::ViewRef,
    pub registry_proxy: fuipolicy::DeviceListenerRegistrySynchronousProxy,
    pub device_listener_registry: Option<fuiinput::DeviceListenerRegistrySynchronousProxy>,
    pub display_width: i32,
    pub display_height: i32,
}

#[derive(Default)]
pub struct OpenedFilesState {
    files: Vec<Weak<InputFile>>,
    has_been_opened: bool,
    buffered_events: Vec<uapi::input_event>,
}

impl OpenedFilesState {
    pub fn on_file_opened(&mut self, file: &Arc<InputFile>) {
        if !self.has_been_opened {
            self.has_been_opened = true;
            if !self.buffered_events.is_empty() {
                file.add_events(std::mem::take(&mut self.buffered_events));
            }
        }
        self.files.push(Arc::downgrade(file));
    }
}

impl std::ops::Deref for OpenedFilesState {
    type Target = Vec<Weak<InputFile>>;
    fn deref(&self) -> &Self::Target {
        &self.files
    }
}

impl std::ops::DerefMut for OpenedFilesState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.files
    }
}

pub type OpenedFiles = Arc<LockDepMutex<OpenedFilesState, InputEventRelayOpenedFilesLock>>;

pub enum InputDeviceType {
    Touch(FuchsiaTouchEventToLinuxTouchEventConverter),
    Keyboard,
    Mouse(FuchsiaMouseEventToLinuxMouseEventConverter),
}

impl std::fmt::Display for InputDeviceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputDeviceType::Touch(_) => write!(f, "touch"),
            InputDeviceType::Keyboard => write!(f, "keyboard"),
            InputDeviceType::Mouse(_) => write!(f, "mouse"),
        }
    }
}

enum DeviceRegistration {
    Pending { kernel: Arc<Kernel>, device: crate::InputDevice, device_id: DeviceId },
    Registered,
    Failed,
}

impl DeviceRegistration {
    /// Creates a `Pending` registration for `device`, which will be registered with `kernel`
    /// under `device_id` on the first call to `ensure_registered`.
    ///
    /// `device` must not already be registered: `DeviceRegistry::add_device_with_dir` silently
    /// overwrites an existing minor device entry, so a double registration would otherwise only
    /// surface as a warning log.
    fn pending(kernel: Arc<Kernel>, device: crate::InputDevice, device_id: DeviceId) -> Self {
        debug_assert!(
            {
                let devt = starnix_uapi::device_id::DeviceId::new(
                    starnix_uapi::device_id::INPUT_MAJOR,
                    device_id,
                );
                let next_devt = starnix_uapi::device_id::DeviceId::new(
                    starnix_uapi::device_id::INPUT_MAJOR,
                    device_id + 1,
                );
                kernel
                    .device_registry
                    .list_minor_devices(starnix_core::device::DeviceMode::Char, devt..next_devt)
                    .is_empty()
            },
            "input device {device_id} must not be registered before lazy registration",
        );
        Self::Pending { kernel, device, device_id }
    }

    fn ensure_registered(&mut self) {
        let Self::Pending { kernel, device, device_id } = self else { return };
        match device.clone().register(kernel, *device_id) {
            Ok(_) => *self = Self::Registered,
            Err(e) => {
                log_warn!("unable to register input device {device_id:?}: {e:?}");
                // Intentionally abandon registering the mouse device after one failed attempt
                // rather than retrying and logging on every subsequent input event.
                *self = Self::Failed;
            }
        }
    }
}

struct DeviceState {
    device_type: InputDeviceType,
    open_files: OpenedFiles,
    inspect_status: Option<Arc<InputDeviceStatus>>,
    registration: DeviceRegistration,
    starnix_device: Option<Device>,
    minor: u32,
}

impl DeviceState {
    #[cfg(test)]
    fn new_touch(
        device_id: DeviceId,
        open_files: OpenedFiles,
        inspect_status: Option<Arc<InputDeviceStatus>>,
    ) -> Self {
        Self {
            device_type: InputDeviceType::Touch(
                FuchsiaTouchEventToLinuxTouchEventConverter::create(),
            ),
            open_files,
            inspect_status,
            registration: DeviceRegistration::Registered,
            starnix_device: None,
            minor: device_id,
        }
    }

    #[cfg(test)]
    fn new_keyboard(
        device_id: DeviceId,
        open_files: OpenedFiles,
        inspect_status: Option<Arc<InputDeviceStatus>>,
    ) -> Self {
        Self {
            device_type: InputDeviceType::Keyboard,
            open_files,
            inspect_status,
            registration: DeviceRegistration::Registered,
            starnix_device: None,
            minor: device_id,
        }
    }

    fn new_mouse(
        device_id: DeviceId,
        open_files: OpenedFiles,
        inspect_status: Option<Arc<InputDeviceStatus>>,
    ) -> Self {
        Self {
            device_type: InputDeviceType::Mouse(
                FuchsiaMouseEventToLinuxMouseEventConverter::create(),
            ),
            open_files,
            inspect_status,
            registration: DeviceRegistration::Registered,
            starnix_device: None,
            minor: device_id,
        }
    }

    fn new_pending_mouse(
        kernel: Arc<Kernel>,
        device: crate::InputDevice,
        device_id: DeviceId,
    ) -> Self {
        Self {
            device_type: InputDeviceType::Mouse(
                FuchsiaMouseEventToLinuxMouseEventConverter::create(),
            ),
            open_files: device.open_files.clone(),
            inspect_status: Some(device.inspect_status.clone()),
            registration: DeviceRegistration::pending(kernel, device, device_id),
            starnix_device: None,
            minor: device_id,
        }
    }
}

pub struct TrackedWakeLease {
    _lease: fidl::EventPair,
    device_status: Arc<InputDeviceStatus>,
}

impl TrackedWakeLease {
    pub fn new(lease: fidl::EventPair, device_status: Arc<InputDeviceStatus>) -> Self {
        device_status.increment_active_wake_leases(1);
        device_status.count_events_with_wake_lease(1);
        Self { _lease: lease, device_status }
    }
}

impl Drop for TrackedWakeLease {
    fn drop(&mut self) {
        self.device_status.decrement_active_wake_leases(1);
    }
}

pub type DeviceId = u32;

// TODO(https://fxbug.dev/502662433): Default devices are no longer needed because
// existing devices are enumerated via the DeviceIterator before start_relays().
// Remove them in a follow-up change.
pub const DEFAULT_TOUCH_DEVICE_ID: DeviceId = 0;
pub const DEFAULT_KEYBOARD_DEVICE_ID: DeviceId = 1;
pub const DEFAULT_MOUSE_DEVICE_ID: DeviceId = 2;

const DEVICE_LISTENER_NOT_ATTEMPTED: u8 = 0;
const DEVICE_LISTENER_REGISTERED: u8 = 1;
const DEVICE_LISTENER_DISCONNECTED: u8 = 2;

// TODO(https://fxbug.dev/502662433): Migrate `add_touch_device` and `add_keyboard_device`
// in tests to `DeviceListener`, and remove `DeviceStateChange` and this channel.
#[cfg(test)]
enum DeviceStateChange {
    Add(DeviceId, Box<DeviceState>, Sender<()>),
}

pub fn new_input_relay() -> (InputEventsRelay, Arc<InputEventsRelayHandle>) {
    #[cfg(test)]
    let (sender, receiver) = unbounded();
    let num_unregistered_device_events = Arc::new(AtomicU64::new(0));
    let device_infos = Arc::new(LockDepMutex::new(SortedVecMap::new()));
    let display_size = Arc::new(LockDepMutex::new(None));

    (
        InputEventsRelay {
            devices: SortedVecMap::new(),
            #[cfg(test)]
            receiver: Some(receiver),
            num_unregistered_device_events: num_unregistered_device_events.clone(),
            num_devices: Arc::new(AtomicU64::new(0)),
            device_listener_registered: Arc::new(AtomicU8::new(DEVICE_LISTENER_NOT_ATTEMPTED)),
            _inspect_node: None,
            device_infos: device_infos.clone(),
            display_size: display_size.clone(),
        },
        Arc::new(InputEventsRelayHandle {
            #[cfg(test)]
            sender,
            num_unregistered_device_events,
            device_infos,
            display_size,
        }),
    )
}

/// Handle for managing input devices registered with the input event relay.
pub struct InputEventsRelayHandle {
    #[cfg(test)]
    sender: UnboundedSender<DeviceStateChange>,
    num_unregistered_device_events: Arc<AtomicU64>,
    pub(crate) device_infos:
        Arc<LockDepMutex<SortedVecMap<DeviceId, InputDeviceInfoHandle>, InputRelayDeviceInfosLock>>,
    pub(crate) display_size: Arc<LockDepMutex<Option<(i32, i32)>, InputRelayDisplaySizeLock>>,
}

impl InputEventsRelayHandle {
    /// Returns the number of events received for unregistered devices across all input streams.
    pub fn num_unregistered_device_events(&self) -> u64 {
        self.num_unregistered_device_events.load(Ordering::Relaxed)
    }

    /// Updates or inserts metadata (such as device name and input ID) for a uinput device.
    pub(crate) fn update_uinput_device(
        &self,
        device_id: DeviceId,
        input_id: uapi::input_id,
        name: String,
    ) {
        let mut device_infos = self.device_infos.lock();
        if let Some(info) = device_infos.get(&device_id) {
            *info.lock() = InputDeviceInfo { input_id, name };
        } else {
            device_infos.insert(device_id, InputDeviceInfo::new(input_id, name));
        }
    }

    /// Adds a touch device to the relay.
    ///
    /// # Precondition / Lifecycle
    /// Only call after [`InputEventsRelay::start_relays`].
    #[cfg(test)]
    pub fn add_touch_device(
        &self,
        device_id: DeviceId,
        open_files: OpenedFiles,
        inspect_status: Option<Arc<InputDeviceStatus>>,
    ) {
        let (sender, receiver) = oneshot::channel();
        let _ = self.sender.unbounded_send(DeviceStateChange::Add(
            device_id,
            Box::new(DeviceState::new_touch(device_id, open_files, inspect_status)),
            sender,
        ));
        let _ = block_on(receiver);
    }

    /// Adds a keyboard device to the relay.
    ///
    /// # Precondition / Lifecycle
    /// Only call after [`InputEventsRelay::start_relays`].
    #[cfg(test)]
    pub fn add_keyboard_device(
        &self,
        device_id: DeviceId,
        open_files: OpenedFiles,
        inspect_status: Option<Arc<InputDeviceStatus>>,
    ) {
        let (sender, receiver) = oneshot::channel();
        let _ = self.sender.unbounded_send(DeviceStateChange::Add(
            device_id,
            Box::new(DeviceState::new_keyboard(device_id, open_files, inspect_status)),
            sender,
        ));
        let _ = block_on(receiver);
    }
}

pub struct InputEventsRelay {
    devices: SortedVecMap<DeviceId, DeviceState>,
    #[cfg(test)]
    receiver: Option<UnboundedReceiver<DeviceStateChange>>,
    num_unregistered_device_events: Arc<AtomicU64>,
    /// Number of devices currently tracked by the relay, exposed via inspect.
    num_devices: Arc<AtomicU64>,
    device_listener_registered: Arc<AtomicU8>,
    _inspect_node: Option<fuchsia_inspect::Node>,
    device_infos:
        Arc<LockDepMutex<SortedVecMap<DeviceId, InputDeviceInfoHandle>, InputRelayDeviceInfosLock>>,
    display_size: Arc<LockDepMutex<Option<(i32, i32)>, InputRelayDisplaySizeLock>>,
}

impl InputEventsRelay {
    /// Updates the inspect-visible device count. Must be called after every mutation of
    /// `self.devices` once the relay has started.
    fn update_num_devices(&self) {
        self.num_devices.store(self.devices.len() as u64, Ordering::Relaxed);
    }

    /// Adds a touch device to the relay prior to starting the relay loop.
    #[cfg(test)]
    pub fn add_touch_device(
        &mut self,
        device_id: DeviceId,
        open_files: OpenedFiles,
        inspect_status: Option<Arc<InputDeviceStatus>>,
    ) {
        self.devices
            .insert(device_id, DeviceState::new_touch(device_id, open_files, inspect_status));
        self.update_num_devices();
    }

    /// Adds a keyboard device to the relay prior to starting the relay loop.
    #[cfg(test)]
    pub fn add_keyboard_device(
        &mut self,
        device_id: DeviceId,
        open_files: OpenedFiles,
        inspect_status: Option<Arc<InputDeviceStatus>>,
    ) {
        self.devices
            .insert(device_id, DeviceState::new_keyboard(device_id, open_files, inspect_status));
        self.update_num_devices();
    }

    /// Adds a mouse device to the relay prior to starting the relay loop.
    pub fn add_mouse_device(
        &mut self,
        device_id: DeviceId,
        open_files: OpenedFiles,
        inspect_status: Option<Arc<InputDeviceStatus>>,
    ) {
        self.devices
            .insert(device_id, DeviceState::new_mouse(device_id, open_files, inspect_status));
        self.update_num_devices();
    }

    /// Adds a pending mouse device to the relay prior to starting the relay loop.
    pub fn add_pending_mouse_device(
        &mut self,
        kernel: Arc<Kernel>,
        device: crate::InputDevice,
        device_id: DeviceId,
    ) {
        self.devices.insert(device_id, DeviceState::new_pending_mouse(kernel, device, device_id));
        self.update_num_devices();
    }

    fn record_inspect(&mut self, parent: &fuchsia_inspect::Node) {
        let inspect_node = parent.create_child("input_events_relay");
        let unregistered_counter = self.num_unregistered_device_events.clone();
        let num_devices = self.num_devices.clone();
        let device_listener_registered = self.device_listener_registered.clone();
        // `record_lazy_values` records an inline lazy node (LinkNodeDisposition::Inline),
        // exposing properties directly on `inspect_node` (input_events_relay).
        inspect_node.record_lazy_values("status", move || {
            let count = unregistered_counter.load(Ordering::Relaxed);
            let num_devices = num_devices.load(Ordering::Relaxed);
            let registration_state = device_listener_registered.load(Ordering::Relaxed);
            async move {
                let inspector = fuchsia_inspect::Inspector::default();
                inspector.root().record_uint("num_unregistered_device_events", count);
                inspector.root().record_uint("num_devices", num_devices);
                match registration_state {
                    DEVICE_LISTENER_REGISTERED => {
                        inspector.root().record_bool("device_listener_registered", true);
                    }
                    DEVICE_LISTENER_DISCONNECTED => {
                        inspector.root().record_bool("device_listener_registered", false);
                    }
                    _ => {}
                }
                Ok(inspector)
            }
            .boxed()
        });
        self._inspect_node = Some(inspect_node);
    }

    #[cfg(test)]
    pub fn with_inspect_node(mut self, parent: &fuchsia_inspect::Node) -> Self {
        self.record_inspect(parent);
        self
    }

    // TODO(https://fxbug.dev/371602479): Use `fuchsia.ui.SupportedInputDevices` to create
    // relays.
    // start_relays will take over the ownership of InputEventsRelay.
    pub fn start_relays(mut self: Self, kernel: &Kernel, args: StartRelaysArgs) {
        if self._inspect_node.is_none() {
            self.record_inspect(&kernel.inspect_node);
        }
        self.update_num_devices();

        let display_width = args.display_width;
        let display_height = args.display_height;
        let event_proxy_mode = args.event_proxy_mode;
        #[cfg(test)]
        let mut receiver = self.receiver.take().expect("start_relays called once");
        *self.display_size.lock() = if display_width > 0 && display_height > 0 {
            Some((display_width, display_height))
        } else {
            None
        };
        let f = async move |current_task: &CurrentTask| {
            let kernel = current_task.kernel();
            // touch
            let (touch_source_proxy, mut touch_waking_stream) =
                setup_touch_relay(kernel, event_proxy_mode, args.touch_source_client_end);

            // mouse
            let (mouse_source_proxy, mut mouse_waking_stream) =
                setup_mouse_relay(kernel, event_proxy_mode, args.mouse_source_client_end);

            // keyboard
            // `_keyboard_proxy` is load-bearing despite being unused: it holds the channel to
            // text_manager open for the lifetime of the relay loop below. Dropping it closes
            // the channel, which deregisters our `KeyboardListener` and silently stops all
            // key delivery. Do not remove it as an unused binding.
            let (mut keyboard_event_stream, _keyboard_proxy) =
                setup_keyboard_relay(args.keyboard_proxy, args.view_ref);

            // button
            let (mut media_buttons_waking_stream, mut touch_buttons_waking_stream) =
                setup_button_relay(kernel, args.registry_proxy, event_proxy_mode);

            let (mut device_listener_stream, existing_devices_iterator) =
                if let Some(registry) = args.device_listener_registry {
                    let (client_end, server_end) =
                        fidl::endpoints::create_endpoints::<fuiinput::DeviceListenerMarker>();
                    let stream = setup_device_listener_relay(kernel, server_end, event_proxy_mode);
                    match registry.register_listener(client_end, zx::MonotonicInstant::INFINITE) {
                        Ok(iterator) => {
                            self.device_listener_registered
                                .store(DEVICE_LISTENER_REGISTERED, Ordering::Relaxed);
                            (Some(stream), Some(iterator))
                        }
                        Err(e) => {
                            log_error!("Failed to register device listener: {:?}", e);
                            self.device_listener_registered
                                .store(DEVICE_LISTENER_DISCONNECTED, Ordering::Relaxed);
                            drop(stream);
                            (None, None)
                        }
                    }
                } else {
                    self.device_listener_registered
                        .store(DEVICE_LISTENER_DISCONNECTED, Ordering::Relaxed);
                    (None, None)
                };

            if let Some(iterator) = existing_devices_iterator {
                let iterator_proxy = iterator.into_proxy();
                loop {
                    match iterator_proxy.get_next().await {
                        Ok(devices) => {
                            if devices.is_empty() {
                                log_debug!("Finished getting existing devices (empty batch)");
                                break;
                            }
                            for device in devices {
                                match device.action {
                                    Some(fuiinput::Action::Added) => {
                                        let (Some(device_id), Some(descriptor)) =
                                            (device.device_id, device.descriptor)
                                        else {
                                            log_warn!(
                                                "DeviceEvent in existing devices iterator missing device_id or descriptor"
                                            );
                                            continue;
                                        };
                                        if let Err(e) = register_and_add_device(
                                            current_task,
                                            &mut self,
                                            device_id,
                                            descriptor,
                                            display_width,
                                            display_height,
                                        ) {
                                            log_warn!(
                                                "Failed to register existing device {}: {:?}",
                                                device_id,
                                                e
                                            );
                                        }
                                    }
                                    other => {
                                        log_warn!(
                                            "Unexpected action in existing devices iterator: {:?}",
                                            other
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            log_warn!("Error getting existing devices: {:?}", e);
                            break;
                        }
                    }
                }
            }

            let mut touch_future = touch_waking_stream.next().fuse();
            let mut mouse_future = mouse_waking_stream.next().fuse();
            let mut keyboard_future = keyboard_event_stream.next().fuse();
            let mut media_buttons_future = media_buttons_waking_stream.next().fuse();
            let mut touch_buttons_future = touch_buttons_waking_stream.next().fuse();
            #[cfg(test)]
            let mut receiver_future = receiver.next().fuse();
            #[cfg(not(test))]
            let mut receiver_future =
                futures::future::Fuse::<futures::future::Pending<()>>::terminated();
            let mut device_listener_future = futures::future::OptionFuture::from(
                device_listener_stream.as_mut().map(|s| s.next()),
            );

            let mut power_was_pressed = false;
            let mut function_was_pressed = false;
            let mut volume_up_was_pressed = false;
            let mut volume_down_was_pressed = false;
            let mut touch_buttons_were_pressed = new_touch_buttons_bitvec();

            loop {
                futures::select! {
                    touch_res = touch_future => {
                        match touch_res {
                            Some(Ok(fuipointer::TouchSourceV2Event::OnTouchEvents {
                                events,
                                last_event_stamp,
                            })) => {
                                self.process_touch_event(
                                    events,
                                );
                                if let Err(e) = touch_source_proxy.acknowledge_events(last_event_stamp) {
                                    log_warn!("error acknowledging touch events: {:?}", e);
                                }
                                drop(touch_future);
                                touch_future = touch_waking_stream.next().fuse();
                            }
                            Some(Ok(fuipointer::TouchSourceV2Event::_UnknownEvent { ordinal, .. })) => {
                                log_warn!("unknown event on TouchSourceV2: {}", ordinal);
                                drop(touch_future);
                                touch_future = touch_waking_stream.next().fuse();
                            }
                            Some(Err(e)) => {
                                log_warn!(
                                    "error {:?} reading from TouchSourceV2Proxy; input is stopped",
                                    e
                                );
                                touch_future = futures::future::Fuse::terminated();
                            }
                            None => {
                                touch_future = futures::future::Fuse::terminated();
                            }
                        }
                    }
                    mouse_res = mouse_future => {
                        match mouse_res {
                            Some(Ok(fuipointer::MouseSourceV2Event::OnMouseEvents {
                                events,
                                last_event_stamp,
                            })) => {
                                self.process_mouse_event(events);
                                if let Err(e) = mouse_source_proxy.acknowledge_events(last_event_stamp) {
                                    log_warn!("error acknowledging mouse events: {:?}", e);
                                }
                                drop(mouse_future);
                                mouse_future = mouse_waking_stream.next().fuse();
                            }
                            Some(Ok(fuipointer::MouseSourceV2Event::_UnknownEvent { ordinal, .. })) => {
                                log_warn!("unknown event on MouseSourceV2: {}", ordinal);
                                drop(mouse_future);
                                mouse_future = mouse_waking_stream.next().fuse();
                            }
                            Some(Err(e)) => {
                                log_warn!(
                                    "error {:?} reading from MouseSourceV2Proxy; input is stopped",
                                    e
                                );
                                mouse_future = futures::future::Fuse::terminated();
                            }
                            None => {
                                mouse_future = futures::future::Fuse::terminated();
                            }
                        }
                    }
                    media_buttons_res = media_buttons_future => {
                        match media_buttons_res {
                            Some(Ok(event)) => {
                                (
                                    power_was_pressed,
                                    function_was_pressed,
                                    volume_up_was_pressed,
                                    volume_down_was_pressed,
                                ) = self.process_media_button_event(
                                    event,
                                    power_was_pressed,
                                    function_was_pressed,
                                    volume_up_was_pressed,
                                    volume_down_was_pressed,
                                );
                                drop(media_buttons_future);
                                media_buttons_future = media_buttons_waking_stream.next().fuse();
                            }
                            _ => {
                                media_buttons_future = futures::future::Fuse::terminated();
                            }
                        }
                    }
                    touch_buttons_res = touch_buttons_future => {
                        match touch_buttons_res {
                            Some(Ok(event)) => {
                                touch_buttons_were_pressed = self.process_touch_button_event(
                                    event,
                                    &touch_buttons_were_pressed,
                                );
                                drop(touch_buttons_future);
                                touch_buttons_future = touch_buttons_waking_stream.next().fuse();
                            }
                            _ => {
                                touch_buttons_future = futures::future::Fuse::terminated();
                            }
                        }
                    }
                    device_listener_res = device_listener_future => {
                        match device_listener_res {
                            Some(Some(Ok(request))) => {
                                self.process_device_listener_request(
                                    current_task,
                                    request,
                                    display_width,
                                    display_height,
                                );
                                device_listener_future = futures::future::OptionFuture::from(
                                    device_listener_stream.as_mut().map(|s| s.next()),
                                );
                            }
                            Some(Some(Err(e))) => {
                                log_warn!("DeviceListener stream error: {:?}", e);
                                self.device_listener_registered
                                    .store(DEVICE_LISTENER_DISCONNECTED, Ordering::Relaxed);
                                device_listener_stream = None;
                                device_listener_future = futures::future::OptionFuture::from(None);
                            }
                            Some(None) => {
                                log_warn!("DeviceListener stream closed by peer");
                                self.device_listener_registered
                                    .store(DEVICE_LISTENER_DISCONNECTED, Ordering::Relaxed);
                                device_listener_stream = None;
                                device_listener_future = futures::future::OptionFuture::from(None);
                            }
                            None => {
                                self.device_listener_registered
                                    .store(DEVICE_LISTENER_DISCONNECTED, Ordering::Relaxed);
                                device_listener_stream = None;
                                device_listener_future = futures::future::OptionFuture::from(None);
                            }
                        }
                    }
                    keyboard_res = keyboard_future => {
                        match keyboard_res {
                            Some(Ok(request)) => {
                                self.process_keyboard(request);
                                drop(keyboard_future);
                                keyboard_future = keyboard_event_stream.next().fuse();
                            }
                            _ => {
                                keyboard_future = futures::future::Fuse::terminated();
                            }
                        }
                    }
                    e = receiver_future => {
                        #[cfg(test)]
                        match e {
                            Some(event) => {
                                match event {
                                    DeviceStateChange::Add(id, device_state, sender) => {
                                        self.devices.insert(id, *device_state);
                                        self.update_num_devices();
                                        let _ = sender.send(());
                                    }
                                }
                                drop(receiver_future);
                                receiver_future = receiver.next().fuse();
                            }
                            None => {
                                receiver_future = futures::future::Fuse::terminated();
                            }
                        }
                        #[cfg(not(test))]
                        let _ = e;
                    }
                    complete => break,
                }
            }
        };
        let req = SpawnRequestBuilder::new()
            .with_debug_name("input-event-relay")
            .with_role(INPUT_RELAY_ROLE_NAME)
            .with_async_closure(f)
            .build();
        kernel.kthreads.spawner().spawn_from_request(req);
    }

    fn get_device_mut(
        &mut self,
        device_id: Option<DeviceId>,
        default_id: DeviceId,
        predicate: impl Fn(&InputDeviceType) -> bool,
    ) -> Option<&mut DeviceState> {
        let target_device_id = device_id
            .filter(|id| self.devices.get(id).is_some_and(|d| predicate(&d.device_type)))
            .unwrap_or(default_id);
        self.devices.get_mut(&target_device_id).filter(|d| predicate(&d.device_type))
    }

    fn process_touch_event(self: &mut Self, touch_events: Vec<FidlTouchEvent>) {
        fuchsia_trace::duration!("input", "starnix_process_touch_event");
        for e in &touch_events {
            match e.trace_flow_id {
                Some(trace_flow_id) => {
                    fuchsia_trace::flow_end!(
                        "input",
                        "dispatch_event_to_client",
                        trace_flow_id.into()
                    );
                }
                None => {
                    log_warn!("touch event has no tracing id");
                }
            }
        }
        let num_received_events: u64 = touch_events.len().try_into().unwrap();

        let mut num_ignored_events: u64 = 0;

        // 1 vec may contains events from different device.
        let (events_by_device, ignored_events) = group_touch_events_by_device_id(touch_events);
        num_ignored_events += ignored_events;

        for (device_id, mut events) in events_by_device {
            fuchsia_trace::duration_begin!("input", "starnix_process_per_device_touch_event");

            // TODO(https://fxbug.dev/502662433): Default devices are no longer needed because
            // existing devices are enumerated via the DeviceIterator before start_relays().
            // Remove them in a follow-up change.
            let Some(dev) = self.get_device_mut(Some(device_id), DEFAULT_TOUCH_DEVICE_ID, |t| {
                matches!(t, InputDeviceType::Touch(_))
            }) else {
                fuchsia_trace::duration_end!("input", "starnix_process_per_device_touch_event");
                log_warn!(
                    "Received touch event for unregistered device {} and default touch device is missing",
                    device_id
                );
                self.num_unregistered_device_events
                    .fetch_add(events.len() as u64, Ordering::Relaxed);
                continue;
            };

            let mut num_converted_events: u64 = 0;
            let mut num_unexpected_events: u64 = 0;
            let mut new_events: VecDeque<uapi::input_event> = VecDeque::new();

            #[allow(clippy::collection_is_never_read)]
            let mut tracked_leases = vec![];
            for event in &mut events {
                if let Some(lease) = event.wake_lease.take() {
                    if let Some(status) = &dev.inspect_status {
                        tracked_leases.push(TrackedWakeLease::new(lease, status.clone()));
                    }
                }
            }

            let InputDeviceType::Touch(ref mut converter) = dev.device_type else { unreachable!() };
            let mut batch = converter.handle(events);
            new_events.append(&mut batch.events);
            num_converted_events += batch.count_converted_fidl_events;
            num_ignored_events += batch.count_ignored_fidl_events;
            num_unexpected_events += batch.count_unexpected_fidl_events;
            let last_event_time_ns = batch.last_event_time_ns;

            if let Some(dev_inspect_status) = &dev.inspect_status {
                dev_inspect_status.count_total_received_events(num_received_events);
                dev_inspect_status.count_total_ignored_events(num_ignored_events);
                dev_inspect_status.count_total_unexpected_events(num_unexpected_events);
                dev_inspect_status.count_total_converted_events(num_converted_events);
                dev_inspect_status.count_total_generated_events(
                    new_events.len().try_into().unwrap(),
                    last_event_time_ns,
                );
            } else {
                log_warn!(
                    "unable to record inspect for device_id: {}, device_type: {}",
                    device_id,
                    dev.device_type
                );
            }

            fuchsia_trace::duration_end!("input", "starnix_process_per_device_touch_event");
            dev.open_files.lock().retain(|f| {
                let Some(file) = f.upgrade() else {
                    log_warn!("Dropping input file for touch that failed to upgrade");
                    return false;
                };
                match &file.inspect_status {
                    Some(file_inspect_status) => {
                        file_inspect_status.count_received_events(num_received_events);
                        file_inspect_status.count_ignored_events(num_ignored_events);
                        file_inspect_status.count_unexpected_events(num_unexpected_events);
                        file_inspect_status.count_converted_events(num_converted_events);
                    }
                    None => {
                        log_warn!("unable to record inspect within the input file")
                    }
                }
                if !new_events.is_empty() {
                    // TODO(https://fxbug.dev/42075438): Reading from an `InputFile` should
                    // not provide access to events that occurred before the file was
                    // opened.
                    if let Some(file_inspect_status) = &file.inspect_status {
                        file_inspect_status.count_generated_events(
                            new_events.len().try_into().unwrap(),
                            last_event_time_ns,
                        );
                    }
                    file.add_events(new_events.clone().into_iter().collect());
                }

                true
            });
        }
    }

    fn process_keyboard(self: &mut Self, request: KeyboardListenerRequest) {
        match request {
            KeyboardListenerRequest::OnKeyEvent { event, responder } => {
                fuchsia_trace::duration!("input", "starnix_process_keyboard_event");

                let Some(dev) =
                    self.get_device_mut(event.device_id, DEFAULT_KEYBOARD_DEVICE_ID, |t| {
                        matches!(t, InputDeviceType::Keyboard)
                    })
                else {
                    log_warn!(
                        "Received key event for device {:?} but neither it nor default keyboard device is registered",
                        event.device_id
                    );
                    self.num_unregistered_device_events.fetch_add(1, Ordering::Relaxed);
                    let _ = responder.send(KeyEventStatus::NotHandled);
                    return;
                };

                let new_events = parse_fidl_keyboard_event_to_linux_input_event(
                    &event,
                    uinput::uinput_running(),
                );

                // These counters are denominated in FIDL events, not uapi events: the
                // documented invariant is received = ignored + unexpected + converted (see
                // `InputDeviceStatus`). One FIDL key event converts to several uapi events
                // (the key itself plus a SYN), so only the *generated* counters take
                // `new_events.len()`.
                let (converted_events, ignored_events, generated_events) = match new_events.len() {
                    0 => (0u64, 1u64, 0u64),
                    len => (1u64, 0u64, len as u64),
                };
                let last_time = event.timestamp.unwrap_or(0);

                if let Some(dev_inspect_status) = &dev.inspect_status {
                    dev_inspect_status.count_total_received_events(1);
                    dev_inspect_status.count_total_ignored_events(ignored_events);
                    dev_inspect_status.count_total_converted_events(converted_events);
                    // Guarded because `count_total_generated_events` *stores* the timestamp:
                    // calling it with a count of 0 would move
                    // `last_generated_uapi_event_timestamp_ns` on an event that generated
                    // nothing.
                    if generated_events > 0 {
                        dev_inspect_status
                            .count_total_generated_events(generated_events, last_time);
                    }
                } else {
                    log_warn!("unable to record inspect for keyboard device");
                }

                dev.open_files.lock().retain(|f| {
                    let Some(file) = f.upgrade() else {
                        log_warn!("Dropping input file for keyboard that failed to upgrade");
                        return false;
                    };
                    match &file.inspect_status {
                        Some(file_inspect_status) => {
                            file_inspect_status.count_received_events(1);
                            file_inspect_status.count_ignored_events(ignored_events);
                            file_inspect_status.count_converted_events(converted_events);
                            if generated_events > 0 {
                                file_inspect_status
                                    .count_generated_events(generated_events, last_time);
                            }
                        }
                        None => {
                            log_warn!("unable to record inspect within the input file")
                        }
                    }
                    if !new_events.is_empty() {
                        file.add_events(new_events.clone().into_iter().collect());
                    }

                    true
                });

                let _ = responder.send(KeyEventStatus::Handled);
            }
        }
    }

    fn process_device_listener_request(
        &mut self,
        current_task: &CurrentTask,
        request: fuiinput::DeviceListenerRequest,
        display_width: i32,
        display_height: i32,
    ) {
        match request {
            fuiinput::DeviceListenerRequest::OnDeviceChanged { event, control_handle: _ } => {
                let action = match event.action {
                    Some(fuiinput::Action::Added) => "add",
                    Some(fuiinput::Action::Removed) => "remove",
                    _ => "unknown",
                };
                fuchsia_trace::duration!(
                    "input",
                    "starnix_process_device_changed",
                    "action" => action
                );
                match event.action {
                    Some(fuiinput::Action::Added) => {
                        let (Some(device_id), Some(descriptor)) =
                            (event.device_id, event.descriptor)
                        else {
                            log_warn!("DeviceEvent missing device_id or descriptor");
                            return;
                        };
                        if let Err(e) = register_and_add_device(
                            current_task,
                            self,
                            device_id,
                            descriptor,
                            display_width,
                            display_height,
                        ) {
                            log_warn!("Failed to register dynamic device {}: {:?}", device_id, e);
                        }
                    }
                    Some(fuiinput::Action::Removed) => {
                        let Some(device_id) = event.device_id else {
                            log_warn!("DeviceEvent missing device_id");
                            return;
                        };
                        self.device_infos.lock().remove(&device_id);
                        if let Some(device_state) = self.devices.remove(&device_id) {
                            self.update_num_devices();
                            if let Some(starnix_device) = device_state.starnix_device {
                                InputDevice::unregister(current_task, starnix_device);
                            }
                        }
                    }
                    action => {
                        log_warn!("Unexpected action in DeviceListener event: {:?}", action);
                    }
                }
            }
        }
    }

    fn process_media_button_event(
        &mut self,
        button_event: fuipolicy::MediaButtonsListenerRequest,
        power_was_pressed: bool,
        function_was_pressed: bool,
        volume_up_was_pressed: bool,
        volume_down_was_pressed: bool,
    ) -> (bool, bool, bool, bool) {
        let mut power_was_pressed_after = power_was_pressed;
        let mut function_was_pressed_after = function_was_pressed;
        let mut volume_up_was_pressed_after = volume_up_was_pressed;
        let mut volume_down_was_pressed_after = volume_down_was_pressed;
        match button_event {
            fuipolicy::MediaButtonsListenerRequest::OnEvent { mut event, responder } => {
                if let Some(trace_flow_id) = event.trace_flow_id {
                    fuchsia_trace::flow_end!(
                        "input",
                        "dispatch_media_buttons_to_listeners",
                        trace_flow_id.into()
                    );
                }
                fuchsia_trace::duration!("input", "starnix_process_media_button_event");

                let Some(dev) =
                    self.get_device_mut(event.device_id, DEFAULT_KEYBOARD_DEVICE_ID, |t| {
                        matches!(t, InputDeviceType::Keyboard)
                    })
                else {
                    log_warn!(
                        "Received media button event for device {:?} but neither it nor default keyboard device is registered",
                        event.device_id
                    );
                    self.num_unregistered_device_events.fetch_add(1, Ordering::Relaxed);
                    let _ = responder.send();
                    return (
                        power_was_pressed,
                        function_was_pressed,
                        volume_up_was_pressed,
                        volume_down_was_pressed,
                    );
                };

                let batch = parse_fidl_media_button_event(
                    &event,
                    power_was_pressed,
                    function_was_pressed,
                    volume_up_was_pressed,
                    volume_down_was_pressed,
                );

                power_was_pressed_after = batch.power_is_pressed;
                function_was_pressed_after = batch.function_is_pressed;
                volume_up_was_pressed_after = batch.volume_up_is_pressed;
                volume_down_was_pressed_after = batch.volume_down_is_pressed;

                let (converted_events, ignored_events, generated_events) = match batch.events.len()
                {
                    0 => (0u64, 1u64, 0u64),
                    len => {
                        if len % 2 == 1 {
                            log_warn!(
                                "unexpectedly received {} events: there should always be an even number of non-empty events.",
                                len
                            );
                        }
                        (1u64, 0u64, len as u64)
                    }
                };

                #[allow(clippy::collection_is_never_read)]
                let mut tracked_leases = vec![];
                if let Some(lease) = event.wake_lease.take() {
                    if let Some(status) = &dev.inspect_status {
                        tracked_leases.push(TrackedWakeLease::new(lease, status.clone()));
                    }
                }

                if let Some(dev_inspect_status) = &dev.inspect_status {
                    dev_inspect_status.count_total_received_events(1);
                    dev_inspect_status.count_total_ignored_events(ignored_events);
                    dev_inspect_status.count_total_converted_events(converted_events);
                    dev_inspect_status.count_total_generated_events(
                        generated_events,
                        batch.event_time.into_nanos().try_into().unwrap(),
                    );
                } else {
                    log_warn!("unable to record inspect for button device");
                }

                dev.open_files.lock().retain(|f| {
                    let Some(file) = f.upgrade() else {
                        log_warn!("Dropping input file for buttons that failed to upgrade");
                        return false;
                    };
                    match &file.inspect_status {
                        Some(file_inspect_status) => {
                            file_inspect_status.count_received_events(1);
                            file_inspect_status.count_ignored_events(ignored_events);
                            file_inspect_status.count_converted_events(converted_events);
                        }
                        None => {
                            log_warn!("unable to record inspect within the input file")
                        }
                    }
                    if !batch.events.is_empty() {
                        if let Some(file_inspect_status) = &file.inspect_status {
                            file_inspect_status.count_generated_events(
                                generated_events,
                                batch.event_time.into_nanos().try_into().unwrap(),
                            );
                        }
                        file.add_events(batch.events.clone());
                    }

                    true
                });

                let _ = responder.send();
            }
            _ => { /* Ignore deprecated OnMediaButtonsEvent */ }
        }

        (
            power_was_pressed_after,
            function_was_pressed_after,
            volume_up_was_pressed_after,
            volume_down_was_pressed_after,
        )
    }

    fn process_touch_button_event(
        &mut self,
        button_event: fuipolicy::TouchButtonsListenerRequest,
        touch_buttons_were_pressed: &bit_vec::BitVec,
    ) -> bit_vec::BitVec {
        fuchsia_trace::duration!("input", "starnix_process_touch_button_event");
        match button_event {
            fuipolicy::TouchButtonsListenerRequest::OnEvent { mut event, responder } => {
                if let Some(trace_flow_id) = event.trace_flow_id {
                    fuchsia_trace::flow_end!(
                        "input",
                        "dispatch_touch_button_to_listeners",
                        trace_flow_id.into()
                    );
                }
                let device_id = match &event.device_info {
                    Some(TouchDeviceInfo { id: Some(id), .. }) => Some(*id),
                    _ => None,
                };

                let Some(dev) = self.get_device_mut(device_id, DEFAULT_TOUCH_DEVICE_ID, |t| {
                    matches!(t, InputDeviceType::Touch(_))
                }) else {
                    log_warn!(
                        "Received touch button event for device {:?} but neither it nor default touch device is registered",
                        device_id
                    );
                    self.num_unregistered_device_events.fetch_add(1, Ordering::Relaxed);
                    let _ = responder.send();
                    return touch_buttons_were_pressed.clone();
                };

                let batch = parse_fidl_touch_button_event(&event, touch_buttons_were_pressed);

                let (converted_events, ignored_events, generated_events) = match batch.events.len()
                {
                    0 => (0u64, 1u64, 0u64),
                    len => {
                        if len % 2 == 1 {
                            log_warn!(
                                "unexpectedly received {} events: there should always be an even number of non-empty events.",
                                len
                            );
                        }
                        (1u64, 0u64, len as u64)
                    }
                };

                #[allow(clippy::collection_is_never_read)]
                let mut tracked_leases = vec![];
                if let Some(lease) = event.wake_lease.take() {
                    if let Some(status) = &dev.inspect_status {
                        tracked_leases.push(TrackedWakeLease::new(lease, status.clone()));
                    }
                }

                if let Some(dev_inspect_status) = &dev.inspect_status {
                    dev_inspect_status.count_total_received_events(1);
                    dev_inspect_status.count_total_ignored_events(ignored_events);
                    dev_inspect_status.count_total_converted_events(converted_events);
                    dev_inspect_status.count_total_generated_events(
                        generated_events,
                        batch.event_time.into_nanos().try_into().unwrap(),
                    );
                } else {
                    log_warn!("unable to record inspect for touch device");
                }

                dev.open_files.lock().retain(|f| {
                    let Some(file) = f.upgrade() else {
                        log_warn!("Dropping input file for touch that failed to upgrade");
                        return false;
                    };
                    match &file.inspect_status {
                        Some(file_inspect_status) => {
                            file_inspect_status.count_received_events(1);
                            file_inspect_status.count_ignored_events(ignored_events);
                            file_inspect_status.count_converted_events(converted_events);
                        }
                        None => {
                            log_warn!("unable to record inspect within the input file")
                        }
                    }
                    if !batch.events.is_empty() {
                        if let Some(file_inspect_status) = &file.inspect_status {
                            file_inspect_status.count_generated_events(
                                generated_events,
                                batch.event_time.into_nanos().try_into().unwrap(),
                            );
                        }
                        file.add_events(batch.events.clone());
                    }

                    true
                });

                let _ = responder.send();

                batch.touch_buttons
            }
            fuipolicy::TouchButtonsListenerRequest::_UnknownMethod { ordinal, .. } => {
                log_warn!("Received an unknown method with ordinal {ordinal}");
                touch_buttons_were_pressed.clone()
            }
        }
    }

    fn process_mouse_event(self: &mut Self, mouse_events: Vec<FidlMouseEvent>) {
        fuchsia_trace::duration!("input", "starnix_process_mouse_event");
        for e in &mouse_events {
            if let Some(trace_flow_id) = e.trace_flow_id {
                fuchsia_trace::flow_end!("input", "dispatch_event_to_client", trace_flow_id.into());
            }
        }
        // TODO(https://fxbug.dev/563345995): `num_received_events` counts the whole
        // batch and `num_ignored_events` accumulates across devices, yet both are
        // recorded against every device in the loop below. With more than one mouse
        // device in use simultaneously these counters over-report and violate the
        // inspect invariants. Scope them per device before multi-mouse is supported.
        let num_received_events: u64 = mouse_events.len().try_into().unwrap();
        let mut num_ignored_events: u64 = 0;

        let (events_by_device, ignored_events) = group_mouse_events_by_device_id(mouse_events);
        num_ignored_events += ignored_events;

        for (device_id, mut events) in events_by_device {
            fuchsia_trace::duration_begin!("input", "starnix_process_per_device_mouse_event");

            let Some(dev) = self.get_device_mut(Some(device_id), DEFAULT_MOUSE_DEVICE_ID, |t| {
                matches!(t, InputDeviceType::Mouse(_))
            }) else {
                fuchsia_trace::duration_end!("input", "starnix_process_per_device_mouse_event");
                log_warn!(
                    "Received mouse event for unregistered device {} and default mouse device is missing",
                    device_id
                );
                self.num_unregistered_device_events
                    .fetch_add(events.len() as u64, Ordering::Relaxed);
                continue;
            };

            let mut num_converted_events: u64 = 0;
            let mut num_unexpected_events: u64 = 0;
            let mut new_events: VecDeque<uapi::input_event> = VecDeque::new();

            #[allow(clippy::collection_is_never_read)]
            let mut tracked_leases = vec![];
            for event in &mut events {
                if let Some(lease) = event.wake_lease.take() {
                    if let Some(status) = &dev.inspect_status {
                        tracked_leases.push(TrackedWakeLease::new(lease, status.clone()));
                    }
                }
            }

            let InputDeviceType::Mouse(ref mut converter) = dev.device_type else { unreachable!() };
            let mut batch = converter.handle(events);
            new_events.append(&mut batch.events);
            num_converted_events += batch.count_converted_events;
            num_ignored_events += batch.count_ignored_events;
            num_unexpected_events += batch.count_unexpected_events;
            let last_event_time_ns = batch.last_event_time_ns;

            if !new_events.is_empty() {
                dev.registration.ensure_registered();
            }

            if let Some(dev_inspect_status) = &dev.inspect_status {
                dev_inspect_status.count_total_received_events(num_received_events);
                dev_inspect_status.count_total_ignored_events(num_ignored_events);
                dev_inspect_status.count_total_unexpected_events(num_unexpected_events);
                dev_inspect_status.count_total_converted_events(num_converted_events);
                if !new_events.is_empty() {
                    dev_inspect_status.count_total_generated_events(
                        new_events.len().try_into().unwrap(),
                        last_event_time_ns,
                    );
                }
            } else {
                log_warn!(
                    "unable to record inspect for device_id: {}, device_type: {}",
                    device_id,
                    dev.device_type
                );
            }

            fuchsia_trace::duration_end!("input", "starnix_process_per_device_mouse_event");
            let mut open_files = dev.open_files.lock();
            if !open_files.has_been_opened && !new_events.is_empty() {
                open_files.buffered_events.extend(new_events.iter().copied());
            }
            open_files.retain(|f| {
                let Some(file) = f.upgrade() else {
                    log_warn!("Dropping input file for mouse that failed to upgrade");
                    return false;
                };
                if let Some(file_inspect_status) = &file.inspect_status {
                    file_inspect_status.count_received_events(num_received_events);
                    file_inspect_status.count_ignored_events(num_ignored_events);
                    file_inspect_status.count_unexpected_events(num_unexpected_events);
                    file_inspect_status.count_converted_events(num_converted_events);
                }
                if !new_events.is_empty() {
                    if let Some(file_inspect_status) = &file.inspect_status {
                        file_inspect_status.count_generated_events(
                            new_events.len().try_into().unwrap(),
                            last_event_time_ns,
                        );
                    }
                    file.add_events(new_events.clone().into_iter().collect());
                }
                true
            });
        }
    }
}

fn setup_touch_relay(
    kernel: &Arc<Kernel>,
    event_proxy_mode: EventProxyMode,
    touch_source_client_end: ClientEnd<fuipointer::TouchSourceV2Marker>,
) -> (fuipointer::TouchSourceV2Proxy, ContainerWakingStream<fuipointer::TouchSourceV2EventStream>) {
    let touch_counter_name = "touch";
    let (touch_source_proxy, counter) = match event_proxy_mode {
        EventProxyMode::WakeContainer => {
            // Proxy the touch events through the Starnix runner. This allows touch events to
            // wake the container when it is suspended.
            let (touch_source_channel, counter) = create_proxy_for_wake_events_counter(
                touch_source_client_end.into_channel(),
                touch_counter_name.to_string(),
            );
            (
                fuipointer::TouchSourceV2Proxy::new(fidl::AsyncChannel::from_channel(
                    touch_source_channel,
                )),
                Some(counter),
            )
        }
        EventProxyMode::None => (touch_source_client_end.into_proxy(), None),
    };
    let waking_stream = ContainerWakingStream::new(
        kernel.suspend_resume_manager.add_message_counter(touch_counter_name, counter),
        touch_source_proxy.take_event_stream(),
    );
    (touch_source_proxy, waking_stream)
}

fn setup_keyboard_relay(
    keyboard: KeyboardSynchronousProxy,
    view_ref: fuiviews::ViewRef,
) -> (KeyboardListenerRequestStream, KeyboardSynchronousProxy) {
    let (keyboard_listener, event_stream) =
        fidl::endpoints::create_request_stream::<KeyboardListenerMarker>();
    if let Err(e) =
        keyboard.add_listener(view_ref, keyboard_listener, zx::MonotonicInstant::INFINITE)
    {
        log_warn!("Could not register keyboard listener: {:?}", e);
    }

    (event_stream, keyboard)
}

fn setup_button_relay(
    kernel: &Arc<Kernel>,
    registry_proxy: fuipolicy::DeviceListenerRegistrySynchronousProxy,
    event_proxy_mode: EventProxyMode,
) -> (
    ContainerWakingStream<fuipolicy::MediaButtonsListenerRequestStream>,
    ContainerWakingStream<fuipolicy::TouchButtonsListenerRequestStream>,
) {
    let media_buttons_name = "media buttons";
    let touch_buttons_name = "touch buttons";

    let (remote_media_button_client, remote_media_button_server) =
        fidl::endpoints::create_endpoints::<fuipolicy::MediaButtonsListenerMarker>();
    if let Err(e) =
        registry_proxy.register_listener(remote_media_button_client, zx::MonotonicInstant::INFINITE)
    {
        log_warn!("Failed to register media buttons listener: {:?}", e);
    }

    let (remote_touch_button_client, remote_touch_button_server) =
        fidl::endpoints::create_endpoints::<fuipolicy::TouchButtonsListenerMarker>();
    if let Err(e) = registry_proxy
        .register_touch_buttons_listener(remote_touch_button_client, zx::MonotonicInstant::INFINITE)
    {
        log_warn!("Failed to register touch buttons listener: {:?}", e);
    }

    let (
        local_media_buttons_listener_stream,
        media_buttons_counter,
        local_touch_buttons_listener_stream,
        touch_buttons_counter,
    ) = match event_proxy_mode {
        EventProxyMode::WakeContainer => {
            let (local_media_buttons_channel, media_buttons_counter) =
                create_proxy_for_wake_events_counter(
                    remote_media_button_server.into_channel(),
                    media_buttons_name.to_string(),
                );
            let local_media_buttons_listener_stream =
                fuipolicy::MediaButtonsListenerRequestStream::from_channel(
                    fidl::AsyncChannel::from_channel(local_media_buttons_channel),
                );

            let (local_touch_buttons_channel, touch_buttons_counter) =
                create_proxy_for_wake_events_counter(
                    remote_touch_button_server.into_channel(),
                    touch_buttons_name.to_string(),
                );
            let local_touch_buttons_listener_stream =
                fuipolicy::TouchButtonsListenerRequestStream::from_channel(
                    fidl::AsyncChannel::from_channel(local_touch_buttons_channel),
                );
            (
                local_media_buttons_listener_stream,
                Some(media_buttons_counter),
                local_touch_buttons_listener_stream,
                Some(touch_buttons_counter),
            )
        }
        EventProxyMode::None => (
            remote_media_button_server.into_stream(),
            None,
            remote_touch_button_server.into_stream(),
            None,
        ),
    };

    (
        ContainerWakingStream::new(
            kernel
                .suspend_resume_manager
                .add_message_counter(media_buttons_name, media_buttons_counter),
            local_media_buttons_listener_stream,
        ),
        ContainerWakingStream::new(
            kernel
                .suspend_resume_manager
                .add_message_counter(touch_buttons_name, touch_buttons_counter),
            local_touch_buttons_listener_stream,
        ),
    )
}

fn setup_device_listener_relay(
    kernel: &Arc<Kernel>,
    device_listener_server: ServerEnd<fuiinput::DeviceListenerMarker>,
    event_proxy_mode: EventProxyMode,
) -> ContainerWakingStream<fuiinput::DeviceListenerRequestStream> {
    let device_listener_name = "device listener";

    let (local_device_listener_stream, device_listener_counter) = match event_proxy_mode {
        EventProxyMode::WakeContainer => {
            let (local_device_listener_channel, device_listener_counter) =
                create_proxy_for_wake_events_counter(
                    device_listener_server.into_channel(),
                    device_listener_name.to_string(),
                );
            let local_device_listener_stream = fuiinput::DeviceListenerRequestStream::from_channel(
                fidl::AsyncChannel::from_channel(local_device_listener_channel),
            );
            (local_device_listener_stream, Some(device_listener_counter))
        }
        EventProxyMode::None => (device_listener_server.into_stream(), None),
    };

    ContainerWakingStream::new(
        kernel
            .suspend_resume_manager
            .add_message_counter(device_listener_name, device_listener_counter),
        local_device_listener_stream,
    )
}

fn parse_input_id(
    info: Option<&fuiinput::DeviceInformation>,
    fallback: uapi::input_id,
) -> uapi::input_id {
    if let Some(info) = info {
        uapi::input_id {
            bustype: fallback.bustype,
            vendor: info.vendor_id.filter(|&id| id != 0).unwrap_or(fallback.vendor as u32) as u16,
            product: info.product_id.filter(|&id| id != 0).unwrap_or(fallback.product as u32)
                as u16,
            version: info.version.unwrap_or(fallback.version as u32) as u16,
        }
    } else {
        fallback
    }
}

fn register_and_add_device(
    current_task: &CurrentTask,
    devices_relay: &mut InputEventsRelay,
    device_id: u32,
    descriptor: fuiinput::DeviceDescriptor,
    display_width: i32,
    display_height: i32,
) -> Result<(), Errno> {
    let kernel = current_task.kernel();

    // Composite devices are registered under their primary function only
    // (priority: touch > keyboard/consumer control > mouse).
    let (default_name, default_input_id, device_type) = if let Some(_touch) = &descriptor.touch {
        let name = descriptor
            .device_information
            .as_ref()
            .and_then(|info| info.product_name.clone())
            .unwrap_or_else(|| "starnix_touch".to_string());
        let input_id = parse_input_id(descriptor.device_information.as_ref(), TOUCH_INPUT_ID);
        (
            name,
            input_id,
            InputDeviceType::Touch(FuchsiaTouchEventToLinuxTouchEventConverter::create()),
        )
    } else if descriptor.keyboard.is_some() || descriptor.consumer_control.is_some() {
        let name = descriptor
            .device_information
            .as_ref()
            .and_then(|info| info.product_name.clone())
            .unwrap_or_else(|| "starnix_buttons".to_string());
        let input_id = parse_input_id(descriptor.device_information.as_ref(), KEYBOARD_INPUT_ID);
        (name, input_id, InputDeviceType::Keyboard)
    } else if descriptor.mouse.is_some() {
        let name = descriptor
            .device_information
            .as_ref()
            .and_then(|info| info.product_name.clone())
            .unwrap_or_else(|| "starnix_mouse".to_string());
        let input_id = parse_input_id(descriptor.device_information.as_ref(), MOUSE_INPUT_ID);
        (
            name,
            input_id,
            InputDeviceType::Mouse(FuchsiaMouseEventToLinuxMouseEventConverter::create()),
        )
    } else {
        return error!(ENOTSUP);
    };
    let info = {
        let mut device_infos = devices_relay.device_infos.lock();
        if let Some(info) = device_infos.get(&device_id) {
            info.clone()
        } else {
            let info = InputDeviceInfo::new(default_input_id, default_name);
            device_infos.insert(device_id, info.clone());
            info
        }
    };

    let input_device = match &device_type {
        InputDeviceType::Touch(_) => InputDevice::new_touch(
            display_width,
            display_height,
            info,
            &format!("touch_device_{}", device_id),
            &kernel.inspect_node,
        ),
        InputDeviceType::Keyboard => InputDevice::new_keyboard(
            info,
            &format!("keyboard_device_{}", device_id),
            &kernel.inspect_node,
        ),
        InputDeviceType::Mouse(_) => InputDevice::new_mouse(
            info,
            &format!("mouse_device_{}", device_id),
            &kernel.inspect_node,
        ),
    };

    if let Some(old_state) = devices_relay.devices.remove(&device_id) {
        devices_relay.update_num_devices();
        if let Some(old_starnix_device) = old_state.starnix_device {
            InputDevice::unregister(current_task, old_starnix_device);
        }
    }

    // Allocate the lowest available minor number for this device.
    // Note: `DeviceRegistry` does not provide dynamic minor allocation or recycling for
    // `INPUT_MAJOR`, so minors are tracked and recycled locally here.
    let minor = (0u32..)
        .find(|&m| !devices_relay.devices.values().any(|s| s.minor == m))
        .expect("failed to allocate minor number");

    let starnix_device = input_device.clone().register(kernel, minor)?;
    let device_state = DeviceState {
        device_type,
        open_files: input_device.open_files,
        inspect_status: Some(input_device.inspect_status),
        registration: DeviceRegistration::Registered,
        starnix_device: Some(starnix_device),
        minor,
    };
    devices_relay.devices.insert(device_id, device_state);
    devices_relay.update_num_devices();
    Ok(())
}

fn setup_mouse_relay(
    kernel: &Arc<Kernel>,
    event_proxy_mode: EventProxyMode,
    mouse_source_client_end: ClientEnd<fuipointer::MouseSourceV2Marker>,
) -> (fuipointer::MouseSourceV2Proxy, ContainerWakingStream<fuipointer::MouseSourceV2EventStream>) {
    let mouse_counter_name = "mouse";
    let (mouse_source_proxy, counter) = match event_proxy_mode {
        EventProxyMode::WakeContainer => {
            // Proxy the mouse events through the Starnix runner. This allows mouse events to
            // wake the container when it is suspended.
            let (mouse_source_channel, resume_event) = create_proxy_for_wake_events_counter(
                mouse_source_client_end.into_channel(),
                "mouse".to_string(),
            );
            (
                fuipointer::MouseSourceV2Proxy::new(fidl::AsyncChannel::from_channel(
                    mouse_source_channel,
                )),
                Some(resume_event),
            )
        }
        EventProxyMode::None => (mouse_source_client_end.into_proxy(), None),
    };

    let waking_stream = ContainerWakingStream::new(
        kernel.suspend_resume_manager.add_message_counter(mouse_counter_name, counter),
        mouse_source_proxy.take_event_stream(),
    );

    (mouse_source_proxy, waking_stream)
}

fn group_mouse_events_by_device_id(
    events: Vec<FidlMouseEvent>,
) -> (SortedVecMap<DeviceId, Vec<FidlMouseEvent>>, u64) {
    let mut events_by_device: SortedVecMap<u32, Vec<FidlMouseEvent>> = SortedVecMap::new();
    let mut ignored_events: u64 = 0;
    for e in events {
        match e {
            FidlMouseEvent { pointer_sample: Some(ref sample), .. } => {
                let id = sample.device_id.unwrap_or(DEFAULT_MOUSE_DEVICE_ID);
                if let Some(vec) = events_by_device.get_mut(&id) {
                    vec.push(e);
                } else {
                    events_by_device.insert(id, vec![e]);
                }
            }
            _ => {
                ignored_events += 1;
            }
        }
    }

    (events_by_device, ignored_events)
}

fn group_touch_events_by_device_id(
    events: Vec<FidlTouchEvent>,
) -> (SortedVecMap<DeviceId, Vec<FidlTouchEvent>>, u64) {
    let mut events_by_device: SortedVecMap<u32, Vec<FidlTouchEvent>> = SortedVecMap::new();
    let mut ignored_events: u64 = 0;
    for e in events {
        match e {
            FidlTouchEvent {
                pointer_sample: Some(TouchPointerSample { interaction: Some(id), .. }),
                ..
            } => {
                if let Some(vec) = events_by_device.get_mut(&id.device_id) {
                    vec.push(e);
                } else {
                    events_by_device.insert(id.device_id, vec![e]);
                }
            }
            _ => {
                ignored_events += 1;
            }
        }
    }

    (events_by_device, ignored_events)
}

#[cfg(test)]
pub async fn start_input_relays_for_test(
    current_task: &starnix_core::task::CurrentTask,
    event_proxy_mode: EventProxyMode,
) -> (
    Arc<InputEventsRelayHandle>,
    crate::InputDevice,
    crate::InputDevice,
    crate::InputDevice,
    starnix_core::vfs::FileHandle,
    starnix_core::vfs::FileHandle,
    starnix_core::vfs::FileHandle,
    fuipointer::TouchSourceV2RequestStream,
    fuipointer::MouseSourceV2RequestStream,
    fidl_fuchsia_ui_input3::KeyboardListenerProxy,
    fuipolicy::MediaButtonsListenerProxy,
    fuipolicy::TouchButtonsListenerProxy,
) {
    let inspector = fuchsia_inspect::Inspector::default();

    let touch_device = crate::InputDevice::new_touch(
        700,
        1200,
        crate::InputDeviceInfo::new(TOUCH_INPUT_ID, "starnix_touch".to_string()),
        "touch_device",
        inspector.root(),
    );
    let touch_file = touch_device.open_test(current_task).expect("Failed to create input file");

    let keyboard_device = crate::InputDevice::new_keyboard(
        crate::InputDeviceInfo::new(KEYBOARD_INPUT_ID, "starnix_buttons".to_string()),
        "keyboard_device",
        inspector.root(),
    );
    let keyboard_file =
        keyboard_device.open_test(current_task).expect("Failed to create input file");

    let mouse_device = crate::InputDevice::new_mouse(
        crate::InputDeviceInfo::new(MOUSE_INPUT_ID, "starnix_mouse".to_string()),
        "mouse_device",
        inspector.root(),
    );
    let mouse_file = mouse_device.open_test(current_task).expect("Failed to create input file");

    let (touch_source_client_end, touch_source_stream) =
        fidl::endpoints::create_request_stream::<fuipointer::TouchSourceV2Marker>();
    let (mouse_source_client_end, mouse_stream) =
        fidl::endpoints::create_request_stream::<fuipointer::MouseSourceV2Marker>();
    let (keyboard_proxy, mut keyboard_stream) =
        fidl::endpoints::create_sync_proxy_and_stream::<fidl_fuchsia_ui_input3::KeyboardMarker>();
    let view_ref_pair = fuchsia_scenic::ViewRefPair::new().expect("Failed to create ViewRefPair");
    let (device_registry_proxy, mut device_listener_stream) =
        fidl::endpoints::create_sync_proxy_and_stream::<fuipolicy::DeviceListenerRegistryMarker>();

    let (mut relay, relay_handle) = new_input_relay();
    // TODO(https://fxbug.dev/502662433): Default devices are no longer needed because
    // existing devices are enumerated via the DeviceIterator before start_relays().
    // Remove them in a follow-up change.
    relay.add_touch_device(
        DEFAULT_TOUCH_DEVICE_ID,
        touch_device.open_files.clone(),
        Some(touch_device.inspect_status.clone()),
    );
    relay.add_keyboard_device(
        DEFAULT_KEYBOARD_DEVICE_ID,
        keyboard_device.open_files.clone(),
        Some(keyboard_device.inspect_status.clone()),
    );
    relay.add_mouse_device(
        DEFAULT_MOUSE_DEVICE_ID,
        mouse_device.open_files.clone(),
        Some(mouse_device.inspect_status.clone()),
    );
    relay.start_relays(
        &current_task.kernel(),
        StartRelaysArgs {
            event_proxy_mode,
            touch_source_client_end,
            keyboard_proxy,
            mouse_source_client_end,
            view_ref: view_ref_pair.view_ref,
            registry_proxy: device_registry_proxy,
            device_listener_registry: None,
            display_width: 700,
            display_height: 1200,
        },
    );

    let keyboard_listener = match keyboard_stream.next().await {
        Some(Ok(fidl_fuchsia_ui_input3::KeyboardRequest::AddListener {
            view_ref: _,
            listener,
            responder,
        })) => {
            let _ = responder.send();
            listener.into_proxy()
        }
        _ => {
            panic!("Failed to get event");
        }
    };

    let media_buttons_listener = match device_listener_stream.next().await {
        Some(Ok(fuipolicy::DeviceListenerRegistryRequest::RegisterListener {
            listener,
            responder,
        })) => {
            let _ = responder.send();
            listener.into_proxy()
        }
        _ => {
            panic!("Failed to get event");
        }
    };

    let touch_buttons_listener = match device_listener_stream.next().await {
        Some(Ok(fuipolicy::DeviceListenerRegistryRequest::RegisterTouchButtonsListener {
            listener,
            responder,
        })) => {
            let _ = responder.send();
            listener.into_proxy()
        }
        _ => {
            panic!("Failed to get event");
        }
    };

    (
        relay_handle,
        touch_device,
        keyboard_device,
        mouse_device,
        touch_file,
        keyboard_file,
        mouse_file,
        touch_source_stream,
        mouse_stream,
        keyboard_listener,
        media_buttons_listener,
        touch_buttons_listener,
    )
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::anyhow;
    use diagnostics_assertions::assert_data_tree;
    use fidl_fuchsia_ui_input::{
        MediaButtonsEvent, TouchButton, TouchButtonsEvent, TouchDeviceInfo,
    };
    use fidl_fuchsia_ui_input3 as fuiinput;
    use fuipointer::{
        EventPhase, MouseEvent, MousePointerSample, TouchEvent, TouchInteractionId,
        TouchPointerSample, TouchSourceV2Request, TouchSourceV2RequestStream,
    };
    use starnix_core::task::CurrentTask;
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_core::vfs::{FileHandle, FileObject, VecOutputBuffer};

    use starnix_types::time::timeval_from_time;
    use starnix_uapi::errors::{EAGAIN, Errno};
    use starnix_uapi::input_id;
    use starnix_uapi::open_flags::OpenFlags;
    use zerocopy::FromBytes as _;

    const INPUT_EVENT_SIZE: usize = std::mem::size_of::<uapi::input_event>();

    // Sends `touch_events` to the client stream and waits for `AcknowledgeEvents`.
    async fn answer_next_touch_watch_request(
        request_stream: &mut TouchSourceV2RequestStream,
        touch_events: Vec<TouchEvent>,
    ) {
        let control_handle = request_stream.control_handle();
        control_handle
            .send_on_touch_events(touch_events, 1)
            .expect("failure sending OnTouchEvents");
        match request_stream.next().await {
            Some(Ok(TouchSourceV2Request::AcknowledgeEvents { .. })) => {}
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }
    }

    // Sends `mouse_events` to the client stream and waits for `AcknowledgeEvents`.
    async fn answer_next_mouse_watch_request(
        request_stream: &mut fuipointer::MouseSourceV2RequestStream,
        mouse_events: Vec<MouseEvent>,
    ) {
        let control_handle = request_stream.control_handle();
        control_handle
            .send_on_mouse_events(mouse_events, 1)
            .expect("failure sending OnMouseEvents");
        match request_stream.next().await {
            Some(Ok(fuipointer::MouseSourceV2Request::AcknowledgeEvents { .. })) => {}
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }
    }

    fn make_empty_touch_event(device_id: u32) -> TouchEvent {
        TouchEvent {
            pointer_sample: Some(TouchPointerSample {
                interaction: Some(TouchInteractionId {
                    pointer_id: 0,
                    device_id,
                    interaction_id: 0,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn make_touch_event_with_phase_device_id(
        phase: EventPhase,
        pointer_id: u32,
        device_id: u32,
    ) -> TouchEvent {
        make_touch_event_with_phase_device_id_position(phase, pointer_id, device_id, 0.0, 0.0)
    }

    fn make_touch_event_with_phase_device_id_position(
        phase: EventPhase,
        pointer_id: u32,
        device_id: u32,
        x: f32,
        y: f32,
    ) -> TouchEvent {
        TouchEvent {
            timestamp: Some(0),
            pointer_sample: Some(TouchPointerSample {
                position_in_viewport: Some([x, y]),
                phase: Some(phase),
                interaction: Some(TouchInteractionId { pointer_id, device_id, interaction_id: 0 }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn make_mouse_wheel_event(scroll_v_ticks: i64, device_id: u32) -> MouseEvent {
        MouseEvent {
            timestamp: Some(0),
            pointer_sample: Some(MousePointerSample {
                device_id: Some(device_id),
                scroll_v: Some(scroll_v_ticks),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn read_uapi_events(file: &FileHandle, current_task: &CurrentTask) -> Vec<uapi::input_event> {
        std::iter::from_fn(|| {
            let mut event_bytes = VecOutputBuffer::new(INPUT_EVENT_SIZE);
            match file.read(current_task, &mut event_bytes) {
                Ok(INPUT_EVENT_SIZE) => Some(
                    uapi::input_event::read_from_bytes(Vec::from(event_bytes).as_slice())
                        .map_err(|_| anyhow!("failed to read input_event from buffer")),
                ),
                Ok(other_size) => {
                    Some(Err(anyhow!("got {} bytes (expected {})", other_size, INPUT_EVENT_SIZE)))
                }
                Err(Errno { code: EAGAIN, .. }) => None,
                Err(other_error) => Some(Err(anyhow!("read failed: {:?}", other_error))),
            }
        })
        .enumerate()
        .map(|(i, read_res)| match read_res {
            Ok(event) => event,
            Err(e) => panic!("unexpected result {:?} on iteration {}", e, i),
        })
        .collect()
    }

    fn create_test_touch_device(
        current_task: &CurrentTask,
        input_relay: Arc<InputEventsRelayHandle>,
        device_id: u32,
    ) -> FileHandle {
        let open_files: OpenedFiles = Default::default();
        input_relay.add_touch_device(device_id, open_files.clone(), None);
        let inspector = fuchsia_inspect::Inspector::default();
        let device_file = Arc::new(InputFile::new_touch(
            InputDeviceInfo::new(
                input_id { bustype: 0, vendor: 0, product: 0, version: 0 },
                "touch_device".to_string(),
            ),
            1000,
            1000,
            inspector.root(),
        ));
        open_files.lock().push(Arc::downgrade(&device_file));

        let root_namespace_node = current_task
            .lookup_path_from_root(".".into())
            .expect("failed to get namespace node for root");

        FileObject::new(
            &current_task,
            Box::new(crate::input_file::ArcInputFile(device_file)),
            root_namespace_node,
            OpenFlags::empty(),
        )
        .expect("FileObject::new failed")
    }

    fn make_uapi_input_event(ty: u32, code: u32, value: i32) -> uapi::input_event {
        uapi::input_event {
            time: timeval_from_time(zx::MonotonicInstant::from_nanos(0)),
            type_: ty as u16,
            code: code as u16,
            value,
        }
    }

    #[::fuchsia::test]
    async fn route_touch_event_by_device_id() {
        spawn_kernel_and_run(async move |current_task| {
            // Set up resources.

            let (
                input_relay,
                _touch_device,
                _keyboard_device,
                _mouse_device,
                input_file,
                _keyboard_file,
                _mouse_file,
                mut touch_source_stream,
                _mouse_source_stream,
                _keyboard_listener,
                _media_buttons_listener,
                _touch_buttons_listener,
            ) = start_input_relays_for_test(&current_task, EventProxyMode::None).await;

            const DEVICE_ID: u32 = 10;

            answer_next_touch_watch_request(
                &mut touch_source_stream,
                vec![make_touch_event_with_phase_device_id(EventPhase::Add, 1, DEVICE_ID)],
            )
            .await;

            // Wait for another `Watch` to ensure input_file done processing the first reply.
            // Use an empty `TouchEvent`, to minimize the chance that this event creates unexpected
            // `uapi::input_event`s.
            answer_next_touch_watch_request(
                &mut touch_source_stream,
                vec![make_empty_touch_event(DEVICE_ID)],
            )
            .await;

            // Consume all of the `uapi::input_event`s that are available.
            let events = read_uapi_events(&input_file, &current_task);
            // Default device should receive events because device id 10 falls back to default.
            assert_ne!(events.len(), 0);
            assert_eq!(input_relay.num_unregistered_device_events(), 0);

            // add a device, mock uinput.
            let device_id_10_file =
                create_test_touch_device(&current_task, input_relay.clone(), DEVICE_ID);

            answer_next_touch_watch_request(
                &mut touch_source_stream,
                vec![make_touch_event_with_phase_device_id(EventPhase::Add, 1, DEVICE_ID)],
            )
            .await;

            answer_next_touch_watch_request(
                &mut touch_source_stream,
                vec![make_empty_touch_event(DEVICE_ID)],
            )
            .await;

            let events = read_uapi_events(&input_file, &current_task);
            // Default device should not receive events because they matched device id 10.
            assert_eq!(events.len(), 0);

            let events = read_uapi_events(&device_id_10_file, &current_task);
            // file of device id 10 should receive events.
            assert_ne!(events.len(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn route_touch_event_with_wake_lease() {
        spawn_kernel_and_run(async move |current_task| {
            let (
                _input_relay,
                touch_device,
                _keyboard_device,
                _mouse_device,
                _input_file,
                _keyboard_file,
                _mouse_file,
                mut touch_source_stream,
                _mouse_source_stream,
                _keyboard_listener,
                _media_buttons_listener,
                _touch_buttons_listener,
            ) = start_input_relays_for_test(&current_task, EventProxyMode::None).await;

            const DEVICE_ID: u32 = DEFAULT_TOUCH_DEVICE_ID;
            let mut event = make_touch_event_with_phase_device_id(EventPhase::Add, 1, DEVICE_ID);
            let (p1, _p2) = fidl::EventPair::create();
            event.wake_lease = Some(p1);

            answer_next_touch_watch_request(&mut touch_source_stream, vec![event]).await;

            // Wait for another `Watch` to ensure input_file done processing the first reply.
            answer_next_touch_watch_request(
                &mut touch_source_stream,
                vec![make_empty_touch_event(DEVICE_ID)],
            )
            .await;

            let status = &touch_device.inspect_status;
            assert_eq!(
                status
                    .total_events_with_wake_lease_count
                    .load(std::sync::atomic::Ordering::Relaxed),
                1
            );
            assert_eq!(
                status.active_wake_leases_count.load(std::sync::atomic::Ordering::Relaxed),
                0
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn route_touch_event_by_device_id_multi_device_events_in_one_sequence() {
        spawn_kernel_and_run(async move |current_task| {
            let (
                input_relay,
                _touch_device,
                _keyboard_device,
                _mouse_device,
                _input_file,
                _keyboard_file,
                _mouse_file,
                mut touch_source_stream,
                _mouse_source_stream,
                _keyboard_listener,
                _media_buttons_listener,
                _touch_buttons_listener,
            ) = start_input_relays_for_test(&current_task, EventProxyMode::None).await;

            const DEVICE_ID_10: u32 = 10;
            const DEVICE_ID_11: u32 = 11;

            let device_id_10_file =
                create_test_touch_device(&current_task, input_relay.clone(), DEVICE_ID_10);

            let device_id_11_file =
                create_test_touch_device(&current_task, input_relay.clone(), DEVICE_ID_11);

            // 2 pointer down on different touch device.
            answer_next_touch_watch_request(
                &mut touch_source_stream,
                vec![
                    make_touch_event_with_phase_device_id_position(
                        EventPhase::Add,
                        1,
                        DEVICE_ID_10,
                        10.0,
                        20.0,
                    ),
                    make_touch_event_with_phase_device_id_position(
                        EventPhase::Add,
                        2,
                        DEVICE_ID_11,
                        30.0,
                        40.0,
                    ),
                ],
            )
            .await;

            answer_next_touch_watch_request(&mut touch_source_stream, vec![]).await;

            let events_10 = read_uapi_events(&device_id_10_file, &current_task);
            let events_11 = read_uapi_events(&device_id_11_file, &current_task);
            assert_eq!(events_10.len(), events_11.len());

            assert_eq!(
                events_10,
                vec![
                    make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_SLOT, 0),
                    make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_TRACKING_ID, 1),
                    make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_X, 10),
                    make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_Y, 20),
                    make_uapi_input_event(uapi::EV_KEY, uapi::BTN_TOUCH, 1),
                    make_uapi_input_event(uapi::EV_SYN, uapi::SYN_REPORT, 0),
                ]
            );

            assert_eq!(
                events_11,
                vec![
                    make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_SLOT, 0),
                    make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_TRACKING_ID, 2),
                    make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_X, 30),
                    make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_Y, 40),
                    make_uapi_input_event(uapi::EV_KEY, uapi::BTN_TOUCH, 1),
                    make_uapi_input_event(uapi::EV_SYN, uapi::SYN_REPORT, 0),
                ]
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn route_key_event_by_device_id() {
        spawn_kernel_and_run(async move |current_task| {
            // Set up resources.

            let (
                input_relay,
                _touch_device,
                _keyboard_device,
                _mouse_device,
                _touch_file,
                keyboard_file,
                _mouse_file,
                _touch_source_stream,
                _mouse_source_stream,
                keyboard_listener,
                _media_buttons_listener,
                _touch_buttons_listener,
            ) = start_input_relays_for_test(&current_task, EventProxyMode::None).await;

            const DEVICE_ID: u32 = 10;

            let key_event = fuiinput::KeyEvent {
                timestamp: Some(0),
                type_: Some(fuiinput::KeyEventType::Pressed),
                key: Some(fidl_fuchsia_input::Key::A),
                device_id: Some(DEVICE_ID),
                ..Default::default()
            };

            let _ = keyboard_listener.on_key_event(&key_event).await;

            let events = read_uapi_events(&keyboard_file, &current_task);
            // Default device should receive events because device id 10 falls back to default.
            assert_ne!(events.len(), 0);
            assert_eq!(input_relay.num_unregistered_device_events(), 0);

            // add a device, mock uinput.
            let open_files: OpenedFiles = Default::default();
            input_relay.add_keyboard_device(DEVICE_ID, open_files.clone(), None);
            let inspector = fuchsia_inspect::Inspector::default();
            let device_id_10_file = Arc::new(InputFile::new_keyboard(
                InputDeviceInfo::new(
                    input_id { bustype: 0, vendor: 0, product: 0, version: 0 },
                    "keyboard_10".to_string(),
                ),
                inspector.root(),
            ));
            open_files.lock().push(Arc::downgrade(&device_id_10_file));
            let root_namespace_node = current_task
                .lookup_path_from_root(".".into())
                .expect("failed to get namespace node for root");
            let device_id_10_file_object = FileObject::new(
                &current_task,
                Box::new(crate::input_file::ArcInputFile(device_id_10_file)),
                root_namespace_node,
                OpenFlags::empty(),
            )
            .expect("FileObject::new failed");

            let _ = keyboard_listener.on_key_event(&key_event).await;

            let events = read_uapi_events(&keyboard_file, &current_task);
            // Default device should not receive events because they matched device id 10.
            assert_eq!(events.len(), 0);

            let events = read_uapi_events(&device_id_10_file_object, &current_task);
            // file of device id 10 should receive events.
            assert_ne!(events.len(), 0);

            std::mem::drop(keyboard_listener); // Close Zircon channel.
        })
        .await;
    }

    #[::fuchsia::test]
    async fn route_media_button_event_by_device_id() {
        spawn_kernel_and_run(async move |current_task| {
            // Set up resources.

            let (
                input_relay,
                _touch_device,
                _keyboard_device,
                _mouse_device,
                _touch_file,
                keyboard_file,
                _mouse_file,
                _touch_source_stream,
                _mouse_source_stream,
                _keyboard_listener,
                media_buttons_listener,
                _touch_buttons_listener,
            ) = start_input_relays_for_test(&current_task, EventProxyMode::None).await;

            const DEVICE_ID: u32 = 10;

            let power_pressed_event = MediaButtonsEvent {
                volume: Some(0),
                mic_mute: Some(false),
                pause: Some(false),
                camera_disable: Some(false),
                power: Some(true),
                function: Some(false),
                device_id: Some(DEVICE_ID),
                ..Default::default()
            };

            let _ = media_buttons_listener.on_event(power_pressed_event).await;

            let events = read_uapi_events(&keyboard_file, &current_task);
            // Default device should receive events because device id 10 falls back to default.
            assert_ne!(events.len(), 0);
            assert_eq!(input_relay.num_unregistered_device_events(), 0);

            // add a device, mock uinput.
            let open_files: OpenedFiles = Default::default();
            input_relay.add_keyboard_device(DEVICE_ID, open_files.clone(), None);
            let inspector = fuchsia_inspect::Inspector::default();
            let device_id_10_file = Arc::new(InputFile::new_keyboard(
                InputDeviceInfo::new(
                    input_id { bustype: 0, vendor: 0, product: 0, version: 0 },
                    "keyboard_10".to_string(),
                ),
                inspector.root(),
            ));
            open_files.lock().push(Arc::downgrade(&device_id_10_file));
            let root_namespace_node = current_task
                .lookup_path_from_root(".".into())
                .expect("failed to get namespace node for root");
            let device_id_10_file_object = FileObject::new(
                &current_task,
                Box::new(crate::input_file::ArcInputFile(device_id_10_file)),
                root_namespace_node,
                OpenFlags::empty(),
            )
            .expect("FileObject::new failed");

            let power_released_event = MediaButtonsEvent {
                volume: Some(0),
                mic_mute: Some(false),
                pause: Some(false),
                camera_disable: Some(false),
                power: Some(false),
                function: Some(false),
                device_id: Some(DEVICE_ID),
                ..Default::default()
            };

            let _ = media_buttons_listener.on_event(power_released_event).await;

            let events = read_uapi_events(&keyboard_file, &current_task);
            // Default device should not receive events because they matched device id 10.
            assert_eq!(events.len(), 0);

            let events = read_uapi_events(&device_id_10_file_object, &current_task);
            // file of device id 10 should receive events.
            assert_ne!(events.len(), 0);

            std::mem::drop(media_buttons_listener); // Close Zircon channel.
        })
        .await;
    }

    #[::fuchsia::test]
    async fn route_touch_button_event_by_device_id() {
        spawn_kernel_and_run(async move |current_task| {
            // Set up resources.

            let (
                input_relay,
                _touch_device,
                _keyboard_device,
                _mouse_device,
                touch_file,
                _keyboard_file,
                _mouse_file,
                _touch_source_stream,
                _mouse_source_stream,
                _keyboard_listener,
                _media_buttons_listener,
                touch_buttons_listener,
            ) = start_input_relays_for_test(&current_task, EventProxyMode::None).await;

            const DEVICE_ID: u32 = 10;

            let palm_pressed_event: TouchButtonsEvent = TouchButtonsEvent {
                pressed_buttons: Some(vec![TouchButton::Palm]),
                device_info: Some(TouchDeviceInfo { id: Some(DEVICE_ID), ..Default::default() }),
                ..Default::default()
            };

            let _ = touch_buttons_listener.on_event(palm_pressed_event).await;

            let events = read_uapi_events(&touch_file, &current_task);
            // Default device should receive events because device id 10 falls back to default.
            assert_ne!(events.len(), 0);
            assert_eq!(input_relay.num_unregistered_device_events(), 0);

            // add a device, mock uinput.
            let device_id_10_file =
                create_test_touch_device(&current_task, input_relay.clone(), DEVICE_ID);

            let palm_released_event: TouchButtonsEvent = TouchButtonsEvent {
                pressed_buttons: Some(vec![]),
                device_info: Some(TouchDeviceInfo { id: Some(DEVICE_ID), ..Default::default() }),
                ..Default::default()
            };

            let _ = touch_buttons_listener.on_event(palm_released_event).await;

            let events = read_uapi_events(&touch_file, &current_task);
            // Default device should not receive events because they matched device id 10.
            assert_eq!(events.len(), 0);

            let events = read_uapi_events(&device_id_10_file, &current_task);
            // file of device id 10 should receive events.
            assert_ne!(events.len(), 0);

            std::mem::drop(touch_buttons_listener); // Close Zircon channel.
        })
        .await;
    }

    #[::fuchsia::test]
    async fn touch_device_multi_reader() {
        spawn_kernel_and_run(async move |current_task| {
            // Set up resources.

            let (
                _input_relay,
                touch_device,
                _keyboard_device,
                _mouse_device,
                touch_reader1,
                _keyboard_file,
                _mouse_file,
                mut touch_source_stream,
                _mouse_source_stream,
                _keyboard_listener,
                _media_buttons_listener,
                _touch_buttons_listener,
            ) = start_input_relays_for_test(&current_task, EventProxyMode::None).await;

            let touch_reader2 =
                touch_device.open_test(&current_task).expect("Failed to create input file");

            const DEVICE_ID: u32 = DEFAULT_TOUCH_DEVICE_ID;

            answer_next_touch_watch_request(
                &mut touch_source_stream,
                vec![make_touch_event_with_phase_device_id(EventPhase::Add, 1, DEVICE_ID)],
            )
            .await;

            // Wait for another `Watch` to ensure input_file done processing the first reply.
            // Use an empty `TouchEvent`, to minimize the chance that this event creates unexpected
            // `uapi::input_event`s.
            answer_next_touch_watch_request(
                &mut touch_source_stream,
                vec![make_empty_touch_event(DEVICE_ID)],
            )
            .await;

            // Consume all of the `uapi::input_event`s that are available.
            let events_from_reader1 = read_uapi_events(&touch_reader1, &current_task);
            let events_from_reader2 = read_uapi_events(&touch_reader2, &current_task);
            assert_ne!(events_from_reader1.len(), 0);
            assert_eq!(events_from_reader1.len(), events_from_reader2.len());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn keyboard_device_multi_reader() {
        spawn_kernel_and_run(async move |current_task| {
            // Set up resources.

            let (
                _input_relay,
                _touch_device,
                keyboard_device,
                _mouse_device,
                _touch_file,
                keyboard_reader1,
                _mouse_file,
                _touch_source_stream,
                _mouse_source_stream,
                keyboard_listener,
                _media_buttons_listener,
                _touch_buttons_listener,
            ) = start_input_relays_for_test(&current_task, EventProxyMode::None).await;

            let keyboard_reader2 =
                keyboard_device.open_test(&current_task).expect("Failed to create input file");

            const DEVICE_ID: u32 = DEFAULT_KEYBOARD_DEVICE_ID;

            let key_event = fuiinput::KeyEvent {
                timestamp: Some(0),
                type_: Some(fuiinput::KeyEventType::Pressed),
                key: Some(fidl_fuchsia_input::Key::A),
                device_id: Some(DEVICE_ID),
                ..Default::default()
            };

            let _ = keyboard_listener.on_key_event(&key_event).await;

            // Consume all of the `uapi::input_event`s that are available.
            let events_from_reader1 = read_uapi_events(&keyboard_reader1, &current_task);
            let events_from_reader2 = read_uapi_events(&keyboard_reader2, &current_task);
            assert_ne!(events_from_reader1.len(), 0);
            assert_eq!(events_from_reader1.len(), events_from_reader2.len());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn button_device_multi_reader() {
        spawn_kernel_and_run(async move |current_task| {
            // Set up resources.

            let (
                _input_relay,
                _touch_device,
                keyboard_device,
                _mouse_device,
                _touch_file,
                keyboard_reader1,
                _mouse_file,
                _touch_source_stream,
                _mouse_source_stream,
                _keyboard_listener,
                media_buttons_listener,
                _touch_buttons_listener,
            ) = start_input_relays_for_test(&current_task, EventProxyMode::None).await;

            let keyboard_reader2 =
                keyboard_device.open_test(&current_task).expect("Failed to create input file");

            const DEVICE_ID: u32 = DEFAULT_KEYBOARD_DEVICE_ID;

            let power_pressed_event = MediaButtonsEvent {
                volume: Some(0),
                mic_mute: Some(false),
                pause: Some(false),
                camera_disable: Some(false),
                power: Some(true),
                function: Some(false),
                device_id: Some(DEVICE_ID),
                ..Default::default()
            };

            let _ = media_buttons_listener.on_event(power_pressed_event).await;

            // Consume all of the `uapi::input_event`s that are available.
            let events_from_reader1 = read_uapi_events(&keyboard_reader1, &current_task);
            let events_from_reader2 = read_uapi_events(&keyboard_reader2, &current_task);
            assert_ne!(events_from_reader1.len(), 0);
            assert_eq!(events_from_reader1.len(), events_from_reader2.len());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn mouse_device_multi_reader() {
        spawn_kernel_and_run(async move |current_task| {
            // Set up resources.

            let (
                _input_relay,
                _touch_device,
                _keyboard_device,
                mouse_device,
                _touch_file,
                _keyboard_file,
                mouse_reader1,
                _touch_stream,
                mut mouse_stream,
                _keyboard_listener,
                _media_buttons_listener,
                _touch_buttons_listener,
            ) = start_input_relays_for_test(&current_task, EventProxyMode::None).await;

            let mouse_reader2 =
                mouse_device.open_test(&current_task).expect("Failed to create input file");

            const DEVICE_ID: u32 = DEFAULT_MOUSE_DEVICE_ID;

            answer_next_mouse_watch_request(
                &mut mouse_stream,
                vec![make_mouse_wheel_event(1, DEVICE_ID)],
            )
            .await;

            // Wait for another `Watch` to ensure input_file done processing the first reply.
            // Use an empty `MouseEvent`, to minimize the chance that this event creates unexpected
            // `uapi::input_event`s.
            answer_next_mouse_watch_request(
                &mut mouse_stream,
                vec![make_mouse_wheel_event(0, DEVICE_ID)],
            )
            .await;

            // Consume all of the `uapi::input_event`s that are available.
            let events_from_reader1 = read_uapi_events(&mouse_reader1, &current_task);
            let events_from_reader2 = read_uapi_events(&mouse_reader2, &current_task);
            assert_ne!(events_from_reader1.len(), 0);
            assert_eq!(events_from_reader1.len(), events_from_reader2.len());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn input_message_counters() {
        spawn_kernel_and_run(async move |current_task| {
            // Set up resources.
            let kernel = current_task.kernel().clone();
            let (
                _input_relay,
                _touch_device,
                _keyboard_device,
                _mouse_device,
                _touch_file,
                keyboard_file,
                _mouse_file,
                _touch_source_stream,
                _mouse_source_stream,
                keyboard_listener,
                _media_buttons_listener,
                _touch_buttons_listener,
            ) = start_input_relays_for_test(&current_task, EventProxyMode::WakeContainer).await;

            const DEVICE_ID: u32 = DEFAULT_KEYBOARD_DEVICE_ID;

            let key_event = fuiinput::KeyEvent {
                timestamp: Some(0),
                type_: Some(fuiinput::KeyEventType::Pressed),
                key: Some(fidl_fuchsia_input::Key::A),
                device_id: Some(DEVICE_ID),
                ..Default::default()
            };

            let _ = keyboard_listener.on_key_event(&key_event).await;

            let events = read_uapi_events(&keyboard_file, &current_task);
            assert_ne!(events.len(), 0);

            assert!(!kernel.suspend_resume_manager.has_nonzero_message_counter());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn mouse_device_lazily_registered_on_first_mouse_event() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let inspector = fuchsia_inspect::Inspector::default();

            let touch_device = crate::InputDevice::new_touch(
                700,
                1200,
                crate::InputDeviceInfo::new(TOUCH_INPUT_ID, "starnix_touch".to_string()),
                "touch_device",
                inspector.root(),
            );
            let keyboard_device = crate::InputDevice::new_keyboard(
                crate::InputDeviceInfo::new(KEYBOARD_INPUT_ID, "starnix_buttons".to_string()),
                "keyboard_device",
                inspector.root(),
            );
            // Do not open `mouse_device` before registration so we test production ordering:
            // userspace can only open `/dev/input/event2` after `DeviceRegistry` registration.
            let mouse_device = crate::InputDevice::new_mouse(
                crate::InputDeviceInfo::new(MOUSE_INPUT_ID, "starnix_mouse".to_string()),
                "mouse_device",
                inspector.root(),
            );

            let (touch_source_client_end, _touch_source_stream) =
                fidl::endpoints::create_request_stream::<fuipointer::TouchSourceV2Marker>();
            let (mouse_source_client_end, mut mouse_stream) =
                fidl::endpoints::create_request_stream::<fuipointer::MouseSourceV2Marker>();
            let (keyboard_proxy, mut keyboard_stream) =
                fidl::endpoints::create_sync_proxy_and_stream::<fuiinput::KeyboardMarker>();
            let view_ref_pair =
                fuchsia_scenic::ViewRefPair::new().expect("Failed to create ViewRefPair");
            let (device_registry_proxy, mut device_listener_stream) =
                fidl::endpoints::create_sync_proxy_and_stream::<
                    fuipolicy::DeviceListenerRegistryMarker,
                >();

            let (mut relay, _relay_handle) = new_input_relay();
            relay.add_touch_device(
                DEFAULT_TOUCH_DEVICE_ID,
                touch_device.open_files.clone(),
                Some(touch_device.inspect_status.clone()),
            );
            relay.add_keyboard_device(
                DEFAULT_KEYBOARD_DEVICE_ID,
                keyboard_device.open_files.clone(),
                Some(keyboard_device.inspect_status.clone()),
            );
            relay.add_pending_mouse_device(
                kernel.clone(),
                mouse_device.clone(),
                DEFAULT_MOUSE_DEVICE_ID,
            );
            relay.start_relays(
                &kernel,
                StartRelaysArgs {
                    event_proxy_mode: EventProxyMode::None,
                    touch_source_client_end,
                    keyboard_proxy,
                    mouse_source_client_end,
                    view_ref: view_ref_pair.view_ref,
                    registry_proxy: device_registry_proxy,
                    device_listener_registry: None,
                    display_width: 700,
                    display_height: 1200,
                },
            );

            if let Some(Ok(fuiinput::KeyboardRequest::AddListener { responder, .. })) =
                keyboard_stream.next().await
            {
                let _ = responder.send();
            }
            if let Some(Ok(fuipolicy::DeviceListenerRegistryRequest::RegisterListener {
                responder,
                ..
            })) = device_listener_stream.next().await
            {
                let _ = responder.send();
            }
            if let Some(Ok(
                fuipolicy::DeviceListenerRegistryRequest::RegisterTouchButtonsListener {
                    responder,
                    ..
                },
            )) = device_listener_stream.next().await
            {
                let _ = responder.send();
            }

            let mouse_dev_id = starnix_uapi::device_id::DeviceId::new(
                starnix_uapi::device_id::INPUT_MAJOR,
                DEFAULT_MOUSE_DEVICE_ID,
            );
            let next_dev_id = starnix_uapi::device_id::DeviceId::new(
                starnix_uapi::device_id::INPUT_MAJOR,
                DEFAULT_MOUSE_DEVICE_ID + 1,
            );

            // Before any mouse event occurs, the mouse device should not be registered in
            // DeviceRegistry.
            assert!(
                kernel
                    .device_registry
                    .list_minor_devices(
                        starnix_core::device::DeviceMode::Char,
                        mouse_dev_id..next_dev_id,
                    )
                    .is_empty()
            );

            // Send an empty/no-op mouse event (wheel delta 0) that produces no uapi input_events.
            answer_next_mouse_watch_request(
                &mut mouse_stream,
                vec![make_mouse_wheel_event(0, DEFAULT_MOUSE_DEVICE_ID)],
            )
            .await;

            // Still should not be registered.
            assert!(
                kernel
                    .device_registry
                    .list_minor_devices(
                        starnix_core::device::DeviceMode::Char,
                        mouse_dev_id..next_dev_id,
                    )
                    .is_empty()
            );

            // Send a real mouse event (wheel delta 1) to trigger Pending -> Registered, followed by
            // a second real mouse event (wheel delta 1) to synchronize the stream and exercise the
            // DeviceRegistration::Registered idempotency path.
            answer_next_mouse_watch_request(
                &mut mouse_stream,
                vec![make_mouse_wheel_event(1, DEFAULT_MOUSE_DEVICE_ID)],
            )
            .await;
            answer_next_mouse_watch_request(
                &mut mouse_stream,
                vec![make_mouse_wheel_event(1, DEFAULT_MOUSE_DEVICE_ID)],
            )
            .await;
            answer_next_mouse_watch_request(
                &mut mouse_stream,
                vec![make_mouse_wheel_event(0, DEFAULT_MOUSE_DEVICE_ID)],
            )
            .await;

            // The mouse device should now be registered in DeviceRegistry.
            let registered = kernel.device_registry.list_minor_devices(
                starnix_core::device::DeviceMode::Char,
                mouse_dev_id..next_dev_id,
            );
            assert_eq!(registered.len(), 1);
            assert_eq!(registered[0].0, mouse_dev_id);

            // Open the mouse device *after* registration (matching production ordering) and verify
            // that the converted events from the pre-open batches were buffered and flushed on
            // first open (2 wheel events * 2 uapi events [EV_REL, EV_SYN] each = 4 events).
            let mouse_file =
                mouse_device.open_test(&current_task).expect("Failed to open mouse file");
            let events = read_uapi_events(&mouse_file, &current_task);
            assert_eq!(events.len(), 4);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn events_dropped_and_counted_when_no_default_device_registered() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel().clone();
            let (touch_source_client_end, mut touch_source_stream) =
                fidl::endpoints::create_request_stream::<fuipointer::TouchSourceV2Marker>();
            let (mouse_source_client_end, mut mouse_stream) =
                fidl::endpoints::create_request_stream::<fuipointer::MouseSourceV2Marker>();
            let (keyboard_proxy, mut keyboard_stream) =
                fidl::endpoints::create_sync_proxy_and_stream::<
                    fidl_fuchsia_ui_input3::KeyboardMarker,
                >();
            let view_ref_pair =
                fuchsia_scenic::ViewRefPair::new().expect("Failed to create ViewRefPair");
            let (device_registry_proxy, mut device_listener_stream) =
                fidl::endpoints::create_sync_proxy_and_stream::<
                    fuipolicy::DeviceListenerRegistryMarker,
                >();

            let inspector = fuchsia_inspect::Inspector::default();
            let (relay, relay_handle) = new_input_relay();
            let relay = relay.with_inspect_node(inspector.root());
            relay.start_relays(
                &kernel,
                StartRelaysArgs {
                    event_proxy_mode: EventProxyMode::None,
                    touch_source_client_end,
                    keyboard_proxy,
                    mouse_source_client_end,
                    view_ref: view_ref_pair.view_ref,
                    registry_proxy: device_registry_proxy,
                    device_listener_registry: None,
                    display_width: 0,
                    display_height: 0,
                },
            );

            let keyboard_listener = match keyboard_stream.next().await {
                Some(Ok(fidl_fuchsia_ui_input3::KeyboardRequest::AddListener {
                    listener,
                    responder,
                    ..
                })) => {
                    let _ = responder.send();
                    listener.into_proxy()
                }
                _ => panic!("Failed to get AddListener event"),
            };
            let media_buttons_listener = match device_listener_stream.next().await {
                Some(Ok(fuipolicy::DeviceListenerRegistryRequest::RegisterListener {
                    listener,
                    responder,
                })) => {
                    let _ = responder.send();
                    listener.into_proxy()
                }
                _ => panic!("Failed to get RegisterListener event"),
            };
            let touch_buttons_listener = match device_listener_stream.next().await {
                Some(Ok(
                    fuipolicy::DeviceListenerRegistryRequest::RegisterTouchButtonsListener {
                        listener,
                        responder,
                    },
                )) => {
                    let _ = responder.send();
                    listener.into_proxy()
                }
                _ => panic!("Failed to get RegisterTouchButtonsListener event"),
            };

            const UNREGISTERED_ID: u32 = 42;

            // 1. Key event with unregistered ID and no default keyboard device.
            let key_event = fidl_fuchsia_ui_input3::KeyEvent {
                timestamp: Some(0),
                type_: Some(fidl_fuchsia_ui_input3::KeyEventType::Pressed),
                key: Some(fidl_fuchsia_input::Key::A),
                device_id: Some(UNREGISTERED_ID),
                ..Default::default()
            };
            let status = keyboard_listener.on_key_event(&key_event).await.unwrap();
            assert_eq!(status, KeyEventStatus::NotHandled);
            assert_eq!(relay_handle.num_unregistered_device_events(), 1);

            // 2. Media button event with unregistered ID and no default keyboard device.
            let media_event = MediaButtonsEvent {
                volume: Some(0),
                power: Some(true),
                device_id: Some(UNREGISTERED_ID),
                ..Default::default()
            };
            let _ = media_buttons_listener.on_event(media_event).await;
            assert_eq!(relay_handle.num_unregistered_device_events(), 2);

            // 3. Touch button event with unregistered ID and no default touch device.
            let touch_btn_event = TouchButtonsEvent {
                pressed_buttons: Some(vec![TouchButton::Palm]),
                device_info: Some(TouchDeviceInfo {
                    id: Some(UNREGISTERED_ID),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let _ = touch_buttons_listener.on_event(touch_btn_event).await;
            assert_eq!(relay_handle.num_unregistered_device_events(), 3);

            // 4. Touch event with unregistered ID and no default touch device.
            answer_next_touch_watch_request(
                &mut touch_source_stream,
                vec![make_touch_event_with_phase_device_id(EventPhase::Add, 1, UNREGISTERED_ID)],
            )
            .await;
            answer_next_touch_watch_request(&mut touch_source_stream, vec![]).await;
            assert_eq!(relay_handle.num_unregistered_device_events(), 4);

            // 5. Mouse event with unregistered ID and no default mouse device.
            answer_next_mouse_watch_request(
                &mut mouse_stream,
                vec![make_mouse_wheel_event(1, UNREGISTERED_ID)],
            )
            .await;
            assert_eq!(relay_handle.num_unregistered_device_events(), 5);

            assert_data_tree!(inspector, root: contains {
                input_events_relay: {
                    num_unregistered_device_events: 5u64,
                    num_devices: 0u64,
                    device_listener_registered: false,
                }
            });
        })
        .await;
    }
}
