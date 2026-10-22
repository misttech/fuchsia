// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "256"]

pub(crate) mod device;
pub(crate) mod sysfs;

use crate::device::{
    USB_SPEED_FULL, USB_SPEED_HIGH, USB_SPEED_LOW, USB_SPEED_SUPER, USB_SPEED_SUPER_PLUS,
    USB_SPEED_UNKNOWN, UsbDevice, UsbDeviceFile,
};
use crate::sysfs::build_usb_device_directory;
use fidl::endpoints::Proxy;
use fidl_fuchsia_hardware_usb as fhardware_usb;
use fidl_fuchsia_hardware_usb_descriptor as fusb_descriptor;
use fuchsia_component::client::Service;
use futures::TryStreamExt;
use starnix_core::device::DeviceMode;
use starnix_core::device::kobject::{Bus, Device, DeviceMetadata};
use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::NamespaceNode;
use starnix_logging::{log_error, log_info};
use starnix_sync::Mutex;
use starnix_uapi::device_id::{DeviceId, USB_DEVICE_MAJOR};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{errno, usb_device_descriptor};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub(crate) fn fuchsia_speed_to_linux_speed(speed: fusb_descriptor::UsbSpeed) -> u32 {
    match speed {
        fusb_descriptor::UsbSpeed::Full => USB_SPEED_FULL,
        fusb_descriptor::UsbSpeed::Low => USB_SPEED_LOW,
        fusb_descriptor::UsbSpeed::High => USB_SPEED_HIGH,
        fusb_descriptor::UsbSpeed::Super => USB_SPEED_SUPER,
        fusb_descriptor::UsbSpeed::EnhancedSuper => USB_SPEED_SUPER_PLUS,
        fusb_descriptor::UsbSpeed::Undefined => USB_SPEED_UNKNOWN,
    }
}

pub(crate) fn fidl_device_descriptor_to_linux(
    desc: &fusb_descriptor::UsbDeviceDescriptor,
) -> usb_device_descriptor {
    usb_device_descriptor {
        bLength: desc.b_length,
        bDescriptorType: desc.b_descriptor_type,
        bcdUSB: desc.bcd_usb.to_le(),
        bDeviceClass: desc.b_device_class,
        bDeviceSubClass: desc.b_device_sub_class,
        bDeviceProtocol: desc.b_device_protocol,
        bMaxPacketSize0: desc.b_max_packet_size0,
        idVendor: desc.id_vendor.to_le(),
        idProduct: desc.id_product.to_le(),
        bcdDevice: desc.bcd_device.to_le(),
        iManufacturer: desc.i_manufacturer,
        iProduct: desc.i_product,
        iSerialNumber: desc.i_serial_number,
        bNumConfigurations: desc.b_num_configurations,
    }
}

struct DevNumAllocator {
    allocated: HashSet<u16>,
    next: u16,
}

impl DevNumAllocator {
    fn new() -> Self {
        Self { allocated: HashSet::new(), next: 2 }
    }

    fn allocate(&mut self) -> Option<u16> {
        let start = self.next;
        for i in 0..126 {
            let num = ((start - 2 + i) % 126) + 2;
            if self.allocated.insert(num) {
                self.next = if num >= 127 { 2 } else { num + 1 };
                return Some(num);
            }
        }
        None
    }

    fn release(&mut self, dev_num: u16) {
        self.allocated.remove(&dev_num);
    }
}

#[derive(Clone)]
pub struct UsbDeviceManager {
    pub(crate) bus: Bus,
    pub(crate) devices: Arc<Mutex<BTreeMap<DeviceId, Arc<UsbDevice>>>>,
    pub(crate) allocator: Arc<Mutex<DevNumAllocator>>,
}

impl UsbDeviceManager {
    pub(crate) fn new(bus: Bus) -> Self {
        Self {
            bus,
            devices: Arc::new(Mutex::new(BTreeMap::new())),
            allocator: Arc::new(Mutex::new(DevNumAllocator::new())),
        }
    }

    pub(crate) fn allocate_dev_num(&self) -> Option<u16> {
        self.allocator.lock().allocate()
    }

    pub(crate) fn release_dev_num(&self, dev_num: u16) {
        self.allocator.lock().release(dev_num);
    }

    pub(crate) fn get_device(&self, id: DeviceId) -> Option<Arc<UsbDevice>> {
        self.devices.lock().get(&id).cloned()
    }

