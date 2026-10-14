// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::iio_file::{
    build_battery_power_supply_directory, build_iio0_directory, build_iio1_directory,
    build_usb_power_supply_directory,
};
use super::qbg_battery_file::create_battery_profile_device;
use super::qbg_file::create_qbg_device;
use super::utils::ReadWriteBytesFile;
use fidl_fuchsia_hardware_qcom_hvdcpopti as fhvdcpopti;
use starnix_core::device::DeviceMode;
use starnix_core::device::kobject::DeviceMetadata;
use starnix_core::device::service_watcher::watch_fidl_service;
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::NamespaceNode;
use starnix_logging::log_warn;
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::open_flags::OpenFlags;
use std::cell::Cell;
use std::sync::Arc;

pub fn hvdcp_opti_init(kernel: &Kernel) {
    let device_added = Cell::new(false);
    watch_fidl_service(
        kernel,
        fhvdcpopti::ServiceMarker,
        "hvdcp_opti",
        async move |weak_kernel, service_proxy| {
            let Some(kernel) = weak_kernel.upgrade() else {
                return;
            };
            if device_added.get() {
                log_warn!("Ignoring additional hvdcp_opti service instance");
                return;
            }
            match register_hvdcp_opti_devices(&kernel, Arc::new(service_proxy)) {
                Ok(()) => device_added.set(true),
                Err(e) => log_warn!("Failed to initialize hvdcp_opti devices: {e:?}"),
            }
        },
    );
}

fn register_hvdcp_opti_devices(
    kernel: &Kernel,
    service_proxy: Arc<fhvdcpopti::ServiceProxy>,
) -> Result<(), Errno> {
    let proxy = Arc::new(service_proxy.connect_to_iio_sync().map_err(|e| {
        log_warn!("Could not connect to hvdcp_opti IIO protocol: {e}");
        errno!(EINVAL)
    })?);

    let registry = &kernel.device_registry;

    let qdb_class = registry.objects.class_with_dir("qbg".into(), |dir| {
        dir.entry("qbg_context", ReadWriteBytesFile::new_node(), mode!(IFREG, 0o666));
    });

    // /dev/qbg
    let qbg_service = service_proxy.clone();
    registry.register_device(
        kernel,
        "qbg".into(),
        DeviceMetadata::new("qbg".into(), DeviceId::new(484, 0), DeviceMode::Char),
        qdb_class,
        move |_current_task: &CurrentTask,
              _id: DeviceId,
              _node: &NamespaceNode,
              _flags: OpenFlags| create_qbg_device(&qbg_service),
    )?;

    // /dev/qbg_battery
    registry.register_device(
        kernel,
        "qbg_battery".into(),
        DeviceMetadata::new("qbg_battery".into(), DeviceId::new(485, 0), DeviceMode::Char),
        registry.objects.get_or_create_class("qbg_battery".into()),
        move |_current_task: &CurrentTask,
              _id: DeviceId,
              _node: &NamespaceNode,
              _flags: OpenFlags| create_battery_profile_device(&service_proxy),
    )?;

    let soc = registry.objects.soc_device();
    let platform_bus = registry.objects.platform_bus();
    let spmi = registry.add_bus_device(
        "1c40000.qcom,spmi".into(),
        Some(soc),
        platform_bus.clone(),
        build_device_directory,
    );
    let spmi_bus = registry.objects.get_or_create_bus("spmi".into());
    let spmi_0 = registry.add_bus_device(
        "spmi-0".into(),
        Some(spmi),
        spmi_bus.clone(),
        build_device_directory,
    );
    let spmi_0_00 =
        registry.add_bus_device("0-00".into(), Some(spmi_0), spmi_bus, build_device_directory);
    let smblite = registry.add_bus_device(
        "1c40000.qcom,spmi:qcom,pm5100@0:qcom,qpnp-smblite".into(),
        Some(spmi_0_00.clone()),
        platform_bus.clone(),
        build_device_directory,
    );
    let qbg = registry.add_bus_device(
        "1c40000.qcom,spmi:qcom,pm5100@0:qpnp,qbg@4f00".into(),
        Some(spmi_0_00),
        platform_bus,
        build_device_directory,
    );

    // /sys/bus/iio/devices/iio:device{0,1}
    let iio = registry.objects.get_or_create_bus("iio".into());
    registry.add_bus_device("iio:device0".into(), Some(smblite), iio.clone(), |device, dir| {
        build_iio0_directory(device, &proxy, dir)
    });

    registry.add_bus_device("iio:device1".into(), Some(qbg), iio, |device, dir| {
        build_iio1_directory(device, &proxy, dir)
    });

    let power_supply = registry.objects.get_or_create_class("power_supply".into());
    // /sys/class/power_supply/usb
    registry.add_numberless_device(
        "usb".into(),
        /* parent = */ None,
        power_supply.clone(),
        |device, dir| build_usb_power_supply_directory(device, &proxy, dir),
    );

    // /sys/class/power_supply/battery
    registry.add_numberless_device(
        "battery".into(),
        /* parent = */ None,
        power_supply.clone(),
        |device, dir| build_battery_power_supply_directory(device, &proxy, dir),
    );

    // /sys/class/power_supply/bms
    registry.add_numberless_device(
        "bms".into(),
        /* parent = */ None,
        power_supply,
        build_device_directory,
    );
    Ok(())
}
