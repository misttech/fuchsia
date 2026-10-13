// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::sysfs::build_device_directory;
use crate::task::{CurrentTask, Kernel};
use crate::vfs::FsNodeOps;
use crate::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use anyhow::Context as _;
use fidl_fuchsia_hardware_power_stats as fpowerstats;
use fuchsia_component::client as fclient;
use futures::TryStreamExt;
use starnix_logging::{log_info, log_warn};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::{errno, error};
use std::borrow::Cow;
use std::fmt::Write as _;
use std::sync::{Arc, Weak};

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

/// How long reading a residency file waits for the power stats provider before failing.
const PROVIDER_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(1);

/// Ticks per millisecond of the counter that some residency files report timestamps in.
///
/// Those files report raw values of the platform timer counter, which is expected to run at
/// 38.4 MHz, so the provider's millisecond timestamps are converted at 38,400 ticks per
/// millisecond.
const TIMESTAMP_TICKS_PER_MS: u64 = 38_400;

/// Creates the node serving a residency file, reading from the given power stats provider.
type CreateResidencyNode =
    Box<dyn FnOnce(Arc<fpowerstats::DeviceSynchronousProxy>) -> Box<dyn FsNodeOps> + Send>;

/// How a power entity is served.
struct ResidencyFileSpec {
    /// Name of the file within the device directory.
    file: String,
    create_node: CreateResidencyNode,
}

/// Serves a file for each power entity reported over `fuchsia.hardware.power.stats.Service` that
/// [`residency_file_spec`] maps to one, under the [`RESIDENCY_DEVICE_NAME`] platform device.
///
/// The entities are only known once a provider has started, so the device is added in the
/// background, once a provider reports at least one served entity.
pub fn init_power_residency_device(kernel: &Kernel) {
    let weak_kernel = kernel.weak_self.clone();
    kernel.kthreads.spawn_future(
        move || async move {
            let result = match fclient::Service::open(fpowerstats::ServiceMarker) {
                Ok(service) => add_residency_device(weak_kernel, service).await,
                Err(e) => Err(e.context("failed to open power stats service directory")),
            };
            if let Err(e) = result {
                log_info!("Power residency files unavailable: {e:#}");
            }
        },
        "power_residency",
    );
}

/// Watches `service` for power stats providers and adds the device, with a file for each served
/// entity, for the first provider that reports any. Served entities from other providers are
/// ignored.
async fn add_residency_device(
    kernel: Weak<Kernel>,
    service: fclient::Service<fpowerstats::ServiceMarker>,
) -> Result<(), anyhow::Error> {
    let mut instances = service.watch().await.context("failed to watch power stats service")?;
    let mut device_added = false;
    while let Some(instance) = instances.try_next().await? {
        let name = instance.instance_name().to_string();
        let entities = match instance.connect_to_device() {
            Ok(device) => device.get_entities().await.map_err(anyhow::Error::from),
            Err(e) => Err(anyhow::Error::from(e)),
        };
        let entities = match entities {
            Ok(entities) => entities,
            Err(e) => {
                log_warn!("Failed to get power entities from {name}: {e:#}");
                continue;
            }
        };
        let files: Vec<_> =
            entities.iter().filter_map(|entity| residency_file_spec(&entity.name)).collect();
        if files.is_empty() {
            continue;
        }
        if device_added {
            log_warn!("Ignoring power residency entities from additional provider {name}");
            continue;
        }
        let Some(kernel) = kernel.upgrade() else {
            return Ok(());
        };
        let provider = match instance.connect_to_device_sync() {
            Ok(provider) => Arc::new(provider),
            Err(e) => {
                log_warn!("Failed to connect to power stats provider {name}: {e:#}");
                continue;
            }
        };
        let registry = &kernel.device_registry;
        registry.add_bus_device(
            RESIDENCY_DEVICE_NAME.into(),
            Some(registry.objects.platform_device()),
            registry.objects.platform_bus(),
            |device, dir| {
                build_device_directory(device, dir);
                for spec in files {
                    dir.entry(
                        &spec.file,
                        (spec.create_node)(provider.clone()),
                        mode!(IFREG, 0o444),
                    );
                }
            },
        );
        device_added = true;
    }
    Ok(())
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
            create_node: Box::new(move |provider| -> Box<dyn FsNodeOps> {
                Box::new(BytesFile::new_node(PerformanceStatesFile { entity, provider }))
            }),
        })
    } else if prefixed(POWER_DOMAIN_FILE_PREFIX) {
        let file = entity.to_string();
        let entity = entity.to_string();
        Some(ResidencyFileSpec {
            file,
            create_node: Box::new(move |provider| -> Box<dyn FsNodeOps> {
                Box::new(BytesFile::new_node(PowerDomainStatesFile { entity, provider }))
            }),
        })
    } else if entity == PLATFORM_POWER_ENTITY {
        Some(ResidencyFileSpec {
            file: PLATFORM_POWER_FILE.to_string(),
            create_node: Box::new(|provider| -> Box<dyn FsNodeOps> {
                Box::new(BytesFile::new_node(PlatformPowerStatesFile { provider }))
            }),
        })
    } else {
        None
    }
}

