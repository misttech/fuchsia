// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg_attr(test, recursion_limit = "256")]

use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::Kernel;
use starnix_core::vfs::pseudo::stub_bytes_file::StubBytesFile;
use starnix_logging::bug_ref;
use starnix_uapi::file_mode::mode;

/// Initializes the stub `battery` power-supply device in sysfs.
pub fn battery_manager_device_init(kernel: &Kernel) {
    let registry = &kernel.device_registry;
    // TODO(https://fxbug.dev/574568519): Back `/sys/class/power_supply/battery` with
    // `fuchsia.power.battery.BatteryManager` / `BatterySimulator`.
    registry.add_numberless_device(
        "battery".into(),
        /* parent = */ None,
        registry.objects.power_supply_class(),
        |device, dir| {
            build_device_directory(device, dir);
            dir.entry(
                "capacity",
                StubBytesFile::new_node_with_data(bug_ref!("https://fxbug.dev/573313995"), b"0"),
                mode!(IFREG, 0o644),
            );
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use starnix_core::testing::{anon_test_file, spawn_kernel_and_run};
    use starnix_core::vfs::{VecInputBuffer, VecOutputBuffer};
    use starnix_uapi::open_flags::OpenFlags;

    #[::fuchsia::test]
    async fn battery_manager_device_init_registers_power_supply_battery() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            battery_manager_device_init(kernel);

            let root = &kernel.device_registry.objects.root;
            assert!(root.lookup("class/power_supply/battery".into()).is_some());
            let battery_capacity = root
                .lookup("devices/virtual/power_supply/battery/capacity".into())
                .expect("battery capacity node should exist");
            assert_eq!(battery_capacity.info().mode, mode!(IFREG, 0o644));

            let file_ops = battery_capacity.create_file_ops(current_task, OpenFlags::RDWR).unwrap();
            let file = anon_test_file(current_task, file_ops, OpenFlags::RDWR);
            let mut output = VecOutputBuffer::new(16);
            let n = file.read(current_task, &mut output).unwrap();
            assert_eq!(&Vec::from(output)[..n], b"0");

            let mut input = VecInputBuffer::new(b"77\n");
            assert_eq!(file.write(current_task, &mut input).unwrap(), 3);
        })
        .await;
    }
}
