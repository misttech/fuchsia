// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Exposes power residency entities from [`PowerStatsManager`] under
//! `/sys/devices/platform/powerdashboard`.

use fidl_fuchsia_hardware_power_stats as fpowerstats;
use starnix_core::device::power_stats::PowerStatsManager;
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use starnix_logging::log_warn;
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use std::borrow::Cow;
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

/// Name of the platform device under which the residency files are served.
const RESIDENCY_DEVICE_NAME: &str = "powerdashboard";

/// Ticks per millisecond of the counter that some residency files report timestamps in.
///
/// Those files report raw values of the platform timer counter, which is expected to run at
/// 38.4 MHz, so the provider's millisecond timestamps are converted at 38,400 ticks per
/// millisecond.
const TIMESTAMP_TICKS_PER_MS: u64 = 38_400;

/// Creates the node serving a residency file, reading from the given [`PowerStatsManager`].
type CreateResidencyNode = Box<dyn FnOnce(PowerStatsManager) -> Box<dyn FsNodeOps> + Send>;

/// How a power entity is served.
struct ResidencyFileSpec {
    /// Name of the file within the device directory.
    file: String,
    create_node: CreateResidencyNode,
}

/// Serves a file for each power entity reported over `fuchsia.hardware.power.stats.Service` that
/// [`residency_file_spec`] maps to one, under the [`RESIDENCY_DEVICE_NAME`] platform device.
///
/// The entities are only known once a provider has started, so the device is added asynchronously
/// via [`PowerStatsManager`], once a provider reports at least one served entity.
pub fn init_power_residency_device(kernel: &Kernel) {
    let mut device_added = false;
    let manager = PowerStatsManager::get_or_init(kernel);
    let manager_for_files = manager.clone();
    manager.watch_providers(kernel, move |kernel, provider| {
        let files: Vec<_> =
            provider.entity_names().iter().filter_map(|name| residency_file_spec(name)).collect();
        if files.is_empty() {
            return;
        }
        if device_added {
            log_warn!(
                "Ignoring power residency entities from additional provider {}",
                provider.instance_name()
            );
            return;
        }
        device_added = true;
        let manager = manager_for_files.clone();
        let registry = &kernel.device_registry;
        registry.add_bus_device(
            RESIDENCY_DEVICE_NAME.into(),
            Some(registry.objects.platform_device()),
            registry.objects.platform_bus(),
            |device, dir| {
                build_device_directory(device, dir);
                for spec in files {
                    dir.entry(&spec.file, (spec.create_node)(manager.clone()), mode!(IFREG, 0o444));
                }
            },
        );
    });
}

/// Returns how to serve the power entity named `entity`, or [`None`] if it is not served.
/// Prefixed entities are only served if their names are safe to use as file names.
fn residency_file_spec(entity: &str) -> Option<ResidencyFileSpec> {
    let prefixed = |prefix: &str| {
        entity.len() > prefix.len()
            && entity.starts_with(prefix)
            && entity.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    };
    if prefixed(PERF_STATE_FILE_PREFIX) {
        let file = entity.to_string();
        let entity = entity.to_string();
        Some(ResidencyFileSpec {
            file,
            create_node: Box::new(move |manager| -> Box<dyn FsNodeOps> {
                Box::new(BytesFile::new_node(PerformanceStatesFile { entity, manager }))
            }),
        })
    } else if prefixed(POWER_DOMAIN_FILE_PREFIX) {
        let file = entity.to_string();
        let entity = entity.to_string();
        Some(ResidencyFileSpec {
            file,
            create_node: Box::new(move |manager| -> Box<dyn FsNodeOps> {
                Box::new(BytesFile::new_node(PowerDomainStatesFile { entity, manager }))
            }),
        })
    } else if entity == PLATFORM_POWER_ENTITY {
        Some(ResidencyFileSpec {
            file: PLATFORM_POWER_FILE.to_string(),
            create_node: Box::new(|manager| -> Box<dyn FsNodeOps> {
                Box::new(BytesFile::new_node(PlatformPowerStatesFile { manager }))
            }),
        })
    } else {
        None
    }
}

/// Returns the residency that `manager` reports for `entity`.
fn read_entity_stats(
    manager: &PowerStatsManager,
    entity: &str,
) -> Result<fpowerstats::Stats, Errno> {
    manager.entity_stats(entity)?.ok_or_else(|| {
        log_warn!("No power stats provider registered for {entity}");
        errno!(EIO)
    })
}

/// Serves the time a single component, such as a memory controller, spent at each of its
/// performance (DVFS) levels.
struct PerformanceStatesFile {
    entity: String,
    manager: PowerStatsManager,
}

impl BytesFileOps for PerformanceStatesFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let stats = read_entity_stats(&self.manager, &self.entity)?;
        Ok(format_performance_states(&stats).into_bytes().into())
    }
}

/// Serves the time a single power domain spent in each of its power states.
struct PowerDomainStatesFile {
    entity: String,
    manager: PowerStatsManager,
}

impl BytesFileOps for PowerDomainStatesFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let stats = read_entity_stats(&self.manager, &self.entity)?;
        Ok(format_power_domain_states(&stats).into_bytes().into())
    }
}

