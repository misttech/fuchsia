// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::InputFile;
use crate::input_event_relay::OpenedFiles;
use futures::FutureExt;
use starnix_core::device::kobject::DeviceMetadata;
use starnix_core::device::{DeviceMode, DeviceOps};
use starnix_core::task::CurrentTask;
use starnix_core::vfs::{FileOps, FsString, NamespaceNode};
#[cfg(test)]
use starnix_sync::Unlocked;
use starnix_sync::{FileOpsCore, LockEqualOrBefore, Locked, Mutex};
use starnix_uapi::device_id::{DeviceId as StarnixDeviceId, INPUT_MAJOR};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{BUS_VIRTUAL, input_id};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

// Add a fuchsia-specific vendor ID. 0xfc1a is currently not allocated
// to any vendor in the USB spec.
//
// May not be zero, see below.
const FUCHSIA_VENDOR_ID: u16 = 0xfc1a;

// May not be zero, see below.
const FUCHSIA_TOUCH_PRODUCT_ID: u16 = 0x2;

// May not be zero, see below.
const FUCHSIA_KEYBOARD_PRODUCT_ID: u16 = 0x1;

// May not be zero, see below.
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
const TOUCH_INPUT_ID: input_id = input_id {
    bustype: BUS_VIRTUAL as u16,
    // Make sure that vendor ID and product ID at least seem plausible.  See
    // above for details.
    vendor: FUCHSIA_VENDOR_ID,
    product: FUCHSIA_TOUCH_PRODUCT_ID,
    // Version is OK to be zero, but config files named `Product_yyyy_Vendor_zzzz_Version_ttt.*`
    // will not work.
    version: 0,
};
const KEYBOARD_INPUT_ID: input_id = input_id {
    bustype: BUS_VIRTUAL as u16,
    // Make sure that vendor ID and product ID at least seem plausible.  See
    // above for details.
    vendor: FUCHSIA_VENDOR_ID,
    product: FUCHSIA_KEYBOARD_PRODUCT_ID,
    version: 1,
};

const MOUSE_INPUT_ID: input_id = input_id {
    bustype: BUS_VIRTUAL as u16,
    // Make sure that vendor ID and product ID at least seem plausible.  See
    // above for details.
    vendor: FUCHSIA_VENDOR_ID,
    product: FUCHSIA_MOUSE_PRODUCT_ID,
    version: 1,
};

#[derive(Clone)]
enum InputDeviceId {
    // A touch device, containing (display width, display height).
    Touch(i32, i32),

    // A keyboard device.
    Keyboard,

    // A mouse device.
    Mouse,
}

/// An [`InputDeviceStatus`] is tied to an [`InputDeviceBinding`] and provides properties
/// detailing its Inspect status.
/// We expect all (non-timestamp) properties' counts to equal the sum of that property from all
/// files opened on that device. So for example, if a device had 3 separate input files opened, we
/// would expect it's `total_fidl_events_received_count` to equal the sum of
/// `fidl_events_received_count` from all 3 files and so forth.
pub struct InputDeviceStatus {
    /// A node that contains the state below.
    pub node: fuchsia_inspect::Node,

    /// Hold onto inspect nodes for files opened on this device, so that when these files are
    /// closed, their inspect data is maintained.
    pub file_nodes: Mutex<Vec<fuchsia_inspect::Node>>,

    /// The number of FIDL events received by this device from Fuchsia input system.
    ///
    /// We expect:
    /// total_fidl_events_received_count = total_fidl_events_ignored_count +
    ///                                    total_fidl_events_unexpected_count +
    ///                                    total_fidl_events_converted_count
    /// otherwise starnix ignored events unexpectedly.
    ///
    /// total_fidl_events_unexpected_count should be 0, if not it hints issues from upstream of
    /// ui stack.
    pub total_fidl_events_received_count: AtomicU64,

    /// The number of FIDL events ignored by this device when attempting conversion to this
    /// module’s representation of a TouchEvent.
    pub total_fidl_events_ignored_count: AtomicU64,

    /// The unexpected number of FIDL events reached to this module should be filtered out
    /// earlier in the UI stack.
    /// It maybe unexpected format or unexpected order.
    pub total_fidl_events_unexpected_count: AtomicU64,

    /// The number of FIDL events converted by this device to this module’s representation of
    /// TouchEvent.
    pub total_fidl_events_converted_count: AtomicU64,

    /// The number of uapi::input_events generated by this device from TouchEvents.
    pub total_uapi_events_generated_count: AtomicU64,

    /// The event time of the last generated uapi::input_event by one of this device's InputFiles.
    pub last_generated_uapi_event_timestamp_ns: AtomicI64,

    /// The number of events that entered with wake leases.
    pub total_events_with_wake_lease_count: AtomicU64,

    /// The number of active incoming wake leases.
    pub active_wake_leases_count: AtomicU64,
}

impl InputDeviceStatus {
    pub fn new(node: fuchsia_inspect::Node) -> Arc<Self> {
        let status = Arc::new(Self {
            node,
            file_nodes: Mutex::new(vec![]),
            total_fidl_events_received_count: AtomicU64::new(0),
            total_fidl_events_ignored_count: AtomicU64::new(0),
            total_fidl_events_unexpected_count: AtomicU64::new(0),
            total_fidl_events_converted_count: AtomicU64::new(0),
            total_uapi_events_generated_count: AtomicU64::new(0),
            last_generated_uapi_event_timestamp_ns: AtomicI64::new(0),
            total_events_with_wake_lease_count: AtomicU64::new(0),
            active_wake_leases_count: AtomicU64::new(0),
        });

        let weak_status = Arc::downgrade(&status);
        status.node.record_lazy_values("status", move || {
            let status = weak_status.upgrade();
            async move {
                let inspector = fuchsia_inspect::Inspector::default();
                if let Some(status) = status {
                    let root = inspector.root();
                    root.record_uint(
                        "total_fidl_events_received_count",
                        status.total_fidl_events_received_count.load(Ordering::Relaxed),
                    );
                    root.record_uint(
                        "total_fidl_events_ignored_count",
                        status.total_fidl_events_ignored_count.load(Ordering::Relaxed),
                    );
                    root.record_uint(
                        "total_fidl_events_unexpected_count",
                        status.total_fidl_events_unexpected_count.load(Ordering::Relaxed),
                    );
                    root.record_uint(
                        "total_fidl_events_converted_count",
                        status.total_fidl_events_converted_count.load(Ordering::Relaxed),
                    );
                    root.record_uint(
                        "total_uapi_events_generated_count",
                        status.total_uapi_events_generated_count.load(Ordering::Relaxed),
                    );
                    root.record_int(
                        "last_generated_uapi_event_timestamp_ns",
                        status.last_generated_uapi_event_timestamp_ns.load(Ordering::Relaxed),
                    );
                    root.record_uint(
                        "total_events_with_wake_lease_count",
                        status.total_events_with_wake_lease_count.load(Ordering::Relaxed),
                    );
                    root.record_uint(
                        "active_wake_leases_count",
                        status.active_wake_leases_count.load(Ordering::Relaxed),
                    );
                }
                Ok(inspector)
            }
            .boxed()
        });

        status
    }

    pub fn count_total_received_events(&self, count: u64) {
        self.total_fidl_events_received_count.fetch_add(count, Ordering::Relaxed);
    }

    pub fn count_events_with_wake_lease(&self, count: u64) {
        self.total_events_with_wake_lease_count.fetch_add(count, Ordering::Relaxed);
    }

    pub fn increment_active_wake_leases(&self, count: u64) {
        self.active_wake_leases_count.fetch_add(count, Ordering::Relaxed);
    }

    pub fn decrement_active_wake_leases(&self, count: u64) {
        self.active_wake_leases_count.fetch_sub(count, Ordering::Relaxed);
    }

    pub fn count_total_ignored_events(&self, count: u64) {
        self.total_fidl_events_ignored_count.fetch_add(count, Ordering::Relaxed);
    }

    pub fn count_total_unexpected_events(&self, count: u64) {
        self.total_fidl_events_unexpected_count.fetch_add(count, Ordering::Relaxed);
    }

    pub fn count_total_converted_events(&self, count: u64) {
        self.total_fidl_events_converted_count.fetch_add(count, Ordering::Relaxed);
    }

