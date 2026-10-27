// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::InputEventsRelayHandle;
use crate::uinput::vfs::{CloseFreeSafe, NamespaceNode};

use bit_vec::BitVec;
use fidl_fuchsia_ui_test_input::{
    self as futinput, CoordinateUnit, DisplayDimensions, KeyboardSimulateKeyEventRequest,
    RegistryRegisterKeyboardAndGetDeviceInfoRequest,
    RegistryRegisterTouchScreenAndGetDeviceInfoRequest,
};

use starnix_core::device::kobject::DeviceMetadata;
use starnix_core::device::{DeviceMode, DeviceOps};
use starnix_core::fileops_impl_seekless;
use starnix_core::mm::MemoryAccessorExt;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::{self, FileObject, FileOps, fileops_impl_noop_sync};
use starnix_logging::log_warn;
use starnix_modules_input_app_event_conversion::key_linux_to_fuchsia::LinuxKeyboardEventParser;
use starnix_modules_input_app_event_conversion::touch_linux_to_fuchsia::LinuxTouchEventParser;
use starnix_sync::{LockDepMutex, UinputDeviceStateLock};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};

use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::{MultiArchUserRef, UserRef};
use starnix_uapi::{device_id, errno, error, uapi};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

// Return the current uinput API version 5, it also told caller this uinput
// supports UI_DEV_SETUP.
const UINPUT_VERSION: u32 = 5;

const DEFAULT_TOUCHSCREEN_WIDTH: i32 = 1280;
const DEFAULT_TOUCHSCREEN_HEIGHT: i32 = 800;
const DEFAULT_UINPUT_DEVICE_NAME: &str = "starnix_uinput";

type InputEventPtr = MultiArchUserRef<uapi::input_event, uapi::arch32::input_event>;

#[derive(Clone)]
enum DeviceId {
    Keyboard,
    Touchscreen,
}

pub fn register_uinput_device(
    kernel: &Kernel,
    input_relay_handle: Arc<InputEventsRelayHandle>,
) -> Result<(), Errno> {
    let registry = &kernel.device_registry;
    let misc_class = registry.objects.misc_class();
    let device = UinputDevice::new(input_relay_handle);
    registry.register_device(
        kernel,
        "uinput".into(),
        DeviceMetadata::new("uinput".into(), device_id::DeviceId::UINPUT, DeviceMode::Char),
        misc_class,
        device,
    );
    Ok(())
}
#[derive(Clone)]
struct UinputDevice {
    input_relay_handle: Arc<InputEventsRelayHandle>,
}

impl UinputDevice {
    pub fn new(input_relay_handle: Arc<InputEventsRelayHandle>) -> Self {
        Self { input_relay_handle }
    }
}

impl DeviceOps for UinputDevice {
    fn open(
        &self,
        _current_task: &CurrentTask,
        _id: device_id::DeviceId,
        _node: &NamespaceNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(UinputDeviceFile::new(self.input_relay_handle.clone())))
    }
}

enum CreatedDevice {
    None,
    Keyboard(futinput::KeyboardSynchronousProxy, LinuxKeyboardEventParser),
    // LinuxTouchEventParser need to boxed to avoid warning: large-enum-variant.
    Touchscreen(futinput::TouchScreenSynchronousProxy, Box<LinuxTouchEventParser>),
}

#[derive(Clone, Copy)]
struct Range {
    min: i32,
    max: i32,
}
struct UinputDeviceMutableState {
    enabled_evbits: BitVec,
    input_id: Option<uapi::input_id>,
    name: Option<String>,
    created_device: CreatedDevice,
    x_range: Option<Range>,
    y_range: Option<Range>,
}

impl UinputDeviceMutableState {
    fn get_id_and_device_type(&self) -> Option<(uapi::input_id, DeviceId)> {
        let input_id = match self.input_id {
            Some(input_id) => input_id,
            None => return None,
        };
        // Currently only support Keyboard and Touchscreen, if evbits contains
        // EV_ABS, consider it is Touchscreen. This need to be revisit when we
        // want to support more device types.
        let device_type = match self.enabled_evbits.get(uapi::EV_ABS as usize) {
            Some(true) => DeviceId::Touchscreen,
            Some(false) | None => DeviceId::Keyboard,
        };

        Some((input_id, device_type))
    }

    fn teardown_device(&mut self) -> bool {
        if matches!(self.created_device, CreatedDevice::None) {
            return false;
        }
        self.created_device = CreatedDevice::None;
        destroy_device();
        true
    }
}

struct UinputDeviceFile {
    inner: LockDepMutex<UinputDeviceMutableState, UinputDeviceStateLock>,
    input_relay_handle: Arc<InputEventsRelayHandle>,
}

impl UinputDeviceFile {
    pub fn new(input_relay_handle: Arc<InputEventsRelayHandle>) -> Self {
        Self {
            inner: UinputDeviceMutableState {
                enabled_evbits: BitVec::from_elem(uapi::EV_CNT as usize, false),
                input_id: None,
                name: None,
                created_device: CreatedDevice::None,
                x_range: None,
                y_range: None,
            }
            .into(),
            input_relay_handle,
        }
    }

    /// UI_SET_EVBIT caller pass a u32 as the event type "EV_*" to set this
    /// uinput device may handle events with the given event type.
    fn ui_set_evbit(&self, arg: SyscallArg) -> Result<SyscallResult, Errno> {
        let evbit: u32 = arg.into();
        match evbit {
            uapi::EV_KEY | uapi::EV_ABS => {
                let mut inner = self.inner.lock();
                if !matches!(inner.created_device, CreatedDevice::None) {
                    return error!(EINVAL);
                }
                inner.enabled_evbits.set(evbit as usize, true);
                Ok(SUCCESS)
            }
            _ => {
                log_warn!("UI_SET_EVBIT with unsupported evbit {}", evbit);
                error!(EPERM)
            }
        }
    }

