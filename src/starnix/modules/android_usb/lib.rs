// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "256"]

use fidl_fuchsia_hardware_usb_policy::DeviceState;
use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_logging::log_error;

use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, error};

use fidl_fuchsia_usb_policy::{
    ConfigurationMarker, ConfigurationSynchronousProxy, PolicyProviderMarker,
};
use fuchsia_component::client::{connect_to_protocol, connect_to_protocol_sync};
use starnix_core::device::kobject::{Device, UEventAction};
use starnix_core::device::mem::DevNull;
use starnix_core::device::simple_device_ops;
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use starnix_core::vfs::{FsStr, FsString};
use starnix_sync::{AndroidUsbCcToggleEnableLock, LockDepMutex};
use starnix_uapi::file_mode::mode;
use std::borrow::Cow;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UsbGadgetState {
    Disconnected = 0,
    Configured = 1,
    Connected = 2,
    Unknown = 3,
}

impl From<u8> for UsbGadgetState {
    fn from(val: u8) -> Self {
        match val {
            0 => UsbGadgetState::Disconnected,
            1 => UsbGadgetState::Configured,
            2 => UsbGadgetState::Connected,
            _ => UsbGadgetState::Unknown,
        }
    }
}

impl UsbGadgetState {
    pub fn to_fs_str(&self) -> &'static FsStr {
        match self {
            UsbGadgetState::Disconnected => b"DISCONNECTED".into(),
            UsbGadgetState::Configured => b"CONFIGURED".into(),
            UsbGadgetState::Connected => b"CONNECTED".into(),
            UsbGadgetState::Unknown => b"UNKNOWN".into(),
        }
    }

    pub fn map_from_device_state(state: DeviceState, previous: Option<Self>) -> Option<Self> {
        match state {
            DeviceState::NotAttached | DeviceState::Attached | DeviceState::Powered => {
                Some(UsbGadgetState::Disconnected)
            }
            DeviceState::Default | DeviceState::Address => Some(UsbGadgetState::Connected),
            DeviceState::Configured => Some(UsbGadgetState::Configured),
            DeviceState::Suspended => previous, // No change when suspended
            _ => {
                log_error!("Unexpected DeviceState: {:?}", state);
                previous
            }
        }
    }
}

#[derive(Clone)]
struct UsbStateSysfsFile {
    state: Arc<AtomicU8>,
}

impl BytesFileOps for UsbStateSysfsFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let state_val = self.state.load(Ordering::Relaxed);
        let state = UsbGadgetState::from(state_val);
        let mut content = state.to_fs_str().to_vec();
        content.push(b'\n'); // standard sysfs newline
        Ok(Cow::Owned(content))
    }
}

struct CcToggleEnableState {
    /// Last value successfully applied via `fuchsia.usb.policy.Configuration`.
    enabled: bool,
    /// Lazily connected `fuchsia.usb.policy.Configuration` proxy, reused across writes so all
    /// requests are serialized on a single channel.
    config_proxy: Option<ConfigurationSynchronousProxy>,
}

struct CcToggleEnableSysfsFile {
    /// Guards both the FIDL call and the `enabled` update so concurrent writers can't apply
    /// `DisconnectCable`/`ConnectCable` in one order and record `enabled` in another.
    state: LockDepMutex<CcToggleEnableState, AndroidUsbCcToggleEnableLock>,
}

impl CcToggleEnableSysfsFile {
    fn new() -> Self {
        Self { state: CcToggleEnableState { enabled: true, config_proxy: None }.into() }
    }

    #[cfg(test)]
    fn new_with_proxy(config_proxy: ConfigurationSynchronousProxy) -> Self {
        Self {
            state: CcToggleEnableState { enabled: true, config_proxy: Some(config_proxy) }.into(),
        }
    }
}

impl BytesFileOps for CcToggleEnableSysfsFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let content: &[u8] = if self.state.lock().enabled { b"1\n" } else { b"0\n" };
        Ok(Cow::Borrowed(content))
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let input = std::str::from_utf8(&data).map_err(|_| errno!(EINVAL))?;
        let enable = match input.trim_matches(|c: char| c.is_whitespace() || c == '\0') {
            "0" => false,
            "1" => true,
            _ => return error!(EINVAL),
        };

        let mut state = self.state.lock();
        if state.config_proxy.is_none() {
            state.config_proxy =
                Some(connect_to_protocol_sync::<ConfigurationMarker>().map_err(|e| {
                    log_error!("Failed to connect to fuchsia.usb.policy.Configuration: {:?}", e);
                    errno!(EIO)
                })?);
        }
        let Some(proxy) = state.config_proxy.as_ref() else {
            return error!(EIO);
        };

        let (method, result) = if enable {
            ("ConnectCable", proxy.connect_cable(zx::MonotonicInstant::INFINITE))
        } else {
            ("DisconnectCable", proxy.disconnect_cable(0, zx::MonotonicInstant::INFINITE))
        };
        match result {
            Ok(Ok(())) => {}
            Ok(Err(status)) => {
                log_error!("{method} returned error: {:?}", zx::Status::err_from_raw(status));
                return error!(EIO);
            }
            Err(e) => {
                log_error!("{method} FIDL error: {:?}", e);
                // Drop the proxy so the next write reconnects (e.g. if usb-policy restarted).
                state.config_proxy = None;
                return error!(EIO);
            }
        }

        state.enabled = enable;
        Ok(())
    }
}

