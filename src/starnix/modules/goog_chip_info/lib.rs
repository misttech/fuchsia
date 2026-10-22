// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg_attr(test, recursion_limit = "256")]

use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::Kernel;
use starnix_core::vfs::pseudo::stub_bytes_file::StubBytesFile;
use starnix_logging::bug_ref;
use starnix_uapi::file_mode::mode;

/// Initializes the stub `/sys/devices/system/goog-chip-info` device in sysfs.
pub fn goog_chip_info_device_init(kernel: &Kernel) {
    let registry = &kernel.device_registry;
    registry.add_subsystemless_device(
        "goog-chip-info".into(),
        Some(registry.objects.system_device()),
        |device, dir| {
            build_device_directory(device, dir);
            for name in ["device_table", "dvfs_table", "ids_table"] {
                dir.entry(
                    name,
                    StubBytesFile::new_node_with_data(
                        bug_ref!("https://fxbug.dev/560212202"),
                        b"Placeholder",
                    ),
                    mode!(IFREG, 0o444),
                );
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use starnix_core::testing::{anon_test_file, spawn_kernel_and_run};
    use starnix_core::vfs::VecOutputBuffer;
    use starnix_uapi::open_flags::OpenFlags;

    #[::fuchsia::test]
    async fn goog_chip_info_device_init_registers_system_device() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            goog_chip_info_device_init(kernel);

            let root = &kernel.device_registry.objects.root;
            assert!(root.lookup("devices/system/goog-chip-info/uevent".into()).is_some());
            for name in ["device_table", "dvfs_table", "ids_table"] {
                let path = format!("devices/system/goog-chip-info/{name}");
                let node =
                    root.lookup(path.as_str().into()).unwrap_or_else(|| panic!("{path} missing"));
                assert_eq!(node.info().mode, mode!(IFREG, 0o444));
                let file_ops = node.create_file_ops(current_task, OpenFlags::RDONLY).unwrap();
                let file = anon_test_file(current_task, file_ops, OpenFlags::RDONLY);
                let mut buf = VecOutputBuffer::new(16);
                let n = file.read(current_task, &mut buf).unwrap();
                assert_eq!(&Vec::from(buf)[..n], b"Placeholder");
            }
        })
        .await;
    }
}