    /// UI_ABS_SETUP caller pass in event codes and min and max value for
    /// the event code.
    fn ui_abs_setup(
        &self,
        current_task: &CurrentTask,
        abs_setup: UserRef<starnix_uapi::uinput_abs_setup>,
    ) -> Result<SyscallResult, Errno> {
        let setup: starnix_uapi::uinput_abs_setup = current_task.read_object(abs_setup)?;
        if setup.absinfo.minimum >= setup.absinfo.maximum {
            return error!(EINVAL);
        }
        let code: u32 = setup.code.into();
        match code {
            uapi::ABS_MT_POSITION_X => {
                let mut inner = self.inner.lock();
                if !matches!(inner.created_device, CreatedDevice::None) {
                    return error!(EINVAL);
                }
                inner.x_range =
                    Some(Range { min: setup.absinfo.minimum, max: setup.absinfo.maximum });
            }
            uapi::ABS_MT_POSITION_Y => {
                let mut inner = self.inner.lock();
                if !matches!(inner.created_device, CreatedDevice::None) {
                    return error!(EINVAL);
                }
                inner.y_range =
                    Some(Range { min: setup.absinfo.minimum, max: setup.absinfo.maximum });
            }
            _ => {
                log_warn!("UI_ABS_SETUP ignore event code {}", setup.code);
            }
        }
        Ok(SUCCESS)
    }

    /// UI_GET_VERSION caller pass a address for u32 to `arg` to receive the
    /// uinput version. ioctl returns SUCCESS(0) for success calls, and
    /// EFAULT(14) for given address is null.
    fn ui_get_version(
        &self,
        current_task: &CurrentTask,
        user_version: UserRef<u32>,
    ) -> Result<SyscallResult, Errno> {
        let response: u32 = UINPUT_VERSION;
        current_task.write_object(user_version, &response)?;
        Ok(SUCCESS)
    }

