// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests for the Starnix power residency sysfs files, as seen from userspace.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

const DEVICE_DIR: &str = "/sys/devices/platform/powerdashboard";
const BUS_DEVICE_LINK: &str = "/sys/bus/platform/devices/powerdashboard";
const RESIDENCY_FILES: [&str; 2] = ["lpcm_gmc0", "platform_power_residency"];

/// How long to wait for the device to be added before failing.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(10);

/// Returns the device directory, waiting up to [`DEVICE_TIMEOUT`] for the device to be added.
///
/// The device is only added once the kernel has queried the power stats provider, which happens
/// in the background after the container starts. The bus link is added after the device
/// directory has been populated, so waiting for it ensures that all of the files are present.
fn device_dir() -> &'static Path {
    let deadline = Instant::now() + DEVICE_TIMEOUT;
    while !Path::new(BUS_DEVICE_LINK).exists() {
        assert!(
            Instant::now() < deadline,
            "{BUS_DEVICE_LINK} was not added within {DEVICE_TIMEOUT:?}; is the power stats \
             provider running?"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    Path::new(DEVICE_DIR)
}

fn read(file: &str) -> String {
    std::fs::read_to_string(device_dir().join(file))
        .unwrap_or_else(|e| panic!("failed to read {file}: {e}"))
}

#[test]
fn device_is_on_platform_bus() {
    let dir = device_dir();
    let linked = std::fs::canonicalize(BUS_DEVICE_LINK).expect("failed to resolve bus link");
    assert_eq!(linked, dir);
    assert!(read("uevent").contains("SUBSYSTEM=platform\n"));
}

#[test]
fn perf_state_residency_is_reported() {
    assert_eq!(
        read("lpcm_gmc0"),
        "num_pf_states: 2\n\
         pf_state: 0\npf_entry_count: 11\npf_time_in_state: 2200000 (us)\n\n\
         pf_state: 1\npf_entry_count: 4\npf_time_in_state: 7000 (us)\n\n"
    );
}

#[test]
fn platform_power_residency_is_reported() {
    assert_eq!(
        read("platform_power_residency"),
        "power_state: ACTIVE\nentry_count: 2\ntime_in_state_ms: 100\n\
         last_entry_timestamp_ticks: 38400\nlast_exit_timestamp_ticks: 0\n\n\
         power_state: IDLE\nentry_count: 0\ntime_in_state_ms: 0\n\
         last_entry_timestamp_ticks: 0\nlast_exit_timestamp_ticks: 0\n\n"
    );
}

#[test]
fn only_residency_entities_are_served() {
    let mut files: Vec<_> = std::fs::read_dir(device_dir())
        .expect("failed to read device directory")
        .map(|entry| entry.expect("failed to read entry").file_name().into_string().unwrap())
        .filter(|name| name != "uevent")
        .collect();
    files.sort();
    assert_eq!(files, RESIDENCY_FILES);
}

#[test]
fn residency_files_are_read_only() {
    for file in RESIDENCY_FILES {
        let path = device_dir().join(file);
        let metadata = std::fs::metadata(&path)
            .unwrap_or_else(|e| panic!("failed to get metadata for {file}: {e}"));
        assert_eq!(metadata.permissions().mode() & 0o777, 0o444, "{file}");
        // The test runs as root, which bypasses the mode bits, so also check that the files
        // cannot be written.
        assert!(std::fs::write(&path, "test").is_err(), "{file} is writable");
    }
}