    pub(crate) fn add_device(
        &self,
        kernel: &Kernel,
        device: Arc<UsbDevice>,
    ) -> Result<Device, Errno> {
        let bus_str = format!("{:03}", device.bus_num);
        let dev_str = format!("{:03}", device.dev_num);
        let devname = format!("bus/usb/{}/{}", bus_str, dev_str);
        let sysname = format!("{}-{}", device.bus_num, device.dev_num);

        let devt = DeviceId::new(USB_DEVICE_MAJOR, device.minor());

        let desc = &device.device_descriptor;
        let id_vendor = u16::from_le(desc.idVendor);
        let id_product = u16::from_le(desc.idProduct);
        let bcd_device = u16::from_le(desc.bcdDevice);
        let b_device_class = desc.bDeviceClass;
        let b_device_sub_class = desc.bDeviceSubClass;
        let b_device_protocol = desc.bDeviceProtocol;

        let metadata =
            DeviceMetadata::new(devname.into(), devt, DeviceMode::Char).with_devtype("usb_device");

        metadata.properties.insert(
            b"PRODUCT".into(),
            format!("{:x}/{:x}/{:x}", id_vendor, id_product, bcd_device).into(),
        );
        metadata.properties.insert(
            b"TYPE".into(),
            format!("{}/{}/{}", b_device_class, b_device_sub_class, b_device_protocol).into(),
        );
        metadata.properties.insert(b"BUSNUM".into(), format!("{:03}", device.bus_num).into());
        metadata.properties.insert(b"DEVNUM".into(), format!("{:03}", device.dev_num).into());

        self.devices.lock().insert(devt, device.clone());

        let device_clone = device.clone();
        let registered_device = match kernel.device_registry.add_device(
            kernel,
            sysname.as_bytes().into(),
            metadata,
            None,
            self.bus.clone(),
            move |device_obj, dir| {
                build_usb_device_directory(device_obj, dir, &device_clone);
            },
        ) {
            Ok(dev) => dev,
            Err(e) => {
                self.devices.lock().remove(&devt);
                return Err(e);
            }
        };

        Ok(registered_device)
    }

    pub(crate) fn remove_device(
        &self,
        current_task: &CurrentTask,
        registered_device: Device,
    ) -> Result<(), Errno> {
        if let Some(metadata) = registered_device.metadata() {
            if let Some(device) = self.devices.lock().remove(&metadata.devt) {
                self.release_dev_num(device.dev_num);
                device.on_disconnect();
            }
        }
        current_task.kernel().device_registry.remove_device(current_task, registered_device);
        Ok(())
    }
}

async fn run_in_kernel_task_async<F, R>(
    kernel: &Kernel,
    debug_name: &'static str,
    f: F,
) -> Result<R, Errno>
where
    F: FnOnce(&CurrentTask) -> R + Send + 'static,
    R: Send + 'static,
{
    let closure = move |current_task: &CurrentTask| {
        current_task.override_creds(
            starnix_core::security::creds_start_internal_operation(current_task),
            || f(current_task),
        )
    };
    let (result_fut, req) = SpawnRequestBuilder::new()
        .with_debug_name(debug_name)
        .with_sync_closure(closure)
        .build_with_async_result();
    kernel.kthreads.spawner().spawn_from_request(req);
    result_fut.await
}

pub fn usbfs_init(kernel: &Arc<Kernel>) -> Result<UsbDeviceManager, Errno> {
    let registry = &kernel.device_registry;

    let usb_bus = registry.objects.get_or_create_bus("usb".into());
    let manager = UsbDeviceManager::new(usb_bus);
    let manager_clone = manager.clone();

    registry.register_major(
        "usb".into(),
        DeviceMode::Char,
        USB_DEVICE_MAJOR,
        move |_current_task: &CurrentTask,
              id: DeviceId,
              _node: &NamespaceNode,
              _flags: OpenFlags|
              -> Result<Box<dyn starnix_core::vfs::FileOps>, Errno> {
            let device = manager_clone.get_device(id).ok_or_else(|| errno!(ENODEV))?;
            Ok(Box::new(UsbDeviceFile::new(device)))
        },
    )?;

    let kernel_clone = Arc::clone(kernel);
    let manager_for_watcher = manager.clone();
    kernel.kthreads.spawn_future(
        move || async move {
            watch_usb_devices(kernel_clone, manager_for_watcher).await;
        },
        "usbfs_watcher",
    );

    Ok(manager)
}