    /// UI_DEV_SETUP set the name of device and input_id (bustype, vendor id,
    /// product id, version) to the uinput device.
    fn ui_dev_setup(
        &self,
        current_task: &CurrentTask,
        user_uinput_setup: UserRef<uapi::uinput_setup>,
    ) -> Result<SyscallResult, Errno> {
        let uinput_setup = current_task.read_object(user_uinput_setup)?;
        let mut inner = self.inner.lock();
        if !matches!(inner.created_device, CreatedDevice::None) {
            return error!(EINVAL);
        }
        inner.input_id = Some(uinput_setup.id);
        // Parse name from null-terminated C string in uinput_setup.name
        let name_bytes: &[u8] = zerocopy::IntoBytes::as_bytes(&uinput_setup.name);
        let name_len = name_bytes.iter().position(|&c| c == 0).unwrap_or(name_bytes.len());
        let name = std::str::from_utf8(&name_bytes[..name_len])
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| DEFAULT_UINPUT_DEVICE_NAME.to_string());
        inner.name = Some(name);
        Ok(SUCCESS)
    }

    fn ui_dev_create(&self, _current_task: &CurrentTask) -> Result<SyscallResult, Errno> {
        {
            let inner = self.inner.lock();
            if !matches!(inner.created_device, CreatedDevice::None) {
                return error!(EINVAL);
            }
            if inner.get_id_and_device_type().is_none() {
                return error!(EINVAL);
            }
        }

        // Only eng and userdebug builds include the `fuchsia.ui.test.input` service.
        let registry = match fuchsia_component::client::connect_to_protocol_sync::<
            futinput::RegistryMarker,
        >() {
            Ok(proxy) => Some(proxy),
            Err(_) => {
                log_warn!("Could not connect to fuchsia.ui.test.input/Registry");
                None
            }
        };
        self.ui_dev_create_inner(registry)
    }

    /// UI_DEV_CREATE calls create the uinput device with given information
    /// from previous ioctl() calls.
    fn ui_dev_create_inner(
        &self,
        // Takes `registry` arg so we can manually inject a mock registry in unit tests.
        registry: Option<futinput::RegistrySynchronousProxy>,
    ) -> Result<SyscallResult, Errno> {
        let (input_id, device_type, name, x_range, y_range) = {
            let inner = self.inner.lock();
            if !matches!(inner.created_device, CreatedDevice::None) {
                return error!(EINVAL);
            }

            let (input_id, device_type) = match inner.get_id_and_device_type() {
                Some((id, dev)) => (id, dev),
                None => return error!(EINVAL),
            };
            let name = inner.name.clone().unwrap_or_else(|| DEFAULT_UINPUT_DEVICE_NAME.to_string());
            (input_id, device_type, name, inner.x_range, inner.y_range)
        };

        let proxy = match registry {
            Some(proxy) => proxy,
            None => {
                log_warn!("No Registry available for Uinput.");
                return error!(EPERM);
            }
        };

        let (created_device, device_id) = match device_type {
            DeviceId::Keyboard => {
                let (key_client, key_server) =
                    fidl::endpoints::create_sync_proxy::<futinput::KeyboardMarker>();

                // Register a keyboard
                let register_res = proxy.register_keyboard_and_get_device_info(
                    RegistryRegisterKeyboardAndGetDeviceInfoRequest {
                        device: Some(key_server),
                        ..Default::default()
                    },
                    zx::MonotonicInstant::INFINITE,
                );

                match register_res {
                    Ok(resp) => match resp.device_id {
                        Some(device_id) => {
                            let created_device = CreatedDevice::Keyboard(
                                key_client,
                                LinuxKeyboardEventParser::create(),
                            );
                            Ok((created_device, device_id))
                        }
                        None => {
                            log_warn!(
                                "register_keyboard_and_get_device_info response does not include a device_id"
                            );
                            error!(EPERM)
                        }
                    },
                    Err(e) => {
                        log_warn!("Uinput could not register Keyboard device to Registry: {:?}", e);
                        error!(EPERM)
                    }
                }
            }
            DeviceId::Touchscreen => {
                let (touch_client, touch_server) =
                    fidl::endpoints::create_sync_proxy::<futinput::TouchScreenMarker>();

                let (display_width, display_height) =
                    (*self.input_relay_handle.display_size.lock())
                        .filter(|&(w, h)| w > 0 && h > 0)
                        .unwrap_or((DEFAULT_TOUCHSCREEN_WIDTH, DEFAULT_TOUCHSCREEN_HEIGHT));
                let (min_x, max_x) = x_range.map(|r| (r.min, r.max)).unwrap_or((0, display_width));
                let (min_y, max_y) = y_range.map(|r| (r.min, r.max)).unwrap_or((0, display_height));
                if min_x >= max_x || min_y >= max_y {
                    return error!(EINVAL);
                }

                let request = RegistryRegisterTouchScreenAndGetDeviceInfoRequest {
                    device: Some(touch_server),
                    coordinate_unit: Some(CoordinateUnit::RegisteredDimensions),
                    display_dimensions: Some(DisplayDimensions {
                        min_x: min_x.into(),
                        max_x: max_x.into(),
                        min_y: min_y.into(),
                        max_y: max_y.into(),
                    }),
                    ..Default::default()
                };

                // Register a touchscreen
                let register_res = proxy.register_touch_screen_and_get_device_info(
                    request,
                    zx::MonotonicInstant::INFINITE,
                );

                match register_res {
                    Ok(resp) => match resp.device_id {
                        Some(device_id) => {
                            let created_device = CreatedDevice::Touchscreen(
                                touch_client,
                                Box::new(LinuxTouchEventParser::create()),
                            );
                            Ok((created_device, device_id))
                        }
                        None => {
                            log_warn!(
                                "register_touch_screen_and_get_device_info response does not include a device_id"
                            );
                            error!(EPERM)
                        }
                    },
                    Err(e) => {
                        log_warn!(
                            "Uinput could not register TouchScreen device to Registry: {:?}",
                            e
                        );
                        error!(EPERM)
                    }
                }
            }
        }?;

        {
            let mut inner = self.inner.lock();
            if !matches!(inner.created_device, CreatedDevice::None) {
                return error!(EINVAL);
            }
            inner.created_device = created_device;
        }

        // NOTE: The UI test registry often broadcasts `OnDeviceChanged(Added)` to
        // `input_event_relay.rs` before `register_*_and_get_device_info` returns
        // `device_id` here. When this happens, `register_and_add_device` initializes
        // `/dev/input/eventX` and broadcasts the `KOBJECT_ADD` uevent with fallback IDs
        // (`TOUCH_INPUT_ID` / `KEYBOARD_INPUT_ID` and `"starnix_touch"` / `"starnix_buttons"`).
        //
        // This is safe for synchronous callers that open the device node after `UI_DEV_CREATE`
        // returns, because both paths share an `Arc<LockDepMutex<InputDeviceInfo, ...>>`,
        // allowing `update_uinput_device` to overwrite those fallbacks before userspace opens
        // the device node.
        //
        // However, there is a race window for asynchronous watchers (such as `ueventd`,
        // `libinput`, or Android's `EventHub` reacting directly to the `KOBJECT_ADD` uevent
        // or an inotify event on `/dev/input`): if they open `/dev/input/eventX` and query
        // `EVIOCGID` or `EVIOCGNAME` before `update_uinput_device` executes below, they may
        // observe the fallback IDs and names.
        //
        // TODO(https://fxbug.dev/502662433): Update `fuchsia.ui.test.input.Registry` so that
        // caller-provided device metadata (e.g. name, input_id) can be passed during initial
        // registration. This will allow `OnDeviceChanged(Added)` to immediately carry the
        // correct metadata and eliminate the race window for asynchronous uevent watchers.
        //
        // Device metadata in `device_infos` remains alive until the device is removed via
        // `OnDeviceChanged(Removed)`, preventing race conditions where an early `UI_DEV_DESTROY`
        // could cause an in-flight addition to lose its metadata.
        self.input_relay_handle.update_uinput_device(device_id, input_id, name);
        new_device();

        Ok(SUCCESS)
    }

    fn ui_dev_destroy(&self, _current_task: &CurrentTask) -> Result<SyscallResult, Errno> {
        let mut inner = self.inner.lock();
        if !inner.teardown_device() {
            return error!(EPERM);
        }
        Ok(SUCCESS)
    }
}

// TODO(b/312467059): Remove once ESC -> Power workaround can be remove.
static COUNT_OF_UINPUT_DEVICE: AtomicI32 = AtomicI32::new(0);

fn new_device() {
    let _ = COUNT_OF_UINPUT_DEVICE.fetch_add(1, Ordering::SeqCst);
}

fn destroy_device() {
    let _ = COUNT_OF_UINPUT_DEVICE.fetch_sub(1, Ordering::SeqCst);
}

pub fn uinput_running() -> bool {
    COUNT_OF_UINPUT_DEVICE.load(Ordering::SeqCst) > 0
}

/// `UinputDeviceFile` doesn't implement the `close` method.
impl CloseFreeSafe for UinputDeviceFile {}

impl Drop for UinputDeviceFile {
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        inner.teardown_device();
    }
}

