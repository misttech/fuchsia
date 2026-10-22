// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Exposes power residency entities from [`PowerStatsManager`] under platform devices in
//! `/sys/devices/platform`, such as `powerdashboard` and `cap_sysfs`.
//!
//! These devices are not generic Linux sysfs interfaces. They emulate the SoC-specific vendor
//! devices that the Android PowerStats HAL reads residency from: the CPM power dashboard and the
//! CAP (CPU Auxiliary Processor) stats device.

use fidl_fuchsia_hardware_power_stats as fpowerstats;
use starnix_core::device::power_stats::PowerStatsManager;
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use starnix_logging::log_warn;
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

/// Power entities whose names start with this prefix are served as performance state residency
/// files of the same name.
const PERF_STATE_FILE_PREFIX: &str = "lpcm_";

/// Power entities whose names start with this prefix are served as power domain residency files
/// of the same name.
const POWER_DOMAIN_FILE_PREFIX: &str = "pwrblk_";

/// Power entity served as the platform power state residency file.
const PLATFORM_POWER_ENTITY: &str = "soc_power_state";

/// Name of the file [`PLATFORM_POWER_ENTITY`] is served as.
const PLATFORM_POWER_FILE: &str = "platform_power_residency";

/// Name of the platform device under which the residency files are served, emulating the
/// SoC-specific CPM power dashboard device read by the PowerStats HAL.
const RESIDENCY_DEVICE_NAME: &str = "powerdashboard";

/// Power entity served as the power state stats file.
const POWER_STATE_STATS_ENTITY: &str = "cap_power_state";

/// Name of the file [`POWER_STATE_STATS_ENTITY`] is served as.
const POWER_STATE_STATS_FILE: &str = "power_state_stats";

/// Name of the platform device under which [`POWER_STATE_STATS_FILE`] and the DVFS stats files are
/// served, emulating the SoC-specific CAP stats device read by the PowerStats HAL.
const CAP_SYSFS_DEVICE_NAME: &str = "cap_sysfs";

/// Power entities whose names start with this prefix, followed by the name of a clock domain, are
/// served as DVFS stats files.
const DVFS_STATS_ENTITY_PREFIX: &str = "dvfs_";

/// Prefix of the name of the DVFS stats file of each clock domain, which is followed by the name of
/// the clock domain.
const DVFS_STATS_FILE_PREFIX: &str = "dvfs_stats_";

/// Ticks per millisecond of the counter that some residency files report timestamps in.
///
/// Those files report raw values of the platform timer counter, which is expected to run at
/// 38.4 MHz, so the provider's millisecond timestamps are converted at 38,400 ticks per
/// millisecond.
const TIMESTAMP_TICKS_PER_MS: u64 = 38_400;

/// Formats the residency of a power entity as the contents of its residency file.
type FormatResidency = fn(&fpowerstats::Stats) -> String;

/// Where and how a power entity is served.
struct ResidencyFileSpec {
    /// Name of the platform device under which the file is served.
    device: &'static str,
    /// Name of the file within the device directory.
    file: String,
    /// Name of the power entity whose residency the file serves.
    entity: String,
    format: FormatResidency,
}