pub fn usb_device_init(kernel: &Arc<Kernel>) -> Result<Device, Errno> {
    let registry = &kernel.device_registry;

    let android_usb_class = registry.objects.get_or_create_class("android_usb".into());

    let shared_state = Arc::new(AtomicU8::new(UsbGadgetState::Disconnected as u8));
    let state_clone = shared_state.clone();

    let device = registry.register_dyn_device_with_dir(
        kernel,
        "android0".into(),
        android_usb_class,
        |device, dir| {
            build_device_directory(device, dir);
            dir.entry(
                "state",
                BytesFile::new_node(UsbStateSysfsFile { state: state_clone }),
                mode!(IFREG, 0o444),
            );
        },
        simple_device_ops::<DevNull>,
    )?;

    // Register `/sys/devices/platform/10d50000.hsi2c/i2c-6/i2c-max77759tcpc/cc_toggle_enable` so
    // Android's `cable_breaker` tool can simulate Type-C cable disconnects and reconnects via
    // `fuchsia.usb.policy.Configuration`.
    // TODO(https://fxbug.dev/452096300): Drive the hardcoded `10d50000.hsi2c/i2c-6/i2c-max77759tcpc`
    // topology from board configuration rather than registering it for all `android_usb`
    // containers.
    let platform_bus = registry.objects.platform_bus();
    let i2c_bus = registry.objects.get_or_create_bus("i2c".into());
    let hsi2c = registry.add_bus_device(
        "10d50000.hsi2c".into(),
        Some(registry.objects.platform_device()),
        platform_bus,
        build_device_directory,
    );
    let i2c_6 = registry.add_bus_device(
        "i2c-6".into(),
        Some(hsi2c),
        i2c_bus.clone(),
        build_device_directory,
    );
    registry.add_bus_device("i2c-max77759tcpc".into(), Some(i2c_6), i2c_bus, |device, dir| {
        build_device_directory(device, dir);
        dir.entry(
            "cc_toggle_enable",
            BytesFile::new_node(CcToggleEnableSysfsFile::new()),
            mode!(IFREG, 0o644),
        );
    });

    let kernel_clone = Arc::clone(kernel);
    let device_clone = device.clone();
    kernel.kthreads.spawn_future(
        move || async move {
            monitor_usb_device_state(kernel_clone, device_clone, shared_state).await;
        },
        "usb_device_state_monitor",
    );

    Ok(device)
}

// Prepare a request to broadcast a USB state change and dispatch it on a kernel thread.
fn dispatch_usb_state_change(
    kernel: &Arc<Kernel>,
    device: &Device,
    usb_state: FsString,
) -> impl Future<Output = Result<(), Errno>> {
    let spawner = kernel.kthreads.spawner();
    let device_clone = device.clone();
    let closure = move |current_task: &CurrentTask| {
        if let Some(metadata) = device_clone.metadata() {
            metadata.properties.insert("USB_STATE".into(), usb_state);
        }

        current_task.kernel.device_registry.dispatch_uevent(UEventAction::Change, device_clone);
    };
    let (result, request) =
        SpawnRequestBuilder::new().with_sync_closure(closure).build_with_async_result();
    spawner.spawn_from_request(request);
    result
}