impl FileOps for UinputDeviceFile {
    fileops_impl_seekless!();
    fileops_impl_noop_sync!();

    fn ioctl(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        match request {
            uapi::UI_GET_VERSION => self.ui_get_version(current_task, arg.into()),
            uapi::UI_SET_EVBIT => self.ui_set_evbit(arg),
            uapi::UI_ABS_SETUP => self.ui_abs_setup(current_task, arg.into()),
            // `fuchsia.ui.test.input.Registry` does not use some uinput ioctl
            // request, just ignore the request and return SUCCESS, even args
            // is invalid.
            uapi::UI_SET_KEYBIT
            | uapi::UI_SET_ABSBIT
            | uapi::UI_SET_PHYS
            | uapi::UI_SET_PROPBIT => Ok(SUCCESS),
            uapi::UI_DEV_SETUP => self.ui_dev_setup(current_task, arg.into()),
            uapi::UI_DEV_CREATE => self.ui_dev_create(current_task),
            uapi::UI_DEV_DESTROY => self.ui_dev_destroy(current_task),
            // default_ioctl() handles file system related requests and reject
            // others.
            _ => {
                log_warn!("receive unknown ioctl request: {:?}", request);
                error!(ENOTTY)
            }
        }
    }

    fn write(
        &self,
        _file: &vfs::FileObject,
        current_task: &starnix_core::task::CurrentTask,
        _offset: usize,
        data: &mut dyn vfs::buffers::InputBuffer,
    ) -> Result<usize, Errno> {
        let content = data.read_all()?;
        let event =
            InputEventPtr::read_from_prefix(current_task, &content).map_err(|_| errno!(EINVAL))?;
        let mut inner = self.inner.lock();

        match &mut inner.created_device {
            CreatedDevice::Keyboard(proxy, parser) => {
                let input_report = parser.handle(event);
                match input_report {
                    Ok(Some(report)) => {
                        if let Some(keyboard_report) = report.keyboard {
                            let res = proxy.simulate_key_event(
                                &KeyboardSimulateKeyEventRequest {
                                    pressed_keys: keyboard_report.pressed_keys3,
                                    ..Default::default()
                                },
                                zx::MonotonicInstant::INFINITE,
                            );
                            if res.is_err() {
                                return error!(EIO);
                            }
                        }
                    }
                    Ok(None) => (),
                    Err(e) => return Err(e),
                }
            }
            CreatedDevice::Touchscreen(proxy, parser) => {
                let input_report: Result<Option<futinput::TouchInputReport>, Errno> =
                    parser.handle(event);
                match input_report {
                    Ok(Some(report)) => {
                        let res =
                            proxy.simulate_touch_event(&report, zx::MonotonicInstant::INFINITE);
                        if res.is_err() {
                            return error!(EIO);
                        }
                    }
                    Ok(None) => (),
                    Err(e) => return Err(e),
                }
            }
            CreatedDevice::None => return error!(EINVAL),
        }

        Ok(content.len())
    }

    fn read(
        &self,
        _file: &vfs::FileObject,
        _current_task: &starnix_core::task::CurrentTask,
        _offset: usize,
        _data: &mut dyn vfs::buffers::OutputBuffer,
    ) -> Result<usize, Errno> {
        log_warn!("uinput FD does not support read().");
        error!(EINVAL)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{EventProxyMode, start_input_relays_for_test};
    use futures::TryStreamExt;
    use starnix_core::testing::{map_memory, spawn_kernel_and_run};
    use starnix_core::vfs::FileHandle;
    use starnix_uapi::user_address::UserAddress;
    use std::sync::Arc;
    use test_case::test_case;

    static UINPUT_RUNNING_LOCK: starnix_sync::Mutex<()> = starnix_sync::Mutex::new(());

    fn ensure_uinput_running_lock() -> starnix_sync::MutexGuard<'static, ()> {
        UINPUT_RUNNING_LOCK.lock()
    }

    async fn new_kernel_objects(current_task: &CurrentTask) -> (Arc<UinputDeviceFile>, FileHandle) {
        let (input_relay_handle, _, _, _, _, _, _, _, _, _, _, _) =
            start_input_relays_for_test(current_task, EventProxyMode::None).await;
        let dev = Arc::new(UinputDeviceFile::new(input_relay_handle));

        let root_namespace_node = current_task
            .lookup_path_from_root(".".into())
            .expect("failed to get namespace node for root");

        let file_object = FileObject::new(
            current_task,
            Box::new(dev.clone()),
            // The input node doesn't really live at the root of the filesystem.
            // But the test doesn't need to be 100% representative of production.
            root_namespace_node,
            OpenFlags::empty(),
        )
        .expect("FileObject::new failed");
        (dev, file_object)
    }