/// Serves a file for each power entity reported over `fuchsia.hardware.power.stats.Service` that
/// [`residency_file_spec`] maps to one, under its platform device.
///
/// The entities are only known once a provider has started, so each device is added asynchronously
/// via [`PowerStatsManager`], once a provider reports at least one entity served under it. Served
/// entities for a device from other providers are ignored.
pub fn init_power_residency_device(kernel: &Kernel) {
    let mut added_devices: HashSet<&'static str> = HashSet::new();
    let manager = PowerStatsManager::get_or_init(kernel);
    let manager_for_files = manager.clone();
    manager.watch_providers(kernel, move |kernel, provider| {
        let mut files_by_device: BTreeMap<&'static str, Vec<ResidencyFileSpec>> = BTreeMap::new();
        for spec in provider.entity_names().iter().filter_map(|name| residency_file_spec(name)) {
            files_by_device.entry(spec.device).or_default().push(spec);
        }
        let registry = &kernel.device_registry;
        for (device_name, files) in files_by_device {
            if !added_devices.insert(device_name) {
                log_warn!(
                    "Ignoring {device_name} power residency entities from additional provider {}",
                    provider.instance_name()
                );
                continue;
            }
            registry.add_bus_device(
                device_name.into(),
                Some(registry.objects.platform_device()),
                registry.objects.platform_bus(),
                |device, dir| {
                    build_device_directory(device, dir);
                    for spec in files {
                        let file = ResidencyFile {
                            entity: spec.entity,
                            format: spec.format,
                            manager: manager_for_files.clone(),
                        };
                        dir.entry(&spec.file, BytesFile::new_node(file), mode!(IFREG, 0o444));
                    }
                },
            );
        }
    });
}

/// Returns where and how to serve the power entity named `entity`, or [`None`] if it is not
/// served. Prefixed entities are only served if their names are safe to use as file names.
fn residency_file_spec(entity: &str) -> Option<ResidencyFileSpec> {
    let prefixed = |prefix: &str| {
        entity.len() > prefix.len()
            && entity.starts_with(prefix)
            && entity.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    };
    let (device, file, format): (_, _, FormatResidency) = if prefixed(PERF_STATE_FILE_PREFIX) {
        (RESIDENCY_DEVICE_NAME, entity.to_string(), format_performance_states)
    } else if prefixed(POWER_DOMAIN_FILE_PREFIX) {
        (RESIDENCY_DEVICE_NAME, entity.to_string(), format_power_domain_states)
    } else if entity == PLATFORM_POWER_ENTITY {
        (RESIDENCY_DEVICE_NAME, PLATFORM_POWER_FILE.to_string(), format_platform_power_states)
    } else if prefixed(DVFS_STATS_ENTITY_PREFIX) {
        let file = format!("{DVFS_STATS_FILE_PREFIX}{}", &entity[DVFS_STATS_ENTITY_PREFIX.len()..]);
        (CAP_SYSFS_DEVICE_NAME, file, format_dvfs_stats)
    } else if entity == POWER_STATE_STATS_ENTITY {
        (CAP_SYSFS_DEVICE_NAME, POWER_STATE_STATS_FILE.to_string(), format_power_state_stats)
    } else {
        return None;
    };
    Some(ResidencyFileSpec { device, file, entity: entity.to_string(), format })
}

/// Serves the residency of a single power entity, as formatted by `format`.
struct ResidencyFile {
    entity: String,
    format: FormatResidency,
    manager: PowerStatsManager,
}

impl BytesFileOps for ResidencyFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let stats = self.manager.entity_stats(&self.entity)?.ok_or_else(|| {
            log_warn!("No power stats provider registered for {}", self.entity);
            errno!(EIO)
        })?;
        Ok((self.format)(&stats).into_bytes().into())
    }
}

/// Returns the states in `stats` whose names are safe to write to a line-based residency file,
/// logging a warning for any state left out.
///
/// States whose names contain control characters, such as newlines, are left out, as their names
/// would break the line-based layout.
fn safe_states(stats: &fpowerstats::Stats) -> impl Iterator<Item = &fpowerstats::StateResidency> {
    stats.state_residency_data.iter().filter(|state| {
        if state.name.chars().any(char::is_control) {
            log_warn!("Leaving out {} state {} with an unsafe name", stats.entity, state.id);
            return false;
        }
        true
    })
}

/// Formats `stats` as a `num_pf_states` line followed by `pf_*` lines for each performance level.
///
/// The power stats API does not report times with sub-millisecond precision, so times are whole
/// milliseconds converted to microseconds.
fn format_performance_states(stats: &fpowerstats::Stats) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "num_pf_states: {}", stats.state_residency_data.len());
    for state in &stats.state_residency_data {
        let time_in_state_us = state.total_time_ms.saturating_mul(1000);
        let _ = writeln!(out, "pf_state: {}", state.id);
        let _ = writeln!(out, "pf_entry_count: {}", state.entry_count);
        let _ = writeln!(out, "pf_time_in_state: {time_in_state_us} (us)\n");
    }
    out
}