    pub fn count_total_generated_events(&self, count: u64, event_time_ns: i64) {
        self.total_uapi_events_generated_count.fetch_add(count, Ordering::Relaxed);
        self.last_generated_uapi_event_timestamp_ns.store(event_time_ns, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct InputDevice {
    device_type: InputDeviceId,

    pub open_files: OpenedFiles,

    pub inspect_status: Arc<InputDeviceStatus>,
}

impl InputDevice {
    pub fn new_touch(
        display_width: i32,
        display_height: i32,
        inspect_node: &fuchsia_inspect::Node,
    ) -> Self {
        let node = inspect_node.create_child("touch_device");
        InputDevice {
            device_type: InputDeviceId::Touch(display_width, display_height),
            open_files: Default::default(),
            inspect_status: InputDeviceStatus::new(node),
        }
    }

    pub fn new_keyboard(inspect_node: &fuchsia_inspect::Node) -> Self {
        let node = inspect_node.create_child("keyboard_device");
        InputDevice {
            device_type: InputDeviceId::Keyboard,
            open_files: Default::default(),
            inspect_status: InputDeviceStatus::new(node),
        }
    }

    pub fn new_mouse(inspect_node: &fuchsia_inspect::Node) -> Self {
        let node = inspect_node.create_child("mouse_device");
        InputDevice {
            device_type: InputDeviceId::Mouse,
            open_files: Default::default(),
            inspect_status: InputDeviceStatus::new(node),
        }
    }

    pub fn register<L>(
        self,
        locked: &mut Locked<L>,
        system_task: &CurrentTask,
        device_id: u32,
    ) -> Result<(), Errno>
    where
        L: LockEqualOrBefore<FileOpsCore>,
    {
        let kernel = system_task.kernel();
        let registry = &kernel.device_registry;

        let input_class = registry.objects.input_class();
        registry.register_device(
            locked,
            system_task.kernel(),
            FsString::from(format!("event{}", device_id)).as_ref(),
            DeviceMetadata::new(
                format!("input/event{}", device_id).into(),
                StarnixDeviceId::new(INPUT_MAJOR, device_id),
                DeviceMode::Char,
            ),
            input_class,
            self,
        )?;
        Ok(())
    }

    pub fn open_internal(&self) -> Box<dyn FileOps> {
        let input_file = match self.device_type {
            InputDeviceId::Touch(display_width, display_height) => {
                let mut file_nodes = self.inspect_status.file_nodes.lock();
                let child_node = self
                    .inspect_status
                    .node
                    .create_child(format!("touch_file_{}", file_nodes.len()));
                let file = Arc::new(InputFile::new_touch(
                    TOUCH_INPUT_ID,
                    display_width,
                    display_height,
                    &child_node,
                ));
                file_nodes.push(child_node);
                file
            }
            InputDeviceId::Keyboard => {
                let mut file_nodes = self.inspect_status.file_nodes.lock();
                let child_node = self
                    .inspect_status
                    .node
                    .create_child(format!("keyboard_file_{}", file_nodes.len()));
                let file = Arc::new(InputFile::new_keyboard(KEYBOARD_INPUT_ID, &child_node));
                file_nodes.push(child_node);
                file
            }
            InputDeviceId::Mouse => {
                let mut file_nodes = self.inspect_status.file_nodes.lock();
                let child_node = self
                    .inspect_status
                    .node
                    .create_child(format!("mouse_file_{}", file_nodes.len()));
                let file = Arc::new(InputFile::new_mouse(MOUSE_INPUT_ID, &child_node));
                file_nodes.push(child_node);
                file
            }
        };
        input_file.init_inspect_status();
        self.open_files.lock().push(Arc::downgrade(&input_file));
        Box::new(crate::input_file::ArcInputFile(input_file))
    }

    #[cfg(test)]
    pub fn open_test(
        &self,
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
    ) -> Result<starnix_core::vfs::FileHandle, Errno> {
        let input_file = self.open_internal();
        let root_namespace_node = current_task
            .lookup_path_from_root(locked, ".".into())
            .expect("failed to get namespace node for root");

        let file_object = starnix_core::vfs::FileObject::new(
            locked,
            current_task,
            input_file,
            root_namespace_node,
            OpenFlags::empty(),
        )
        .expect("FileObject::new failed");
        Ok(file_object)
    }
}

impl DeviceOps for InputDevice {
    fn open(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _current_task: &CurrentTask,
        _id: StarnixDeviceId,
        _node: &NamespaceNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let input_file = self.open_internal();
        Ok(input_file)
    }
}

#[cfg(test)]
mod test {
    #![allow(clippy::unused_unit)] // for compatibility with `test_case`

    use super::*;
    use crate::input_event_relay::{self, EventProxyMode};
    use anyhow::anyhow;
    use assert_matches::assert_matches;
    use diagnostics_assertions::{AnyProperty, assert_data_tree};
    use fidl_fuchsia_ui_input::MediaButtonsEvent;
    use fidl_fuchsia_ui_input3 as fuiinput;
    use fidl_fuchsia_ui_pointer as fuipointer;
    use fidl_fuchsia_ui_policy as fuipolicy;
    use fuipointer::{
        EventPhase, TouchEvent, TouchInteractionId, TouchPointerSample, TouchResponse,
        TouchSourceMarker, TouchSourceRequest,
    };
    use futures::StreamExt as _;
    use pretty_assertions::assert_eq;
    use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
    use starnix_core::task::{EventHandler, Waiter};
    #[allow(deprecated, reason = "pre-existing usage")]
    use starnix_core::testing::create_kernel_task_and_unlocked;
    use starnix_core::vfs::FileHandle;
    use starnix_core::vfs::buffers::VecOutputBuffer;
    use starnix_types::time::timeval_from_time;
    use starnix_uapi::errors::EAGAIN;
    use starnix_uapi::uapi;
    use starnix_uapi::vfs::FdEvents;
    use test_case::test_case;
    use test_util::assert_near;
    use zerocopy::FromBytes as _;

    const INPUT_EVENT_SIZE: usize = std::mem::size_of::<uapi::input_event>();

    async fn start_touch_input(
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
    ) -> (InputDevice, FileHandle, fuipointer::TouchSourceRequestStream) {
        let inspector = fuchsia_inspect::Inspector::default();
        start_touch_input_inspect_and_dimensions(locked, current_task, 700, 1200, &inspector).await
    }

    async fn start_touch_input_inspect(
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        inspector: &fuchsia_inspect::Inspector,
    ) -> (InputDevice, FileHandle, fuipointer::TouchSourceRequestStream) {
        start_touch_input_inspect_and_dimensions(locked, current_task, 700, 1200, &inspector).await
    }

    async fn init_keyboard_listener(
        keyboard_stream: &mut fuiinput::KeyboardRequestStream,
    ) -> fuiinput::KeyboardListenerProxy {
        let keyboard_listener = match keyboard_stream.next().await {
            Some(Ok(fuiinput::KeyboardRequest::AddListener {
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

        keyboard_listener
    }

    async fn init_button_listeners(
        device_listener_stream: &mut fuipolicy::DeviceListenerRegistryRequestStream,
    ) -> (fuipolicy::MediaButtonsListenerProxy, fuipolicy::TouchButtonsListenerProxy) {
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

        (media_buttons_listener, touch_buttons_listener)
    }

    async fn start_touch_input_inspect_and_dimensions(
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        x_max: i32,
        y_max: i32,
        inspector: &fuchsia_inspect::Inspector,
    ) -> (InputDevice, FileHandle, fuipointer::TouchSourceRequestStream) {
        let input_device = InputDevice::new_touch(x_max, y_max, inspector.root());
        let input_file =
            input_device.open_test(locked, current_task).expect("Failed to create input file");

        let (touch_source_client_end, touch_source_stream) =
            fidl::endpoints::create_request_stream::<TouchSourceMarker>();

        let (mouse_source_client_end, _mouse_source_stream) =
            fidl::endpoints::create_request_stream::<fuipointer::MouseSourceMarker>();

        let (keyboard_proxy, mut keyboard_stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<fuiinput::KeyboardMarker>();
        let view_ref_pair =
            fuchsia_scenic::ViewRefPair::new().expect("Failed to create ViewRefPair");

        let (device_registry_proxy, mut device_listener_stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<fuipolicy::DeviceListenerRegistryMarker>(
            );

        let (relay, _relay_handle) = input_event_relay::new_input_relay();
        relay.start_relays(
            &current_task.kernel(),
            EventProxyMode::None,
            touch_source_client_end,
            keyboard_proxy,
            mouse_source_client_end,
            view_ref_pair.view_ref,
            device_registry_proxy,
            input_device.open_files.clone(),
            Default::default(),
            Default::default(),
            Some(input_device.inspect_status.clone()),
            None,
            None,
        );

        let _ = init_keyboard_listener(&mut keyboard_stream).await;
        let _ = init_button_listeners(&mut device_listener_stream).await;

        (input_device, input_file, touch_source_stream)
    }

    async fn start_keyboard_input(
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
    ) -> (InputDevice, FileHandle, fuiinput::KeyboardListenerProxy) {
        let inspector = fuchsia_inspect::Inspector::default();
        let input_device = InputDevice::new_keyboard(inspector.root());
        let input_file =
            input_device.open_test(locked, current_task).expect("Failed to create input file");
        let (keyboard_proxy, mut keyboard_stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<fuiinput::KeyboardMarker>();
        let view_ref_pair =
            fuchsia_scenic::ViewRefPair::new().expect("Failed to create ViewRefPair");

        let (device_registry_proxy, mut device_listener_stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<fuipolicy::DeviceListenerRegistryMarker>(
            );

        let (touch_source_client_end, _touch_source_stream) =
            fidl::endpoints::create_request_stream::<TouchSourceMarker>();

        let (mouse_source_client_end, _mouse_source_stream) =
            fidl::endpoints::create_request_stream::<fuipointer::MouseSourceMarker>();

        let (relay, _relay_handle) = input_event_relay::new_input_relay();
        relay.start_relays(
            current_task.kernel(),
            EventProxyMode::None,
            touch_source_client_end,
            keyboard_proxy,
            mouse_source_client_end,
            view_ref_pair.view_ref,
            device_registry_proxy,
            Default::default(),
            input_device.open_files.clone(),
            Default::default(),
            None,
            Some(input_device.inspect_status.clone()),
            None,
        );

        let keyboad_listener = init_keyboard_listener(&mut keyboard_stream).await;
        let _ = init_button_listeners(&mut device_listener_stream).await;

        (input_device, input_file, keyboad_listener)
    }

    async fn start_button_input(
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
    ) -> (InputDevice, FileHandle, fuipolicy::MediaButtonsListenerProxy) {
        let inspector = fuchsia_inspect::Inspector::default();
        start_button_input_inspect(locked, current_task, &inspector).await
    }

    async fn start_button_input_inspect(
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        inspector: &fuchsia_inspect::Inspector,
    ) -> (InputDevice, FileHandle, fuipolicy::MediaButtonsListenerProxy) {
        let input_device = InputDevice::new_keyboard(inspector.root());
        let input_file =
            input_device.open_test(locked, current_task).expect("Failed to create input file");
        let (device_registry_proxy, mut device_listener_stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<fuipolicy::DeviceListenerRegistryMarker>(
            );

        let (touch_source_client_end, _touch_source_stream) =
            fidl::endpoints::create_request_stream::<TouchSourceMarker>();
        let (mouse_source_client_end, _mouse_source_stream) =
            fidl::endpoints::create_request_stream::<fuipointer::MouseSourceMarker>();
        let (keyboard_proxy, mut keyboard_stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<fuiinput::KeyboardMarker>();
        let view_ref_pair =
            fuchsia_scenic::ViewRefPair::new().expect("Failed to create ViewRefPair");

        let (relay, _relay_handle) = input_event_relay::new_input_relay();
        relay.start_relays(
            current_task.kernel(),
            EventProxyMode::None,
            touch_source_client_end,
            keyboard_proxy,
            mouse_source_client_end,
            view_ref_pair.view_ref,
            device_registry_proxy,
            Default::default(),
            input_device.open_files.clone(),
            Default::default(),
            None,
            Some(input_device.inspect_status.clone()),
            None,
        );

        let _ = init_keyboard_listener(&mut keyboard_stream).await;
        let (button_listener, _) = init_button_listeners(&mut device_listener_stream).await;

        (input_device, input_file, button_listener)
    }

    async fn start_mouse_input(
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
    ) -> (InputDevice, FileHandle, fuipointer::MouseSourceRequestStream) {
        let inspector = fuchsia_inspect::Inspector::default();
        start_mouse_input_inspect(locked, current_task, &inspector).await
    }

    async fn start_mouse_input_inspect(
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        inspector: &fuchsia_inspect::Inspector,
    ) -> (InputDevice, FileHandle, fuipointer::MouseSourceRequestStream) {
        let input_device = InputDevice::new_mouse(inspector.root());
        let input_file =
            input_device.open_test(locked, current_task).expect("Failed to create input file");

        let (touch_source_client_end, _touch_source_stream) =
            fidl::endpoints::create_request_stream::<TouchSourceMarker>();

        let (mouse_source_client_end, mouse_source_stream) =
            fidl::endpoints::create_request_stream::<fuipointer::MouseSourceMarker>();

        let (keyboard_proxy, mut keyboard_stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<fuiinput::KeyboardMarker>();
        let view_ref_pair =
            fuchsia_scenic::ViewRefPair::new().expect("Failed to create ViewRefPair");

        let (device_registry_proxy, mut device_listener_stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<fuipolicy::DeviceListenerRegistryMarker>(
            );

        let (relay, _relay_handle) = input_event_relay::new_input_relay();
        relay.start_relays(
            &current_task.kernel(),
            EventProxyMode::None,
            touch_source_client_end,
            keyboard_proxy,
            mouse_source_client_end,
            view_ref_pair.view_ref,
            device_registry_proxy,
            Default::default(),
            Default::default(),
            input_device.open_files.clone(),
            None,
            None,
            Some(input_device.inspect_status.clone()),
        );

        let _ = init_keyboard_listener(&mut keyboard_stream).await;
        let _ = init_button_listeners(&mut device_listener_stream).await;

        (input_device, input_file, mouse_source_stream)
    }

    fn make_touch_event(pointer_id: u32) -> fuipointer::TouchEvent {
        // Default to `Change`, because that has the fewest side effects.
        make_touch_event_with_phase(EventPhase::Change, pointer_id)
    }

    fn make_touch_event_with_phase(phase: EventPhase, pointer_id: u32) -> fuipointer::TouchEvent {
        make_touch_event_with_coords_phase(0.0, 0.0, phase, pointer_id)
    }

    fn make_touch_event_with_coords_phase(
        x: f32,
        y: f32,
        phase: EventPhase,
        pointer_id: u32,
    ) -> fuipointer::TouchEvent {
        make_touch_event_with_coords_phase_timestamp(x, y, phase, pointer_id, 0)
    }

    fn make_touch_event_with_coords(x: f32, y: f32, pointer_id: u32) -> fuipointer::TouchEvent {
        make_touch_event_with_coords_phase(x, y, EventPhase::Change, pointer_id)
    }

    fn make_touch_event_with_coords_phase_timestamp(
        x: f32,
        y: f32,
        phase: EventPhase,
        pointer_id: u32,
        time_nanos: i64,
    ) -> fuipointer::TouchEvent {
        make_touch_event_with_coords_phase_timestamp_device_id(
            x, y, phase, pointer_id, time_nanos, 0,
        )
    }

    fn make_empty_touch_event() -> fuipointer::TouchEvent {
        TouchEvent {
            pointer_sample: Some(TouchPointerSample {
                interaction: Some(TouchInteractionId {
                    pointer_id: 0,
                    device_id: 0,
                    interaction_id: 0,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn make_touch_event_with_coords_phase_timestamp_device_id(
        x: f32,
        y: f32,
        phase: EventPhase,
        pointer_id: u32,
        time_nanos: i64,
        device_id: u32,
    ) -> fuipointer::TouchEvent {
        TouchEvent {
            timestamp: Some(time_nanos),
            pointer_sample: Some(TouchPointerSample {
                position_in_viewport: Some([x, y]),
                // Default to `Change`, because that has the fewest side effects.
                phase: Some(phase),
                interaction: Some(TouchInteractionId { pointer_id, device_id, interaction_id: 0 }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn make_mouse_wheel_event(ticks: i64) -> fuipointer::MouseEvent {
        make_mouse_wheel_event_with_timestamp(ticks, 0)
    }

    fn make_mouse_wheel_event_with_timestamp(ticks: i64, timestamp: i64) -> fuipointer::MouseEvent {
        fuipointer::MouseEvent {
            timestamp: Some(timestamp),
            pointer_sample: Some(fuipointer::MousePointerSample {
                device_id: Some(0),
                scroll_v: Some(ticks),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn read_uapi_events<L>(
        locked: &mut Locked<L>,
        file: &FileHandle,
        current_task: &CurrentTask,
    ) -> Vec<uapi::input_event>
    where
        L: LockEqualOrBefore<FileOpsCore>,
    {
        std::iter::from_fn(|| {
            let locked = locked.cast_locked::<FileOpsCore>();
            let mut event_bytes = VecOutputBuffer::new(INPUT_EVENT_SIZE);
            match file.read(locked, current_task, &mut event_bytes) {
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

    // Waits for a `Watch()` request to arrive on `request_stream`, and responds with
    // `touch_event`. Returns the arguments to the `Watch()` call.
    async fn answer_next_touch_watch_request(
        request_stream: &mut fuipointer::TouchSourceRequestStream,
        touch_events: Vec<TouchEvent>,
    ) -> Vec<TouchResponse> {
        match request_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responses, responder })) => {
                responder.send(touch_events).expect("failure sending Watch reply");
                responses
            }
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }
    }

    // Waits for a `Watch()` request to arrive on `request_stream`, and responds with
    // `mouse_events`.
    async fn answer_next_mouse_watch_request(
        request_stream: &mut fuipointer::MouseSourceRequestStream,
        mouse_events: Vec<fuipointer::MouseEvent>,
    ) {
        match request_stream.next().await {
            Some(Ok(fuipointer::MouseSourceRequest::Watch { responder })) => {
                responder.send(mouse_events).expect("failure sending Watch reply");
            }
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }
    }

    #[::fuchsia::test()]
    async fn initial_watch_request_has_empty_responses_arg() {
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        // Set up resources.
        let (_input_device, _input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;

        // Verify that the watch request has empty `responses`.
        assert_matches!(
            touch_source_stream.next().await,
            Some(Ok(TouchSourceRequest::Watch { responses, .. }))
                => assert_eq!(responses.as_slice(), [])
        );
    }

    #[::fuchsia::test]
    async fn later_watch_requests_have_responses_arg_matching_earlier_watch_replies() {
        // Set up resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, _input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;

        // Reply to first `Watch` with two `TouchEvent`s.
        match touch_source_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responder, .. })) => responder
                .send(vec![make_empty_touch_event(), make_empty_touch_event()])
                .expect("failure sending Watch reply"),
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }

        // Verify second `Watch` has two elements in `responses`.
        // Then reply with five `TouchEvent`s.
        match touch_source_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responses, responder })) => {
                assert_matches!(responses.as_slice(), [_, _]);
                responder
                    .send(vec![
                        make_empty_touch_event(),
                        make_empty_touch_event(),
                        make_empty_touch_event(),
                        make_empty_touch_event(),
                        make_empty_touch_event(),
                    ])
                    .expect("failure sending Watch reply")
            }
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }

        // Verify third `Watch` has five elements in `responses`.
        match touch_source_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responses, .. })) => {
                assert_matches!(responses.as_slice(), [_, _, _, _, _]);
            }
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }
    }

    #[::fuchsia::test]
    async fn notifies_polling_waiters_of_new_data() {
        // Set up resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;
        let waiter1 = Waiter::new();
        let waiter2 = Waiter::new();

        // Ask `input_file` to notify waiters when data is available to read.
        [&waiter1, &waiter2].iter().for_each(|waiter| {
            input_file.wait_async(
                locked,
                &current_task,
                waiter,
                FdEvents::POLLIN,
                EventHandler::None,
            );
        });
        assert_matches!(
            waiter1.wait_until(locked, &current_task, zx::MonotonicInstant::ZERO),
            Err(_)
        );
        assert_matches!(
            waiter2.wait_until(locked, &current_task, zx::MonotonicInstant::ZERO),
            Err(_)
        );

        // Reply to first `Watch` request.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_phase(EventPhase::Add, 1)],
        )
        .await;
        answer_next_touch_watch_request(&mut touch_source_stream, vec![make_touch_event(1)]).await;

        // `InputFile` should be done processing the first reply, since it has sent its second
        // request. And, as part of processing the first reply, `InputFile` should have notified
        // the interested waiters.
        assert_eq!(waiter1.wait_until(locked, &current_task, zx::MonotonicInstant::ZERO), Ok(()));
        assert_eq!(waiter2.wait_until(locked, &current_task, zx::MonotonicInstant::ZERO), Ok(()));
    }

    #[::fuchsia::test]
    async fn notifies_blocked_waiter_of_new_data() {
        // Set up resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;
        let waiter = Waiter::new();

        // Ask `input_file` to notify `waiter` when data is available to read.
        input_file.wait_async(locked, &current_task, &waiter, FdEvents::POLLIN, EventHandler::None);

        let closure =
            move |locked: &mut Locked<Unlocked>, task: &CurrentTask| waiter.wait(locked, &task);

        let (waiter_thread, req) = SpawnRequestBuilder::new()
            .with_debug_name("input-device-waiter")
            .with_sync_closure(closure)
            .build_with_async_result();
        kernel.kthreads.spawner().spawn_from_request(req);

        let mut waiter_thread = Box::pin(waiter_thread);
        assert_matches!(futures::poll!(&mut waiter_thread), futures::task::Poll::Pending);

        // Reply to first `Watch` request.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_phase(EventPhase::Add, 1)],
        )
        .await;

        // Wait for another `Watch`.
        //
        // TODO(https://fxbug.dev/42075452): Without this, `relay_thread` gets stuck `await`-ing
        // the reply to its first request. Figure out why that happens, and remove this second
        // reply.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![make_touch_event(1)]).await;
    }

    #[::fuchsia::test]
    async fn does_not_notify_polling_waiters_without_new_data() {
        // Set up resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;
        let waiter1 = Waiter::new();
        let waiter2 = Waiter::new();

        // Ask `input_file` to notify waiters when data is available to read.
        [&waiter1, &waiter2].iter().for_each(|waiter| {
            input_file.wait_async(
                locked,
                &current_task,
                waiter,
                FdEvents::POLLIN,
                EventHandler::None,
            );
        });
        assert_matches!(
            waiter1.wait_until(locked, &current_task, zx::MonotonicInstant::ZERO),
            Err(_)
        );
        assert_matches!(
            waiter2.wait_until(locked, &current_task, zx::MonotonicInstant::ZERO),
            Err(_)
        );

        // Reply to first `Watch` request with an empty set of events.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![]).await;

        // `InputFile` should be done processing the first reply. Since there
        // were no touch_events given, `InputFile` should not have notified the
        // interested waiters.
        assert_matches!(
            waiter1.wait_until(locked, &current_task, zx::MonotonicInstant::ZERO),
            Err(_)
        );
        assert_matches!(
            waiter2.wait_until(locked, &current_task, zx::MonotonicInstant::ZERO),
            Err(_)
        );
    }

    // Note: a user program may also want to be woken if events were already ready at the
    // time that the program called `epoll_wait()`. However, there's no test for that case
    // in this module, because:
    //
    // 1. Not all programs will want to be woken in such a case. In particular, some programs
    //    use "edge-triggered" mode instead of "level-tiggered" mode. For details on the
    //    two modes, see https://man7.org/linux/man-pages/man7/epoll.7.html.
    // 2. For programs using "level-triggered" mode, the relevant behavior is implemented in
    //    the `epoll` module, and verified by `epoll::tests::test_epoll_ready_then_wait()`.
    //
    // See also: the documentation for `FileOps::wait_async()`.

    #[::fuchsia::test]
    async fn honors_wait_cancellation() {
        // Set up input resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;
        let waiter1 = Waiter::new();
        let waiter2 = Waiter::new();

        // Ask `input_file` to notify `waiter` when data is available to read.
        let waitkeys = [&waiter1, &waiter2]
            .iter()
            .map(|waiter| {
                input_file
                    .wait_async(locked, &current_task, waiter, FdEvents::POLLIN, EventHandler::None)
                    .expect("wait_async")
            })
            .collect::<Vec<_>>();

        // Cancel wait for `waiter1`.
        waitkeys.into_iter().next().expect("failed to get first waitkey").cancel();

        // Reply to first `Watch` request.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_phase(EventPhase::Add, 1)],
        )
        .await;
        // Wait for another `Watch`.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![make_touch_event(1)]).await;

        // `InputFile` should be done processing the first reply, since it has sent its second
        // request. And, as part of processing the first reply, `InputFile` should have notified
        // the interested waiters.
        assert_matches!(
            waiter1.wait_until(locked, &current_task, zx::MonotonicInstant::ZERO),
            Err(_)
        );
        assert_eq!(waiter2.wait_until(locked, &current_task, zx::MonotonicInstant::ZERO), Ok(()));
    }

    #[::fuchsia::test]
    async fn query_events() {
        // Set up resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;

        // Check initial expectation.
        assert_eq!(
            input_file.query_events(locked, &current_task).expect("query_events"),
            FdEvents::empty(),
            "events should be empty before data arrives"
        );

        // Reply to first `Watch` request.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_phase(EventPhase::Add, 1)],
        )
        .await;

        // Wait for another `Watch`.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![make_touch_event(1)]).await;

        // Check post-watch expectation.
        assert_eq!(
            input_file.query_events(locked, &current_task).expect("query_events"),
            FdEvents::POLLIN | FdEvents::POLLRDNORM,
            "events should be POLLIN after data arrives"
        );
    }

    fn make_uapi_input_event(ty: u32, code: u32, value: i32) -> uapi::input_event {
        make_uapi_input_event_with_timestamp(ty, code, value, 0)
    }

    fn make_uapi_input_event_with_timestamp(
        ty: u32,
        code: u32,
        value: i32,
        time_nanos: i64,
    ) -> uapi::input_event {
        uapi::input_event {
            time: timeval_from_time(zx::MonotonicInstant::from_nanos(time_nanos)),
            type_: ty as u16,
            code: code as u16,
            value,
        }
    }

    #[::fuchsia::test]
    async fn touch_event_ignored() {
        // Set up resources.
        let inspector = fuchsia_inspect::Inspector::default();
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input_inspect(locked, &current_task, &inspector).await;

        // Touch add for pointer 1. This should be counted as a received event and a converted
        // event. It should also yield 6 generated events.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_phase(EventPhase::Add, 1)],
        )
        .await;

        // Wait for another `Watch` to ensure input_file done processing the first reply.
        // Use an empty `TouchEvent`, to minimize the chance that this event creates unexpected
        // `uapi::input_event`s. This should be counted as a received event and an ignored event.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![make_empty_touch_event()])
            .await;

        // Consume all of the `uapi::input_event`s that are available.
        let events = read_uapi_events(locked, &input_file, &current_task);

        assert_eq!(events.len(), 6);

        // Reply to `Watch` request of empty event. This should be counted as a received event and
        // an ignored event.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![make_empty_touch_event()])
            .await;

        // Wait for another `Watch`.
        match touch_source_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responses, .. })) => {
                assert_matches!(responses.as_slice(), [_])
            }
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }

        let events = read_uapi_events(locked, &input_file, &current_task);
        assert_eq!(events, vec![]);
        assert_data_tree!(inspector, root: {
            touch_device: {
                active_wake_leases_count: 0u64,
                total_events_with_wake_lease_count: 0u64,
                total_fidl_events_received_count: 3u64,
                total_fidl_events_ignored_count: 2u64,
                total_fidl_events_unexpected_count: 0u64,
                total_fidl_events_converted_count: 1u64,
                total_uapi_events_generated_count: 6u64,
                last_generated_uapi_event_timestamp_ns: 0i64,
                touch_file_0: {
                    fidl_events_received_count: 3u64,
                    fidl_events_ignored_count: 2u64,
                    fidl_events_unexpected_count: 0u64,
                    fidl_events_converted_count: 1u64,
                    uapi_events_generated_count: 6u64,
                    uapi_events_read_count: 6u64,
                    fd_read_count: 8u64,
                    fd_notify_count: 1u64,
                    last_generated_uapi_event_timestamp_ns: 0i64,
                    last_read_uapi_event_timestamp_ns: 0i64,
                    opened_without_nonblock: AnyProperty,
                    open_timestamp_ns: AnyProperty,
                    closed: AnyProperty,
                    close_timestamp_ns: AnyProperty,
                },
            }
        });
    }

    #[test_case(make_touch_event_with_phase(EventPhase::Add, 1); "touch add for pointer already added")]
    #[test_case(make_touch_event_with_phase(EventPhase::Change, 2); "touch change for pointer not added")]
    #[test_case(make_touch_event_with_phase(EventPhase::Remove, 2); "touch remove for pointer not added")]
    #[test_case(make_touch_event_with_phase(EventPhase::Cancel, 1); "touch cancel")]
    #[::fuchsia::test]
    async fn touch_event_unexpected(event: TouchEvent) {
        // Set up resources.
        let inspector = fuchsia_inspect::Inspector::default();
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input_inspect(locked, &current_task, &inspector).await;

        // Touch add for pointer 1. This should be counted as a received event and a converted
        // event. It should also yield 6 generated events.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_phase(EventPhase::Add, 1)],
        )
        .await;

        // Wait for another `Watch` to ensure input_file done processing the first reply.
        // Use an empty `TouchEvent`, to minimize the chance that this event creates unexpected
        // `uapi::input_event`s. This should be counted as a received event and an ignored event.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![make_empty_touch_event()])
            .await;

        // Consume all of the `uapi::input_event`s that are available.
        let events = read_uapi_events(locked, &input_file, &current_task);

        assert_eq!(events.len(), 6);

        // Reply to `Watch` request of given event. This should be counted as a received event and
        // an unexpected event.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![event]).await;

        // Wait for another `Watch`.
        match touch_source_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responses, .. })) => {
                assert_matches!(responses.as_slice(), [_])
            }
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }

        let events = read_uapi_events(locked, &input_file, &current_task);
        assert_eq!(events, vec![]);
        assert_data_tree!(inspector, root: {
            touch_device: {
                active_wake_leases_count: 0u64,
                total_events_with_wake_lease_count: 0u64,
                total_fidl_events_received_count: 3u64,
                total_fidl_events_ignored_count: 1u64,
                total_fidl_events_unexpected_count: 1u64,
                total_fidl_events_converted_count: 1u64,
                total_uapi_events_generated_count: 6u64,
                last_generated_uapi_event_timestamp_ns: 0i64,
                touch_file_0: {
                    fidl_events_received_count: 3u64,
                    fidl_events_ignored_count: 1u64,
                    fidl_events_unexpected_count: 1u64,
                    fidl_events_converted_count: 1u64,
                    uapi_events_generated_count: 6u64,
                    uapi_events_read_count: 6u64,
                    fd_read_count: 8u64,
                    fd_notify_count: 1u64,
                    last_generated_uapi_event_timestamp_ns: 0i64,
                    last_read_uapi_event_timestamp_ns: 0i64,
                    opened_without_nonblock: AnyProperty,
                    open_timestamp_ns: AnyProperty,
                    closed: AnyProperty,
                    close_timestamp_ns: AnyProperty,
                },
            }
        });
    }

    #[::fuchsia::test]
    async fn translates_touch_add() {
        // Set up resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;

        // Touch add for pointer 1.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_phase(EventPhase::Add, 1)],
        )
        .await;

        // Wait for another `Watch` to ensure input_file done processing the first reply.
        // Use an empty `TouchEvent`, to minimize the chance that this event
        // creates unexpected `uapi::input_event`s.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![make_empty_touch_event()])
            .await;

        // Consume all of the `uapi::input_event`s that are available.
        let events = read_uapi_events(locked, &input_file, &current_task);

        assert_eq!(
            events,
            vec![
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_SLOT, 0),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_TRACKING_ID, 1),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_X, 0),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_Y, 0),
                make_uapi_input_event(uapi::EV_KEY, uapi::BTN_TOUCH, 1),
                make_uapi_input_event(uapi::EV_SYN, uapi::SYN_REPORT, 0),
            ]
        );
    }

