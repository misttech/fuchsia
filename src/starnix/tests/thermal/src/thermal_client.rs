// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assert_matches::assert_matches;
use linux_uapi::{THERMAL_GENL_EVENT_GROUP_NAME, THERMAL_GENL_SAMPLING_GROUP_NAME};
use netlink_packet_core::{NLM_F_REQUEST, NetlinkMessage, NetlinkPayload};
use netlink_packet_generic::GenlMessage;
use netlink_packet_generic::ctrl::nlas::{GenlCtrlAttrs, McastGrpAttrs};
use netlink_packet_generic::ctrl::{GenlCtrl, GenlCtrlCmd};
use netlink_packet_generic::message::EmptyDeserializeOptions;
use nix::sys::socket;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};
use thermal_netlink::{GenlThermalCmd, GenlThermalPayload, ThermalAttr, celsius_to_millicelsius};

pub const EXPECTED_TEMP_C: f32 = 25.0;
const FREQUENCIES_HZ: [&'static str; 2] = ["1128000 1256000 1512000 2024000", "512000 1024000"];
const MAX_FREQUENCIES_HZ: [&'static str; 2] = ["2024000", "1024000"];
const MIN_FREQUENCIES_HZ: [&'static str; 2] = ["1128000", "512000"];

fn main() {
    println!("started");
    check_soft_max_freq_releases_external_cap();

    check_thermal_zone_is_available();
    check_emul_temp();
    check_trip_points();

    check_cpu_cooling_device_is_available();
    check_cpu_cooling_device();
    check_cpufreq();

    check_nlctrl_is_available();

    check_cooling_fcc_is_available();
    check_cooling_fcc();

    check_powercap();

    let thermal_mcast_groups = check_thermal_is_available();
    let sampling_group_id =
        thermal_mcast_groups.get(THERMAL_GENL_SAMPLING_GROUP_NAME.to_str().unwrap()).unwrap();
    check_thermal_sampling_returns_samples(*sampling_group_id);
    println!("done");
}

fn check_thermal_zone_is_available() {
    let sensor_name = std::fs::read("/sys/class/thermal/thermal_zone0/type").unwrap();
    assert_eq!("fake-trippoint\n", str::from_utf8(&sensor_name).unwrap());

    // Due to races between DriverTestRealm and the test environment the
    // expected value may not immediately be set. Loop until we get a match.
    let now = Instant::now();
    loop {
        let temp_c = std::fs::read("/sys/class/thermal/thermal_zone0/temp").unwrap();
        if str::from_utf8(&temp_c).unwrap()
            == &format!("{}\n", celsius_to_millicelsius(EXPECTED_TEMP_C) as u32)
        {
            break;
        }
        if now.elapsed() > Duration::from_secs(5) {
            println!("Temperature reading taking longer than 5 seconds...");
        }
    }
}

fn check_emul_temp() {
    {
        let real_temp_str = std::fs::read("/sys/class/thermal/thermal_zone0/temp").unwrap();
        let expected_real_temp = celsius_to_millicelsius(EXPECTED_TEMP_C) as u32;
        assert_eq!(&format!("{}\n", expected_real_temp), str::from_utf8(&real_temp_str).unwrap());
    }
    {
        let expected_temp = "100000\n";
        std::fs::write("/sys/class/thermal/thermal_zone0/emul_temp", expected_temp).unwrap();
        let fake_temp_str = std::fs::read("/sys/class/thermal/thermal_zone0/temp").unwrap();
        assert_eq!(expected_temp, str::from_utf8(&fake_temp_str).unwrap());
    }
    {
        let expected_temp = "50000\n";
        std::fs::write("/sys/class/thermal/thermal_zone0/emul_temp", expected_temp).unwrap();
        let fake_temp_str = std::fs::read("/sys/class/thermal/thermal_zone0/temp").unwrap();
        assert_eq!(expected_temp, str::from_utf8(&fake_temp_str).unwrap());
    }
    {
        let expected_temp = "-274000\n";
        std::fs::write("/sys/class/thermal/thermal_zone0/emul_temp", expected_temp).unwrap();
        let fake_temp_str = std::fs::read("/sys/class/thermal/thermal_zone0/temp").unwrap();
        assert_eq!(expected_temp, str::from_utf8(&fake_temp_str).unwrap());
    }

    // Reset emul_temp.
    std::fs::write("/sys/class/thermal/thermal_zone0/emul_temp", "0").unwrap();
    let real_temp_str = std::fs::read("/sys/class/thermal/thermal_zone0/temp").unwrap();
    let expected_real_temp = celsius_to_millicelsius(EXPECTED_TEMP_C) as u32;
    assert_eq!(&format!("{}\n", expected_real_temp), str::from_utf8(&real_temp_str).unwrap());
}

fn check_trip_points() {
    let temp_path = "/sys/class/thermal/thermal_zone0/trip_point_0_temp";
    let hyst_path = "/sys/class/thermal/thermal_zone0/trip_point_0_hyst";

    // Initial trip point values are 0.
    assert_eq!("0\n", std::fs::read_to_string(temp_path).unwrap());
    assert_eq!("0\n", std::fs::read_to_string(hyst_path).unwrap());

    // Configure hysteresis then trip temperature (matching Android Thermal HAL order).
    std::fs::write(hyst_path, "2000\n").unwrap();
    assert_eq!("2000\n", std::fs::read_to_string(hyst_path).unwrap());

    std::fs::write(temp_path, "75000\n").unwrap();
    assert_eq!("75000\n", std::fs::read_to_string(temp_path).unwrap());

    // Updating hysteresis while a trip point is active updates the low threshold.
    std::fs::write(hyst_path, "3000").unwrap();
    assert_eq!("3000\n", std::fs::read_to_string(hyst_path).unwrap());

    // Negative hysteresis is rejected with EINVAL.
    let err = std::fs::write(hyst_path, "-1000").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    assert_eq!("3000\n", std::fs::read_to_string(hyst_path).unwrap());

    // Out-of-range trip temperature (below absolute zero) is rejected by the driver with
    // ZX_ERR_OUT_OF_RANGE, which maps to EINVAL and leaves trip_point_0_temp unchanged.
    let err = std::fs::write(temp_path, "-300000").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    assert_eq!("75000\n", std::fs::read_to_string(temp_path).unwrap());

    // Writing 0 clears the trip point.
    std::fs::write(temp_path, "0").unwrap();
    assert_eq!("0\n", std::fs::read_to_string(temp_path).unwrap());
    std::fs::write(hyst_path, "0").unwrap();
    assert_eq!("0\n", std::fs::read_to_string(hyst_path).unwrap());

    // Leave a known non-zero trip point and hysteresis configured so the host integration test
    // can verify the programmed OneshotTempAbove and OneshotTempBelow descriptors over FIDL.
    std::fs::write(hyst_path, "3000\n").unwrap();
    std::fs::write(temp_path, "75000\n").unwrap();
    assert_eq!("3000\n", std::fs::read_to_string(hyst_path).unwrap());
    assert_eq!("75000\n", std::fs::read_to_string(temp_path).unwrap());
}

/// The test harness caps domain 0 (`policy0`, 4 OPPs) at intermediate OPP 1 (`1512000` kHz)
/// through `DomainController` before Starnix starts, mirroring a boot-time limit (`boot_max_opp`)
/// applied by cpu-manager. Starnix seeds that initial ceiling into `vote_manager/soft_max_freq`
/// while leaving `cooling_device0` at state 0:
/// - unvoted voters clearing their votes must not drop the boot cap;
/// - a stricter runtime vote (`1256000` kHz) lowers `scaling_max_freq` further, and releasing that
///   stricter runtime vote restores the `soft_max_freq` boot ceiling (`1512000` kHz) on
///   `DomainController`;
/// - clearing `soft_max_freq` releases the cap completely (`2024000` kHz).
fn check_soft_max_freq_releases_external_cap() {
    const COOLING0_STATE: &str = "/sys/class/thermal/cooling_device0/cur_state";
    const POLICY0_MAX: &str = "/sys/devices/system/cpu/cpufreq/policy0/scaling_max_freq";
    const POLICY0_SOFT_MAX: &str =
        "/sys/devices/system/cpu/cpufreq/policy0/vote_manager/soft_max_freq";
    const POLICY0_POWERHINT_MAX: &str =
        "/sys/devices/system/cpu/cpufreq/policy0/vote_manager/powerhint_max_freq";

    assert_eq!("1512000\n", read_sysfs(POLICY0_MAX));
    assert_eq!("1512000\n", read_sysfs(POLICY0_SOFT_MAX));
    assert_eq!("2024000\n", read_sysfs(POLICY0_POWERHINT_MAX));
    assert_eq!("0\n", read_sysfs(COOLING0_STATE));

    // Uncapping an unvoted voter while `soft_max_freq` holds the boot cap keeps the cap.
    std::fs::write(POLICY0_POWERHINT_MAX, "9999999").unwrap();
    std::fs::write(COOLING0_STATE, "0").unwrap();
    assert_eq!("1512000\n", read_sysfs(POLICY0_MAX));
    assert_eq!("1512000\n", read_sysfs(POLICY0_SOFT_MAX));

    // Applying a stricter runtime vote lowers `scaling_max_freq` below the boot cap, and releasing
    // that vote restores the `soft_max_freq` boot ceiling on `DomainController`.
    std::fs::write(POLICY0_POWERHINT_MAX, "1256000").unwrap();
    assert_eq!("1256000\n", read_sysfs(POLICY0_MAX));
    assert_eq!("1512000\n", read_sysfs(POLICY0_SOFT_MAX));
    std::fs::write(POLICY0_POWERHINT_MAX, "9999999").unwrap();
    assert_eq!("1512000\n", read_sysfs(POLICY0_MAX));
    assert_eq!("1512000\n", read_sysfs(POLICY0_SOFT_MAX));

    // Clearing `soft_max_freq` releases the boot cap completely.
    std::fs::write(POLICY0_SOFT_MAX, "9999999").unwrap();
    assert_eq!("2024000\n", read_sysfs(POLICY0_SOFT_MAX));
    assert_eq!("2024000\n", read_sysfs(POLICY0_MAX));
}

fn check_cpu_cooling_device_is_available() {
    {
        let sensor_name = std::fs::read("/sys/class/thermal/cooling_device0/type").unwrap();
        assert_eq!("test-cluster0\n", str::from_utf8(&sensor_name).unwrap());
        let cur_state = std::fs::read("/sys/class/thermal/cooling_device0/cur_state").unwrap();
        assert_eq!("0\n", str::from_utf8(&cur_state).unwrap());
        let max_state = std::fs::read("/sys/class/thermal/cooling_device0/max_state").unwrap();
        assert_eq!("3\n", str::from_utf8(&max_state).unwrap());
    }

    {
        let sensor_name = std::fs::read("/sys/class/thermal/cooling_device1/type").unwrap();
        assert_eq!("test-cluster1\n", str::from_utf8(&sensor_name).unwrap());
        let cur_state = std::fs::read("/sys/class/thermal/cooling_device1/cur_state").unwrap();
        assert_eq!("0\n", str::from_utf8(&cur_state).unwrap());
        let max_state = std::fs::read("/sys/class/thermal/cooling_device1/max_state").unwrap();
        assert_eq!("1\n", str::from_utf8(&max_state).unwrap());
    }
}

fn check_cpu_cooling_device() {
    std::fs::write("/sys/class/thermal/cooling_device0/cur_state", "1").unwrap();
    let new_cur_state = std::fs::read("/sys/class/thermal/cooling_device0/cur_state").unwrap();
    assert_eq!("1\n", str::from_utf8(&new_cur_state).unwrap());

    let err = std::fs::write("/sys/class/thermal/cooling_device0/cur_state", "4").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    let unchanged_cur_state =
        std::fs::read("/sys/class/thermal/cooling_device0/cur_state").unwrap();
    assert_eq!("1\n", str::from_utf8(&unchanged_cur_state).unwrap());

    std::fs::write("/sys/class/thermal/cooling_device0/cur_state", "0").unwrap();
    let reset_cur_state = std::fs::read("/sys/class/thermal/cooling_device0/cur_state").unwrap();
    assert_eq!("0\n", str::from_utf8(&reset_cur_state).unwrap());
}

fn check_cpufreq() {
    assert_eq!(
        "0-5\n",
        str::from_utf8(&std::fs::read("/sys/devices/system/cpu/possible").unwrap()).unwrap()
    );
    check_cpufreq_dir(0, 0);
    check_cpufreq_dir(1, 0);
    check_cpufreq_dir(2, 1);
    check_cpufreq_dir(3, 1);
    check_cpufreq_dir(4, 1);
    check_cpufreq_dir(5, 1);

    check_cpufreq_policy_dir(0, 0, "0 1");
    check_cpufreq_policy_dir(2, 1, "2 3 4 5");

    check_scaling_max_freq();
    check_scaling_min_freq();
}

fn read_sysfs(path: &str) -> String {
    String::from_utf8(std::fs::read(path).unwrap()).unwrap()
}

/// Exercises `scaling_max_freq`, which is served by `fuchsia.power.cpu/DomainController` rather
/// than by a value stored in the file.
///
/// Cluster 0's operating points run highest-first, the reverse of the order
/// `scaling_available_frequencies` lists them in, so these cases also cover mapping a requested
/// frequency onto the right operating point.
fn check_scaling_max_freq() {
    const POLICY0: &str = "/sys/devices/system/cpu/cpufreq/policy0/scaling_max_freq";
    const POLICY2: &str = "/sys/devices/system/cpu/cpufreq/policy2/scaling_max_freq";
    const CPU1: &str = "/sys/devices/system/cpu/cpu1/cpufreq/scaling_max_freq";

    // Initially uncapped, reporting the highest operating point of each cluster.
    assert_eq!("2024000\n", read_sysfs(POLICY0));
    assert_eq!("1024000\n", read_sysfs(POLICY2));

    // An exact match caps at that operating point and persists across `close()` (which is what
    // `std::fs::write` does, matching `libperfmgr` with default `HoldFd: false` and shell `echo`).
    // 1256000 sits at sorted position 1 but at operating point 2, so confusing the two orders
    // would show up here.
    std::fs::write(POLICY0, "1256000").unwrap();
    assert_eq!("1256000\n", read_sysfs(POLICY0));
    // A cap belongs to the domain, so the per-core view of the same domain agrees.
    assert_eq!("1256000\n", read_sysfs(CPU1));
    // Other domains are left alone.
    assert_eq!("1024000\n", read_sysfs(POLICY2));

    // Reopening the node (or writing through the per-core alias of the same node) with the
    // sentinel clears the cap.
    std::fs::write(CPU1, "9999999").unwrap();
    assert_eq!("2024000\n", read_sysfs(POLICY0));

    // A request that falls between two operating points snaps down to the lower one.
    std::fs::write(POLICY0, "1300000").unwrap();
    assert_eq!("1256000\n", read_sysfs(POLICY0));

    // A request below every operating point snaps up to the lowest one.
    std::fs::write(POLICY0, "1").unwrap();
    assert_eq!("1128000\n", read_sysfs(POLICY0));

    // Writing the sentinel on a held descriptor also releases the cap. An invalid write must fail
    // with EINVAL without disturbing an active cap.
    let mut file = std::fs::OpenOptions::new().write(true).open(POLICY0).unwrap();
    file.write_all(b"1128000").unwrap();
    assert_eq!("1128000\n", read_sysfs(POLICY0));
    let err = file.write_all(b"not_a_number").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    assert_eq!("1128000\n", read_sysfs(POLICY0));
    file.write_all(b"9999999").unwrap();
    assert_eq!("2024000\n", read_sysfs(POLICY0));
    drop(file);
    assert_eq!("2024000\n", read_sysfs(POLICY0));

    // Concurrent voters (across `vote_manager/*` and `scaling_max_freq`) are keyed by sysfs node
    // identity, aggregate by taking the lowest ceiling, persist across `close()`, and step back to
    // the remaining active vote when cleared with `"9999999"`. Each `vote_manager/*_max_freq` node
    // reads back its own vote (or the domain maximum when uncapped), while `scaling_max_freq`
    // reads back the aggregated domain ceiling.
    const POWERHINT_MAX: &str =
        "/sys/devices/system/cpu/cpu0/cpufreq/vote_manager/powerhint_max_freq";
    const THERMAL_MAX: &str = "/sys/devices/system/cpu/cpu0/cpufreq/vote_manager/thermal_max_freq";
    const SOFT_MAX: &str = "/sys/devices/system/cpu/cpu0/cpufreq/vote_manager/soft_max_freq";
    const DEBUG_MAX: &str = "/sys/devices/system/cpu/cpu0/cpufreq/vote_manager/debug_max_freq";
    assert_eq!("2024000\n", read_sysfs(SOFT_MAX));
    assert_eq!("2024000\n", read_sysfs(POWERHINT_MAX));
    assert_eq!("2024000\n", read_sysfs(THERMAL_MAX));
    assert_eq!("2024000\n", read_sysfs(DEBUG_MAX));

    std::fs::write(SOFT_MAX, "1512000").unwrap();
    assert_eq!("1512000\n", read_sysfs(SOFT_MAX));
    assert_eq!("2024000\n", read_sysfs(POWERHINT_MAX));
    assert_eq!("1512000\n", read_sysfs(POLICY0));
    std::fs::write(POWERHINT_MAX, "1512000").unwrap();
    assert_eq!("1512000\n", read_sysfs(POWERHINT_MAX));
    assert_eq!("1512000\n", read_sysfs(POLICY0));
    std::fs::write(THERMAL_MAX, "1256000").unwrap();
    assert_eq!("1256000\n", read_sysfs(THERMAL_MAX));
    assert_eq!("1512000\n", read_sysfs(POWERHINT_MAX));
    assert_eq!("1256000\n", read_sysfs(POLICY0));
    std::fs::write(DEBUG_MAX, "1128000").unwrap();
    assert_eq!("1128000\n", read_sysfs(DEBUG_MAX));
    assert_eq!("1256000\n", read_sysfs(THERMAL_MAX));
    assert_eq!("1512000\n", read_sysfs(POWERHINT_MAX));
    assert_eq!("1512000\n", read_sysfs(SOFT_MAX));
    assert_eq!("1128000\n", read_sysfs(POLICY0));

    // Clearing the strictest voter restores the next strictest active voter's ceiling.
    std::fs::write(DEBUG_MAX, "9999999").unwrap();
    assert_eq!("2024000\n", read_sysfs(DEBUG_MAX));
    assert_eq!("1256000\n", read_sysfs(POLICY0));
    std::fs::write(THERMAL_MAX, "9999999").unwrap();
    assert_eq!("2024000\n", read_sysfs(THERMAL_MAX));
    assert_eq!("1512000\n", read_sysfs(POLICY0));
    std::fs::write(POWERHINT_MAX, "9999999").unwrap();
    assert_eq!("2024000\n", read_sysfs(POWERHINT_MAX));
    assert_eq!("1512000\n", read_sysfs(SOFT_MAX));
    assert_eq!("1512000\n", read_sysfs(POLICY0));
    std::fs::write(SOFT_MAX, "9999999").unwrap();
    assert_eq!("2024000\n", read_sysfs(SOFT_MAX));
    assert_eq!("2024000\n", read_sysfs(POLICY0));

    // Thermal `cooling_device0/cur_state` and `cpufreq` max-frequency nodes share the domain's
    // ceiling vote table so neither overrides a stricter limit or clears the other's active cap.
    const COOLING0_STATE: &str = "/sys/class/thermal/cooling_device0/cur_state";
    std::fs::write(COOLING0_STATE, "2").unwrap();
    assert_eq!("2\n", read_sysfs(COOLING0_STATE));
    assert_eq!("1256000\n", read_sysfs(POLICY0));

    // A looser cpufreq vote (1512000, OPP 1) must not override the stricter cooling state (OPP 2).
    std::fs::write(POWERHINT_MAX, "1512000").unwrap();
    assert_eq!("1512000\n", read_sysfs(POWERHINT_MAX));
    assert_eq!("1256000\n", read_sysfs(POLICY0));
    assert_eq!("2\n", read_sysfs(COOLING0_STATE));

    // A stricter cpufreq vote (1128000, OPP 3) lowers the effective frequency while preserving
    // the cooling device's reported `cur_state`.
    std::fs::write(THERMAL_MAX, "1128000").unwrap();
    assert_eq!("1128000\n", read_sysfs(THERMAL_MAX));
    assert_eq!("1128000\n", read_sysfs(POLICY0));
    assert_eq!("2\n", read_sysfs(COOLING0_STATE));

    // Releasing the stricter cpufreq vote steps back to the cooling device's cap (1256000).
    std::fs::write(THERMAL_MAX, "9999999").unwrap();
    assert_eq!("2024000\n", read_sysfs(THERMAL_MAX));
    assert_eq!("1256000\n", read_sysfs(POLICY0));

    // Clearing the cooling device's state steps back to the remaining cpufreq vote (1512000)
    // rather than wiping out all caps.
    std::fs::write(COOLING0_STATE, "0").unwrap();
    assert_eq!("0\n", read_sysfs(COOLING0_STATE));
    assert_eq!("1512000\n", read_sysfs(POLICY0));

    std::fs::write(POWERHINT_MAX, "9999999").unwrap();
    assert_eq!("2024000\n", read_sysfs(POWERHINT_MAX));
    assert_eq!("2024000\n", read_sysfs(POLICY0));
}

/// Exercises `scaling_min_freq` and `vote_manager/*_min_freq`, where each voter holds its own
/// persistent `fuchsia.power.cpu/DomainController` lease in `CpuFreqDomain`.
fn check_scaling_min_freq() {
    const POLICY0: &str = "/sys/devices/system/cpu/cpufreq/policy0/scaling_min_freq";
    const POLICY2: &str = "/sys/devices/system/cpu/cpufreq/policy2/scaling_min_freq";
    const CPU1: &str = "/sys/devices/system/cpu/cpu1/cpufreq/scaling_min_freq";

    // Initially unconstrained, reporting the lowest operating point of each cluster.
    assert_eq!("1128000\n", read_sysfs(POLICY0));
    assert_eq!("512000\n", read_sysfs(POLICY2));

    // An exact match raises the floor to that operating point and persists across `close()`.
    // 1512000 sits at sorted position 2 but at operating point 1.
    std::fs::write(POLICY0, "1512000").unwrap();
    assert_eq!("1512000\n", read_sysfs(POLICY0));
    assert_eq!("1512000\n", read_sysfs(CPU1));
    assert_eq!("512000\n", read_sysfs(POLICY2));

    // Writing the domain's lowest frequency (`cpuinfo_min_freq`, which is what `libperfmgr` uses
    // as `DefaultIndex`) or `0` (used by `Release_Hints`) clears the floor.
    std::fs::write(CPU1, "1128000").unwrap();
    assert_eq!("1128000\n", read_sysfs(POLICY0));

    // A request that falls between two operating points snaps up to the higher one.
    std::fs::write(POLICY0, "1300000").unwrap();
    assert_eq!("1512000\n", read_sysfs(POLICY0));

    // A request above every operating point clamps to the highest one.
    std::fs::write(POLICY0, "9999999").unwrap();
    assert_eq!("2024000\n", read_sysfs(POLICY0));

    // Writing `0` on a held descriptor also releases the floor. An invalid write must fail with
    // EINVAL without disturbing an active floor.
    let mut file = std::fs::OpenOptions::new().write(true).open(POLICY0).unwrap();
    file.write_all(b"1512000").unwrap();
    assert_eq!("1512000\n", read_sysfs(POLICY0));
    let err = file.write_all(b"not_a_number").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    assert_eq!("1512000\n", read_sysfs(POLICY0));
    file.write_all(b"0").unwrap();
    assert_eq!("1128000\n", read_sysfs(POLICY0));
    drop(file);
    assert_eq!("1128000\n", read_sysfs(POLICY0));

    // Concurrent min-frequency voters aggregate by taking the highest floor, persist across
    // `close()`, and step down to the remaining active floor when cleared. Each
    // `vote_manager/*_min_freq` node reads back its own floor (or the domain minimum when
    // unconstrained), while `scaling_min_freq` reads back the aggregated domain floor.
    const POWERHINT_MIN: &str =
        "/sys/devices/system/cpu/cpu0/cpufreq/vote_manager/powerhint_min_freq";
    const SOFT_MIN: &str = "/sys/devices/system/cpu/cpu0/cpufreq/vote_manager/soft_min_freq";
    const DEBUG_MIN: &str = "/sys/devices/system/cpu/cpu0/cpufreq/vote_manager/debug_min_freq";
    assert_eq!("1128000\n", read_sysfs(SOFT_MIN));
    assert_eq!("1128000\n", read_sysfs(POWERHINT_MIN));
    assert_eq!("1128000\n", read_sysfs(DEBUG_MIN));

    std::fs::write(SOFT_MIN, "1256000").unwrap();
    assert_eq!("1256000\n", read_sysfs(SOFT_MIN));
    assert_eq!("1128000\n", read_sysfs(POWERHINT_MIN));
    assert_eq!("1256000\n", read_sysfs(POLICY0));
    std::fs::write(POWERHINT_MIN, "1512000").unwrap();
    assert_eq!("1512000\n", read_sysfs(POWERHINT_MIN));
    assert_eq!("1256000\n", read_sysfs(SOFT_MIN));
    assert_eq!("1512000\n", read_sysfs(POLICY0));
    std::fs::write(DEBUG_MIN, "2024000").unwrap();
    assert_eq!("2024000\n", read_sysfs(DEBUG_MIN));
    assert_eq!("1512000\n", read_sysfs(POWERHINT_MIN));
    assert_eq!("1256000\n", read_sysfs(SOFT_MIN));
    assert_eq!("2024000\n", read_sysfs(POLICY0));

    // Clearing the highest voter steps down to the next highest active voter's floor.
    std::fs::write(DEBUG_MIN, "1128000").unwrap();
    assert_eq!("1128000\n", read_sysfs(DEBUG_MIN));
    assert_eq!("1512000\n", read_sysfs(POLICY0));
    std::fs::write(POWERHINT_MIN, "0").unwrap();
    assert_eq!("1128000\n", read_sysfs(POWERHINT_MIN));
    assert_eq!("1256000\n", read_sysfs(SOFT_MIN));
    assert_eq!("1256000\n", read_sysfs(POLICY0));
    std::fs::write(SOFT_MIN, "0").unwrap();
    assert_eq!("1128000\n", read_sysfs(SOFT_MIN));
    assert_eq!("1128000\n", read_sysfs(POLICY0));
}

/// Checks the cpufreq nodes that the Power HAL writes but that Fuchsia cannot act on: they must
/// accept writes and report a stable value, rather than failing the write.
fn check_inert_cpufreq_nodes(dir: &str) {
    std::fs::write(format!("{dir}/scaling_governor"), "schedutil").unwrap();
    assert_eq!("performance\n", read_sysfs(&format!("{dir}/scaling_governor")));
    assert_eq!("performance\n", read_sysfs(&format!("{dir}/scaling_available_governors")));
}

fn check_cpufreq_policy_dir(policy_id: u64, cluster_id: u64, expected_related_cpus: &str) {
    assert!(std::fs::exists(format!("/sys/devices/system/cpu/cpufreq/policy{policy_id}")).unwrap());

    let max_frequency_str = MAX_FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(
        &format!("{max_frequency_str}\n"),
        str::from_utf8(
            &std::fs::read(format!(
                "/sys/devices/system/cpu/cpufreq/policy{policy_id}/cpuinfo_max_freq"
            ))
            .unwrap()
        )
        .unwrap()
    );

    let min_frequency_str = MIN_FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(
        &format!("{min_frequency_str}\n"),
        &read_sysfs(&format!("/sys/devices/system/cpu/cpufreq/policy{policy_id}/cpuinfo_min_freq"))
    );

    let frequencies_str = FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(
        &format!("{frequencies_str}\n"),
        str::from_utf8(
            &std::fs::read(format!(
                "/sys/devices/system/cpu/cpufreq/policy{policy_id}/scaling_available_frequencies"
            ))
            .unwrap()
        )
        .unwrap()
    );

    assert_eq!(
        &format!("{expected_related_cpus}\n"),
        str::from_utf8(
            &std::fs::read(format!(
                "/sys/devices/system/cpu/cpufreq/policy{policy_id}/related_cpus"
            ))
            .unwrap()
        )
        .unwrap()
    );

    let cur_frequency_str = MAX_FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(
        &format!("{cur_frequency_str}\n"),
        str::from_utf8(
            &std::fs::read(format!(
                "/sys/devices/system/cpu/cpufreq/policy{policy_id}/scaling_cur_freq"
            ))
            .unwrap()
        )
        .unwrap()
    );

    check_inert_cpufreq_nodes(&format!("/sys/devices/system/cpu/cpufreq/policy{policy_id}"));
}

fn check_cpufreq_dir(core_id: u64, cluster_id: u64) {
    assert!(std::fs::exists(format!("/sys/devices/system/cpu/cpu{core_id}")).unwrap());
    assert!(std::fs::exists(format!("/sys/devices/system/cpu/cpu{core_id}/cpufreq")).unwrap());

    let max_frequency_str = MAX_FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(
        &format!("{max_frequency_str}\n"),
        str::from_utf8(
            &std::fs::read(format!(
                "/sys/devices/system/cpu/cpu{core_id}/cpufreq/cpuinfo_max_freq"
            ))
            .unwrap()
        )
        .unwrap()
    );

    let min_frequency_str = MIN_FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(
        &format!("{min_frequency_str}\n"),
        &read_sysfs(&format!("/sys/devices/system/cpu/cpu{core_id}/cpufreq/cpuinfo_min_freq"))
    );

    let frequencies_str = FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(
        &format!("{frequencies_str}\n"),
        str::from_utf8(
            &std::fs::read(format!(
                "/sys/devices/system/cpu/cpu{core_id}/cpufreq/scaling_available_frequencies"
            ))
            .unwrap()
        )
        .unwrap()
    );

    assert_eq!(
        &format!("{max_frequency_str}\n"),
        str::from_utf8(
            &std::fs::read(format!(
                "/sys/devices/system/cpu/cpu{core_id}/cpufreq/scaling_cur_freq"
            ))
            .unwrap()
        )
        .unwrap()
    );

    check_inert_cpufreq_nodes(&format!("/sys/devices/system/cpu/cpu{core_id}/cpufreq"));

    assert!(std::fs::exists(format!("/sys/devices/system/cpu/cpu{core_id}/topology")).unwrap());
    assert_eq!(
        &format!("{cluster_id}\n"),
        str::from_utf8(
            &std::fs::read(format!("/sys/devices/system/cpu/cpu{core_id}/topology/cluster_id"))
                .unwrap()
        )
        .unwrap()
    );
    assert_eq!(
        &format!("{cluster_id}\n"),
        str::from_utf8(
            &std::fs::read(format!(
                "/sys/devices/system/cpu/cpu{core_id}/topology/physical_package_id"
            ))
            .unwrap()
        )
        .unwrap()
    );
}

fn check_nlctrl_is_available() {
    let nl_socket = socket::socket(
        socket::AddressFamily::Netlink,
        socket::SockType::Datagram,
        socket::SockFlag::SOCK_CLOEXEC,
        socket::SockProtocol::NetlinkGeneric,
    )
    .unwrap();
    socket::bind(nl_socket.as_raw_fd(), &socket::NetlinkAddr::new(0, 0)).unwrap();
    socket::connect(nl_socket.as_raw_fd(), &socket::NetlinkAddr::new(0, 0)).unwrap();

    let mut genlmsg = GenlMessage::from_payload(GenlCtrl {
        cmd: GenlCtrlCmd::GetFamily,
        nlas: vec![GenlCtrlAttrs::FamilyName("nlctrl".to_owned())],
    });
    genlmsg.finalize();
    let mut nlmsg = NetlinkMessage::from(genlmsg);
    nlmsg.header.flags = NLM_F_REQUEST;
    nlmsg.finalize();

    let mut txbuf = vec![0u8; nlmsg.buffer_len()];
    nlmsg.serialize(&mut txbuf);

    socket::send(nl_socket.as_raw_fd(), &txbuf, socket::MsgFlags::empty()).unwrap();

    let mut rxbuf = vec![0u8; 1024];
    socket::recvfrom::<socket::NetlinkAddr>(nl_socket.as_raw_fd(), &mut rxbuf).unwrap();
    let rx_packet =
        <NetlinkMessage<GenlMessage<GenlCtrl>>>::deserialize(&rxbuf, EmptyDeserializeOptions)
            .unwrap();

    if let NetlinkPayload::InnerMessage(genlmsg) = rx_packet.payload {
        if GenlCtrlCmd::NewFamily == genlmsg.payload.cmd {
            let family_id = genlmsg
                .payload
                .nlas
                .iter()
                .find_map(
                    |nla| {
                        if let GenlCtrlAttrs::FamilyId(id) = nla { Some(*id) } else { None }
                    },
                )
                .expect("Cannot find FamilyId attribute");
            // nlctrl's family must be 16.
            assert_eq!(16, family_id);
        } else {
            panic!("Invalid payload type: {:?}", genlmsg.payload.cmd);
        }
    } else {
        panic!("Failed to get family ID");
    }
}

fn check_cooling_fcc_is_available() {
    let sensor_name = std::fs::read("/sys/class/thermal/cooling_device2/type").unwrap();
    assert_eq!("fcc\n", str::from_utf8(&sensor_name).unwrap());
    let cur_state = std::fs::read("/sys/class/thermal/cooling_device2/cur_state").unwrap();
    assert_eq!("0\n", str::from_utf8(&cur_state).unwrap());
    let max_state = std::fs::read("/sys/class/thermal/cooling_device2/max_state").unwrap();
    assert_eq!("8\n", str::from_utf8(&max_state).unwrap());
}

fn check_cooling_fcc() {
    std::fs::write("/sys/class/thermal/cooling_device2/cur_state", "8").unwrap();
    let new_cur_state = std::fs::read("/sys/class/thermal/cooling_device2/cur_state").unwrap();
    assert_eq!("8\n", str::from_utf8(&new_cur_state).unwrap());

    // Writes greater than max_state wrap to 0.
    std::fs::write("/sys/class/thermal/cooling_device2/cur_state", "9").unwrap();
    let new_cur_state = std::fs::read("/sys/class/thermal/cooling_device2/cur_state").unwrap();
    assert_eq!("0\n", str::from_utf8(&new_cur_state).unwrap());
}

#[derive(Clone)]
struct NetlinkAddMembership;

impl socket::SetSockOpt for NetlinkAddMembership {
    type Val = u32;

    fn set<F: AsFd>(&self, fd: &F, val: &Self::Val) -> nix::Result<()> {
        unsafe {
            let res = libc::setsockopt(
                fd.as_fd().as_raw_fd(),
                libc::SOL_NETLINK,
                libc::NETLINK_ADD_MEMBERSHIP,
                <*const _>::cast(val),
                std::mem::size_of_val(val) as libc::socklen_t,
            );
            nix::Error::result(res).map(drop)
        }
    }
}

fn check_thermal_is_available() -> HashMap<String, u32> {
    let nl_socket = socket::socket(
        socket::AddressFamily::Netlink,
        socket::SockType::Datagram,
        socket::SockFlag::SOCK_CLOEXEC,
        socket::SockProtocol::NetlinkGeneric,
    )
    .unwrap();
    socket::bind(nl_socket.as_raw_fd(), &socket::NetlinkAddr::new(0, 0)).unwrap();
    socket::connect(nl_socket.as_raw_fd(), &socket::NetlinkAddr::new(0, 0)).unwrap();

    let mut genlmsg = GenlMessage::from_payload(GenlCtrl {
        cmd: GenlCtrlCmd::GetFamily,
        nlas: vec![GenlCtrlAttrs::FamilyName("thermal".to_owned())],
    });
    genlmsg.finalize();
    let mut nlmsg = NetlinkMessage::from(genlmsg);
    nlmsg.header.flags = NLM_F_REQUEST;
    nlmsg.finalize();

    let mut txbuf = vec![0u8; nlmsg.buffer_len()];
    nlmsg.serialize(&mut txbuf);

    socket::send(nl_socket.as_raw_fd(), &txbuf, socket::MsgFlags::empty()).unwrap();

    let mut rxbuf = vec![0u8; 1024];
    socket::recvfrom::<socket::NetlinkAddr>(nl_socket.as_raw_fd(), &mut rxbuf).unwrap();
    let rx_packet =
        <NetlinkMessage<GenlMessage<GenlCtrl>>>::deserialize(&rxbuf, EmptyDeserializeOptions)
            .unwrap();

    let genlmsg = assert_matches!(rx_packet.payload, NetlinkPayload::InnerMessage(g) => g);
    assert_eq!(genlmsg.payload.cmd, GenlCtrlCmd::NewFamily);

    let family_id = genlmsg
        .payload
        .nlas
        .iter()
        .find_map(|nla| if let GenlCtrlAttrs::FamilyId(id) = nla { Some(*id) } else { None })
        .expect("Cannot find FamilyId attribute");
    assert!(family_id > 16);

    let groups = genlmsg
        .payload
        .nlas
        .iter()
        .find_map(|nla| {
            if let GenlCtrlAttrs::McastGroups(groups) = nla {
                let mut group_map: HashMap<String, u32> = HashMap::new();
                for group in groups {
                    let name = assert_matches!(&group[0], McastGrpAttrs::Name(name) => name);
                    let id = assert_matches!(&group[1], McastGrpAttrs::Id(id) => id);
                    group_map.insert(name.clone(), *id);
                }
                Some(group_map)
            } else {
                None
            }
        })
        .expect("Cannot find FamilyId attribute");

    let mut expected_groups = HashSet::new();
    expected_groups.insert(THERMAL_GENL_SAMPLING_GROUP_NAME.to_str().unwrap().to_string());
    expected_groups.insert(THERMAL_GENL_EVENT_GROUP_NAME.to_str().unwrap().to_string());

    assert_eq!(expected_groups.len(), groups.len());
    assert_eq!(expected_groups, groups.keys().map(|s| s.to_string()).collect::<HashSet<String>>());
    return groups;
}

fn check_thermal_sampling_returns_samples(sampling_group_id: u32) {
    let nl_socket = socket::socket(
        socket::AddressFamily::Netlink,
        socket::SockType::Datagram,
        socket::SockFlag::SOCK_CLOEXEC,
        socket::SockProtocol::NetlinkGeneric,
    )
    .unwrap();
    socket::bind(nl_socket.as_raw_fd(), &socket::NetlinkAddr::new(0, 0)).unwrap();
    socket::connect(nl_socket.as_raw_fd(), &socket::NetlinkAddr::new(0, 0)).unwrap();
    socket::setsockopt(&nl_socket, NetlinkAddMembership, &sampling_group_id).unwrap();

    // Receive one normal sample first to synchronize with the start of run_samplers's 2-second
    // SAMPLING_DELAY timer before setting emul_temp to -274000.
    let mut rxbuf = vec![0u8; 256];
    let (recv_size, _addr) =
        socket::recvfrom::<socket::NetlinkAddr>(nl_socket.as_raw_fd(), &mut rxbuf).unwrap();
    assert!(recv_size > 0);
    let rx_packet = <NetlinkMessage<GenlMessage<GenlThermalPayload>>>::deserialize(
        &rxbuf[..recv_size],
        EmptyDeserializeOptions,
    )
    .unwrap();
    let genlmsg = assert_matches!(rx_packet.payload, NetlinkPayload::InnerMessage(m) => m);
    assert_eq!(GenlThermalCmd::ThermalGenlSamplingTemp, genlmsg.payload.cmd);
    let temp = assert_matches!(genlmsg.payload.nlas[1], ThermalAttr::ThermalZoneTemp(temp) => temp);
    assert_eq!(celsius_to_millicelsius(EXPECTED_TEMP_C) as u32, temp);

    // Set emul_temp to -274000 (-274.0°C, THERMAL_TEMP_INVALID) and verify that power-gated
    // sentinel readings are suppressed from netlink sampling notifications.
    let invalid_temp = "-274000\n";
    std::fs::write("/sys/class/thermal/thermal_zone0/emul_temp", invalid_temp).unwrap();
    let fake_temp_str = std::fs::read("/sys/class/thermal/thermal_zone0/temp").unwrap();
    assert_eq!(invalid_temp, str::from_utf8(&fake_temp_str).unwrap());

    // Drain any sample packet that was already queued before emul_temp was set.
    while let Ok(recv_size) =
        socket::recv(nl_socket.as_raw_fd(), &mut rxbuf, socket::MsgFlags::MSG_DONTWAIT)
    {
        let rx_packet = <NetlinkMessage<GenlMessage<GenlThermalPayload>>>::deserialize(
            &rxbuf[..recv_size],
            EmptyDeserializeOptions,
        )
        .unwrap();
        let genlmsg = assert_matches!(rx_packet.payload, NetlinkPayload::InnerMessage(m) => m);
        let temp =
            assert_matches!(genlmsg.payload.nlas[1], ThermalAttr::ThermalZoneTemp(temp) => temp);
        assert_eq!(celsius_to_millicelsius(EXPECTED_TEMP_C) as u32, temp);
    }

    // Wait longer than SAMPLING_DELAY (2s) while emul_temp is -274000 and verify no sample is sent.
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(
        Err(nix::errno::Errno::EAGAIN),
        socket::recv(nl_socket.as_raw_fd(), &mut rxbuf, socket::MsgFlags::MSG_DONTWAIT)
    );

    // Reset emul_temp and verify normal sampling resumes.
    std::fs::write("/sys/class/thermal/thermal_zone0/emul_temp", "0").unwrap();
    let real_temp_str = std::fs::read("/sys/class/thermal/thermal_zone0/temp").unwrap();
    let expected_real_temp = celsius_to_millicelsius(EXPECTED_TEMP_C) as u32;
    assert_eq!(&format!("{}\n", expected_real_temp), str::from_utf8(&real_temp_str).unwrap());

    let now = std::time::Instant::now();
    for _ in 0..3 {
        let mut rxbuf = vec![0u8; 256];
        let (recv_size, _addr) =
            socket::recvfrom::<socket::NetlinkAddr>(nl_socket.as_raw_fd(), &mut rxbuf).unwrap();
        assert!(recv_size > 0);

        println!(
            "Received {} bytes after {} seconds: {:?}",
            recv_size,
            now.elapsed().as_secs(),
            &rxbuf[..recv_size]
        );

        let rx_packet = <NetlinkMessage<GenlMessage<GenlThermalPayload>>>::deserialize(
            &rxbuf,
            EmptyDeserializeOptions,
        )
        .unwrap();
        let genlmsg = assert_matches!(rx_packet.payload, NetlinkPayload::InnerMessage(m) => m);
        assert_eq!(GenlThermalCmd::ThermalGenlSamplingTemp, genlmsg.payload.cmd);

        assert_eq!(2, genlmsg.payload.nlas.len());
        let id = assert_matches!(genlmsg.payload.nlas[0], ThermalAttr::ThermalZoneId(id) => id);
        let temp =
            assert_matches!(genlmsg.payload.nlas[1], ThermalAttr::ThermalZoneTemp(temp) => temp);

        // ID should match the thermal zone number.
        assert_eq!(0u32, id);
        assert_eq!(celsius_to_millicelsius(EXPECTED_TEMP_C) as u32, temp);
    }

    // Should take less than 10 seconds to get 3 samples.
    // This assumes the thermal netlink server serves samples every 2 seconds
    // plus some buffer time for test variance.
    assert!(now.elapsed().as_secs() < 10);
}

fn check_powercap() {
    let expected_zones = [
        ("gpc:0", "/sys/devices/virtual/powercap/gpc:0", "gpc"),
        ("gpc:0:0", "/sys/devices/virtual/powercap/gpc:0/gpc:0:0", "TestZone"),
        ("gpc:0:0:0", "/sys/devices/virtual/powercap/gpc:0/gpc:0:0/gpc:0:0:0", "LITTLE"),
        ("gpc:0:0:1", "/sys/devices/virtual/powercap/gpc:0/gpc:0:0/gpc:0:0:1", "MID"),
        ("gpc:0:0:2", "/sys/devices/virtual/powercap/gpc:0/gpc:0:0/gpc:0:0:2", "BIG"),
        ("gpc:0:0:3", "/sys/devices/virtual/powercap/gpc:0/gpc:0:0/gpc:0:0:3", "GPU"),
    ];

    for (zone_path, canonical_dir, expected_name) in expected_zones {
        let class_link = format!("/sys/class/powercap/{zone_path}");
        let meta = std::fs::symlink_metadata(&class_link).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "Expected {class_link} to be a DT_LNK symlink for ThermalHelperImpl"
        );
        assert_eq!(
            std::path::PathBuf::from(canonical_dir),
            std::fs::canonicalize(&class_link).unwrap()
        );
        assert_eq!(format!("{expected_name}\n"), read_sysfs(&format!("{class_link}/name")));
        assert_eq!(format!("{expected_name}\n"), read_sysfs(&format!("{canonical_dir}/name")));
    }

    // Invalid, duplicate, orphaned, and cyclic nodes returned by `GetPowercapTree` must be skipped.
    for rejected in [
        "gpc:missing-id",
        "gpc:missing-name",
        "gpc:dup-id",
        "gpc:orphan",
        "gpc:cycle-a",
        "gpc:cycle-b",
        "name",
        "uevent",
    ] {
        assert!(
            !std::fs::exists(format!("/sys/class/powercap/{rejected}")).unwrap(),
            "Expected rejected zone {rejected} to be absent from /sys/class/powercap"
        );
    }

    // `PowercapError::Internal` from `GetPowerLimit` on `gpc:0` maps to `EIO`.
    let read_err = std::fs::read("/sys/class/powercap/gpc:0/constraint_0_power_limit_uw")
        .expect_err("Expected EIO when reading gpc:0 constraint_0_power_limit_uw");
    assert_eq!(read_err.raw_os_error(), Some(libc::EIO));

    // Intermediate aggregator zone (`gpc:0:0`) exposes constraint limits but no discrete power
    // levels or bypass timer.
    assert_eq!("TestConstraint\n", read_sysfs("/sys/class/powercap/gpc:0:0/constraint_0_name"));
    assert_eq!("15000000\n", read_sysfs("/sys/class/powercap/gpc:0:0/constraint_0_max_power_uw"));
    assert_eq!("1150000\n", read_sysfs("/sys/class/powercap/gpc:0:0/constraint_0_min_power_uw"));
    assert_eq!("15000000\n", read_sysfs("/sys/class/powercap/gpc:0:0/constraint_0_power_limit_uw"));
    assert!(!std::fs::exists("/sys/class/powercap/gpc:0:0/power_levels_uw").unwrap());
    assert!(!std::fs::exists("/sys/class/powercap/gpc:0:0/power_limit_bypass_msec").unwrap());

    // Leaf zone (`gpc:0:0:2` / `BIG`) exposes discrete power levels, writable power limit, and
    // writable bypass timer.
    const BIG_CLASS: &str = "/sys/class/powercap/gpc:0:0:2";
    const BIG_DEV: &str = "/sys/devices/virtual/powercap/gpc:0/gpc:0:0/gpc:0:0:2";
    assert_eq!("TestConstraint\n", read_sysfs(&format!("{BIG_CLASS}/constraint_0_name")));
    assert_eq!("4565000\n", read_sysfs(&format!("{BIG_CLASS}/constraint_0_max_power_uw")));
    assert_eq!("400000\n", read_sysfs(&format!("{BIG_CLASS}/constraint_0_min_power_uw")));
    assert_eq!(
        "4565000 3500000 2200000 1100000 400000\n",
        read_sysfs(&format!("{BIG_CLASS}/power_levels_uw"))
    );
    assert_eq!("4565000\n", read_sysfs(&format!("{BIG_CLASS}/constraint_0_power_limit_uw")));

    // Write a new power limit via `/sys/class/powercap` and read back via both paths.
    std::fs::write(format!("{BIG_CLASS}/constraint_0_power_limit_uw"), "2200000").unwrap();
    assert_eq!("2200000\n", read_sysfs(&format!("{BIG_CLASS}/constraint_0_power_limit_uw")));
    assert_eq!("2200000\n", read_sysfs(&format!("{BIG_DEV}/constraint_0_power_limit_uw")));

    // Invalid writes to `constraint_0_power_limit_uw` must fail with EINVAL and preserve the limit,
    // and `PowercapError::Internal` must map to `EIO`.
    let mut limit_file = std::fs::OpenOptions::new()
        .write(true)
        .open(format!("{BIG_CLASS}/constraint_0_power_limit_uw"))
        .unwrap();
    let err = limit_file.write_all(b"invalid").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    let internal_err = limit_file.write_all(u64::MAX.to_string().as_bytes()).unwrap_err();
    assert_eq!(internal_err.raw_os_error(), Some(libc::EIO));
    assert_eq!("2200000\n", read_sysfs(&format!("{BIG_CLASS}/constraint_0_power_limit_uw")));

    // Exercise `power_limit_bypass_msec` (used by PowerHAL `CPU_LOAD_SPIKE` / `LAUNCH` hints).
    assert_eq!("0\n", read_sysfs(&format!("{BIG_CLASS}/power_limit_bypass_msec")));
    std::fs::write(format!("{BIG_CLASS}/power_limit_bypass_msec"), "100\n").unwrap();
    assert_eq!("100\n", read_sysfs(&format!("{BIG_CLASS}/power_limit_bypass_msec")));
    assert_eq!("100\n", read_sysfs(&format!("{BIG_DEV}/power_limit_bypass_msec")));

    let mut bypass_file = std::fs::OpenOptions::new()
        .write(true)
        .open(format!("{BIG_CLASS}/power_limit_bypass_msec"))
        .unwrap();
    let err = bypass_file.write_all(b"not_a_number").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    let internal_err = bypass_file.write_all(u64::MAX.to_string().as_bytes()).unwrap_err();
    assert_eq!(internal_err.raw_os_error(), Some(libc::EIO));
    assert_eq!("100\n", read_sysfs(&format!("{BIG_CLASS}/power_limit_bypass_msec")));

    std::fs::write(format!("{BIG_CLASS}/power_limit_bypass_msec"), "0").unwrap();
    assert_eq!("0\n", read_sysfs(&format!("{BIG_CLASS}/power_limit_bypass_msec")));
}
