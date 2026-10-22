// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests for the Starnix power residency sysfs files, as seen from userspace.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const RESIDENCY_DEVICE: &str = "powerdashboard";
const CAP_SYSFS_DEVICE: &str = "cap_sysfs";

/// Each device and the files expected under it, in sorted order.
const DEVICES: [(&str, &[&str]); 2] = [
    (RESIDENCY_DEVICE, &["lpcm_gmc0", "platform_power_residency", "pwrblk_dpu"]),
    (CAP_SYSFS_DEVICE, &["dvfs_stats_clk_lit", "power_state_stats"]),
];

/// How long to wait for a device to be added before failing.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(10);

fn bus_device_link(device: &str) -> PathBuf {
    Path::new("/sys/bus/platform/devices").join(device)
}

/// Returns the directory for `device`, waiting up to [`DEVICE_TIMEOUT`] for it to be added.
///
/// Each device is only added once the kernel has queried the power stats provider, which happens
/// in the background after the container starts. The bus link is added after the device
/// directory has been populated, so waiting for it ensures that all of the files are present.
fn device_dir(device: &str) -> PathBuf {
    let bus_link = bus_device_link(device);
    let deadline = Instant::now() + DEVICE_TIMEOUT;
    while !bus_link.exists() {
        assert!(
            Instant::now() < deadline,
            "{} was not added within {DEVICE_TIMEOUT:?}; is the power stats provider running?",
            bus_link.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    Path::new("/sys/devices/platform").join(device)
}

fn read(device: &str, file: &str) -> String {
    std::fs::read_to_string(device_dir(device).join(file))
        .unwrap_or_else(|e| panic!("failed to read {device}/{file}: {e}"))
}

#[test]
fn devices_are_on_platform_bus() {
    for (device, _) in DEVICES {
        let dir = device_dir(device);
        let linked =
            std::fs::canonicalize(bus_device_link(device)).expect("failed to resolve bus link");
        assert_eq!(linked, dir);
        assert!(read(device, "uevent").contains("SUBSYSTEM=platform\n"));
    }
}

#[test]
fn perf_state_residency_is_reported() {
    assert_eq!(
        read(RESIDENCY_DEVICE, "lpcm_gmc0"),
        "num_pf_states: 2\n\
         pf_state: 0\npf_entry_count: 11\npf_time_in_state: 2200000 (us)\n\n\
         pf_state: 1\npf_entry_count: 4\npf_time_in_state: 7000 (us)\n\n"
    );
}

#[test]
fn power_domain_residency_is_reported() {
    assert_eq!(
        read(RESIDENCY_DEVICE, "pwrblk_dpu"),
        "\nSSWRP lpb\n\n\
         power_state: STATE_OFF\nentry_count: 3\ntime_in_state: 12000 (us)\n\
         last_entry_timestamp: 5000\nlast_exit_timestamp: 0\n\
         power_state: STATE_ON\nentry_count: 4\ntime_in_state: 7000 (us)\n\
         last_entry_timestamp: 9000\nlast_exit_timestamp: 0\n\n"
    );
}

#[test]
fn platform_power_residency_is_reported() {
    assert_eq!(
        read(RESIDENCY_DEVICE, "platform_power_residency"),
        "power_state: ACTIVE\nentry_count: 2\ntime_in_state_ms: 100\n\
         last_entry_timestamp_ticks: 38400\nlast_exit_timestamp_ticks: 0\n\n\
         power_state: IDLE\nentry_count: 0\ntime_in_state_ms: 0\n\
         last_entry_timestamp_ticks: 0\nlast_exit_timestamp_ticks: 0\n\n"
    );
}

#[test]
fn power_state_stats_are_reported() {
    assert_eq!(
        read(CAP_SYSFS_DEVICE, "power_state_stats"),
        "---\nstate: Cluster 0 off\nlast_entry_ts_ticks: 76800\nlast_exit_ts_ticks: 0\n\
         total_state_residency_ms: 250\nlast_state_entry_counts: 6\n\
         ---\nstate: Suspend\nlast_entry_ts_ticks: 0\nlast_exit_ts_ticks: 0\n\
         total_state_residency_ms: 10\nlast_state_entry_counts: 1\n"
    );
}

#[test]
fn dvfs_stats_are_reported() {
    assert_eq!(
        read(CAP_SYSFS_DEVICE, "dvfs_stats_clk_lit"),
        "---\nclock_domain: clk_lit\nper_op_level_stats: \n\
         \x20 - oplevel: 0\n    entry_count: 7\n    total_residency_ms: 120\n\
         \x20   last_entry_timestamp_ticks: 115200\n\
         \x20 - oplevel: 1\n    entry_count: 0\n    total_residency_ms: 0\n\
         \x20   last_entry_timestamp_ticks: 0\n"
    );
}

#[test]
fn only_residency_entities_are_served() {
    for (device, expected_files) in DEVICES {
        let mut files: Vec<_> = std::fs::read_dir(device_dir(device))
            .expect("failed to read device directory")
            .map(|entry| entry.expect("failed to read entry").file_name().into_string().unwrap())
            .filter(|name| name != "uevent")
            .collect();
        files.sort();
        assert_eq!(files, expected_files, "{device}");
    }
}

#[test]
fn residency_files_are_read_only() {
    for (device, files) in DEVICES {
        let dir = device_dir(device);
        for &file in files {
            let path = dir.join(file);
            let metadata = std::fs::metadata(&path)
                .unwrap_or_else(|e| panic!("failed to get metadata for {device}/{file}: {e}"));
            assert_eq!(metadata.permissions().mode() & 0o777, 0o444, "{device}/{file}");
            // The test runs as root, which bypasses the mode bits, so also check that the files
            // cannot be written.
            assert!(std::fs::write(&path, "test").is_err(), "{device}/{file} is writable");
        }
    }
}