/// Serves the time the platform as a whole spent in each of its power states, such as active or
/// idle.
struct PlatformPowerStatesFile {
    manager: PowerStatsManager,
}

impl BytesFileOps for PlatformPowerStatesFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let stats = read_entity_stats(&self.manager, PLATFORM_POWER_ENTITY)?;
        Ok(format_platform_power_states(&stats).into_bytes().into())
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
    async fn read_returns_formatted_residency() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let perf_entity = stats("lpcm_a", vec![state(0, 11, 2200)]);
            let domain_entity = stats("pwrblk_a", vec![state(0, 5, 40)]);
            let platform_entity = stats(PLATFORM_POWER_ENTITY, vec![state(0, 3, 100)]);
            PowerStatsManager::add_provider_for_testing(
                kernel,
                fake_provider(
                    "test",
                    vec![perf_entity.clone(), domain_entity.clone(), platform_entity.clone()],
                ),
            );
            let manager = PowerStatsManager::get_or_init(kernel);

            let perf_file =
                PerformanceStatesFile { entity: "lpcm_a".to_string(), manager: manager.clone() };
            assert_eq!(
                &*perf_file.read(current_task).expect("read perf state file"),
                format_performance_states(&perf_entity).as_bytes()
            );

            let domain_file =
                PowerDomainStatesFile { entity: "pwrblk_a".to_string(), manager: manager.clone() };
            assert_eq!(
                &*domain_file.read(current_task).expect("read power domain file"),
                format_power_domain_states(&domain_entity).as_bytes()
            );

            let platform_file = PlatformPowerStatesFile { manager };
            assert_eq!(
                &*platform_file.read(current_task).expect("read platform power file"),
                format_platform_power_states(&platform_entity).as_bytes()
            );
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
            let file = PerformanceStatesFile {
                entity: "a".to_string(),
                manager: PowerStatsManager::get_or_init(kernel),
            };

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
            let file_a =
                PerformanceStatesFile { entity: "a".to_string(), manager: manager.clone() };
            let file_b = PerformanceStatesFile { entity: "b".to_string(), manager };

            assert_eq!(file_a.read(current_task).map(|_| ()), error!(EIO));
            assert_eq!(file_b.read(current_task).map(|_| ()), error!(EIO));

            let entity = stats("a", vec![state(0, 5, 100)]);
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
            let file = PerformanceStatesFile {
                entity: "a".to_string(),
                manager: PowerStatsManager::get_or_init(kernel),
            };

            assert_eq!(file.read(current_task).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    fn performance_states_are_formatted_per_state() {
        let stats = stats("a", vec![state(0, 11, 2200), state(1, 4, 7)]);

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
            ..state(0, 3, 12)
        };
        let on = fpowerstats::StateResidency {
            name: "STATE_ON".to_string(),
            last_entry_timestamp_ms: 9,
            ..state(1, 4, 7)
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
            ..state(0, 3, 100)
        };
        let stats = stats("a", vec![active, state(1, 0, 0)]);

        assert_eq!(
            format_platform_power_states(&stats),
            "power_state: ACTIVE\nentry_count: 3\ntime_in_state_ms: 100\n\
             last_entry_timestamp_ticks: 76800\nlast_exit_timestamp_ticks: 0\n\n\
             power_state: 1\nentry_count: 0\ntime_in_state_ms: 0\n\
             last_entry_timestamp_ticks: 0\nlast_exit_timestamp_ticks: 0\n\n"
        );
    }

    #[::fuchsia::test]
    fn residency_with_no_states() {
        let stats = stats("a", vec![]);

        assert_eq!(format_performance_states(&stats), "num_pf_states: 0\n");
        assert_eq!(format_power_domain_states(&stats), "\nSSWRP lpb\n\n\n");
        assert_eq!(format_platform_power_states(&stats), "");
    }

    #[::fuchsia::test]
    fn states_with_unsafe_names_are_left_out() {
        let unsafe_state =
            fpowerstats::StateResidency { name: "A\nentry_count: 9".to_string(), ..state(0, 1, 1) };
        let stats = stats("a", vec![unsafe_state, state(1, 2, 2)]);

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
    }

    #[::fuchsia::test]
    fn only_served_entities_with_safe_names_are_mapped_to_files() {
        for prefix in [PERF_STATE_FILE_PREFIX, POWER_DOMAIN_FILE_PREFIX] {
            let names = [
                format!("{prefix}a"),
                format!("{prefix}b_1"),
                PLATFORM_POWER_ENTITY.to_string(),
                "other".to_string(),
                prefix.to_string(),
                format!("{prefix}../x"),
                format!("{prefix}a/b"),
                prefix.to_uppercase() + "A",
                format!("x{prefix}a"),
                format!("{PLATFORM_POWER_ENTITY}_x"),
            ];

            let served: Vec<_> = names
                .into_iter()
                .filter_map(|name| residency_file_spec(&name).map(|spec| spec.file))
                .collect();
            assert_eq!(
                served,
                vec![format!("{prefix}a"), format!("{prefix}b_1"), PLATFORM_POWER_FILE.to_string()]
            );
        }
    }
}