/// Formats `stats` as an `SSWRP lpb` header followed by `power_state` lines for each power
/// state.
///
/// States whose names contain control characters, such as newlines, are left out, as their names
/// would break the line-based layout.
///
/// The power stats API does not report times with sub-millisecond precision or when a state was
/// last exited, so times are whole milliseconds converted to microseconds and last exit
/// timestamps are 0.
fn format_power_domain_states(stats: &fpowerstats::Stats) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\nSSWRP lpb\n");
    for state in safe_states(stats) {
        let time_in_state_us = state.total_time_ms.saturating_mul(1000);
        let last_entry_us = state.last_entry_timestamp_ms.saturating_mul(1000);
        let _ = writeln!(out, "power_state: {}", state.name);
        let _ = writeln!(out, "entry_count: {}", state.entry_count);
        let _ = writeln!(out, "time_in_state: {time_in_state_us} (us)");
        let _ = writeln!(out, "last_entry_timestamp: {last_entry_us}");
        let _ = writeln!(out, "last_exit_timestamp: 0");
    }
    out.push('\n');
    out
}

/// Formats `stats` as a `power_state` line followed by counts and timestamps for each power
/// state.
///
/// States whose names contain control characters, such as newlines, are left out, as their names
/// would break the line-based layout.
///
/// The power stats API does not report which state is current or when a state was last exited,
/// so there are no current state lines and last exit timestamps are 0.
fn format_platform_power_states(stats: &fpowerstats::Stats) -> String {
    let mut out = String::new();
    for state in safe_states(stats) {
        let last_entry_ticks = state.last_entry_timestamp_ms.saturating_mul(TIMESTAMP_TICKS_PER_MS);
        let _ = writeln!(out, "power_state: {}", state.name);
        let _ = writeln!(out, "entry_count: {}", state.entry_count);
        let _ = writeln!(out, "time_in_state_ms: {}", state.total_time_ms);
        let _ = writeln!(out, "last_entry_timestamp_ticks: {last_entry_ticks}");
        let _ = writeln!(out, "last_exit_timestamp_ticks: 0\n");
    }
    out
}

/// Formats `stats` as `---`-delimited records of `state`, timestamps, residency, and entry count
/// for each power state.
///
/// States whose names contain control characters, such as newlines, are left out, as their names
/// would break the line-based layout.
///
/// The power stats API does not report when a state was last exited, so last exit timestamps are
/// 0.
fn format_power_state_stats(stats: &fpowerstats::Stats) -> String {
    let mut out = String::new();
    for state in safe_states(stats) {
        let last_entry_ticks = state.last_entry_timestamp_ms.saturating_mul(TIMESTAMP_TICKS_PER_MS);
        let _ = writeln!(out, "---");
        let _ = writeln!(out, "state: {}", state.name);
        let _ = writeln!(out, "last_entry_ts_ticks: {last_entry_ticks}");
        let _ = writeln!(out, "last_exit_ts_ticks: 0");
        let _ = writeln!(out, "total_state_residency_ms: {}", state.total_time_ms);
        let _ = writeln!(out, "last_state_entry_counts: {}", state.entry_count);
    }
    out
}