async fn watch_usb_devices(kernel: Arc<Kernel>, manager: UsbDeviceManager) {
    log_info!("Starting Starnix usbfs USB device watcher.");

    let service = match Service::open(fhardware_usb::UsbServiceMarker) {
        Ok(service) => service,
        Err(e) => {
            log_info!("Physical USB service not available: {:?}", e);
            return;
        }
    };
    watch_usb_devices_from_service(kernel, manager, service).await;
}

async fn watch_usb_devices_from_service(
    kernel: Arc<Kernel>,
    manager: UsbDeviceManager,
    service: Service<fhardware_usb::UsbServiceMarker>,
) {
    let mut watcher = match service.watch().await {
        Ok(watcher) => watcher,
        Err(e) => {
            log_info!("Failed to create USB service watcher: {:?}", e);
            return;
        }
    };

    let active_hw_ids = Arc::new(Mutex::new(HashSet::<u32>::new()));
    while let Ok(Some(instance)) = watcher.try_next().await {
        let device_proxy = match instance.connect_to_device() {
            Ok(proxy) => proxy,
            Err(e) => {
                log_error!("Failed to connect to USB device: {:?}", e);
                continue;
            }
        };

        let first_config_desc = match device_proxy.get_configuration_descriptor(1).await {
            Err(fidl::Error::ClientChannelClosed { .. }) => continue,
            Ok(Err(zx::sys::ZX_ERR_NOT_SUPPORTED)) => {
                // Child interface node from usb-composite, not parent UsbDevice; skip it.
                continue;
            }
            Ok(Ok(desc)) if !desc.is_empty() => Some(desc),
            _ => None,
        };

        let hw_device_id = match device_proxy.get_device_id().await {
            Ok(id) => id,
            Err(e) => {
                log_error!("Failed to get device ID: {:?}", e);
                continue;
            }
        };
        if !active_hw_ids.lock().insert(hw_device_id) {
            continue;
        }

        let fidl_desc = match device_proxy.get_device_descriptor().await {
            Ok(desc) => desc,
            Err(e) => {
                active_hw_ids.lock().remove(&hw_device_id);
                log_error!("Failed to get device descriptor: {:?}", e);
                continue;
            }
        };

        let device_descriptor = fidl_device_descriptor_to_linux(&fidl_desc);

        let mut config_descriptors = Vec::new();
        if let Some(desc) = first_config_desc {
            config_descriptors.extend_from_slice(&desc);
        }
        for cfg in 2..=device_descriptor.bNumConfigurations {
            match device_proxy.get_configuration_descriptor(cfg).await {
                Ok(Ok(desc)) if !desc.is_empty() => {
                    config_descriptors.extend_from_slice(&desc);
                }
                _ => {}
            }
        }

        let speed = match device_proxy.get_device_speed().await {
            Ok(s) => fuchsia_speed_to_linux_speed(s),
            Err(_) => USB_SPEED_FULL,
        };

        let active_config = device_proxy.get_configuration().await.unwrap_or(1);

        let assigned_dev_num = match manager.allocate_dev_num() {
            Some(n) => n,
            None => {
                active_hw_ids.lock().remove(&hw_device_id);
                log_error!("Exhausted USB device numbers (2..127) for incoming device.");
                continue;
            }
        };

        let sync_proxy = match instance.connect_to_device() {
            Ok(client_proxy) => {
                let client_end = client_proxy.into_client_end().ok();
                client_end
                    .map(|c| Arc::new(fhardware_usb::UsbSynchronousProxy::new(c.into_channel())))
            }
            Err(_) => None,
        };

        let mut usb_device =
            UsbDevice::new(1, assigned_dev_num, speed, device_descriptor, config_descriptors);
        usb_device.active_config.store(active_config, Ordering::Relaxed);
        usb_device.sync_proxy = sync_proxy;
        let usb_device = Arc::new(usb_device);

        let add_res = manager.add_device(&kernel, Arc::clone(&usb_device));

        match add_res {
            Ok(registered_device) => {
                log_info!(
                    "Registered USB device in Starnix usbfs (/dev/bus/usb/001/{:03}) with speed {}",
                    assigned_dev_num,
                    speed
                );

                let kernel_for_close = Arc::clone(&kernel);
                let manager_for_close = manager.clone();
                let active_hw_ids_for_close = Arc::clone(&active_hw_ids);
                let proxy_for_close = device_proxy.clone();
                kernel.kthreads.spawn_future(
                    move || async move {
                        let mut event_stream = proxy_for_close.take_event_stream();
                        let _ = event_stream.try_next().await;
                        active_hw_ids_for_close.lock().remove(&hw_device_id);
                        let _ = run_in_kernel_task_async(
                            &kernel_for_close,
                            "usbfs-device-disconnect",
                            move |current_task| {
                                if manager_for_close
                                    .remove_device(current_task, registered_device)
                                    .is_ok()
                                {
                                    log_info!(
                                        "Removed disconnected USB device from Starnix usbfs (/dev/bus/usb/001/{:03})",
                                        assigned_dev_num
                                    );
                                }
                            },
                        )
                        .await;
                    },
                    "usbfs-device-lifecycle",
                );
            }
            Err(e) => {
                manager.release_dev_num(assigned_dev_num);
                active_hw_ids.lock().remove(&hw_device_id);
                log_error!("Failed to register USB device in Starnix usbfs: {:?}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starnix_core::device::DeviceListener;
    use starnix_core::device::kobject::{UEventAction, UEventContext};
    use starnix_core::task::KernelFeatures;
    use starnix_core::testing::{spawn_kernel_and_run, spawn_kernel_with_features_and_run};
    use starnix_core::vfs::{FsStr, FsString};
    use starnix_uapi::{
        error, usb_config_descriptor, usb_endpoint_descriptor, usb_interface_descriptor,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zerocopy::IntoBytes as _;

    fn create_mock_hid_descriptors() -> (usb_device_descriptor, Vec<u8>) {
        let device_desc = usb_device_descriptor {
            bLength: 18,
            bDescriptorType: 1,
            bcdUSB: 0x0210u16.to_le(),
            bDeviceClass: 0,
            bDeviceSubClass: 0,
            bDeviceProtocol: 0,
            bMaxPacketSize0: 64,
            idVendor: 0x1234u16.to_le(),
            idProduct: 0x5678u16.to_le(),
            bcdDevice: 0x0543u16.to_le(),
            iManufacturer: 1,
            iProduct: 2,
            iSerialNumber: 3,
            bNumConfigurations: 1,
        };

        let config_desc = usb_config_descriptor {
            bLength: 9,
            bDescriptorType: 2,
            wTotalLength: ((9 + 9 + 7 + 7) as u16).to_le(),
            bNumInterfaces: 1,
            bConfigurationValue: 1,
            iConfiguration: 0,
            bmAttributes: 0x80, // Bus powered
            bMaxPower: 50,      // 100mA
        };

        let if_desc = usb_interface_descriptor {
            bLength: 9,
            bDescriptorType: 4,
            bInterfaceNumber: 0,
            bAlternateSetting: 0,
            bNumEndpoints: 2,
            bInterfaceClass: 3, // HID class
            bInterfaceSubClass: 0,
            bInterfaceProtocol: 0,
            iInterface: 0,
        };

        let ep_in_desc = usb_endpoint_descriptor {
            bLength: 7,
            bDescriptorType: 5,
            bEndpointAddress: 0x81, // EP 1 IN
            bmAttributes: 3,        // Interrupt
            wMaxPacketSize: 64u16.to_le(),
            bInterval: 5,
            bRefresh: 0,
            bSynchAddress: 0,
        };

        let ep_out_desc = usb_endpoint_descriptor {
            bLength: 7,
            bDescriptorType: 5,
            bEndpointAddress: 0x01, // EP 1 OUT
            bmAttributes: 3,        // Interrupt
            wMaxPacketSize: 64u16.to_le(),
            bInterval: 5,
            bRefresh: 0,
            bSynchAddress: 0,
        };

        let mut config_bytes = Vec::new();
        config_bytes.extend_from_slice(config_desc.as_bytes());
        config_bytes.extend_from_slice(if_desc.as_bytes());
        config_bytes.extend_from_slice(&ep_in_desc.as_bytes()[..7]);
        config_bytes.extend_from_slice(&ep_out_desc.as_bytes()[..7]);

        (device_desc, config_bytes)
    }

    fn open_dev_node(
        current_task: &CurrentTask,
        path: &FsStr,
        flags: OpenFlags,
    ) -> Result<starnix_core::vfs::FileHandle, Errno> {
        let dev_root = NamespaceNode::new_anonymous(
            starnix_core::fs::devtmpfs::DevTmpFs::from_kernel(current_task.kernel()).root().clone(),
        );
        let mut context = starnix_core::vfs::LookupContext::default();
        let node = current_task.lookup_path(&mut context, dev_root, path)?;
        node.open(current_task, flags)
    }

    #[::fuchsia::test]
    async fn test_usb_device_descriptor_layout() {
        assert_eq!(std::mem::size_of::<usb_device_descriptor>(), 18);
        assert_eq!(std::mem::size_of::<usb_config_descriptor>(), 9);
        assert_eq!(std::mem::size_of::<usb_interface_descriptor>(), 9);
        assert_eq!(std::mem::size_of::<usb_endpoint_descriptor>(), 9);

        let (device_desc, config_bytes) = create_mock_hid_descriptors();
        let device = UsbDevice::new(1, 2, USB_SPEED_HIGH, device_desc, config_bytes);

        assert_eq!(device.bus_num, 1);
        assert_eq!(device.dev_num, 2);
        assert_eq!(device.minor(), 1);
        assert_eq!(device.speed, USB_SPEED_HIGH);
        assert_eq!(device.raw_descriptors.len(), 18 + 9 + 9 + 7 + 7);
    }

    #[::fuchsia::test]
    async fn test_usbfs_registration_and_devtmpfs() {
        spawn_kernel_with_features_and_run(
            async |current_task| {
                let kernel = current_task.kernel();
                let manager = usbfs_init(kernel).expect("usbfs_init failed");

                let (device_desc, config_bytes) = create_mock_hid_descriptors();
                let usb_dev =
                    Arc::new(UsbDevice::new(1, 2, USB_SPEED_HIGH, device_desc, config_bytes));

                let registered = manager.add_device(kernel, usb_dev).expect("add_device failed");
                let metadata = registered.metadata().expect("metadata present");
                assert_eq!(metadata.devname.as_slice(), b"bus/usb/001/002");
                assert_eq!(metadata.devt.major(), USB_DEVICE_MAJOR);
                assert_eq!(metadata.devt.minor(), 1);

                let node = open_dev_node(current_task, b"bus/usb/001/002".into(), OpenFlags::RDWR)
                    .expect("open_dev_node failed");
                assert!(node.ops().is_seekable());

                // Verify device appears under /sys/bus/usb/devices/1-2.
                assert!(
                    kernel
                        .device_registry
                        .objects
                        .root
                        .lookup("bus/usb/devices/1-2".into())
                        .is_some()
                );

                manager.remove_device(current_task, registered).expect("remove_device failed");

                // Verify device is removed from /sys/bus/usb/devices/1-2.
                assert!(
                    kernel
                        .device_registry
                        .objects
                        .root
                        .lookup("bus/usb/devices/1-2".into())
                        .is_none()
                );
            },
            KernelFeatures { devtmpfs: true, ..Default::default() },
        )
        .await;
    }

    #[::fuchsia::test]
    async fn test_usbfs_read_descriptors() {
        spawn_kernel_with_features_and_run(
            async |current_task| {
                let kernel = current_task.kernel();
                let manager = usbfs_init(kernel).expect("usbfs_init failed");

                let (device_desc, config_bytes) = create_mock_hid_descriptors();
                let mut expected_raw = Vec::new();
                expected_raw.extend_from_slice(device_desc.as_bytes());
                expected_raw.extend_from_slice(&config_bytes);

                let usb_dev = Arc::new(UsbDevice::new(
                    1,
                    3,
                    USB_SPEED_FULL,
                    device_desc,
                    config_bytes.clone(),
                ));
                let registered = manager.add_device(kernel, usb_dev).unwrap();

                let file =
                    open_dev_node(current_task, b"bus/usb/001/003".into(), OpenFlags::RDONLY)
                        .expect("open_dev_node failed");

                let mut output_buf = starnix_core::vfs::buffers::VecOutputBuffer::new(128);
                let bytes_read = file.ops().read(&file, current_task, 0, &mut output_buf).unwrap();

                assert_eq!(bytes_read, expected_raw.len());
                assert_eq!(Vec::from(output_buf), expected_raw);

                manager.remove_device(current_task, registered).unwrap();
            },
            KernelFeatures { devtmpfs: true, ..Default::default() },
        )
        .await;
    }

    struct MockUEventListener {
        add_count: Arc<AtomicUsize>,
        remove_count: Arc<AtomicUsize>,
    }

    impl DeviceListener for MockUEventListener {
        fn on_device_event(&self, action: UEventAction, device: Device, _context: UEventContext) {
            if let Some(metadata) = device.metadata() {
                if metadata.devname.as_slice() == b"bus/usb/001/005" {
                    match action {
                        UEventAction::Add => {
                            self.add_count.fetch_add(1, Ordering::SeqCst);
                        }
                        UEventAction::Remove => {
                            self.remove_count.fetch_add(1, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    #[::fuchsia::test]
    async fn test_usbfs_uevent_emission() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let manager = usbfs_init(kernel).expect("usbfs_init failed");

            let add_count = Arc::new(AtomicUsize::new(0));
            let remove_count = Arc::new(AtomicUsize::new(0));
            let listener = MockUEventListener {
                add_count: add_count.clone(),
                remove_count: remove_count.clone(),
            };

            let _listener_key = kernel.device_registry.register_listener(listener);

            let (device_desc, config_bytes) = create_mock_hid_descriptors();
            let usb_dev = Arc::new(UsbDevice::new(1, 5, USB_SPEED_HIGH, device_desc, config_bytes));
            let registered = manager.add_device(kernel, usb_dev).unwrap();

            let props: std::collections::HashMap<FsString, FsString> =
                registered.get_uevent_properties_list().into_iter().collect();
            assert_eq!(props.get(&FsString::from(b"SUBSYSTEM")).unwrap().as_slice(), b"usb");
            assert_eq!(props.get(&FsString::from(b"DEVTYPE")).unwrap().as_slice(), b"usb_device");
            assert_eq!(props.get(&FsString::from(b"BUSNUM")).unwrap().as_slice(), b"001");
            assert_eq!(props.get(&FsString::from(b"DEVNUM")).unwrap().as_slice(), b"005");
            assert_eq!(
                props.get(&FsString::from(b"PRODUCT")).unwrap().as_slice(),
                b"1234/5678/543"
            );

            assert_eq!(add_count.load(Ordering::SeqCst), 1);
            assert_eq!(remove_count.load(Ordering::SeqCst), 0);

            manager.remove_device(current_task, registered).unwrap();
            assert_eq!(remove_count.load(Ordering::SeqCst), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_multidevice_minor_allocation_and_cleanup() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let manager = usbfs_init(kernel).expect("usbfs_init failed");

            let dev1 = manager.allocate_dev_num().expect("alloc 1");
            let dev2 = manager.allocate_dev_num().expect("alloc 2");
            assert_eq!(dev1, 2);
            assert_eq!(dev2, 3);

            manager.release_dev_num(dev1);
            let dev3 = manager.allocate_dev_num().expect("alloc 3");
            assert_eq!(dev3, 4, "Monotonic device number allocation must increment");

            manager.release_dev_num(dev3);
            manager.release_dev_num(dev2);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_watch_usb_devices_missing_service_handled_gracefully() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let manager = usbfs_init(kernel).expect("usbfs_init failed");
            assert!(manager.allocate_dev_num().is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_barebones_ioctl_returns_enotty() {
        spawn_kernel_with_features_and_run(
            async |current_task| {
                let kernel = current_task.kernel();
                let manager = usbfs_init(kernel).expect("usbfs_init failed");

                let (device_desc, config_bytes) = create_mock_hid_descriptors();
                let usb_dev =
                    Arc::new(UsbDevice::new(1, 2, USB_SPEED_HIGH, device_desc, config_bytes));
                let registered = manager.add_device(kernel, usb_dev.clone()).unwrap();

                let file = open_dev_node(current_task, b"bus/usb/001/002".into(), OpenFlags::RDWR)
                    .expect("open_dev_node failed");

                // Calling any ioctl in barebones returns ENOTTY
                let res = file.ops().ioctl(&file, current_task, 0x1234, 0u64.into());
                assert_eq!(res, error!(ENOTTY));

                // Disconnecting the device causes ioctl to return ENODEV
                usb_dev.on_disconnect();
                let res_disconn = file.ops().ioctl(&file, current_task, 0x1234, 0u64.into());
                assert_eq!(res_disconn, error!(ENODEV));

                manager.remove_device(current_task, registered).unwrap();
            },
            KernelFeatures { devtmpfs: true, ..Default::default() },
        )
        .await;
    }
}
