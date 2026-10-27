// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg_attr(test, recursion_limit = "256")]

use starnix_core::device::DeviceMode;
use starnix_core::device::kobject::DeviceMetadata;
use starnix_core::fs::sysfs::{
    BlockDeviceInfo, build_block_device_directory, build_device_directory,
};
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::pseudo::stub_empty_file::StubEmptyFile;
use starnix_core::vfs::{FileOps, FsString, NamespaceNode};
use starnix_logging::{bug_ref, track_stub};
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::open_flags::OpenFlags;
use std::sync::Arc;

struct MmcBlockDevice;

impl BlockDeviceInfo for MmcBlockDevice {
    fn size(&self) -> Result<usize, Errno> {
        track_stub!(TODO("https://fxbug.dev/488067251"), "mmcblk query size");
        Err(errno!(ENOTSUP))
    }
}

/// Initializes stub eMMC host (`mmc0`), card (`mmc0:0001`), and block (`mmcblk0`) devices.
pub fn mmcblk_stub_device_init(kernel: &Kernel) -> Result<(), Errno> {
    let registry = &kernel.device_registry;
    let objects = &registry.objects;

    // TODO(https://fxbug.dev/425942145): Parent `mmc0` under the SoC's SDHCI controller
    // platform device, once the product-specific controller address is known.
    let mmc0 = registry.add_numberless_device(
        "mmc0".into(),
        /* parent = */ None,
        objects.get_or_create_class("mmc_host".into()),
        build_device_directory,
    );
    let mmc_card = registry.add_bus_device(
        "mmc0:0001".into(),
        Some(mmc0),
        objects.get_or_create_bus("mmc".into()),
        |device, dir| {
            build_device_directory(device, dir);
            for name in ["fwrev", "hwrev", "life_time", "manfid", "pre_eol_info", "serial"] {
                dir.entry(
                    name,
                    StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                    mode!(IFREG, 0o444),
                );
            }
        },
    );

    let name = FsString::from("mmcblk0");
    let device = Arc::new(MmcBlockDevice);
    let device_weak = Arc::downgrade(&device);
    registry.register_device_with_dir(
        kernel,
        name.as_ref(),
        DeviceMetadata::new(name.clone(), DeviceId::MMCBLK0, DeviceMode::Block),
        Some(mmc_card),
        objects.block_class(),
        |device, dir| build_block_device_directory(device, device_weak, dir),
        move |_current_task: &CurrentTask,
              _id: DeviceId,
              _node: &NamespaceNode,
              _flags: OpenFlags|
              -> Result<Box<dyn FileOps>, Errno> {
            // Retain `Arc<MmcBlockDevice>` for the lifetime of the registered device so the
            // `Weak<dyn BlockDeviceInfo>` passed to `build_block_device_directory` remains valid.
            let _keep_alive = &device;
            track_stub!(TODO("https://fxbug.dev/488067251"), "mmcblk open device");
            Err(errno!(ENOTSUP))
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use starnix_core::testing::{anon_test_file, spawn_kernel_and_run};
    use starnix_core::vfs::VecOutputBuffer;

    #[::fuchsia::test]
    async fn mmcblk_stub_device_init_registers_host_card_and_block_devices() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            mmcblk_stub_device_init(kernel).expect("mmcblk_stub_device_init should succeed");

            let root = &kernel.device_registry.objects.root;
            assert!(root.lookup("class/mmc_host/mmc0".into()).is_some());
            assert!(root.lookup("bus/mmc/devices/mmc0:0001".into()).is_some());
            assert!(root.lookup("block/mmcblk0".into()).is_some());
            assert!(root.lookup("class/block/mmcblk0".into()).is_some());
            assert!(root.lookup("devices/virtual/mmc_host/mmc0/uevent".into()).is_some());
            assert!(
                root.lookup("devices/virtual/mmc_host/mmc0/mmc0:0001/life_time".into()).is_some()
            );
            assert!(
                root.lookup("devices/virtual/mmc_host/mmc0/mmc0:0001/block/mmcblk0/holders".into())
                    .is_some()
            );

            let size_node = root
                .lookup("devices/virtual/mmc_host/mmc0/mmc0:0001/block/mmcblk0/size".into())
                .expect("size node should exist");
            let file_ops = size_node.create_file_ops(current_task, OpenFlags::RDONLY).unwrap();
            let file = anon_test_file(current_task, file_ops, OpenFlags::RDONLY);
            let mut buf = VecOutputBuffer::new(10);
            assert_eq!(file.read(current_task, &mut buf).unwrap_err(), errno!(ENOTSUP));
        })
        .await;
    }
}