    #[::fuchsia::test]
    async fn translates_touch_change() {
        // Set up resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;

        // Touch add for pointer 1.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_phase(EventPhase::Add, 1)],
        )
        .await;

        // Wait for another `Watch` to ensure input_file done processing the first reply.
        // Use an empty `TouchEvent`, to minimize the chance that this event
        // creates unexpected `uapi::input_event`s.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![TouchEvent::default()])
            .await;

        // Consume all of the `uapi::input_event`s that are available.
        let events = read_uapi_events(locked, &input_file, &current_task);

        assert_eq!(events.len(), 6);

        // Reply to touch change.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_coords(10.0, 20.0, 1)],
        )
        .await;

        // Wait for another `Watch`.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![TouchEvent::default()])
            .await;

        let events = read_uapi_events(locked, &input_file, &current_task);
        assert_eq!(
            events,
            vec![
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_SLOT, 0),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_X, 10),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_Y, 20),
                make_uapi_input_event(uapi::EV_SYN, uapi::SYN_REPORT, 0),
            ]
        );
    }

    #[::fuchsia::test]
    async fn translates_touch_remove() {
        // Set up resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;

        // Touch add for pointer 1.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_phase(EventPhase::Add, 1)],
        )
        .await;

        // Wait for another `Watch` to ensure input_file done processing the first reply.
        // Use an empty `TouchEvent`, to minimize the chance that this event
        // creates unexpected `uapi::input_event`s.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![TouchEvent::default()])
            .await;

        // Consume all of the `uapi::input_event`s that are available.
        let events = read_uapi_events(locked, &input_file, &current_task);

        assert_eq!(events.len(), 6);

        // Reply to touch change.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_phase(EventPhase::Remove, 1)],
        )
        .await;

        // Wait for another `Watch`.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![TouchEvent::default()])
            .await;

        let events = read_uapi_events(locked, &input_file, &current_task);
        assert_eq!(
            events,
            vec![
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_SLOT, 0),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_TRACKING_ID, -1),
                make_uapi_input_event(uapi::EV_KEY, uapi::BTN_TOUCH, 0),
                make_uapi_input_event(uapi::EV_SYN, uapi::SYN_REPORT, 0),
            ]
        );
    }

    #[::fuchsia::test]
    async fn multi_touch_event_sequence() {
        // Set up resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;

        // Touch add for pointer 1.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_phase(EventPhase::Add, 1)],
        )
        .await;

        // Wait for another `Watch`.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![TouchEvent::default()])
            .await;
        let events = read_uapi_events(locked, &input_file, &current_task);

        assert_eq!(events.len(), 6);

        // Touch add for pointer 2.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![
                make_touch_event_with_coords(10.0, 20.0, 1),
                make_touch_event_with_phase(EventPhase::Add, 2),
            ],
        )
        .await;

        // Wait for another `Watch`.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![TouchEvent::default()])
            .await;
        let events = read_uapi_events(locked, &input_file, &current_task);

        assert_eq!(
            events,
            vec![
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_SLOT, 0),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_X, 10),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_Y, 20),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_SLOT, 1),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_TRACKING_ID, 2),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_X, 0),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_Y, 0),
                make_uapi_input_event(uapi::EV_SYN, uapi::SYN_REPORT, 0),
            ]
        );

        // Both pointers move.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![
                make_touch_event_with_coords(11.0, 21.0, 1),
                make_touch_event_with_coords(101.0, 201.0, 2),
            ],
        )
        .await;

        // Wait for another `Watch`.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![TouchEvent::default()])
            .await;
        let events = read_uapi_events(locked, &input_file, &current_task);

        assert_eq!(
            events,
            vec![
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_SLOT, 0),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_X, 11),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_Y, 21),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_SLOT, 1),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_X, 101),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_Y, 201),
                make_uapi_input_event(uapi::EV_SYN, uapi::SYN_REPORT, 0),
            ]
        );

        // Pointer 1 up.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![
                make_touch_event_with_phase(EventPhase::Remove, 1),
                make_touch_event_with_coords(102.0, 202.0, 2),
            ],
        )
        .await;

        // Wait for another `Watch`.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![TouchEvent::default()])
            .await;
        let events = read_uapi_events(locked, &input_file, &current_task);

        assert_eq!(
            events,
            vec![
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_SLOT, 0),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_TRACKING_ID, -1),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_SLOT, 1),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_X, 102),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_POSITION_Y, 202),
                make_uapi_input_event(uapi::EV_SYN, uapi::SYN_REPORT, 0),
            ]
        );

        // Pointer 2 up.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_phase(EventPhase::Remove, 2)],
        )
        .await;

        // Wait for another `Watch`.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![TouchEvent::default()])
            .await;
        let events = read_uapi_events(locked, &input_file, &current_task);

        assert_eq!(
            events,
            vec![
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_SLOT, 1),
                make_uapi_input_event(uapi::EV_ABS, uapi::ABS_MT_TRACKING_ID, -1),
                make_uapi_input_event(uapi::EV_KEY, uapi::BTN_TOUCH, 0),
                make_uapi_input_event(uapi::EV_SYN, uapi::SYN_REPORT, 0),
            ]
        );
    }

    #[::fuchsia::test]
    async fn multi_event_sequence_unsorted_in_one_watch() {
        // Set up resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;

        // Touch add for pointer 1.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![
                make_touch_event_with_coords_phase_timestamp(
                    10.0,
                    20.0,
                    EventPhase::Change,
                    1,
                    100,
                ),
                make_touch_event_with_coords_phase_timestamp(0.0, 0.0, EventPhase::Add, 1, 1),
            ],
        )
        .await;

        // Wait for another `Watch`.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![TouchEvent::default()])
            .await;
        let events = read_uapi_events(locked, &input_file, &current_task);

        assert_eq!(
            events,
            vec![
                make_uapi_input_event_with_timestamp(uapi::EV_ABS, uapi::ABS_MT_SLOT, 0, 1),
                make_uapi_input_event_with_timestamp(uapi::EV_ABS, uapi::ABS_MT_TRACKING_ID, 1, 1),
                make_uapi_input_event_with_timestamp(uapi::EV_ABS, uapi::ABS_MT_POSITION_X, 0, 1),
                make_uapi_input_event_with_timestamp(uapi::EV_ABS, uapi::ABS_MT_POSITION_Y, 0, 1),
                make_uapi_input_event_with_timestamp(uapi::EV_KEY, uapi::BTN_TOUCH, 1, 1),
                make_uapi_input_event_with_timestamp(uapi::EV_SYN, uapi::SYN_REPORT, 0, 1),
                make_uapi_input_event_with_timestamp(uapi::EV_ABS, uapi::ABS_MT_SLOT, 0, 100),
                make_uapi_input_event_with_timestamp(
                    uapi::EV_ABS,
                    uapi::ABS_MT_POSITION_X,
                    10,
                    100
                ),
                make_uapi_input_event_with_timestamp(
                    uapi::EV_ABS,
                    uapi::ABS_MT_POSITION_Y,
                    20,
                    100
                ),
                make_uapi_input_event_with_timestamp(uapi::EV_SYN, uapi::SYN_REPORT, 0, 100),
            ]
        );
    }

    #[test_case((0.0, 0.0); "origin")]
    #[test_case((100.7, 200.7); "above midpoint")]
    #[test_case((100.3, 200.3); "below midpoint")]
    #[test_case((100.5, 200.5); "midpoint")]
    #[::fuchsia::test]
    async fn sends_acceptable_coordinates((x, y): (f32, f32)) {
        // Set up resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;

        // Touch add.
        answer_next_touch_watch_request(
            &mut touch_source_stream,
            vec![make_touch_event_with_coords_phase(x, y, EventPhase::Add, 1)],
        )
        .await;

        // Wait for another `Watch`.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![TouchEvent::default()])
            .await;
        let events = read_uapi_events(locked, &input_file, &current_task);

        // Check that the reported positions are within the acceptable error. The acceptable
        // error is chosen to allow either rounding or truncation.
        const ACCEPTABLE_ERROR: f32 = 1.0;
        let actual_x = events
            .iter()
            .find(|event| {
                event.type_ == uapi::EV_ABS as u16 && event.code == uapi::ABS_MT_POSITION_X as u16
            })
            .unwrap_or_else(|| panic!("did not find `ABS_X` event in {:?}", events))
            .value;
        let actual_y = events
            .iter()
            .find(|event| {
                event.type_ == uapi::EV_ABS as u16 && event.code == uapi::ABS_MT_POSITION_Y as u16
            })
            .unwrap_or_else(|| panic!("did not find `ABS_Y` event in {:?}", events))
            .value;
        assert_near!(x, actual_x as f32, ACCEPTABLE_ERROR);
        assert_near!(y, actual_y as f32, ACCEPTABLE_ERROR);
    }

    // Per the FIDL documentation for `TouchSource::Watch()`:
    //
    // > non-sample events should return an empty |TouchResponse| table to the
    // > server
    #[test_case(
        make_touch_event_with_phase(EventPhase::Add, 2)
            => matches Some(TouchResponse { response_type: Some(_), ..});
        "event_with_sample_yields_some_response_type")]
    #[test_case(
        TouchEvent::default() => matches Some(TouchResponse { response_type: None, ..});
        "event_without_sample_yields_no_response_type")]
    #[::fuchsia::test]
    async fn sends_appropriate_reply_to_touch_source_server(
        event: TouchEvent,
    ) -> Option<TouchResponse> {
        // Set up resources.
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, _input_file, mut touch_source_stream) =
            start_touch_input(locked, &current_task).await;

        // Reply to first `Watch` request.
        answer_next_touch_watch_request(&mut touch_source_stream, vec![event]).await;

        // Get response to `event`.
        let responses =
            answer_next_touch_watch_request(&mut touch_source_stream, vec![TouchEvent::default()])
                .await;

        // Return the value for `test_case` to match on.
        responses.get(0).cloned()
    }

    #[test_case(fidl_fuchsia_input::Key::Escape, uapi::KEY_POWER; "Esc maps to Power")]
    #[test_case(fidl_fuchsia_input::Key::A, uapi::KEY_A; "A maps to A")]
    #[::fuchsia::test]
    async fn sends_keyboard_events(fkey: fidl_fuchsia_input::Key, lkey: u32) {
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_keyboard_device, keyboard_file, keyboard_listener) =
            start_keyboard_input(locked, &current_task).await;

        let key_event = fuiinput::KeyEvent {
            timestamp: Some(0),
            type_: Some(fuiinput::KeyEventType::Pressed),
            key: Some(fkey),
            ..Default::default()
        };

        let _ = keyboard_listener.on_key_event(&key_event).await;
        std::mem::drop(keyboard_listener); // Close Zircon channel.
        let events = read_uapi_events(locked, &keyboard_file, &current_task);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].code, lkey as u16);
    }

    #[::fuchsia::test]
    async fn skips_unknown_keyboard_events() {
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_keyboard_device, keyboard_file, keyboard_listener) =
            start_keyboard_input(locked, &current_task).await;

        let key_event = fuiinput::KeyEvent {
            timestamp: Some(0),
            type_: Some(fuiinput::KeyEventType::Pressed),
            key: Some(fidl_fuchsia_input::Key::AcRefresh),
            ..Default::default()
        };

        let _ = keyboard_listener.on_key_event(&key_event).await;
        std::mem::drop(keyboard_listener); // Close Zircon channel.
        let events = read_uapi_events(locked, &keyboard_file, &current_task);
        assert_eq!(events.len(), 0);
    }

    #[::fuchsia::test]
    async fn sends_power_button_events() {
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, buttons_listener) =
            start_button_input(locked, &current_task).await;

        let power_event = MediaButtonsEvent {
            volume: Some(0),
            mic_mute: Some(false),
            pause: Some(false),
            camera_disable: Some(false),
            power: Some(true),
            function: Some(false),
            ..Default::default()
        };

        let _ = buttons_listener.on_event(power_event).await;
        std::mem::drop(buttons_listener); // Close Zircon channel.

        let events = read_uapi_events(locked, &input_file, &current_task);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].code, uapi::KEY_POWER as u16);
        assert_eq!(events[0].value, 1);
    }

    #[::fuchsia::test]
    async fn sends_function_button_events() {
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, buttons_listener) =
            start_button_input(locked, &current_task).await;

        let function_event = MediaButtonsEvent {
            volume: Some(0),
            mic_mute: Some(false),
            pause: Some(false),
            camera_disable: Some(false),
            power: Some(false),
            function: Some(true),
            ..Default::default()
        };

        let _ = buttons_listener.on_event(function_event).await;
        std::mem::drop(buttons_listener); // Close Zircon channel.

        let events = read_uapi_events(locked, &input_file, &current_task);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].code, uapi::KEY_VOLUMEDOWN as u16);
        assert_eq!(events[0].value, 1);
    }

    #[::fuchsia::test]
    async fn sends_overlapping_button_events() {
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, buttons_listener) =
            start_button_input(locked, &current_task).await;

        let power_event = MediaButtonsEvent {
            volume: Some(0),
            mic_mute: Some(false),
            pause: Some(false),
            camera_disable: Some(false),
            power: Some(true),
            function: Some(false),
            ..Default::default()
        };

        let function_event = MediaButtonsEvent {
            volume: Some(0),
            mic_mute: Some(false),
            pause: Some(false),
            camera_disable: Some(false),
            power: Some(true),
            function: Some(true),
            ..Default::default()
        };

        let function_release_event = MediaButtonsEvent {
            volume: Some(0),
            mic_mute: Some(false),
            pause: Some(false),
            camera_disable: Some(false),
            power: Some(true),
            function: Some(false),
            ..Default::default()
        };

        let power_release_event = MediaButtonsEvent {
            volume: Some(0),
            mic_mute: Some(false),
            pause: Some(false),
            camera_disable: Some(false),
            power: Some(false),
            function: Some(false),
            ..Default::default()
        };

        let _ = buttons_listener.on_event(power_event).await;
        let _ = buttons_listener.on_event(function_event).await;
        let _ = buttons_listener.on_event(function_release_event).await;
        let _ = buttons_listener.on_event(power_release_event).await;
        std::mem::drop(buttons_listener); // Close Zircon channel.

        let events = read_uapi_events(locked, &input_file, &current_task);
        assert_eq!(events.len(), 8);
        assert_eq!(events[0].code, uapi::KEY_POWER as u16);
        assert_eq!(events[0].value, 1);
        assert_eq!(events[2].code, uapi::KEY_VOLUMEDOWN as u16);
        assert_eq!(events[2].value, 1);
        assert_eq!(events[4].code, uapi::KEY_VOLUMEDOWN as u16);
        assert_eq!(events[4].value, 0);
        assert_eq!(events[6].code, uapi::KEY_POWER as u16);
        assert_eq!(events[6].value, 0);
    }

    #[::fuchsia::test]
    async fn sends_simultaneous_button_events() {
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, buttons_listener) =
            start_button_input(locked, &current_task).await;

        let power_and_function_event = MediaButtonsEvent {
            volume: Some(0),
            mic_mute: Some(false),
            pause: Some(false),
            camera_disable: Some(false),
            power: Some(true),
            function: Some(true),
            ..Default::default()
        };

        let _ = buttons_listener.on_event(power_and_function_event).await;
        std::mem::drop(buttons_listener); // Close Zircon channel.

        let events = read_uapi_events(locked, &input_file, &current_task);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].code, uapi::KEY_POWER as u16);
        assert_eq!(events[0].value, 1);
        assert_eq!(events[2].code, uapi::KEY_VOLUMEDOWN as u16);
        assert_eq!(events[2].value, 1);
    }

    #[test_case(1; "Scroll up")]
    #[test_case(-1; "Scroll down")]
    #[::fuchsia::test]
    async fn sends_mouse_wheel_events(ticks: i64) {
        let time = 100;
        let uapi_event = uapi::input_event {
            time: timeval_from_time(zx::MonotonicInstant::from_nanos(time)),
            type_: uapi::EV_REL as u16,
            code: uapi::REL_WHEEL as u16,
            value: ticks as i32,
        };
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_mouse_device, mouse_file, mut mouse_stream) =
            start_mouse_input(locked, &current_task).await;

        answer_next_mouse_watch_request(
            &mut mouse_stream,
            vec![make_mouse_wheel_event_with_timestamp(ticks, time)],
        )
        .await;

        // Wait for another `Watch` to ensure mouse_file is done processing the other replies.
        // Use an empty vec, to ensure no unexpected `uapi::input_event`s are created.
        answer_next_mouse_watch_request(&mut mouse_stream, vec![]).await;

        let events = read_uapi_events(locked, &mouse_file, &current_task);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], uapi_event);
    }

    #[::fuchsia::test]
    async fn ignore_mouse_non_wheel_events() {
        let mouse_move_event = fuipointer::MouseEvent {
            timestamp: Some(0),
            pointer_sample: Some(fuipointer::MousePointerSample {
                device_id: Some(0),
                position_in_viewport: Some([50.0, 50.0]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mouse_click_event = fuipointer::MouseEvent {
            timestamp: Some(0),
            pointer_sample: Some(fuipointer::MousePointerSample {
                device_id: Some(0),
                pressed_buttons: Some(vec![1]),
                ..Default::default()
            }),
            ..Default::default()
        };
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_mouse_device, mouse_file, mut mouse_stream) =
            start_mouse_input(locked, &current_task).await;

        // Expect mouse relay to discard MouseEvents without vertical scroll.
        answer_next_mouse_watch_request(&mut mouse_stream, vec![mouse_move_event]).await;
        answer_next_mouse_watch_request(&mut mouse_stream, vec![mouse_click_event]).await;

        // Wait for another `Watch` to ensure mouse_file is done processing the other replies.
        // Use an empty vec, to ensure no unexpected `uapi::input_event`s are created.
        answer_next_mouse_watch_request(&mut mouse_stream, vec![]).await;

        let events = read_uapi_events(locked, &mouse_file, &current_task);
        assert_eq!(events.len(), 0);
    }

    #[::fuchsia::test]
    async fn touch_input_initialized_with_inspect_node() {
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let inspector = fuchsia_inspect::Inspector::default();
        let touch_device = InputDevice::new_touch(
            1200, /* screen width */
            720,  /* screen height */
            &inspector.root(),
        );
        let _file_obj = touch_device.open_test(locked, &current_task);

        assert_data_tree!(inspector, root: {
            touch_device: {
                active_wake_leases_count: 0u64,
                total_events_with_wake_lease_count: 0u64,
                total_fidl_events_received_count: 0u64,
                total_fidl_events_ignored_count: 0u64,
                total_fidl_events_unexpected_count: 0u64,
                total_fidl_events_converted_count: 0u64,
                total_uapi_events_generated_count: 0u64,
                last_generated_uapi_event_timestamp_ns: 0i64,
                touch_file_0: {
                    fidl_events_received_count: 0u64,
                    fidl_events_ignored_count: 0u64,
                    fidl_events_unexpected_count: 0u64,
                    fidl_events_converted_count: 0u64,
                    uapi_events_generated_count: 0u64,
                    uapi_events_read_count: 0u64,
                    fd_read_count: 0u64,
                    fd_notify_count: 0u64,
                    last_generated_uapi_event_timestamp_ns: 0i64,
                    last_read_uapi_event_timestamp_ns: 0i64,
                    opened_without_nonblock: AnyProperty,
                    open_timestamp_ns: AnyProperty,
                    closed: AnyProperty,
                    close_timestamp_ns: AnyProperty,
                }
            }
        });
    }

    #[::fuchsia::test]
    async fn touch_relay_updates_touch_inspect_status() {
        let inspector = fuchsia_inspect::Inspector::default();
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut touch_source_stream) =
            start_touch_input_inspect(locked, &current_task, &inspector).await;

        // Send 2 TouchEvents to proxy that should be counted as `received` by InputFile
        // A TouchEvent::default() has no pointer sample so these events should be discarded.
        match touch_source_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responder, .. })) => responder
                .send(vec![make_empty_touch_event(), make_empty_touch_event()])
                .expect("failure sending Watch reply"),
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }

        // Send 5 TouchEvents with pointer sample to proxy, these should be received and converted
        // Add/Remove events generate 5 uapi events each. Change events generate 3 uapi events each.
        match touch_source_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responses, responder })) => {
                assert_matches!(responses.as_slice(), [_, _]);
                responder
                    .send(vec![
                        make_touch_event_with_coords_phase_timestamp(
                            0.0,
                            0.0,
                            EventPhase::Add,
                            1,
                            1000,
                        ),
                        make_touch_event_with_coords_phase_timestamp(
                            1.0,
                            1.0,
                            EventPhase::Change,
                            1,
                            2000,
                        ),
                        make_touch_event_with_coords_phase_timestamp(
                            2.0,
                            2.0,
                            EventPhase::Change,
                            1,
                            3000,
                        ),
                        make_touch_event_with_coords_phase_timestamp(
                            3.0,
                            3.0,
                            EventPhase::Change,
                            1,
                            4000,
                        ),
                        make_touch_event_with_coords_phase_timestamp(
                            3.0,
                            3.0,
                            EventPhase::Remove,
                            1,
                            5000,
                        ),
                    ])
                    .expect("failure sending Watch reply");
            }
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }

        // Wait for next `Watch` call and verify it has five elements in `responses`.
        match touch_source_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responses, .. })) => {
                assert_matches!(responses.as_slice(), [_, _, _, _, _])
            }
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }

        let _events = read_uapi_events(locked, &input_file, &current_task);
        assert_data_tree!(inspector, root: {
            touch_device: {
                active_wake_leases_count: 0u64,
                total_events_with_wake_lease_count: 0u64,
                total_fidl_events_received_count: 7u64,
                total_fidl_events_ignored_count: 2u64,
                total_fidl_events_unexpected_count: 0u64,
                total_fidl_events_converted_count: 5u64,
                total_uapi_events_generated_count: 22u64,
                last_generated_uapi_event_timestamp_ns: 5000i64,
                touch_file_0: {
                    fidl_events_received_count: 7u64,
                    fidl_events_ignored_count: 2u64,
                    fidl_events_unexpected_count: 0u64,
                    fidl_events_converted_count: 5u64,
                    uapi_events_generated_count: 22u64,
                    uapi_events_read_count: 22u64,
                    fd_read_count: 23u64,
                    fd_notify_count: 1u64,
                    last_generated_uapi_event_timestamp_ns: 5000i64,
                    last_read_uapi_event_timestamp_ns: 5000i64,
                    opened_without_nonblock: AnyProperty,
                    open_timestamp_ns: AnyProperty,
                    closed: AnyProperty,
                    close_timestamp_ns: AnyProperty,
                },
            }
        });
    }

    #[::fuchsia::test]
    async fn new_file_updates_inspect_status() {
        let inspector = fuchsia_inspect::Inspector::default();
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();

        let input_device = InputDevice::new_touch(700, 700, inspector.root());
        let input_file_0 =
            input_device.open_test(locked, &current_task).expect("Failed to create input file");

        let (touch_source_client_end, mut touch_source_stream) =
            fidl::endpoints::create_request_stream::<TouchSourceMarker>();
        let (mouse_source_client_end, _mouse_source_stream) =
            fidl::endpoints::create_request_stream::<fuipointer::MouseSourceMarker>();
        let (keyboard_proxy, mut keyboard_stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<fuiinput::KeyboardMarker>();
        let view_ref_pair =
            fuchsia_scenic::ViewRefPair::new().expect("Failed to create ViewRefPair");
        let (device_registry_proxy, mut device_listener_stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<fuipolicy::DeviceListenerRegistryMarker>(
            );

        let (relay, relay_handle) = input_event_relay::new_input_relay();
        relay.start_relays(
            &current_task.kernel(),
            EventProxyMode::None,
            touch_source_client_end,
            keyboard_proxy,
            mouse_source_client_end,
            view_ref_pair.view_ref,
            device_registry_proxy,
            input_device.open_files.clone(),
            Default::default(),
            Default::default(),
            Some(input_device.inspect_status.clone()),
            None,
            None,
        );

        let _ = init_keyboard_listener(&mut keyboard_stream).await;
        let _ = init_button_listeners(&mut device_listener_stream).await;

        relay_handle.add_touch_device(
            0,
            input_device.open_files.clone(),
            Some(input_device.inspect_status.clone()),
        );

        // Send 2 TouchEvents to proxy that should be counted as `received` by InputFile
        // A TouchEvent::default() has no pointer sample so these events should be discarded.
        match touch_source_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responder, .. })) => responder
                .send(vec![make_empty_touch_event(), make_empty_touch_event()])
                .expect("failure sending Watch reply"),
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }

        // Wait for next `Watch` call and verify it has two elements in `responses`.
        match touch_source_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responses, responder })) => {
                assert_matches!(responses.as_slice(), [_, _]);
                responder.send(vec![]).expect("failure sending Watch reply");
            }
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }

        // Verify file node & properties remain in inspect tree when file is closed
        input_device.open_files.lock().clear();
        drop(input_file_0);

        // Open new file which should receive input_device's subsequent events
        let input_file_1 =
            input_device.open_test(locked, &current_task).expect("Failed to create input file");

        // Send 5 TouchEvents with pointer sample to proxy, these should be received and converted
        // Add/Remove events generate 5 uapi events each. Change events generate 3 uapi events each.
        match touch_source_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responder, .. })) => {
                responder
                    .send(vec![
                        make_touch_event_with_coords_phase_timestamp(
                            0.0,
                            0.0,
                            EventPhase::Add,
                            1,
                            1000,
                        ),
                        make_touch_event_with_coords_phase_timestamp(
                            1.0,
                            1.0,
                            EventPhase::Change,
                            1,
                            2000,
                        ),
                        make_touch_event_with_coords_phase_timestamp(
                            2.0,
                            2.0,
                            EventPhase::Change,
                            1,
                            3000,
                        ),
                        make_touch_event_with_coords_phase_timestamp(
                            3.0,
                            3.0,
                            EventPhase::Change,
                            1,
                            4000,
                        ),
                        make_touch_event_with_coords_phase_timestamp(
                            3.0,
                            3.0,
                            EventPhase::Remove,
                            1,
                            5000,
                        ),
                    ])
                    .expect("failure sending Watch reply");
            }
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }

        // Wait for next `Watch` call and verify it has five elements in `responses`.
        match touch_source_stream.next().await {
            Some(Ok(TouchSourceRequest::Watch { responses, .. })) => {
                assert_matches!(responses.as_slice(), [_, _, _, _, _])
            }
            unexpected_request => panic!("unexpected request {:?}", unexpected_request),
        }

        let _events = read_uapi_events(locked, &input_file_1, &current_task);

        // Verify file node & properties remain in inspect tree when file is closed
        input_device.open_files.lock().clear();
        drop(input_file_1);

        assert_data_tree!(inspector, root: {
            touch_device: {
                active_wake_leases_count: 0u64,
                total_events_with_wake_lease_count: 0u64,
                total_fidl_events_received_count: 7u64,
                total_fidl_events_ignored_count: 2u64,
                total_fidl_events_unexpected_count: 0u64,
                total_fidl_events_converted_count: 5u64,
                total_uapi_events_generated_count: 22u64,
                last_generated_uapi_event_timestamp_ns: 5000i64,
                touch_file_0: {
                    fidl_events_received_count: 2u64,
                    fidl_events_ignored_count: 2u64,
                    fidl_events_unexpected_count: 0u64,
                    fidl_events_converted_count: 0u64,
                    uapi_events_generated_count: 0u64,
                    uapi_events_read_count: 0u64,
                    fd_read_count: 0u64,
                    fd_notify_count: 0u64,
                    last_generated_uapi_event_timestamp_ns: 0i64,
                    last_read_uapi_event_timestamp_ns: 0i64,
                    opened_without_nonblock: AnyProperty,
                    open_timestamp_ns: AnyProperty,
                    closed: AnyProperty,
                    close_timestamp_ns: AnyProperty,
                },
                touch_file_1: {
                    fidl_events_received_count: 5u64,
                    fidl_events_ignored_count: 0u64,
                    fidl_events_unexpected_count: 0u64,
                    fidl_events_converted_count: 5u64,
                    uapi_events_generated_count: 22u64,
                    uapi_events_read_count: 22u64,
                    fd_read_count: 23u64,
                    fd_notify_count: 1u64,
                    last_generated_uapi_event_timestamp_ns: 5000i64,
                    last_read_uapi_event_timestamp_ns: 5000i64,
                    opened_without_nonblock: AnyProperty,
                    open_timestamp_ns: AnyProperty,
                    closed: AnyProperty,
                    close_timestamp_ns: AnyProperty,
                },
            }
        });
    }

    #[::fuchsia::test]
    async fn file_status_inspect_not_empty_after_close() {
        let inspector = fuchsia_inspect::Inspector::default();
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();

        let input_device = InputDevice::new_touch(700, 700, inspector.root());
        let file_handle =
            input_device.open_test(locked, &current_task).expect("Failed to create input file");

        let status = file_handle
            .downcast_file::<crate::input_file::ArcInputFile>()
            .unwrap()
            .0
            .inspect_status
            .clone()
            .expect("touch file must have status");
        status.count_received_events(5);

        assert_data_tree!(inspector, root: {
            touch_device: {
                active_wake_leases_count: AnyProperty,
                last_generated_uapi_event_timestamp_ns: AnyProperty,
                total_events_with_wake_lease_count: AnyProperty,
                total_fidl_events_converted_count: AnyProperty,
                total_fidl_events_ignored_count: AnyProperty,
                total_fidl_events_received_count: AnyProperty,
                total_fidl_events_unexpected_count: AnyProperty,
                total_uapi_events_generated_count: AnyProperty,
                touch_file_0: {
                    fidl_events_received_count: 5u64,
                    fd_notify_count: 0u64,
                    fd_read_count: 0u64,
                    fidl_events_converted_count: 0u64,
                    fidl_events_ignored_count: 0u64,
                    fidl_events_unexpected_count: 0u64,
                    last_generated_uapi_event_timestamp_ns: 0i64,
                    last_read_uapi_event_timestamp_ns: 0i64,
                    uapi_events_generated_count: 0u64,
                    uapi_events_read_count: 0u64,
                    opened_without_nonblock: true,
                    open_timestamp_ns: AnyProperty,
                    closed: false,
                    close_timestamp_ns: 0i64,
                }
            }
        });

        drop(status);
        input_device.open_files.lock().clear();
        drop(file_handle);

        assert_data_tree!(inspector, root: {
            touch_device: {
                active_wake_leases_count: AnyProperty,
                last_generated_uapi_event_timestamp_ns: AnyProperty,
                total_events_with_wake_lease_count: AnyProperty,
                total_fidl_events_converted_count: AnyProperty,
                total_fidl_events_ignored_count: AnyProperty,
                total_fidl_events_received_count: AnyProperty,
                total_fidl_events_unexpected_count: AnyProperty,
                total_uapi_events_generated_count: AnyProperty,
                touch_file_0: {
                    fidl_events_received_count: 5u64,
                    fd_notify_count: 0u64,
                    fd_read_count: 0u64,
                    fidl_events_converted_count: 0u64,
                    fidl_events_ignored_count: 0u64,
                    fidl_events_unexpected_count: 0u64,
                    last_generated_uapi_event_timestamp_ns: 0i64,
                    last_read_uapi_event_timestamp_ns: 0i64,
                    uapi_events_generated_count: 0u64,
                    uapi_events_read_count: 0u64,
                    opened_without_nonblock: true,
                    open_timestamp_ns: AnyProperty,
                    closed: AnyProperty,
                    close_timestamp_ns: AnyProperty,
                }
            }
        });
    }

    #[::fuchsia::test]
    async fn keyboard_input_initialized_with_inspect_node() {
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let inspector = fuchsia_inspect::Inspector::default();
        let keyboard_device = InputDevice::new_keyboard(&inspector.root());
        let _file_obj = keyboard_device.open_test(locked, &current_task);

        assert_data_tree!(inspector, root: {
            keyboard_device: {
                active_wake_leases_count: 0u64,
                total_events_with_wake_lease_count: 0u64,
                total_fidl_events_received_count: 0u64,
                total_fidl_events_ignored_count: 0u64,
                total_fidl_events_unexpected_count: 0u64,
                total_fidl_events_converted_count: 0u64,
                total_uapi_events_generated_count: 0u64,
                last_generated_uapi_event_timestamp_ns: 0i64,
                keyboard_file_0: {
                    fidl_events_received_count: 0u64,
                    fidl_events_ignored_count: 0u64,
                    fidl_events_unexpected_count: 0u64,
                    fidl_events_converted_count: 0u64,
                    uapi_events_generated_count: 0u64,
                    uapi_events_read_count: 0u64,
                    fd_read_count: 0u64,
                    fd_notify_count: 0u64,
                    last_generated_uapi_event_timestamp_ns: 0i64,
                    last_read_uapi_event_timestamp_ns: 0i64,
                    opened_without_nonblock: AnyProperty,
                    open_timestamp_ns: AnyProperty,
                    closed: AnyProperty,
                    close_timestamp_ns: AnyProperty,
                }
            }
        });
    }

    #[::fuchsia::test]
    async fn button_relay_updates_keyboard_inspect_status() {
        let inspector = fuchsia_inspect::Inspector::default();
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, buttons_listener) =
            start_button_input_inspect(locked, &current_task, &inspector).await;

        // Each of these events should count toward received and converted.
        // They also generate 2 uapi events each.
        let power_event = MediaButtonsEvent {
            volume: Some(0),
            mic_mute: Some(false),
            pause: Some(false),
            camera_disable: Some(false),
            power: Some(true),
            function: Some(false),
            ..Default::default()
        };

        let power_release_event = MediaButtonsEvent {
            volume: Some(0),
            mic_mute: Some(false),
            pause: Some(false),
            camera_disable: Some(false),
            power: Some(false),
            function: Some(false),
            ..Default::default()
        };

        let _ = buttons_listener.on_event(power_event).await;
        let _ = buttons_listener.on_event(power_release_event).await;

        let events = read_uapi_events(locked, &input_file, &current_task);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].code, uapi::KEY_POWER as u16);
        assert_eq!(events[0].value, 1);
        assert_eq!(events[2].code, uapi::KEY_POWER as u16);
        assert_eq!(events[2].value, 0);

        let _events = read_uapi_events(locked, &input_file, &current_task);

        assert_data_tree!(inspector, root: {
            keyboard_device: {
                active_wake_leases_count: 0u64,
                total_events_with_wake_lease_count: 0u64,
                total_fidl_events_received_count: 2u64,
                total_fidl_events_ignored_count: 0u64,
                total_fidl_events_unexpected_count: 0u64,
                total_fidl_events_converted_count: 2u64,
                total_uapi_events_generated_count: 4u64,
                // Button events perform a realtime clockread, so any value will do.
                last_generated_uapi_event_timestamp_ns: AnyProperty,
                keyboard_file_0: {
                    fidl_events_received_count: 2u64,
                    fidl_events_ignored_count: 0u64,
                    fidl_events_unexpected_count: 0u64,
                    fidl_events_converted_count: 2u64,
                    uapi_events_generated_count: 4u64,
                    uapi_events_read_count: 4u64,
                    fd_read_count: 6u64,
                    fd_notify_count: 2u64,
                    // Button events perform a realtime clockread, so any value will do.
                    last_generated_uapi_event_timestamp_ns: AnyProperty,
                    last_read_uapi_event_timestamp_ns: AnyProperty,
                    opened_without_nonblock: AnyProperty,
                    open_timestamp_ns: AnyProperty,
                    closed: AnyProperty,
                    close_timestamp_ns: AnyProperty,
                },
            }
        });
    }

    #[::fuchsia::test]
    async fn mouse_input_initialized_with_inspect_node() {
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let inspector = fuchsia_inspect::Inspector::default();
        let mouse_device = InputDevice::new_mouse(&inspector.root());
        let _file_obj = mouse_device.open_test(locked, &current_task);

        assert_data_tree!(inspector, root: {
            mouse_device: {
                active_wake_leases_count: 0u64,
                total_events_with_wake_lease_count: 0u64,
                total_fidl_events_received_count: 0u64,
                total_fidl_events_ignored_count: 0u64,
                total_fidl_events_unexpected_count: 0u64,
                total_fidl_events_converted_count: 0u64,
                total_uapi_events_generated_count: 0u64,
                last_generated_uapi_event_timestamp_ns: 0i64,
                mouse_file_0: {
                    fidl_events_received_count: 0u64,
                    fidl_events_ignored_count: 0u64,
                    fidl_events_unexpected_count: 0u64,
                    fidl_events_converted_count: 0u64,
                    uapi_events_generated_count: 0u64,
                    uapi_events_read_count: 0u64,
                    fd_read_count: 0u64,
                    fd_notify_count: 0u64,
                    last_generated_uapi_event_timestamp_ns: 0i64,
                    last_read_uapi_event_timestamp_ns: 0i64,
                    opened_without_nonblock: AnyProperty,
                    open_timestamp_ns: AnyProperty,
                    closed: AnyProperty,
                    close_timestamp_ns: AnyProperty,
                }
            }
        });
    }

    #[::fuchsia::test]
    async fn mouse_relay_updates_mouse_inspect_status() {
        let inspector = fuchsia_inspect::Inspector::default();
        #[allow(deprecated, reason = "pre-existing usage")]
        let (_kernel, current_task, locked) = create_kernel_task_and_unlocked();
        let (_input_device, input_file, mut mouse_source_stream) =
            start_mouse_input_inspect(locked, &current_task, &inspector).await;

        let mouse_move_event = fuipointer::MouseEvent {
            timestamp: Some(0),
            pointer_sample: Some(fuipointer::MousePointerSample {
                device_id: Some(0),
                position_in_viewport: Some([50.0, 50.0]),
                scroll_v: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mouse_click_event = fuipointer::MouseEvent {
            timestamp: Some(0),
            pointer_sample: Some(fuipointer::MousePointerSample {
                device_id: Some(0),
                scroll_v: Some(0),
                pressed_buttons: Some(vec![1]),
                ..Default::default()
            }),
            ..Default::default()
        };

        // Send 2 non-wheel MouseEvents to proxy that should be counted as `received` by InputFile
        // These events have no scroll_v delta in the pointer sample so they should be ignored.
        answer_next_mouse_watch_request(
            &mut mouse_source_stream,
            vec![mouse_move_event, mouse_click_event],
        )
        .await;

        // Send 5 MouseEvents with non-zero scroll_v delta to proxy, these should be received and
        // converted to 1 uapi event each, with an extra sync event to signify end of the batch.
        answer_next_mouse_watch_request(
            &mut mouse_source_stream,
            (0..5).map(|_| make_mouse_wheel_event(1)).collect(),
        )
        .await;

        // Send a final mouse wheel event and ensure the inspect tree reflects it's timestamp under
        // last_generated_uapi_event_timestamp_ns and last_read_uapi_event_timestamp_ns.
        answer_next_mouse_watch_request(
            &mut mouse_source_stream,
            vec![make_mouse_wheel_event_with_timestamp(-1, 5000)],
        )
        .await;

        // Wait for another `Watch` to ensure mouse_file is done processing the other replies.
        // Use an empty vec, to ensure no unexpected `uapi::input_event`s are created.
        answer_next_mouse_watch_request(&mut mouse_source_stream, vec![]).await;

        let _events = read_uapi_events(locked, &input_file, &current_task);
        assert_data_tree!(inspector, root: {
            mouse_device: {
                active_wake_leases_count: 0u64,
                total_events_with_wake_lease_count: 0u64,
                total_fidl_events_received_count: 8u64,
                total_fidl_events_ignored_count: 2u64,
                total_fidl_events_unexpected_count: 0u64,
                total_fidl_events_converted_count: 6u64,
                total_uapi_events_generated_count: 4u64,
                last_generated_uapi_event_timestamp_ns: 5000i64,
                mouse_file_0: {
                    fidl_events_received_count: 8u64,
                    fidl_events_ignored_count: 2u64,
                    fidl_events_unexpected_count: 0u64,
                    fidl_events_converted_count: 6u64,
                    uapi_events_generated_count: 4u64,
                    uapi_events_read_count: 4u64,
                    fd_read_count: 5u64,
                    fd_notify_count: 2u64,
                    last_generated_uapi_event_timestamp_ns: 5000i64,
                    last_read_uapi_event_timestamp_ns: 5000i64,
                    opened_without_nonblock: AnyProperty,
                    open_timestamp_ns: AnyProperty,
                    closed: AnyProperty,
                    close_timestamp_ns: AnyProperty,
                },
            }
        });
    }
}