/// Monitor the USB state via FIDL messages from the PolicyProvider and dispatch uevents when the
/// state changes.
async fn monitor_usb_device_state(
    kernel: Arc<Kernel>,
    device: Device,
    shared_state: Arc<AtomicU8>,
) {
    let provider = connect_to_protocol::<PolicyProviderMarker>()
        .expect("USB Failed to connect to PolicyProvider");
    let mut previous_mapped_state: Option<UsbGadgetState> = None;
    loop {
        match provider.watch_device_state().await {
            Ok(Ok(update)) => {
                let state = update.state.unwrap_or_else(DeviceState::unknown);

                // Map the incoming device state onto the new UsbGadgetState.
                let mapped = UsbGadgetState::map_from_device_state(state, previous_mapped_state);

                if previous_mapped_state != mapped {
                    if let Some(val) = mapped {
                        previous_mapped_state = Some(val);
                        shared_state.store(val as u8, Ordering::Relaxed);
                        if let Err(e) =
                            dispatch_usb_state_change(&kernel, &device, val.to_fs_str().to_owned())
                                .await
                        {
                            log_error!("Failed to dispatch USB state change: {:?}", e);
                        }
                    }
                }
            }
            Ok(Err(err)) => {
                log_error!("USB PolicyProvider returned error: {:?}", err);
                break;
            }
            Err(e) => {
                log_error!("USB PolicyProvider watch failed: {:?}", e);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_usb_policy::ConfigurationRequest;
    use futures::StreamExt;
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_rcu::RcuReadScope;

    #[::fuchsia::test]
    async fn test_usb_gadget_state_map_from_device_state() {
        assert_eq!(
            UsbGadgetState::map_from_device_state(DeviceState::NotAttached, None),
            Some(UsbGadgetState::Disconnected)
        );
        assert_eq!(
            UsbGadgetState::map_from_device_state(DeviceState::Configured, None),
            Some(UsbGadgetState::Configured)
        );
        assert_eq!(
            UsbGadgetState::map_from_device_state(DeviceState::Attached, None),
            Some(UsbGadgetState::Disconnected)
        );
        assert_eq!(
            UsbGadgetState::map_from_device_state(DeviceState::Powered, None),
            Some(UsbGadgetState::Disconnected)
        );
        assert_eq!(
            UsbGadgetState::map_from_device_state(DeviceState::Default, None),
            Some(UsbGadgetState::Connected)
        );
        assert_eq!(
            UsbGadgetState::map_from_device_state(DeviceState::Address, None),
            Some(UsbGadgetState::Connected)
        );
        assert_eq!(
            UsbGadgetState::map_from_device_state(
                DeviceState::Suspended,
                Some(UsbGadgetState::Configured)
            ),
            Some(UsbGadgetState::Configured)
        );
        assert_eq!(UsbGadgetState::map_from_device_state(DeviceState::Suspended, None), None);
        assert_eq!(UsbGadgetState::map_from_device_state(DeviceState::unknown(), None), None);
        assert_eq!(
            UsbGadgetState::map_from_device_state(
                DeviceState::unknown(),
                Some(UsbGadgetState::Configured)
            ),
            Some(UsbGadgetState::Configured)
        );
    }

    #[::fuchsia::test]
    async fn test_usb_sysfs_state_reads() {
        spawn_kernel_and_run(async |current_task| {
            let shared_state = Arc::new(AtomicU8::new(UsbGadgetState::Disconnected as u8));

            shared_state.store(UsbGadgetState::Configured as u8, Ordering::Relaxed);

            let sysfs_file = UsbStateSysfsFile { state: shared_state };
            let content = sysfs_file.read(current_task).expect("read failed");
            assert_eq!(content.as_ref(), b"CONFIGURED\n");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_usb_device_state_to_fs_str() {
        assert_eq!(UsbGadgetState::Disconnected.to_fs_str(), b"DISCONNECTED");
        assert_eq!(UsbGadgetState::Configured.to_fs_str(), b"CONFIGURED");
        assert_eq!(UsbGadgetState::Connected.to_fs_str(), b"CONNECTED");
        assert_eq!(UsbGadgetState::Unknown.to_fs_str(), b"UNKNOWN");
    }

    #[::fuchsia::test]
    async fn test_usb_device_state_from_u8() {
        assert_eq!(UsbGadgetState::from(0), UsbGadgetState::Disconnected);
        assert_eq!(UsbGadgetState::from(1), UsbGadgetState::Configured);
        assert_eq!(UsbGadgetState::from(2), UsbGadgetState::Connected);
        assert_eq!(UsbGadgetState::from(255), UsbGadgetState::Unknown);
    }

    #[::fuchsia::test]
    async fn test_usb_device_init_sysfs() {
        spawn_kernel_and_run(async |current_task| {
            let device = usb_device_init(current_task.kernel()).expect("usb_device_init failed");

            assert_eq!(device.name(), "android0");
            let metadata = device.metadata().expect("metadata not found");
            assert_eq!(metadata.devname.as_slice(), b"android0");

            let root = &current_task.kernel().device_registry.objects.root;
            assert!(
                root.lookup(
                    "devices/platform/10d50000.hsi2c/i2c-6/i2c-max77759tcpc/cc_toggle_enable"
                        .into()
                )
                .is_some()
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_usb_device_metadata_property_injection() {
        spawn_kernel_and_run(async |current_task| {
            let device = usb_device_init(current_task.kernel()).expect("usb_device_init failed");

            let kernel = current_task.kernel();

            // Send the state change and wait for the background task to execute.
            dispatch_usb_state_change(
                kernel,
                &device,
                UsbGadgetState::Connected.to_fs_str().to_owned(),
            )
            .await
            .unwrap();

            let metadata = device.metadata().expect("metadata not found");
            let scope = RcuReadScope::new();
            let value = metadata
                .properties
                .get(&scope, FsStr::new(b"USB_STATE"))
                .expect("property not found");
            assert_eq!(value.as_slice(), b"CONNECTED");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_cc_toggle_enable_sysfs_read_and_write() {
        let (config_proxy, mut config_stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<ConfigurationMarker>();

        let server_task = fuchsia_async::Task::spawn(async move {
            let mut requests = Vec::new();
            while let Some(Ok(req)) = config_stream.next().await {
                match req {
                    ConfigurationRequest::DisconnectCable { duration, responder } => {
                        requests.push((false, duration));
                        let _ = responder.send(Ok(()));
                    }
                    ConfigurationRequest::ConnectCable { responder } => {
                        requests.push((true, 0));
                        let _ = responder.send(Ok(()));
                    }
                    _ => panic!("unexpected request: {:?}", req),
                }
                if requests.len() == 4 {
                    break;
                }
            }
            requests
        });

        spawn_kernel_and_run(async move |current_task| {
            let sysfs_file = CcToggleEnableSysfsFile::new_with_proxy(config_proxy);
            assert_eq!(sysfs_file.read(current_task).expect("read failed").as_ref(), b"1\n");

            sysfs_file.write(current_task, b"0\n".to_vec()).expect("write 0 failed");
            assert_eq!(sysfs_file.read(current_task).expect("read failed").as_ref(), b"0\n");

            sysfs_file.write(current_task, b"1".to_vec()).expect("write 1 failed");
            assert_eq!(sysfs_file.read(current_task).expect("read failed").as_ref(), b"1\n");

            sysfs_file.write(current_task, b"0\0".to_vec()).expect("write 0\\0 failed");
            assert_eq!(sysfs_file.read(current_task).expect("read failed").as_ref(), b"0\n");

            sysfs_file.write(current_task, b"1\n\0".to_vec()).expect("write 1\\n\\0 failed");
            assert_eq!(sysfs_file.read(current_task).expect("read failed").as_ref(), b"1\n");

            assert_eq!(sysfs_file.write(current_task, b"invalid".to_vec()), error!(EINVAL));
        })
        .await;

        assert_eq!(server_task.await, vec![(false, 0), (true, 0), (false, 0), (true, 0)]);
    }

    #[::fuchsia::test]
    async fn test_cc_toggle_enable_sysfs_write_error_keeps_state() {
        let (config_proxy, mut config_stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<ConfigurationMarker>();

        let server_task = fuchsia_async::Task::spawn(async move {
            let mut num_requests = 0;
            while let Some(Ok(req)) = config_stream.next().await {
                match req {
                    ConfigurationRequest::DisconnectCable { responder, .. } => {
                        let _ = responder.send(Err(zx::Status::UNAVAILABLE.into_raw()));
                    }
                    ConfigurationRequest::ConnectCable { responder } => {
                        let _ = responder.send(Err(zx::Status::INTERNAL.into_raw()));
                    }
                    _ => panic!("unexpected request: {:?}", req),
                }
                num_requests += 1;
                if num_requests == 2 {
                    break;
                }
            }
            num_requests
        });

        spawn_kernel_and_run(async move |current_task| {
            let sysfs_file = CcToggleEnableSysfsFile::new_with_proxy(config_proxy);

            // A failed DisconnectCable must return EIO and leave `enabled` set.
            assert_eq!(sysfs_file.write(current_task, b"0".to_vec()), error!(EIO));
            assert_eq!(sysfs_file.read(current_task).expect("read failed").as_ref(), b"1\n");

            // Force `enabled` to false, then verify a failed ConnectCable leaves it unset.
            sysfs_file.state.lock().enabled = false;
            assert_eq!(sysfs_file.write(current_task, b"1".to_vec()), error!(EIO));
            assert_eq!(sysfs_file.read(current_task).expect("read failed").as_ref(), b"0\n");
        })
        .await;

        assert_eq!(server_task.await, 2);
    }
}