/// Returns the residency that `provider` reports for `entity`.
fn read_entity_stats(
    provider: &fpowerstats::DeviceSynchronousProxy,
    entity: &str,
) -> Result<fpowerstats::Stats, Errno> {
    let deadline = zx::MonotonicInstant::after(PROVIDER_TIMEOUT);
    let reports = provider.get_stats(deadline).map_err(|e| {
        log_warn!("Failed to read power stats for {entity}: {e}");
        errno!(EIO)
    })?;
    let Some(stats) = reports.into_iter().find(|stats| stats.entity == entity) else {
        log_warn!("Power stats provider no longer reports {entity}");
        return error!(EIO);
    };
    Ok(stats)
}

/// Serves the time a single component, such as a memory controller, spent at each of its
/// performance (DVFS) levels.
struct PerformanceStatesFile {
    entity: String,
    provider: Arc<fpowerstats::DeviceSynchronousProxy>,
}

impl BytesFileOps for PerformanceStatesFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let stats = read_entity_stats(&self.provider, &self.entity)?;
        Ok(format_performance_states(&stats).into_bytes().into())
    }
}

/// Serves the time a single power domain spent in each of its power states.
struct PowerDomainStatesFile {
    entity: String,
    provider: Arc<fpowerstats::DeviceSynchronousProxy>,
}

impl BytesFileOps for PowerDomainStatesFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let stats = read_entity_stats(&self.provider, &self.entity)?;
        Ok(format_power_domain_states(&stats).into_bytes().into())
    }
}

/// Serves the time the platform as a whole spent in each of its power states, such as active or
/// idle.
struct PlatformPowerStatesFile {
    provider: Arc<fpowerstats::DeviceSynchronousProxy>,
}