/// Formats `stats` as a record that starts with `---` and names the clock domain, followed by the
/// entry count, residency, and last entry timestamp of each operating level.
///
/// Operating levels are identified by their state IDs. The power stats API does not report the
/// level a clock domain is at or is moving to, so there are no current or target level lines.
fn format_dvfs_stats(stats: &fpowerstats::Stats) -> String {
    let clock_domain = stats.entity.strip_prefix(DVFS_STATS_ENTITY_PREFIX).unwrap_or(&stats.entity);
    let mut out = String::new();
    let _ = writeln!(out, "---");
    let _ = writeln!(out, "clock_domain: {clock_domain}");
    let _ = writeln!(out, "per_op_level_stats: ");
    for state in &stats.state_residency_data {
        let last_entry_ticks = state.last_entry_timestamp_ms.saturating_mul(TIMESTAMP_TICKS_PER_MS);
        let _ = writeln!(out, "  - oplevel: {}", state.id);
        let _ = writeln!(out, "    entry_count: {}", state.entry_count);
        let _ = writeln!(out, "    total_residency_ms: {}", state.total_time_ms);
        let _ = writeln!(out, "    last_entry_timestamp_ticks: {last_entry_ticks}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::{ServerEnd, create_sync_proxy};
    use fuchsia_async as fasync;
    use futures::StreamExt;
    use starnix_core::device::power_stats::PowerStatsProvider;
    use starnix_core::testing::spawn_kernel_and_run_sync;
    use starnix_uapi::error;

    fn state(id: i32, entry_count: u32, total_time_ms: u64) -> fpowerstats::StateResidency {
        fpowerstats::StateResidency {
            id,
            name: id.to_string(),
            total_time_ms,
            entry_count,
            last_entry_timestamp_ms: 0,
        }
    }

    fn stats(
        entity: &str,
        state_residency_data: Vec<fpowerstats::StateResidency>,
    ) -> fpowerstats::Stats {
        fpowerstats::Stats { entity: entity.to_string(), state_residency_data }
    }

    /// Serves a fake provider that reports `reports` on `stream`.
    async fn serve_provider(
        stream: fpowerstats::DeviceRequestStream,
        reports: Vec<fpowerstats::Stats>,
    ) {
        let _ = stream
            .for_each(|request| {
                let reports = &reports;
                async move {
                    if let Ok(fpowerstats::DeviceRequest::GetStats { responder }) = request {
                        let _ = responder.send(reports);
                    }
                }
            })
            .await;
    }

    /// Runs `serve` on its own thread, as residency files call providers synchronously.
    fn spawn_server<F: Future<Output = ()> + 'static>(serve: impl FnOnce() -> F + Send + 'static) {
        std::thread::spawn(move || fasync::LocalExecutor::default().run_singlethreaded(serve()));
    }

    /// Returns a [`PowerStatsProvider`] reporting `entity_names` for tests that only exercise
    /// device registration.
    fn provider_with_entities(
        instance_name: &str,
        entity_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> PowerStatsProvider {
        fake_provider(
            instance_name,
            entity_names.into_iter().map(|name| stats(&name.into(), vec![])).collect(),
        )
    }

    /// Returns a fake [`PowerStatsProvider`] named `instance_name` that serves `reports`.
    fn fake_provider(instance_name: &str, reports: Vec<fpowerstats::Stats>) -> PowerStatsProvider {
        let entity_names: Vec<_> = reports.iter().map(|stats| stats.entity.clone()).collect();
        let (proxy, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
        spawn_server(move || serve_provider(server.into_stream(), reports));
        PowerStatsProvider::new(instance_name, entity_names, proxy)
    }

    /// Returns a performance state residency file serving `entity` from `manager`.
    fn perf_file(entity: &str, manager: PowerStatsManager) -> ResidencyFile {
        ResidencyFile { entity: entity.to_string(), format: format_performance_states, manager }
    }

    fn exists(kernel: &Kernel, path: &str) -> bool {
        kernel.device_registry.objects.root.lookup(path.into()).is_some()
    }

    #[::fuchsia::test]
    async fn device_is_added_with_served_entities() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let prefix = PERF_STATE_FILE_PREFIX;
            PowerStatsManager::add_provider_for_testing(
                kernel,
                provider_with_entities(
                    "default",
                    [
                        format!("{prefix}a"),
                        format!("{prefix}b"),
                        PLATFORM_POWER_ENTITY.to_string(),
                        "other".to_string(),
                    ],
                ),
            );
            init_power_residency_device(kernel);

            let device = format!("devices/platform/{RESIDENCY_DEVICE_NAME}");
            assert!(exists(kernel, &device));
            assert!(exists(kernel, &format!("bus/platform/devices/{RESIDENCY_DEVICE_NAME}")));
            assert!(exists(kernel, &format!("{device}/uevent")));
            assert!(exists(kernel, &format!("{device}/{prefix}a")));
            assert!(exists(kernel, &format!("{device}/{prefix}b")));
            assert!(exists(kernel, &format!("{device}/{PLATFORM_POWER_FILE}")));
            assert!(!exists(kernel, &format!("{device}/other")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn no_device_without_served_entities() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            PowerStatsManager::add_provider_for_testing(
                kernel,
                provider_with_entities("default", ["other"]),
            );
            init_power_residency_device(kernel);

            assert!(!exists(kernel, &format!("devices/platform/{RESIDENCY_DEVICE_NAME}")));
            assert!(!exists(kernel, &format!("devices/platform/{CAP_SYSFS_DEVICE_NAME}")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn device_is_added_when_later_provider_reports_served_entities() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let prefix = PERF_STATE_FILE_PREFIX;
            let device = format!("devices/platform/{RESIDENCY_DEVICE_NAME}");

            let _ = PowerStatsManager::get_or_init_for_testing(kernel);
            init_power_residency_device(kernel);
            assert!(!exists(kernel, &device));

            PowerStatsManager::add_provider_for_testing(
                kernel,
                provider_with_entities("unserved", ["other"]),
            );
            assert!(!exists(kernel, &device));

            PowerStatsManager::add_provider_for_testing(
                kernel,
                provider_with_entities(
                    "served",
                    [format!("{prefix}a"), PLATFORM_POWER_ENTITY.to_string()],
                ),
            );
            assert!(exists(kernel, &device));
            assert!(exists(kernel, &format!("{device}/{prefix}a")));
            assert!(exists(kernel, &format!("{device}/{PLATFORM_POWER_FILE}")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn only_first_provider_is_served() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let prefix = PERF_STATE_FILE_PREFIX;
            PowerStatsManager::add_provider_for_testing(
                kernel,
                provider_with_entities("first", [format!("{prefix}a")]),
            );
            init_power_residency_device(kernel);
            PowerStatsManager::add_provider_for_testing(
                kernel,
                provider_with_entities("second", [format!("{prefix}b")]),
            );

            let device = format!("devices/platform/{RESIDENCY_DEVICE_NAME}");
            assert!(exists(kernel, &format!("{device}/{prefix}a")));
            assert!(!exists(kernel, &format!("{device}/{prefix}b")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn each_device_is_added_for_first_provider_serving_it() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let prefix = PERF_STATE_FILE_PREFIX;
            PowerStatsManager::add_provider_for_testing(
                kernel,
                provider_with_entities("first", [format!("{prefix}a")]),
            );
            init_power_residency_device(kernel);
            PowerStatsManager::add_provider_for_testing(
                kernel,
                provider_with_entities(
                    "second",
                    [format!("{prefix}b"), POWER_STATE_STATS_ENTITY.to_string()],
                ),
            );

            let residency_device = format!("devices/platform/{RESIDENCY_DEVICE_NAME}");
            assert!(exists(kernel, &format!("{residency_device}/{prefix}a")));
            assert!(!exists(kernel, &format!("{residency_device}/{prefix}b")));
            let stats_device = format!("devices/platform/{CAP_SYSFS_DEVICE_NAME}");
            assert!(exists(kernel, &format!("bus/platform/devices/{CAP_SYSFS_DEVICE_NAME}")));
            assert!(exists(kernel, &format!("{stats_device}/{POWER_STATE_STATS_FILE}")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn read_returns_formatted_residency() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let entities = [
                stats(
                    "lpcm_a",
                    vec![state(
                        /* id = */ 0, /* entry_count = */ 11,
                        /* total_time_ms = */ 2200,
                    )],
                ),
                stats(
                    "pwrblk_a",
                    vec![state(
                        /* id = */ 0, /* entry_count = */ 5,
                        /* total_time_ms = */ 40,
                    )],
                ),
                stats(
                    PLATFORM_POWER_ENTITY,
                    vec![state(
                        /* id = */ 0, /* entry_count = */ 3,
                        /* total_time_ms = */ 100,
                    )],
                ),
                stats(
                    POWER_STATE_STATS_ENTITY,
                    vec![state(
                        /* id = */ 0, /* entry_count = */ 6,
                        /* total_time_ms = */ 250,
                    )],
                ),
            ];
            PowerStatsManager::add_provider_for_testing(
                kernel,
                fake_provider("test", entities.to_vec()),
            );
            let manager = PowerStatsManager::get_or_init(kernel);

            for entity in &entities {
                let spec = residency_file_spec(&entity.entity).expect("entity is served");
                let file = ResidencyFile {
                    entity: spec.entity,
                    format: spec.format,
                    manager: manager.clone(),
                };
                assert_eq!(
                    &*file.read(current_task).expect("read residency file"),
                    (spec.format)(entity).as_bytes(),
                    "{}",
                    entity.entity
                );
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn read_fails_when_entity_is_no_longer_reported() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let (proxy, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            let reports = vec![stats("b", vec![])];
            spawn_server(move || serve_provider(server.into_stream(), reports));
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new("test", ["a", "b"], proxy),
            );
            let file = perf_file("a", PowerStatsManager::get_or_init(kernel));

            assert_eq!(file.read(current_task).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn read_fails_when_provider_is_gone_and_recovers_on_restart() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let (proxy, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            drop::<ServerEnd<_>>(server);
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new("test", ["a", "b"], proxy),
            );
            let manager = PowerStatsManager::get_or_init(kernel);
            let file_a = perf_file("a", manager.clone());
            let file_b = perf_file("b", manager);

            assert_eq!(file_a.read(current_task).map(|_| ()), error!(EIO));
            assert_eq!(file_b.read(current_task).map(|_| ()), error!(EIO));

            let entity = stats(
                "a",
                vec![state(
                    /* id = */ 0, /* entry_count = */ 5, /* total_time_ms = */ 100,
                )],
            );
            PowerStatsManager::add_provider_for_testing(
                kernel,
                fake_provider("test_restarted", vec![entity.clone()]),
            );
            let contents = file_a.read(current_task).expect("read after restart");
            assert_eq!(&*contents, format_performance_states(&entity).as_bytes());
            assert_eq!(file_b.read(current_task).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn read_fails_when_provider_does_not_respond() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            // Keep `_server` open without servicing requests so `GetStats` times out.
            let (proxy, _server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new("test", ["a"], proxy),
            );
            let file = perf_file("a", PowerStatsManager::get_or_init(kernel));

            assert_eq!(file.read(current_task).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    fn performance_states_are_formatted_per_state() {
        let stats = stats(
            "a",
            vec![
                state(
                    /* id = */ 0, /* entry_count = */ 11, /* total_time_ms = */ 2200,
                ),
                state(/* id = */ 1, /* entry_count = */ 4, /* total_time_ms = */ 7),
            ],
        );

        assert_eq!(
            format_performance_states(&stats),
            "num_pf_states: 2\n\
             pf_state: 0\npf_entry_count: 11\npf_time_in_state: 2200000 (us)\n\n\
             pf_state: 1\npf_entry_count: 4\npf_time_in_state: 7000 (us)\n\n"
        );
    }

    #[::fuchsia::test]
    fn power_domain_states_are_formatted_per_state() {
        let off = fpowerstats::StateResidency {
            name: "STATE_OFF".to_string(),
            last_entry_timestamp_ms: 5,
            ..state(/* id = */ 0, /* entry_count = */ 3, /* total_time_ms = */ 12)
        };
        let on = fpowerstats::StateResidency {
            name: "STATE_ON".to_string(),
            last_entry_timestamp_ms: 9,
            ..state(/* id = */ 1, /* entry_count = */ 4, /* total_time_ms = */ 7)
        };
        let stats = stats("a", vec![off, on]);

        assert_eq!(
            format_power_domain_states(&stats),
            "\nSSWRP lpb\n\n\
             power_state: STATE_OFF\nentry_count: 3\ntime_in_state: 12000 (us)\n\
             last_entry_timestamp: 5000\nlast_exit_timestamp: 0\n\
             power_state: STATE_ON\nentry_count: 4\ntime_in_state: 7000 (us)\n\
             last_entry_timestamp: 9000\nlast_exit_timestamp: 0\n\n"
        );
    }

    #[::fuchsia::test]
    fn platform_power_states_are_formatted_per_state() {
        let active = fpowerstats::StateResidency {
            name: "ACTIVE".to_string(),
            last_entry_timestamp_ms: 2,
            ..state(/* id = */ 0, /* entry_count = */ 3, /* total_time_ms = */ 100)
        };
        let stats = stats(
            "a",
            vec![active, state(/* id = */ 1, /* entry_count = */ 0, /* total_time_ms = */ 0)],
        );

        assert_eq!(
            format_platform_power_states(&stats),
            "power_state: ACTIVE\nentry_count: 3\ntime_in_state_ms: 100\n\
             last_entry_timestamp_ticks: 76800\nlast_exit_timestamp_ticks: 0\n\n\
             power_state: 1\nentry_count: 0\ntime_in_state_ms: 0\n\
             last_entry_timestamp_ticks: 0\nlast_exit_timestamp_ticks: 0\n\n"
        );
    }

    #[::fuchsia::test]
    fn power_state_stats_are_formatted_per_state() {
        let cluster_off = fpowerstats::StateResidency {
            name: "Cluster 0 off".to_string(),
            last_entry_timestamp_ms: 2,
            ..state(/* id = */ 0, /* entry_count = */ 6, /* total_time_ms = */ 250)
        };
        let suspend = fpowerstats::StateResidency {
            name: "Suspend".to_string(),
            ..state(/* id = */ 1, /* entry_count = */ 1, /* total_time_ms = */ 10)
        };
        let stats = stats(POWER_STATE_STATS_ENTITY, vec![cluster_off, suspend]);

        assert_eq!(
            format_power_state_stats(&stats),
            "---\nstate: Cluster 0 off\nlast_entry_ts_ticks: 76800\nlast_exit_ts_ticks: 0\n\
             total_state_residency_ms: 250\nlast_state_entry_counts: 6\n\
             ---\nstate: Suspend\nlast_entry_ts_ticks: 0\nlast_exit_ts_ticks: 0\n\
             total_state_residency_ms: 10\nlast_state_entry_counts: 1\n"
        );
    }

    #[::fuchsia::test]
    fn dvfs_stats_are_formatted_per_level() {
        let fast = fpowerstats::StateResidency {
            last_entry_timestamp_ms: 3,
            ..state(/* id = */ 0, /* entry_count = */ 7, /* total_time_ms = */ 120)
        };
        let stats = stats(
            "dvfs_clk_lit",
            vec![fast, state(/* id = */ 1, /* entry_count = */ 0, /* total_time_ms = */ 0)],
        );

        assert_eq!(
            format_dvfs_stats(&stats),
            "---\nclock_domain: clk_lit\nper_op_level_stats: \n\
             \x20 - oplevel: 0\n    entry_count: 7\n    total_residency_ms: 120\n\
             \x20   last_entry_timestamp_ticks: 115200\n\
             \x20 - oplevel: 1\n    entry_count: 0\n    total_residency_ms: 0\n\
             \x20   last_entry_timestamp_ticks: 0\n"
        );
    }

    #[::fuchsia::test]
    fn residency_with_no_states() {
        let stats = stats("a", vec![]);

        assert_eq!(format_performance_states(&stats), "num_pf_states: 0\n");
        assert_eq!(format_power_domain_states(&stats), "\nSSWRP lpb\n\n\n");
        assert_eq!(format_platform_power_states(&stats), "");
        assert_eq!(format_power_state_stats(&stats), "");
        assert_eq!(format_dvfs_stats(&stats), "---\nclock_domain: a\nper_op_level_stats: \n");
    }

    #[::fuchsia::test]
    fn states_with_unsafe_names_are_left_out() {
        let unsafe_state = fpowerstats::StateResidency {
            name: "A\nentry_count: 9".to_string(),
            ..state(/* id = */ 0, /* entry_count = */ 1, /* total_time_ms = */ 1)
        };
        let stats = stats(
            "a",
            vec![unsafe_state, state(/* id = */ 1, /* entry_count = */ 2, /* total_time_ms = */ 2)],
        );

        assert_eq!(
            format_power_domain_states(&stats),
            "\nSSWRP lpb\n\n\
             power_state: 1\nentry_count: 2\ntime_in_state: 2000 (us)\n\
             last_entry_timestamp: 0\nlast_exit_timestamp: 0\n\n"
        );
        assert_eq!(
            format_platform_power_states(&stats),
            "power_state: 1\nentry_count: 2\ntime_in_state_ms: 2\n\
             last_entry_timestamp_ticks: 0\nlast_exit_timestamp_ticks: 0\n\n"
        );
        assert_eq!(
            format_power_state_stats(&stats),
            "---\nstate: 1\nlast_entry_ts_ticks: 0\nlast_exit_ts_ticks: 0\n\
             total_state_residency_ms: 2\nlast_state_entry_counts: 2\n"
        );
    }

    #[::fuchsia::test]
    fn only_served_entities_with_safe_names_are_mapped_to_files() {
        for prefix in [PERF_STATE_FILE_PREFIX, POWER_DOMAIN_FILE_PREFIX] {
            let names = [
                format!("{prefix}a"),
                format!("{prefix}b_1"),
                PLATFORM_POWER_ENTITY.to_string(),
                POWER_STATE_STATS_ENTITY.to_string(),
                "other".to_string(),
                prefix.to_string(),
                format!("{prefix}../x"),
                format!("{prefix}a/b"),
                prefix.to_uppercase() + "A",
                format!("x{prefix}a"),
                format!("{PLATFORM_POWER_ENTITY}_x"),
                format!("{POWER_STATE_STATS_ENTITY}_x"),
            ];

            let served: Vec<_> = names
                .into_iter()
                .filter_map(|name| residency_file_spec(&name).map(|spec| (spec.device, spec.file)))
                .collect();
            assert_eq!(
                served,
                vec![
                    (RESIDENCY_DEVICE_NAME, format!("{prefix}a")),
                    (RESIDENCY_DEVICE_NAME, format!("{prefix}b_1")),
                    (RESIDENCY_DEVICE_NAME, PLATFORM_POWER_FILE.to_string()),
                    (CAP_SYSFS_DEVICE_NAME, POWER_STATE_STATS_FILE.to_string()),
                ]
            );
        }
    }

    #[::fuchsia::test]
    fn dvfs_entities_are_mapped_to_cap_sysfs_files() {
        let names = ["dvfs_clk_lit", "dvfs_clk_mid0", "dvfs_", "dvfs_../x", "dvfs_a-b", "xdvfs_a"];

        let served: Vec<_> = names
            .into_iter()
            .filter_map(|name| residency_file_spec(name).map(|spec| (spec.device, spec.file)))
            .collect();
        assert_eq!(
            served,
            vec![
                (CAP_SYSFS_DEVICE_NAME, "dvfs_stats_clk_lit".to_string()),
                (CAP_SYSFS_DEVICE_NAME, "dvfs_stats_clk_mid0".to_string()),
            ]
        );
    }
}
