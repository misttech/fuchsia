// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::iio_file::{
    build_battery_power_supply_directory, build_bms_power_supply_directory, build_iio0_directory,
    build_iio3_directory, build_usb_power_supply_directory,
};
use super::qbg_battery_file::create_battery_profile_device;
use super::qbg_file::create_qbg_device;
use super::utils::ReadWriteBytesFile;
use fidl_fuchsia_hardware_qcom_hvdcpopti as fhvdcpopti;
use starnix_core::device::DeviceMode;
use starnix_core::device::kobject::{Class, DeviceMetadata};
use starnix_core::device::service_watcher::{ServiceBindingScope, ServiceWatcher};
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::NamespaceNode;
use starnix_logging::log_warn;
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::open_flags::OpenFlags;
use std::sync::Arc;

pub fn hvdcp_opti_init(kernel: &Kernel) {
    let qbg_class = kernel.device_registry.objects.class_with_dir("qbg".into(), |dir| {
        dir.entry("qbg_context", ReadWriteBytesFile::new_node(), mode!(IFREG, 0o666));
    });
    ServiceWatcher::register_singleton(
        kernel,
        fhvdcpopti::ServiceMarker,
        move |current_task, service_proxy, scope| {
            if let Err(e) =
                register_hvdcp_opti_devices(current_task, &qbg_class, service_proxy, scope)
            {
                log_warn!("Failed to initialize hvdcp_opti devices: {e:?}");
            }
        },
    );
}

fn register_hvdcp_opti_devices(
    current_task: &CurrentTask,
    qbg_class: &Class,
    service_proxy: Arc<fhvdcpopti::ServiceProxy>,
    scope: &mut ServiceBindingScope,
) -> Result<(), Errno> {
    let proxy = Arc::new(service_proxy.connect_to_iio_sync().map_err(|e| {
        log_warn!("Could not connect to hvdcp_opti IIO protocol: {e}");
        errno!(EINVAL)
    })?);

    let kernel = current_task.kernel();
    let registry = &kernel.device_registry;
    let objects = &registry.objects;
    let qbg_battery_class = objects.get_or_create_class("qbg_battery".into());
    let soc = objects.soc_device();
    let platform_bus = objects.platform_bus();
    let spmi_bus = objects.get_or_create_bus("spmi".into());
    let iio = objects.get_or_create_bus("iio".into());
    let power_supply = objects.get_or_create_class("power_supply".into());

    // /dev/qbg
    let qbg_service = service_proxy.clone();
    let qbg_dev = registry.register_device(
        kernel,
        "qbg".into(),
        DeviceMetadata::new("qbg".into(), DeviceId::new(484, 0), DeviceMode::Char),
        qbg_class.clone(),
        move |_current_task: &CurrentTask,
              _id: DeviceId,
              _node: &NamespaceNode,
              _flags: OpenFlags| create_qbg_device(&qbg_service),
    )?;

    // /dev/qbg_battery
    let battery_service = service_proxy;
    let qbg_battery_dev = match registry.register_device(
        kernel,
        "qbg_battery".into(),
        DeviceMetadata::new("qbg_battery".into(), DeviceId::new(485, 0), DeviceMode::Char),
        qbg_battery_class,
        move |_current_task: &CurrentTask,
              _id: DeviceId,
              _node: &NamespaceNode,
              _flags: OpenFlags| create_battery_profile_device(&battery_service),
    ) {
        Ok(dev) => dev,
        Err(e) => {
            registry.remove_device(current_task, qbg_dev);
            return Err(e);
        }
    };

    scope.add_scoped_device(qbg_dev);
    scope.add_scoped_device(qbg_battery_dev);

    let spmi = scope.add_scoped_device(registry.add_bus_device(
        "1c40000.qcom,spmi".into(),
        Some(soc),
        platform_bus.clone(),
        build_device_directory,
    ));
    let spmi_0 = scope.add_scoped_device(registry.add_bus_device(
        "spmi-0".into(),
        Some(spmi),
        spmi_bus.clone(),
        build_device_directory,
    ));
    let spmi_0_00 = scope.add_scoped_device(registry.add_bus_device(
        "0-00".into(),
        Some(spmi_0),
        spmi_bus,
        build_device_directory,
    ));
    let smblite = scope.add_scoped_device(registry.add_bus_device(
        "1c40000.qcom,spmi:qcom,pm5100@0:qcom,qpnp-smblite".into(),
        Some(spmi_0_00.clone()),
        platform_bus.clone(),
        build_device_directory,
    ));
    let qbg = scope.add_scoped_device(registry.add_bus_device(
        "1c40000.qcom,spmi:qcom,pm5100@0:qpnp,qbg@4f00".into(),
        Some(spmi_0_00),
        platform_bus,
        build_device_directory,
    ));

    // /sys/bus/iio/devices/iio:device{0,3}
    scope.add_scoped_device(registry.add_bus_device(
        "iio:device0".into(),
        Some(smblite.clone()),
        iio.clone(),
        |device, dir| build_iio0_directory(device, &proxy, dir),
    ));

    scope.add_scoped_device(registry.add_bus_device(
        "iio:device3".into(),
        Some(qbg.clone()),
        iio,
        |device, dir| build_iio3_directory(device, &proxy, dir),
    ));

    // /sys/class/power_supply/usb
    scope.add_scoped_device(registry.add_numberless_device(
        "usb".into(),
        Some(smblite.clone()),
        power_supply.clone(),
        |device, dir| build_usb_power_supply_directory(device, &proxy, dir),
    ));

    // /sys/class/power_supply/battery
    scope.add_scoped_device(registry.add_numberless_device(
        "battery".into(),
        Some(smblite),
        power_supply.clone(),
        |device, dir| build_battery_power_supply_directory(device, &proxy, dir),
    ));

    // /sys/class/power_supply/bms
    scope.add_scoped_device(registry.add_numberless_device(
        "bms".into(),
        Some(qbg),
        power_supply,
        build_bms_power_supply_directory,
    ));

    scope.monitor_channel(proxy);
    Ok(())
}