impl BytesFileOps for PlatformPowerStatesFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let stats = read_entity_stats(&self.provider, PLATFORM_POWER_ENTITY)?;
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
    use crate::testing::spawn_kernel_and_run;
    use fidl::endpoints::{ServerEnd, create_endpoints, create_sync_proxy};
    use fidl_fuchsia_io as fio;
    use fuchsia_async as fasync;
    use fuchsia_component::server::ServiceFs;
    use futures::{FutureExt, StreamExt, select};
    use std::pin::pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    /// Serves a fake provider that reports `reports` on `stream`, counting `GetEntities` calls.
    async fn serve_provider(
        stream: fpowerstats::DeviceRequestStream,
        reports: Vec<fpowerstats::Stats>,
        get_entities_calls: Arc<AtomicUsize>,
    ) {
        let _ = stream
            .for_each(|request| {
                let reports = &reports;
                let get_entities_calls = &get_entities_calls;
                async move {
                    match request {
                        Ok(fpowerstats::DeviceRequest::GetEntities { responder }) => {
                            let entities: Vec<_> = reports
                                .iter()
                                .map(|stats| fpowerstats::Entity {
                                    name: stats.entity.clone(),
                                    states: vec![],
                                })
                                .collect();
                            let _ = responder.send(&entities);
                            get_entities_calls.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(fpowerstats::DeviceRequest::GetStats { responder }) => {
                            let _ = responder.send(reports);
                        }
                        _ => {}
                    }
                }
            })
            .await;
    }

    /// Runs `serve` on its own thread, as residency files call providers synchronously.
    fn spawn_server<F: Future<Output = ()> + 'static>(serve: impl FnOnce() -> F + Send + 'static) {
        std::thread::spawn(move || fasync::LocalExecutor::default().run_singlethreaded(serve()));
    }

    /// Serves `fuchsia.hardware.power.stats.Service` with one instance per entry of `providers`.
    ///
    /// Returns the service and the number of `GetEntities` calls answered so far.
    fn serve_service(
        providers: Vec<(&'static str, Vec<fpowerstats::Stats>)>,
    ) -> (fclient::Service<fpowerstats::ServiceMarker>, Arc<AtomicUsize>) {
        let get_entities_calls = Arc::new(AtomicUsize::new(0));
        let (client, server) = create_endpoints::<fio::DirectoryMarker>();
        let calls = get_entities_calls.clone();
        spawn_server(move || async move {
            let mut fs = ServiceFs::new_local();
            for (instance, reports) in providers {
                let calls = calls.clone();
                fs.add_fidl_service_instance(instance, move |request| {
                    let fpowerstats::ServiceRequest::Device(stream) = request;
                    (stream, reports.clone(), calls.clone())
                });
            }
            fs.serve_connection(server).expect("serve service directory");
            fs.for_each_concurrent(None, |(stream, reports, calls)| {
                serve_provider(stream, reports, calls)
            })
            .await;
        });
        let service = fclient::Service::open_from_dir(client, fpowerstats::ServiceMarker)
            .expect("open service");
        (service, get_entities_calls)
    }

    /// Runs `watcher` until `done` returns true.
    async fn run_until<W>(mut watcher: W, done: impl Fn() -> bool)
    where
        W: Future<Output = Result<(), anyhow::Error>> + futures::future::FusedFuture + Unpin,
    {
        while !done() {
            select! {
                result = watcher => panic!("watcher exited: {result:?}"),
                _ = fasync::Timer::new(zx::MonotonicDuration::from_millis(10)).fuse() => {}
            }
        }
    }

    fn exists(kernel: &Kernel, path: &str) -> bool {
        kernel.device_registry.objects.root.lookup(path.into()).is_some()
    }

    #[::fuchsia::test]
    async fn no_device_without_served_entities() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let (service, get_entities_calls) =
                serve_service(vec![("default", vec![stats("other", vec![])])]);
            let mut watcher = pin!(add_residency_device(kernel.weak_self.clone(), service).fuse());

            run_until(&mut watcher, || get_entities_calls.load(Ordering::SeqCst) == 1).await;

            assert!(!exists(kernel, &format!("devices/platform/{RESIDENCY_DEVICE_NAME}")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn only_first_provider_is_served() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let prefix = PERF_STATE_FILE_PREFIX;
            let (service, get_entities_calls) = serve_service(vec![
                ("first", vec![stats(&format!("{prefix}a"), vec![])]),
                ("second", vec![stats(&format!("{prefix}b"), vec![])]),
            ]);
            let mut watcher = pin!(add_residency_device(kernel.weak_self.clone(), service).fuse());

            let device = format!("devices/platform/{RESIDENCY_DEVICE_NAME}");
            run_until(&mut watcher, || {
                get_entities_calls.load(Ordering::SeqCst) == 2 && exists(kernel, &device)
            })
            .await;

            let served_a = exists(kernel, &format!("{device}/{prefix}a"));
            let served_b = exists(kernel, &format!("{device}/{prefix}b"));
            assert!(served_a != served_b, "expected files from exactly one provider");
        })
        .await;
    }

    /// Returns a performance states file for `entity` backed by `provider`.
    fn performance_states_file(
        entity: &str,
        provider: fpowerstats::DeviceSynchronousProxy,
    ) -> PerformanceStatesFile {
        PerformanceStatesFile { entity: entity.to_string(), provider: Arc::new(provider) }
    }

    #[::fuchsia::test]
    async fn read_fails_when_entity_is_no_longer_reported() {
        spawn_kernel_and_run(async |current_task| {
            let (provider, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            let reports = vec![stats("b", vec![])];
            spawn_server(move || serve_provider(server.into_stream(), reports, Default::default()));
            let file = performance_states_file("a", provider);

            assert_eq!(file.read(current_task).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn read_fails_when_provider_is_gone() {
        spawn_kernel_and_run(async |current_task| {
            let (provider, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            drop::<ServerEnd<_>>(server);
            let file = performance_states_file("a", provider);

            assert_eq!(file.read(current_task).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn read_fails_when_provider_does_not_respond() {
        spawn_kernel_and_run(async |current_task| {
            let (provider, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            // Hold on to the requests without responding to them.
            spawn_server(move || async move {
                let _requests: Vec<_> = server.into_stream().collect().await;
            });
            let file = performance_states_file("a", provider);

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