    #[test_case(uapi::EV_KEY, vec![uapi::EV_KEY as usize] => Ok(SUCCESS))]
    #[test_case(uapi::EV_ABS, vec![uapi::EV_ABS as usize] => Ok(SUCCESS))]
    #[test_case(uapi::EV_REL, vec![] => error!(EPERM))]
    #[::fuchsia::test]
    async fn ui_set_evbit(bit: u32, expected_evbits: Vec<usize>) -> Result<SyscallResult, Errno> {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_SET_EVBIT,
                SyscallArg::from(bit as u64),
            );
            for expected_evbit in expected_evbits {
                assert!(dev.inner.lock().enabled_evbits.get(expected_evbit).unwrap());
            }
            r
        })
        .await
    }

    #[::fuchsia::test]
    async fn ui_set_evbit_call_multi() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_SET_EVBIT,
                SyscallArg::from(uapi::EV_KEY as u64),
            );
            assert_eq!(r, Ok(SUCCESS));
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_SET_EVBIT,
                SyscallArg::from(uapi::EV_ABS as u64),
            );
            assert_eq!(r, Ok(SUCCESS));
            assert!(dev.inner.lock().enabled_evbits.get(uapi::EV_KEY as usize).unwrap());
            assert!(dev.inner.lock().enabled_evbits.get(uapi::EV_ABS as usize).unwrap());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_set_keybit() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_SET_KEYBIT,
                SyscallArg::from(uapi::BTN_TOUCH as u64),
            );
            assert_eq!(r, Ok(SUCCESS));

            // also test call multi times.
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_SET_KEYBIT,
                SyscallArg::from(uapi::KEY_SPACE as u64),
            );
            assert_eq!(r, Ok(SUCCESS));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_set_absbit() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_SET_ABSBIT,
                SyscallArg::from(uapi::ABS_MT_SLOT as u64),
            );
            assert_eq!(r, Ok(SUCCESS));

            // also test call multi times.
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_SET_ABSBIT,
                SyscallArg::from(uapi::ABS_MT_TOUCH_MAJOR as u64),
            );
            assert_eq!(r, Ok(SUCCESS));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_set_propbit() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_SET_PROPBIT,
                SyscallArg::from(uapi::INPUT_PROP_DIRECT as u64),
            );
            assert_eq!(r, Ok(SUCCESS));

            // also test call multi times.
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_SET_PROPBIT,
                SyscallArg::from(uapi::INPUT_PROP_DIRECT as u64),
            );
            assert_eq!(r, Ok(SUCCESS));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_dev_destroy_uncreated() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let _guard = ensure_uinput_running_lock();
            assert!(!uinput_running());
            let r =
                dev.ioctl(&file_object, current_task, uapi::UI_DEV_DESTROY, SyscallArg::from(0u64));
            assert_eq!(r, error!(EPERM));
            assert!(!uinput_running());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_dev_create_updates_existing_device_info() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, _file_object) = new_kernel_objects(current_task).await;
            let device_id = 42;
            let initial_input_id = uapi::input_id { bustype: 1, vendor: 2, product: 3, version: 4 };
            let initial_info =
                crate::InputDeviceInfo::new(initial_input_id, "initial_name".to_string());
            dev.input_relay_handle.device_infos.lock().insert(device_id, initial_info.clone());

            let updated_input_id =
                uapi::input_id { bustype: 10, vendor: 20, product: 30, version: 40 };
            let updated_name = "updated_uinput_device".to_string();

            // Exercise update_uinput_device without holding dev.inner.lock(), matching
            // the updated lock ordering in ui_dev_create_inner where inner lock is dropped before
            // updating uinput device metadata.
            dev.input_relay_handle.update_uinput_device(
                device_id,
                updated_input_id,
                updated_name.clone(),
            );

            // Verify that the existing InputDeviceInfoHandle was updated in-place without LockDep panic.
            let locked_info = initial_info.lock();
            assert_eq!(locked_info.input_id, updated_input_id);
            assert_eq!(locked_info.name, updated_name);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_dev_setup_name_parsing() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;

            // 1. Normal null-terminated string
            let mut setup = uapi::uinput_setup::default();
            setup.id = uapi::input_id { bustype: 3, vendor: 0x1234, product: 0x5678, version: 1 };
            let name_bytes = b"my_uinput_touch\0";
            for (i, &b) in name_bytes.iter().enumerate() {
                setup.name[i] = b as _;
            }

            let user_setup = map_memory(
                current_task,
                UserAddress::default(),
                std::mem::size_of::<uapi::uinput_setup>() as u64,
            );
            current_task.write_object(UserRef::new(user_setup), &setup).expect("write_object");

            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_DEV_SETUP,
                SyscallArg::from(user_setup.ptr() as u64),
            );
            assert_eq!(r, Ok(SUCCESS));

            {
                let inner = dev.inner.lock();
                assert_eq!(inner.input_id, Some(setup.id));
                assert_eq!(inner.name, Some("my_uinput_touch".to_string()));
            }

            // 2. Non-null terminated (full buffer)
            let mut setup_full = uapi::uinput_setup::default();
            setup_full.id = setup.id;
            setup_full.name.fill(b'a' as _);
            current_task.write_object(UserRef::new(user_setup), &setup_full).expect("write_object");
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_DEV_SETUP,
                SyscallArg::from(user_setup.ptr() as u64),
            );
            assert_eq!(r, Ok(SUCCESS));
            {
                let inner = dev.inner.lock();
                assert_eq!(inner.name, Some("a".repeat(setup_full.name.len())));
            }

            // 3. Invalid UTF-8 bytes before null terminator fallback to "starnix_uinput"
            let mut setup_invalid = uapi::uinput_setup::default();
            setup_invalid.id = setup.id;
            setup_invalid.name[0] = 0xff as u8 as _;
            setup_invalid.name[1] = 0;
            current_task
                .write_object(UserRef::new(user_setup), &setup_invalid)
                .expect("write_object");
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_DEV_SETUP,
                SyscallArg::from(user_setup.ptr() as u64),
            );
            assert_eq!(r, Ok(SUCCESS));
            {
                let inner = dev.inner.lock();
                assert_eq!(inner.name, Some(DEFAULT_UINPUT_DEVICE_NAME.to_string()));
            }

            // 4. Empty name (first byte is 0) fallback to "starnix_uinput"
            let mut setup_empty = uapi::uinput_setup::default();
            setup_empty.id = setup.id;
            setup_empty.name[0] = 0;
            current_task
                .write_object(UserRef::new(user_setup), &setup_empty)
                .expect("write_object");
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_DEV_SETUP,
                SyscallArg::from(user_setup.ptr() as u64),
            );
            assert_eq!(r, Ok(SUCCESS));
            {
                let inner = dev.inner.lock();
                assert_eq!(inner.name, Some(DEFAULT_UINPUT_DEVICE_NAME.to_string()));
            }
        })
        .await;
    }

    fn spawn_mock_registry() -> (
        futinput::RegistrySynchronousProxy,
        std::thread::JoinHandle<()>,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<starnix_sync::Mutex<Option<DisplayDimensions>>>,
    ) {
        let (client, server) = fidl::endpoints::create_endpoints::<futinput::RegistryMarker>();
        let proxy = client.into_sync_proxy();
        let received_request = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let received_flag = received_request.clone();
        let captured_display_dimensions = Arc::new(starnix_sync::Mutex::new(None));
        let dimensions_slot = captured_display_dimensions.clone();
        let handle = std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutor::default();
            executor.run_singlethreaded(async move {
                let mut stream = server.into_stream();
                while let Ok(Some(request)) = stream.try_next().await {
                    received_flag.store(true, Ordering::SeqCst);
                    match request {
                        futinput::RegistryRequest::RegisterTouchScreenAndGetDeviceInfo {
                            payload,
                            responder,
                        } => {
                            assert_eq!(
                                payload.coordinate_unit,
                                Some(CoordinateUnit::RegisteredDimensions)
                            );
                            *dimensions_slot.lock() = payload.display_dimensions;
                            let _ = responder.send(
                                futinput::RegistryRegisterTouchScreenAndGetDeviceInfoResponse {
                                    device_id: Some(101),
                                    ..Default::default()
                                },
                            );
                        }
                        futinput::RegistryRequest::RegisterKeyboardAndGetDeviceInfo {
                            responder,
                            ..
                        } => {
                            let _ = responder.send(
                                futinput::RegistryRegisterKeyboardAndGetDeviceInfoResponse {
                                    device_id: Some(102),
                                    ..Default::default()
                                },
                            );
                        }
                        _ => {}
                    }
                }
            });
        });
        (proxy, handle, received_request, captured_display_dimensions)
    }

    #[::fuchsia::test]
    async fn ui_dev_create_keyboard_and_destroy() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let _guard = ensure_uinput_running_lock();
            assert!(!uinput_running());
            let (proxy, join_handle, received, _) = spawn_mock_registry();

            let input_id = uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 };
            {
                let mut inner = dev.inner.lock();
                inner.input_id = Some(input_id);
                inner.name = Some("test_keyboard".to_string());
                inner.enabled_evbits.set(uapi::EV_KEY as usize, true);
            }

            let r = dev.ui_dev_create_inner(Some(proxy));
            assert_eq!(r, Ok(SUCCESS));
            assert!(uinput_running());
            assert!(received.load(Ordering::SeqCst));

            // Verify device was updated in relay handle
            {
                let device_infos = dev.input_relay_handle.device_infos.lock();
                let info = device_infos.get(&102).expect("device info found").lock();
                assert_eq!(info.name, "test_keyboard");
                assert_eq!(info.input_id, input_id);
            }

            // Destroy device via ioctl
            let r =
                dev.ioctl(&file_object, current_task, uapi::UI_DEV_DESTROY, SyscallArg::from(0u64));
            assert_eq!(r, Ok(SUCCESS));
            assert!(!uinput_running());
            // Device metadata remains in device_infos until removed via OnDeviceChanged(Removed)
            assert!(dev.input_relay_handle.device_infos.lock().get(&102).is_some());

            // A second UI_DEV_DESTROY returns EPERM
            let r =
                dev.ioctl(&file_object, current_task, uapi::UI_DEV_DESTROY, SyscallArg::from(0u64));
            assert_eq!(r, error!(EPERM));

            std::mem::drop(dev);
            std::mem::drop(file_object);
            let _ = join_handle.join();
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_dev_create_touchscreen_display_size_fallback_and_drop_cleanup() {
        spawn_kernel_and_run(async move |current_task| {
            let (input_relay_handle, _, _, _, _, _, _, _, _, _, _, _) =
                start_input_relays_for_test(current_task, EventProxyMode::None).await;
            let _guard = ensure_uinput_running_lock();
            assert!(!uinput_running());

            // Pre-set display_size to (0, 0) to ensure zero display sizes are filtered
            // out and fall back to DEFAULT_TOUCHSCREEN_WIDTH and HEIGHT without panicking.
            *input_relay_handle.display_size.lock() = Some((0, 0));

            let dev = Arc::new(UinputDeviceFile::new(input_relay_handle.clone()));
            let (proxy, join_handle, received, captured_dimensions) = spawn_mock_registry();

            let input_id = uapi::input_id { bustype: 3, vendor: 5, product: 6, version: 7 };
            {
                let mut inner = dev.inner.lock();
                inner.input_id = Some(input_id);
                inner.name = Some("test_touchscreen".to_string());
                inner.enabled_evbits.set(uapi::EV_ABS as usize, true);
                // Leave x_range and y_range as None to trigger fallback to display_size
            }

            let r = dev.ui_dev_create_inner(Some(proxy));
            assert_eq!(r, Ok(SUCCESS));
            assert!(uinput_running());
            assert!(received.load(Ordering::SeqCst));
            assert_eq!(
                *captured_dimensions.lock(),
                Some(DisplayDimensions {
                    min_x: 0,
                    max_x: DEFAULT_TOUCHSCREEN_WIDTH.into(),
                    min_y: 0,
                    max_y: DEFAULT_TOUCHSCREEN_HEIGHT.into(),
                })
            );

            // Verify device info was inserted
            assert!(input_relay_handle.device_infos.lock().get(&101).is_some());

            // Dropping dev should trigger Drop::drop and clean up the device and running count
            std::mem::drop(dev);
            assert!(!uinput_running());
            // Device metadata remains in device_infos until removed via OnDeviceChanged(Removed)
            assert!(input_relay_handle.device_infos.lock().get(&101).is_some());

            let _ = join_handle.join();
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_dev_create_touchscreen_explicit_ranges() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let _guard = ensure_uinput_running_lock();
            assert!(!uinput_running());
            let (proxy, join_handle, _, captured_dimensions) = spawn_mock_registry();

            let input_id = uapi::input_id { bustype: 3, vendor: 10, product: 20, version: 30 };
            {
                let mut inner = dev.inner.lock();
                inner.input_id = Some(input_id);
                inner.name = Some("touch_ranges".to_string());
                inner.enabled_evbits.set(uapi::EV_ABS as usize, true);
                inner.x_range = Some(Range { min: 100, max: 2000 });
                inner.y_range = Some(Range { min: 50, max: 1000 });
            }

            let r = dev.ui_dev_create_inner(Some(proxy));
            assert_eq!(r, Ok(SUCCESS));
            assert!(uinput_running());
            assert_eq!(
                *captured_dimensions.lock(),
                Some(DisplayDimensions { min_x: 100, max_x: 2000, min_y: 50, max_y: 1000 })
            );
            assert!(dev.input_relay_handle.device_infos.lock().get(&101).is_some());

            // Destroy and verify clean
            let r = dev.ui_dev_destroy(current_task);
            assert_eq!(r, Ok(SUCCESS));
            assert!(!uinput_running());
            // Device metadata remains in device_infos until removed via OnDeviceChanged(Removed)
            assert!(dev.input_relay_handle.device_infos.lock().get(&101).is_some());

            std::mem::drop(dev);
            std::mem::drop(file_object);
            let _ = join_handle.join();
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_dev_create_already_created_returns_einval() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let _guard = ensure_uinput_running_lock();
            assert!(!uinput_running());
            let (proxy, join_handle, _, _) = spawn_mock_registry();

            let input_id = uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 };
            {
                let mut inner = dev.inner.lock();
                inner.input_id = Some(input_id);
                inner.name = Some("test_keyboard".to_string());
                inner.enabled_evbits.set(uapi::EV_KEY as usize, true);
            }

            let r = dev.ui_dev_create_inner(Some(proxy));
            assert_eq!(r, Ok(SUCCESS));
            assert!(uinput_running());
            assert_eq!(COUNT_OF_UINPUT_DEVICE.load(Ordering::SeqCst), 1);

            // Second UI_DEV_CREATE on same fd must return EINVAL and not increment device count
            let (proxy2, join_handle2, _, _) = spawn_mock_registry();
            let r = dev.ui_dev_create_inner(Some(proxy2));
            assert_eq!(r, error!(EINVAL));
            assert_eq!(COUNT_OF_UINPUT_DEVICE.load(Ordering::SeqCst), 1);

            // Destroy and verify clean
            let r = dev.ui_dev_destroy(current_task);
            assert_eq!(r, Ok(SUCCESS));
            assert!(!uinput_running());
            assert_eq!(COUNT_OF_UINPUT_DEVICE.load(Ordering::SeqCst), 0);

            std::mem::drop(dev);
            std::mem::drop(file_object);
            let _ = join_handle.join();
            let _ = join_handle2.join();
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_abs_setup_validates_ranges() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let user_setup = map_memory(
                current_task,
                UserAddress::default(),
                std::mem::size_of::<starnix_uapi::uinput_abs_setup>() as u64,
            );

            // Valid range
            let valid_setup = starnix_uapi::uinput_abs_setup {
                code: uapi::ABS_MT_POSITION_X as u16,
                absinfo: uapi::input_absinfo { minimum: 0, maximum: 1080, ..Default::default() },
                ..Default::default()
            };
            current_task.write_object(UserRef::new(user_setup), &valid_setup).expect("write");
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_ABS_SETUP,
                SyscallArg::from(user_setup.ptr() as u64),
            );
            assert_eq!(r, Ok(SUCCESS));

            // Invalid range where minimum >= maximum
            let invalid_setup = starnix_uapi::uinput_abs_setup {
                code: uapi::ABS_MT_POSITION_X as u16,
                absinfo: uapi::input_absinfo { minimum: 100, maximum: 100, ..Default::default() },
                ..Default::default()
            };
            current_task.write_object(UserRef::new(user_setup), &invalid_setup).expect("write");
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_ABS_SETUP,
                SyscallArg::from(user_setup.ptr() as u64),
            );
            assert_eq!(r, error!(EINVAL));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_dev_create_touchscreen_invalid_ranges_returns_einval() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let _guard = ensure_uinput_running_lock();
            let (proxy, join_handle, _, _) = spawn_mock_registry();

            let input_id = uapi::input_id { bustype: 3, vendor: 10, product: 20, version: 30 };
            {
                let mut inner = dev.inner.lock();
                inner.input_id = Some(input_id);
                inner.name = Some("touch_invalid_ranges".to_string());
                inner.enabled_evbits.set(uapi::EV_ABS as usize, true);
                // Invalid ranges min >= max
                inner.x_range = Some(Range { min: 2000, max: 100 });
                inner.y_range = Some(Range { min: 50, max: 1000 });
            }

            let r = dev.ui_dev_create_inner(Some(proxy));
            assert_eq!(r, error!(EINVAL));
            assert!(!uinput_running());

            std::mem::drop(dev);
            std::mem::drop(file_object);
            let _ = join_handle.join();
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_dev_create_failure_preserves_running_count_and_state() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let _guard = ensure_uinput_running_lock();
            assert_eq!(COUNT_OF_UINPUT_DEVICE.load(Ordering::SeqCst), 0);
            assert!(!uinput_running());

            let input_id = uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 };
            {
                let mut inner = dev.inner.lock();
                inner.input_id = Some(input_id);
                inner.name = Some("test_keyboard".to_string());
                inner.enabled_evbits.set(uapi::EV_KEY as usize, true);
            }

            // Attempting to create the device when no registry is available (None) must fail.
            let r = dev.ui_dev_create_inner(None);
            assert_eq!(r, error!(EPERM));

            // Verify created_device remains None and COUNT_OF_UINPUT_DEVICE was not incremented.
            assert!(matches!(dev.inner.lock().created_device, CreatedDevice::None));
            assert_eq!(COUNT_OF_UINPUT_DEVICE.load(Ordering::SeqCst), 0);
            assert!(!uinput_running());

            // Dropping dev must not decrement COUNT_OF_UINPUT_DEVICE.
            std::mem::drop(dev);
            std::mem::drop(file_object);
            assert_eq!(COUNT_OF_UINPUT_DEVICE.load(Ordering::SeqCst), 0);
            assert!(!uinput_running());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_dev_create_before_setup_returns_einval() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let _guard = ensure_uinput_running_lock();
            assert!(!uinput_running());

            // Calling UI_DEV_CREATE before UI_DEV_SETUP without registry must return EINVAL (not EPERM).
            let r = dev.ui_dev_create_inner(None);
            assert_eq!(r, error!(EINVAL));

            // Also verify calling via ioctl directly returns EINVAL without attempting registry connection.
            let r =
                dev.ioctl(&file_object, current_task, uapi::UI_DEV_CREATE, SyscallArg::from(0u64));
            assert_eq!(r, error!(EINVAL));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_dev_create_touchscreen_single_axis_range_fallback() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let _guard = ensure_uinput_running_lock();
            assert!(!uinput_running());
            let (proxy, join_handle, _, captured_dimensions) = spawn_mock_registry();

            let input_id = uapi::input_id { bustype: 3, vendor: 10, product: 20, version: 30 };
            {
                let mut inner = dev.inner.lock();
                inner.input_id = Some(input_id);
                inner.name = Some("touch_single_axis".to_string());
                inner.enabled_evbits.set(uapi::EV_ABS as usize, true);
                // Only configure X axis, leave Y axis as None to fallback to display height
                inner.x_range = Some(Range { min: 100, max: 2000 });
            }

            let r = dev.ui_dev_create_inner(Some(proxy));
            assert_eq!(r, Ok(SUCCESS));
            assert!(uinput_running());
            assert_eq!(
                *captured_dimensions.lock(),
                Some(DisplayDimensions { min_x: 100, max_x: 2000, min_y: 0, max_y: 1200 })
            );

            std::mem::drop(dev);
            std::mem::drop(file_object);
            let _ = join_handle.join();
        })
        .await;
    }

    #[::fuchsia::test]
    async fn ui_setup_after_create_returns_einval() {
        spawn_kernel_and_run(async move |current_task| {
            let (dev, file_object) = new_kernel_objects(current_task).await;
            let _guard = ensure_uinput_running_lock();
            let (proxy, join_handle, _, _) = spawn_mock_registry();

            let input_id = uapi::input_id { bustype: 3, vendor: 10, product: 20, version: 30 };
            {
                let mut inner = dev.inner.lock();
                inner.input_id = Some(input_id);
                inner.name = Some("created_dev".to_string());
                inner.enabled_evbits.set(uapi::EV_KEY as usize, true);
            }

            let r = dev.ui_dev_create_inner(Some(proxy));
            assert_eq!(r, Ok(SUCCESS));

            // UI_SET_EVBIT after create must return EINVAL.
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_SET_EVBIT,
                SyscallArg::from(uapi::EV_KEY as u64),
            );
            assert_eq!(r, error!(EINVAL));

            // UI_ABS_SETUP after create must return EINVAL.
            let user_setup = map_memory(
                current_task,
                UserAddress::default(),
                std::mem::size_of::<starnix_uapi::uinput_abs_setup>() as u64,
            );
            let valid_setup = starnix_uapi::uinput_abs_setup {
                code: uapi::ABS_MT_POSITION_X as u16,
                absinfo: uapi::input_absinfo { minimum: 0, maximum: 100, ..Default::default() },
                ..Default::default()
            };
            current_task.write_object(UserRef::new(user_setup), &valid_setup).expect("write");
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_ABS_SETUP,
                SyscallArg::from(user_setup.ptr() as u64),
            );
            assert_eq!(r, error!(EINVAL));

            // UI_DEV_SETUP after create must return EINVAL.
            let user_dev_setup = map_memory(
                current_task,
                UserAddress::default(),
                std::mem::size_of::<uapi::uinput_setup>() as u64,
            );
            let dev_setup = uapi::uinput_setup {
                id: uapi::input_id { bustype: 3, vendor: 1, product: 2, version: 3 },
                ..Default::default()
            };
            current_task.write_object(UserRef::new(user_dev_setup), &dev_setup).expect("write");
            let r = dev.ioctl(
                &file_object,
                current_task,
                uapi::UI_DEV_SETUP,
                SyscallArg::from(user_dev_setup.ptr() as u64),
            );
            assert_eq!(r, error!(EINVAL));

            std::mem::drop(dev);
            std::mem::drop(file_object);
            let _ = join_handle.join();
        })
        .await;
    }
}
