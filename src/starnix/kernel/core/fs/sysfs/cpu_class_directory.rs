// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::sysfs::build_device_directory;
use crate::task::{CurrentTask, Kernel};
use crate::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use crate::vfs::pseudo::simple_file::{
    BytesFile, BytesFileOps, SimpleFileNode, parse_unsigned_file,
};
use crate::vfs::pseudo::stub_empty_file::StubEmptyFile;
use crate::vfs::{FsNodeOps, FsString};
use fidl_fuchsia_hardware_cpu_ctrl as fcpuctrl;
use fidl_fuchsia_hardware_power_stats as fpowerstats;
use fidl_fuchsia_power_broker as fbroker;
use fidl_fuchsia_power_cpu as fcpu;
use fuchsia_component::client::connect_to_protocol_sync;
use itertools::Itertools;
use starnix_logging::{bug_ref, log_warn};
use starnix_sync::{
    CpuC2ProxyCacheLock, CpuC2SuspendAccountingLock, CpuFreqProxyCacheLock, LockDepMutex,
};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::{errno, error, from_status_like_fdio};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use zx;

#[derive(Clone)]
struct CpuFreqDomains(Arc<[Arc<CpuFreqDomain>]>);

/// Returns the per-domain CPU frequency state shared across sysfs `cpufreq` nodes and thermal
/// cooling devices for `kernel`.
pub fn get_cpu_freq_domains(kernel: &Kernel) -> Arc<[Arc<CpuFreqDomain>]> {
    kernel.expando.get_or_init(|| CpuFreqDomains(get_cpu_domains().into())).0.clone()
}

/// Registers `/sys/devices/system/cpu` and the per-core `cpuN` bus devices beneath it.
pub fn register_cpu_devices(kernel: &Kernel) {
    // Each domain is wrapped once and shared across `cpuN/cpufreq`, `cpufreq/policyN`, and
    // thermal `cooling_device*` nodes so all voters aggregate on the same domain state.
    let cpu_domains = get_cpu_freq_domains(kernel);

    let mut core_to_domain_map: Vec<(u64, &Arc<CpuFreqDomain>)> = cpu_domains
        .iter()
        .flat_map(|domain| domain.core_ids.iter().map(move |id| (*id, domain)))
        .collect();
    core_to_domain_map.sort_by_key(|(id, _)| *id);
    core_to_domain_map.dedup_by_key(|(id, _)| *id);

    let core_count = core_to_domain_map.len();
    let registry = &kernel.device_registry;
    let system_device = registry.objects.system_device();
    let cpu_root =
        registry.add_subsystemless_device("cpu".into(), Some(system_device), |device, dir| {
            build_device_directory(device, dir);
            build_cpu_subsystem_directory(dir, &cpu_domains, core_count);
        });

    let cpu_bus = registry.objects.get_or_create_bus("cpu".into());
    let c2_provider = kernel.expando.get::<CpuC2Provider>();
    for (core_id, domain) in &core_to_domain_map {
        let name: FsString = format!("cpu{}", core_id).into();
        let c2_provider = c2_provider.clone();
        registry.add_bus_device(
            name.as_ref(),
            Some(cpu_root.clone()),
            cpu_bus.clone(),
            |device, dir| {
                build_device_directory(device, dir);
                build_cpu_directory(dir, *core_id, domain, c2_provider);
            },
        );
    }
}

/// Populates the subsystem-wide attributes in `/sys/devices/system/cpu`.
fn build_cpu_subsystem_directory(
    dir: &SimpleDirectoryMutator,
    cpu_domains: &[Arc<CpuFreqDomain>],
    core_count: usize,
) {
    dir.entry(
        "online",
        BytesFile::new_node(format!("0-{}\n", core_count.saturating_sub(1)).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "possible",
        BytesFile::new_node(format!("0-{}\n", core_count.saturating_sub(1)).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.subdir("vulnerabilities", 0o755, |dir| {
        for (name, contents) in VULNERABILITIES {
            let contents = contents.to_string();
            dir.entry(name, BytesFile::new_node(contents.into_bytes()), mode!(IFREG, 0o444));
        }
    });
    dir.subdir("cpufreq", 0o755, |dir| {
        for domain in cpu_domains.iter() {
            let min_core_id = domain.core_ids.iter().min().expect("core_ids is empty");
            let name = format!("policy{}", min_core_id);
            dir.subdir(&name, 0o755, |dir| build_cpufreq_directory(dir, domain));
        }
    });
    dir.subdir("soc", 0o755, |dir| {
        dir.subdir("0", 0o755, |dir| {
            dir.entry(
                "machine",
                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                mode!(IFREG, 0o444),
            );
        });
    });
}

/// Retrieves CPU topology and domain information with a tiered fallback approach.
///
/// 1. Get CPU domains from the `fuchsia.power.cpu.DomainController` FIDL protocol.
/// 2. If that fails, use the `fuchsia.hardware.cpu.ctrl.Service` FIDL service to connect to
///    individual CPU control devices.
/// 3. If that fails, get CPU information from the kernel directly. In this case, no CPU control
///    or topological information is available, so only a limited set of sysfs entries
///    will be populated.
fn get_cpu_domains() -> Vec<Arc<CpuFreqDomain>> {
    // Tier 1: Try DomainController
    if let Ok(domain_controller) = connect_to_protocol_sync::<fcpu::DomainControllerMarker>() {
        if let Ok(mut domains) = domain_controller.list_domains(zx::MonotonicInstant::INFINITE) {
            // Remove any domains without an ID or empty core_ids.
            domains
                .retain(|d| d.id.is_some() && d.core_ids.as_ref().map_or(false, |c| !c.is_empty()));
            if !domains.is_empty() {
                return domains
                    .iter()
                    .map(|domain| Arc::new(CpuFreqDomain::new(domain, Some(&domain_controller))))
                    .collect();
            }
        }
    }

    log_warn!(
        "Could not retrieve CPU domains from fuchsia.power.cpu.DomainController, using CPU control devices instead."
    );

    // Tier 2: Try cpu.ctrl devices
    if let Ok(proxies) = connect_to_cpu_devices() {
        let mut domains = Vec::new();

        for proxy in proxies {
            let cpu_count = match proxy.get_num_logical_cores(zx::MonotonicInstant::INFINITE) {
                Ok(count) => count,
                Err(e) => {
                    log_warn!("get_num_logical_cores returned error: {}", e);
                    continue;
                }
            };
            let domain_id = match proxy.get_domain_id(zx::MonotonicInstant::INFINITE) {
                Ok(id) => id as u64,
                Err(e) => {
                    log_warn!("get_domain_id returned error: {}", e);
                    continue;
                }
            };

            let mut core_ids = Vec::with_capacity(cpu_count as usize);
            let mut get_core_failed = false;
            for i in 0..cpu_count {
                let core_id = match proxy.get_logical_core_id(i, zx::MonotonicInstant::INFINITE) {
                    Ok(id) => id,
                    Err(e) => {
                        log_warn!("get_logical_core_id error in domain {}: {}", domain_id, e);
                        get_core_failed = true;
                        break;
                    }
                };
                core_ids.push(core_id);
            }
            if get_core_failed || core_ids.is_empty() {
                log_warn!("get_logical_core_id failed in domain {}, skipping", domain_id);
                continue;
            }

            let available_frequencies_hz =
                match proxy.get_operating_point_count(zx::MonotonicInstant::INFINITE) {
                    Ok(Ok(count)) => {
                        let mut freqs = Vec::with_capacity(count as usize);
                        for i in 0..count {
                            if let Ok(Ok(info)) =
                                proxy.get_operating_point_info(i, zx::MonotonicInstant::INFINITE)
                            {
                                if info.frequency_hz > 0 {
                                    freqs.push(info.frequency_hz as u64);
                                }
                            }
                        }
                        freqs.sort();
                        freqs.dedup();
                        Some(freqs)
                    }
                    _ => None,
                };

            domains.push(fcpu::DomainInfo {
                id: Some(domain_id),
                core_ids: Some(core_ids),
                available_frequencies_hz,
                ..Default::default()
            });
        }

        if !domains.is_empty() {
            return domains
                .iter()
                .map(|domain| Arc::new(CpuFreqDomain::new(domain, None)))
                .collect();
        }
    }

    log_warn!(
        "Could not connect to CPU control devices, using default domain info from kernel CPU count."
    );

    // Tier 3: Fallback to kernel CPU count
    let cpu_count = zx::system_get_num_cpus();
    vec![Arc::new(CpuFreqDomain::new(
        &fcpu::DomainInfo {
            id: Some(0),
            core_ids: Some((0..cpu_count as u64).collect()),
            available_frequencies_hz: None,
            name: None,
            ..Default::default()
        },
        None,
    ))]
}

fn hz_to_khz(hz: u64) -> u64 {
    hz / 1000
}

/// The only CPU frequency governor Starnix reports.
///
/// Frequency selection is not user-tunable on Fuchsia, so `scaling_governor` is a fixed value
/// rather than a real choice.
const SCALING_GOVERNOR: &str = "performance";

/// Formats a frequency for a sysfs node, leaving the value empty when the domain reports none.
fn format_freq_khz(khz: Option<u64>) -> String {
    format!("{}\n", khz.map(|f| f.to_string()).unwrap_or_default())
}

fn build_cpu_directory(
    dir: &SimpleDirectoryMutator,
    core_id: u64,
    domain: &Arc<CpuFreqDomain>,
    c2_provider: Arc<CpuC2Provider>,
) {
    let cluster_id = domain.domain_id;

    dir.entry(
        "cpu_capacity",
        StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
        mode!(IFREG, 0o444),
    );
    dir.subdir("cpuidle", 0o755, |dir| {
        build_cpuidle_directory(dir, core_id, c2_provider);
    });
    dir.subdir("cpufreq", 0o755, |dir| {
        build_cpufreq_directory(dir, domain);
    });
    dir.subdir("topology", 0o755, |dir| {
        dir.entry(
            "cluster_id",
            BytesFile::new_node(format!("{cluster_id}\n").into_bytes()),
            mode!(IFREG, 0o444),
        );
        dir.entry(
            "physical_package_id",
            BytesFile::new_node(format!("{cluster_id}\n").into_bytes()),
            mode!(IFREG, 0o444),
        );
    });
}

fn build_cpuidle_directory(
    dir: &SimpleDirectoryMutator,
    core_id: u64,
    c2_provider: Arc<CpuC2Provider>,
) {
    let state0_c2_provider = c2_provider.clone();
    dir.subdir("state0", 0o755, |dir| {
        dir.entry("name", BytesFile::new_node(b"C1\n".to_vec()), mode!(IFREG, 0o444));
        dir.entry("desc", BytesFile::new_node(b"idle\n".to_vec()), mode!(IFREG, 0o444));
        dir.entry(
            "time",
            BytesFile::new_node(CpuIdleStatFile::new_time(core_id, state0_c2_provider)),
            mode!(IFREG, 0o444),
        );
        dir.entry(
            "usage",
            BytesFile::new_node(CpuIdleStatFile::new_usage(core_id)),
            mode!(IFREG, 0o444),
        );
    });
    dir.subdir("state1", 0o755, |dir| {
        dir.entry("name", BytesFile::new_node(b"C2\n".to_vec()), mode!(IFREG, 0o444));
        dir.entry("desc", BytesFile::new_node(b"deep idle\n".to_vec()), mode!(IFREG, 0o444));
        dir.entry(
            "time",
            BytesFile::new_node(CpuIdleC2StatFile {
                core_id,
                stat_type: CpuIdleStatType::Time,
                provider: c2_provider.clone(),
            }),
            mode!(IFREG, 0o444),
        );
        dir.entry(
            "usage",
            BytesFile::new_node(CpuIdleC2StatFile {
                core_id,
                stat_type: CpuIdleStatType::Usage,
                provider: c2_provider,
            }),
            mode!(IFREG, 0o444),
        );
    });
}

const SOFT_MAX_FREQ_VOTER: &str = "soft_max_freq";

/// Max-frequency voter nodes under `cpufreq/vote_manager/`.
const VOTE_MANAGER_MAX_FREQ_NODES: &[&str] =
    &["powerhint_max_freq", "thermal_max_freq", SOFT_MAX_FREQ_VOTER, "debug_max_freq"];

/// Min-frequency voter nodes under `cpufreq/vote_manager/`.
const VOTE_MANAGER_MIN_FREQ_NODES: &[&str] =
    &["powerhint_min_freq", "soft_min_freq", "debug_min_freq"];

fn add_max_freq_entry(
    dir: &SimpleDirectoryMutator,
    name: &'static str,
    domain: &Arc<CpuFreqDomain>,
) {
    // A domain that reports no operating points cannot be capped, so fall back to an inert node
    // rather than one that would fail every write.
    if domain.is_tunable() {
        dir.entry(name, ScalingMaxFreqFile::new_node(domain, name), mode!(IFREG, 0o644));
    } else {
        dir.entry(
            name,
            InertFile::new_node(format_freq_khz(domain.highest_khz())),
            mode!(IFREG, 0o644),
        );
    }
}

fn add_min_freq_entry(
    dir: &SimpleDirectoryMutator,
    name: &'static str,
    domain: &Arc<CpuFreqDomain>,
) {
    // A domain that reports no operating points cannot have its minimum frequency controlled, so
    // fall back to an inert node rather than one that would fail every write.
    if domain.is_tunable() {
        dir.entry(name, ScalingMinFreqFile::new_node(domain, name), mode!(IFREG, 0o644));
    } else {
        dir.entry(
            name,
            InertFile::new_node(format_freq_khz(domain.lowest_khz())),
            mode!(IFREG, 0o644),
        );
    }
}

fn build_cpufreq_directory(dir: &SimpleDirectoryMutator, domain: &Arc<CpuFreqDomain>) {
    dir.subdir("stats", 0o755, |dir| {
        dir.entry("reset", CpuFreqStatsResetFile::new_node(), mode!(IFREG, 0o200));
        dir.entry(
            "time_in_state",
            StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
            mode!(IFREG, 0o444),
        );
    });

    // Android Power HAL and thermal/debug tooling vote through `cpufreq/vote_manager/*`. Each
    // max/min-frequency node (including `scaling_max_freq` and `scaling_min_freq`) holds its own
    // persistent `DomainController` lease in `CpuFreqDomain`.
    dir.subdir("vote_manager", 0o755, |dir| {
        for name in VOTE_MANAGER_MAX_FREQ_NODES {
            add_max_freq_entry(dir, name, domain);
        }
        for name in VOTE_MANAGER_MIN_FREQ_NODES {
            add_min_freq_entry(dir, name, domain);
        }
    });

    let related_cpus_str = format!("{}\n", domain.core_ids.iter().sorted().join(" "));
    dir.entry(
        "related_cpus",
        BytesFile::new_node(related_cpus_str.into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "scaling_cur_freq",
        create_scaling_cur_freq_file(domain.domain_id),
        mode!(IFREG, 0o444),
    );

    add_min_freq_entry(dir, "scaling_min_freq", domain);
    add_max_freq_entry(dir, "scaling_max_freq", domain);

    dir.entry(
        "scaling_available_frequencies",
        BytesFile::new_node((domain.sorted_khz.iter().join(" ") + "\n").into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "scaling_available_governors",
        BytesFile::new_node(format!("{SCALING_GOVERNOR}\n").into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "scaling_governor",
        InertFile::new_node(format!("{SCALING_GOVERNOR}\n")),
        mode!(IFREG, 0o644),
    );
    dir.entry(
        "cpuinfo_min_freq",
        BytesFile::new_node(format_freq_khz(domain.lowest_khz()).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "cpuinfo_max_freq",
        BytesFile::new_node(format_freq_khz(domain.highest_khz()).into_bytes()),
        mode!(IFREG, 0o444),
    );
}

const VULNERABILITIES: &[(&str, &str)] = &[
    ("gather_data_sampling", "Not affected\n"),
    ("itlb_multihit", "Not affected\n"),
    ("l1tf", "Not affected\n"),
    ("mds", "Not affected\n"),
    ("meltdown", "Not affected\n"),
    ("mmio_stale_data", "Not affected\n"),
    ("retbleed", "Not affected\n"),
    ("spec_rstack_overflow", "Not affected\n"),
    ("spec_store_bypass", "Not affected\n"),
    ("spectre_v1", "Not affected\n"),
    ("spectre_v2", "Not affected\n"),
    ("srbds", "Not affected\n"),
    ("tsx_async_abort", "Not affected\n"),
];

struct CpuFreqStatsResetFile {}

impl CpuFreqStatsResetFile {
    pub fn new_node() -> impl FsNodeOps {
        BytesFile::new_node(Self {})
    }
}

impl BytesFileOps for CpuFreqStatsResetFile {
    // Currently a no-op. The value written to this node does not matter.
    fn write(&self, _current_task: &CurrentTask, _data: Vec<u8>) -> Result<(), Errno> {
        Ok(())
    }
}

#[derive(Default)]
struct CpuFreqDomainState {
    /// Active ceiling leases keyed by voter name (`voter -> (opp_index, lease_token)`).
    /// Each voter holds its own lease on `DomainController`; dropping the `fbroker::LeaseToken`
    /// releases that voter's ceiling lease in `cpu-manager`.
    votes: HashMap<&'static str, (u64, fbroker::LeaseToken)>,

    /// Active floor leases keyed by voter name (`voter -> (opp_index, lease_token)`).
    /// Each voter holds its own lease on `DomainController`; dropping the `fbroker::LeaseToken`
    /// releases that voter's floor lease in `cpu-manager`.
    min_votes: HashMap<&'static str, (u64, fbroker::LeaseToken)>,

    /// Lazily established connection to the server, dropped if a call fails at the transport
    /// level so that the next caller reconnects.
    domain_controller: Option<fcpu::DomainControllerSynchronousProxy>,
}

fn is_lease_open(token: &fbroker::LeaseToken) -> bool {
    token.wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST).is_err()
}

impl CpuFreqDomainState {
    /// Queries the domain's current ceiling on `domain_controller` and seeds any pre-existing cap
    /// (such as a `boot_max_opp` limit configured in `cpu-manager`) into `soft_max_freq`, matching
    /// how Linux `google_vote_manager` initializes `soft_max_freq` from the platform boot ceiling.
    fn from_domain_controller(
        domain_id: u64,
        opps_hz: &[u64],
        domain_controller: &fcpu::DomainControllerSynchronousProxy,
    ) -> Self {
        let max_hz = opps_hz.iter().max().copied();
        match domain_controller.get_max_frequency(domain_id, zx::MonotonicInstant::INFINITE) {
            Ok(Ok(opp_index)) => match opps_hz.get(opp_index as usize).copied() {
                Some(hz) if Some(hz) == max_hz => Self::default(),
                Some(_) => {
                    let lease_name = format!("starnix:{SOFT_MAX_FREQ_VOTER}");
                    match domain_controller.lease_max_frequency(
                        domain_id,
                        opp_index,
                        &lease_name,
                        zx::MonotonicInstant::INFINITE,
                    ) {
                        Ok(Ok(token)) => {
                            match domain_controller
                                .clear_max_frequency(domain_id, zx::MonotonicInstant::INFINITE)
                            {
                                Ok(Ok(())) => {}
                                other => log_warn!(
                                    "Failed to clear initial max frequency for domain {}: {:?}",
                                    domain_id,
                                    other
                                ),
                            }
                            let mut votes = HashMap::new();
                            votes.insert(SOFT_MAX_FREQ_VOTER, (opp_index, token));
                            Self { votes, ..Self::default() }
                        }
                        other => {
                            log_warn!(
                                "Failed to lease initial soft_max_freq for domain {}: {:?}",
                                domain_id,
                                other
                            );
                            Self::default()
                        }
                    }
                }
                None => {
                    log_warn!(
                        "Initial max frequency index {} out of range for domain {} (len {})",
                        opp_index,
                        domain_id,
                        opps_hz.len()
                    );
                    Self::default()
                }
            },
            other => {
                log_warn!(
                    "Failed to query initial max frequency for domain {}: {:?}",
                    domain_id,
                    other
                );
                Self::default()
            }
        }
    }
}

/// Per-domain state shared by a domain's cpufreq nodes and by its corresponding thermal
/// `cooling_device`.
pub struct CpuFreqDomain {
    domain_id: u64,

    /// Optional name of the domain/cluster from `DomainInfo.name`.
    name: Option<String>,

    /// The IDs of the CPU cores that are part of this domain.
    core_ids: Vec<u64>,

    /// Available frequencies in Hz, in `DomainInfo.available_frequencies_hz` order.
    ///
    /// This is operating point order: `DomainController.LeaseMaxFrequency` and
    /// `LeaseMinFrequency` take an index into this list. It is deliberately kept apart from
    /// `sorted_khz` below, which is what `scaling_available_frequencies` reports, because sorting
    /// reorders the entries. The two lists are not positionally related, so a frequency must be
    /// matched by value to find its operating point.
    opps_hz: Vec<u64>,

    /// `opps_hz` in kHz, sorted ascending and deduplicated.
    sorted_khz: Vec<u64>,

    /// Whether `DomainController` is serving this domain (as opposed to the `cpu.ctrl` or
    /// kernel-CPU-count fallbacks).
    has_domain_controller: bool,

    state: LockDepMutex<CpuFreqDomainState, CpuFreqProxyCacheLock>,
}

impl CpuFreqDomain {
    fn new(
        domain: &fcpu::DomainInfo,
        domain_controller: Option<&fcpu::DomainControllerSynchronousProxy>,
    ) -> Self {
        let domain_id = domain.id.expect("id not available");
        let opps_hz = domain.available_frequencies_hz.clone().unwrap_or_default();
        let sorted_khz = opps_hz.iter().map(|hz| hz_to_khz(*hz)).sorted().dedup().collect();
        let state = match domain_controller {
            Some(dc) if !opps_hz.is_empty() => {
                CpuFreqDomainState::from_domain_controller(domain_id, &opps_hz, dc)
            }
            _ => CpuFreqDomainState::default(),
        };
        Self {
            domain_id,
            name: domain.name.clone(),
            core_ids: domain.core_ids.clone().expect("core_ids not available"),
            opps_hz,
            sorted_khz,
            has_domain_controller: domain_controller.is_some(),
            state: LockDepMutex::new(state),
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn opp_count(&self) -> usize {
        self.opps_hz.len()
    }

    pub fn get_vote(&self, voter: &'static str) -> Option<u64> {
        self.state
            .lock()
            .votes
            .get(voter)
            .and_then(|(index, token)| is_lease_open(token).then_some(*index))
    }

    /// Returns `voter`'s active ceiling in kHz, or the domain's highest frequency when `voter` has
    /// no active cap.
    fn voter_frequency_khz(&self, voter: &'static str) -> Option<u64> {
        self.get_vote(voter)
            .and_then(|index| self.opps_hz.get(index as usize))
            .map(|&hz| hz_to_khz(hz))
            .or_else(|| self.highest_khz())
    }

    fn get_min_vote(&self, voter: &'static str) -> Option<u64> {
        self.state
            .lock()
            .min_votes
            .get(voter)
            .and_then(|(index, token)| is_lease_open(token).then_some(*index))
    }

    /// Returns `voter`'s active floor in kHz, or the domain's lowest frequency when `voter` has
    /// no active floor.
    fn min_voter_frequency_khz(&self, voter: &'static str) -> Option<u64> {
        self.get_min_vote(voter)
            .and_then(|index| self.opps_hz.get(index as usize))
            .map(|&hz| hz_to_khz(hz))
            .or_else(|| self.lowest_khz())
    }

    /// Whether the domain is backed by `DomainController` and reports operating points, and can
    /// therefore have its frequency bounds controlled.
    pub fn is_tunable(&self) -> bool {
        self.has_domain_controller && !self.opps_hz.is_empty()
    }

    fn lowest_khz(&self) -> Option<u64> {
        self.sorted_khz.first().copied()
    }

    fn highest_khz(&self) -> Option<u64> {
        self.sorted_khz.last().copied()
    }

    /// Maps a frequency in kHz to the operating point that caps the domain there.
    ///
    /// Returns `None` when the request is at or above the domain's highest operating point,
    /// meaning "no cap". A client releases a cap by writing a sentinel far above any real
    /// frequency, which lands here.
    ///
    /// Otherwise the request is snapped down to the highest operating point that does not
    /// exceed it, or to the lowest operating point if it undershoots them all.
    fn opp_index_for_khz(&self, khz: u64) -> Option<u64> {
        if khz >= self.highest_khz()? {
            return None;
        }
        self.opps_hz
            .iter()
            .enumerate()
            .filter(|(_, hz)| hz_to_khz(**hz) <= khz)
            .max_by_key(|(_, hz)| **hz)
            .or_else(|| self.opps_hz.iter().enumerate().min_by_key(|(_, hz)| **hz))
            .map(|(index, _)| index as u64)
    }

    /// Maps a frequency in kHz to the operating point that sets the domain's floor there.
    ///
    /// Returns `None` when the request is at or below the domain's lowest operating point,
    /// meaning "no floor". A client releases a floor by writing `0` or the domain's minimum
    /// frequency (`cpuinfo_min_freq`), which lands here.
    ///
    /// Otherwise the request is snapped up to the lowest operating point that meets or exceeds
    /// it, or to the highest operating point if it overshoots them all.
    fn opp_index_for_min_khz(&self, khz: u64) -> Option<u64> {
        if khz <= self.lowest_khz()? {
            return None;
        }
        self.opps_hz
            .iter()
            .enumerate()
            .filter(|(_, hz)| hz_to_khz(**hz) >= khz)
            .min_by_key(|(_, hz)| **hz)
            .or_else(|| self.opps_hz.iter().enumerate().max_by_key(|(_, hz)| **hz))
            .map(|(index, _)| index as u64)
    }

    /// Updates `voter`'s ceiling lease (`Some(opp_index)` or `None` to release) on
    /// `DomainController`.
    pub fn update_vote(&self, voter: &'static str, opp_index: Option<u64>) -> Result<(), Errno> {
        self.update_lease_vote(voter, opp_index, |s| &mut s.votes, Self::lease_max_frequency)
    }

    /// Updates `voter`'s floor lease (`Some(opp_index)` or `None` to release) on
    /// `DomainController`.
    fn update_min_vote(&self, voter: &'static str, opp_index: Option<u64>) -> Result<(), Errno> {
        self.update_lease_vote(voter, opp_index, |s| &mut s.min_votes, Self::lease_min_frequency)
    }

    fn update_lease_vote(
        &self,
        voter: &'static str,
        opp_index: Option<u64>,
        votes_mut: impl Fn(
            &mut CpuFreqDomainState,
        ) -> &mut HashMap<&'static str, (u64, fbroker::LeaseToken)>,
        acquire_lease: impl FnOnce(
            &Self,
            &mut CpuFreqDomainState,
            u64,
            &str,
        ) -> Result<fbroker::LeaseToken, Errno>,
    ) -> Result<(), Errno> {
        if let Some(index) = opp_index {
            if index as usize >= self.opps_hz.len() {
                return error!(EINVAL);
            }
        }
        let mut state = self.state.lock();
        match opp_index {
            Some(index) => {
                if let Some((existing_index, existing_token)) = votes_mut(&mut state).get(voter) {
                    if *existing_index == index && is_lease_open(existing_token) {
                        return Ok(());
                    }
                }
                let lease_name = format!("starnix:{voter}");
                let token = acquire_lease(self, &mut state, index, &lease_name)?;
                votes_mut(&mut state).insert(voter, (index, token));
            }
            None => {
                votes_mut(&mut state).remove(voter);
            }
        }
        Ok(())
    }

    /// Runs `call` against a lazily connected `DomainController`.
    ///
    /// A transport failure invalidates the cached connection; a protocol-level error leaves the
    /// connection in place for the caller to interpret.
    fn with_domain_controller<T, E: std::fmt::Debug>(
        &self,
        state: &mut CpuFreqDomainState,
        call: impl FnOnce(&fcpu::DomainControllerSynchronousProxy) -> Result<T, E>,
    ) -> Result<T, Errno> {
        if state.domain_controller.is_none() {
            state.domain_controller =
                Some(connect_to_protocol_sync::<fcpu::DomainControllerMarker>().map_err(|e| {
                    log_warn!("Failed to connect to DomainController: {:?}", e);
                    errno!(ENODEV)
                })?);
        }
        match call(state.domain_controller.as_ref().expect("connection was just established")) {
            Ok(value) => Ok(value),
            Err(e) => {
                log_warn!("DomainController call failed for domain {}: {:?}", self.domain_id, e);
                state.domain_controller = None;
                error!(EIO)
            }
        }
    }

    /// Reads back the frequency in kHz that the domain is currently capped to.
    fn max_frequency_khz(&self) -> Result<u64, Errno> {
        let mut state = self.state.lock();
        let index = self
            .with_domain_controller(&mut state, |dc| {
                dc.get_max_frequency(self.domain_id, zx::MonotonicInstant::INFINITE)
            })?
            .map_err(|e| {
                log_warn!("get_max_frequency failed for domain {}: {:?}", self.domain_id, e);
                errno!(EINVAL)
            })?;
        let hz = self.opps_hz.get(index as usize).ok_or_else(|| errno!(EINVAL))?;
        Ok(hz_to_khz(*hz))
    }

    fn lease_max_frequency(
        &self,
        state: &mut CpuFreqDomainState,
        index: u64,
        lease_name: &str,
    ) -> Result<fbroker::LeaseToken, Errno> {
        self.with_domain_controller(state, |dc| {
            dc.lease_max_frequency(
                self.domain_id,
                index,
                lease_name,
                zx::MonotonicInstant::INFINITE,
            )
        })?
        .map_err(|e| {
            log_warn!("lease_max_frequency failed for domain {}: {:?}", self.domain_id, e);
            errno!(EINVAL)
        })
    }

    /// Reads back the frequency in kHz that the domain's minimum is currently set to.
    fn min_frequency_khz(&self) -> Result<u64, Errno> {
        let mut state = self.state.lock();
        let index = self
            .with_domain_controller(&mut state, |dc| {
                dc.get_min_frequency(self.domain_id, zx::MonotonicInstant::INFINITE)
            })?
            .map_err(|e| {
                log_warn!("get_min_frequency failed for domain {}: {:?}", self.domain_id, e);
                errno!(EINVAL)
            })?;
        let hz = self.opps_hz.get(index as usize).ok_or_else(|| errno!(EINVAL))?;
        Ok(hz_to_khz(*hz))
    }

    fn lease_min_frequency(
        &self,
        state: &mut CpuFreqDomainState,
        index: u64,
        lease_name: &str,
    ) -> Result<fbroker::LeaseToken, Errno> {
        self.with_domain_controller(state, |dc| {
            dc.lease_min_frequency(
                self.domain_id,
                index,
                lease_name,
                zx::MonotonicInstant::INFINITE,
            )
        })?
        .map_err(|e| {
            log_warn!("lease_min_frequency failed for domain {}: {:?}", self.domain_id, e);
            errno!(EINVAL)
        })
    }
}

/// A writable max-frequency node (`scaling_max_freq` and `vote_manager/*_max_freq`), backed by
/// `fuchsia.power.cpu/DomainController`.
///
/// Votes are keyed by the sysfs node identity (`voter`) in `CpuFreqDomain` and persist across
/// `close()` until overwritten or cleared with a value at or above the domain maximum.
struct ScalingMaxFreqFile {
    domain: Arc<CpuFreqDomain>,
    voter: &'static str,
}

impl ScalingMaxFreqFile {
    fn new_node(domain: &Arc<CpuFreqDomain>, voter: &'static str) -> impl FsNodeOps {
        BytesFile::new_node(Self { domain: domain.clone(), voter })
    }
}

impl BytesFileOps for ScalingMaxFreqFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let content = if self.voter == "scaling_max_freq" {
            format!("{}\n", self.domain.max_frequency_khz()?)
        } else {
            format_freq_khz(self.domain.voter_frequency_khz(self.voter))
        };
        Ok(content.into_bytes().into())
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let opp_index = self.domain.opp_index_for_khz(parse_unsigned_file::<u64>(&data)?);
        self.domain.update_vote(self.voter, opp_index)
    }
}

/// A writable min-frequency node (`scaling_min_freq` and `vote_manager/*_min_freq`), backed by
/// `fuchsia.power.cpu/DomainController`.
///
/// Votes are keyed by the sysfs node identity (`voter`) in `CpuFreqDomain` and persist across
/// `close()` until overwritten or cleared with a value at or below the domain minimum.
struct ScalingMinFreqFile {
    domain: Arc<CpuFreqDomain>,
    voter: &'static str,
}

impl ScalingMinFreqFile {
    fn new_node(domain: &Arc<CpuFreqDomain>, voter: &'static str) -> impl FsNodeOps {
        BytesFile::new_node(Self { domain: domain.clone(), voter })
    }
}

impl BytesFileOps for ScalingMinFreqFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let content = if self.voter == "scaling_min_freq" {
            format!("{}\n", self.domain.min_frequency_khz()?)
        } else {
            format_freq_khz(self.domain.min_voter_frequency_khz(self.voter))
        };
        Ok(content.into_bytes().into())
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let opp_index = self.domain.opp_index_for_min_khz(parse_unsigned_file::<u64>(&data)?);
        self.domain.update_min_vote(self.voter, opp_index)
    }
}

/// A sysfs node that accepts writes, discards them, and reads back a fixed value.
///
/// This is for cpufreq nodes that the Android Power HAL writes but that Fuchsia cannot act on.
/// Leaving such a node read-only is worse than making it inert: libperfmgr suppresses its
/// warning and its 500ms retry clamp only when `open()` fails with `ENOENT`, so a node that
/// exists but rejects writes produces log spam and a busy retry loop on every hint. Removing
/// the node instead would return `ENOENT` to every reader, not just to libperfmgr.
struct InertFile(Vec<u8>);

impl InertFile {
    fn new_node(contents: String) -> impl FsNodeOps {
        BytesFile::new_node(Self(contents.into_bytes()))
    }
}

impl BytesFileOps for InertFile {
    fn write(&self, _current_task: &CurrentTask, _data: Vec<u8>) -> Result<(), Errno> {
        Ok(())
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        Ok(self.0.as_slice().into())
    }
}

const CPU_DIRECTORY: &str = "/svc/fuchsia.hardware.cpu.ctrl.Service";

fn connect_to_cpu_devices() -> Result<Vec<fcpuctrl::DeviceSynchronousProxy>, Errno> {
    let dir = std::fs::read_dir(CPU_DIRECTORY).map_err(|_| errno!(EINVAL))?;

    let proxies: Vec<_> = dir
        .filter_map(|r| r.ok())
        .filter_map(|entry| {
            let path = entry.path().join("device").into_os_string().into_string().ok()?;
            let (client, server) = zx::Channel::create();
            fdio::service_connect(&path, server).ok()?;
            Some(fcpuctrl::DeviceSynchronousProxy::new(client))
        })
        .collect();

    if proxies.is_empty() { error!(ENOENT) } else { Ok(proxies) }
}

fn connect_to_cpu_device_by_domain_id(
    domain_id: u64,
) -> Result<fcpuctrl::DeviceSynchronousProxy, Errno> {
    let dir = std::fs::read_dir(CPU_DIRECTORY).map_err(|_| errno!(EINVAL))?;

    dir.filter_map(|r| r.ok())
        .find_map(|entry| {
            let path = entry.path().join("device").into_os_string().into_string().ok()?;
            let (client, server) = zx::Channel::create();
            fdio::service_connect(&path, server).ok()?;
            let proxy = fcpuctrl::DeviceSynchronousProxy::new(client);

            let dev_domain_id = proxy.get_domain_id(zx::MonotonicInstant::INFINITE).ok()?;
            if domain_id == dev_domain_id as u64 { Some(proxy) } else { None }
        })
        .ok_or_else(|| errno!(ENOENT))
}

fn create_scaling_cur_freq_file(domain_id: u64) -> impl FsNodeOps {
    let proxy_cache =
        LockDepMutex::<_, CpuFreqProxyCacheLock>::new(None::<fcpuctrl::DeviceSynchronousProxy>);
    SimpleFileNode::new(move |_| {
        let mut guard = proxy_cache.lock();
        if guard.is_none() {
            let proxy = connect_to_cpu_device_by_domain_id(domain_id)?;
            *guard = Some(proxy);
        }
        let proxy = guard.as_ref().expect("must have a valid proxy");
        let opp = match proxy.get_current_operating_point(zx::MonotonicInstant::INFINITE) {
            Ok(opp) => opp,
            Err(_) => {
                *guard = None;
                return error!(EINVAL);
            }
        };
        let info = match proxy.get_operating_point_info(opp, zx::MonotonicInstant::INFINITE) {
            Ok(info) => info,
            Err(_) => {
                *guard = None;
                return error!(EINVAL);
            }
        };
        let info = info.map_err(|e| from_status_like_fdio!(zx::Status::err_from_raw(e)))?;
        if info.frequency_hz <= 0 {
            return error!(EINVAL);
        }
        let freq_khz = hz_to_khz(info.frequency_hz as u64);
        Ok(BytesFile::new(format!("{}\n", freq_khz).into_bytes()))
    })
}

enum CpuIdleStatType {
    Time,
    Usage,
}

/// Returns the total time the system has spent suspended since boot.
///
/// The monotonic clock pauses while the system is suspended, whereas the boot clock keeps
/// advancing, so the difference between the two is the accumulated suspended time. Reading the
/// monotonic clock before the boot clock ensures that any execution delay or suspend between the
/// two reads inflates (rather than reduces) `boot_ns - mono_ns`.
fn suspended_time() -> zx::BootDuration {
    let mono_ns = zx::MonotonicInstant::get().into_nanos();
    let boot_ns = zx::BootInstant::get().into_nanos();
    zx::BootDuration::from_nanos(boot_ns.saturating_sub(mono_ns))
}

/// Per-core firmware C2 residency, annotated with the portion accrued while the system was
/// awake.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CoreC2Residency {
    /// Core identifier matching the firmware C2 state id.
    id: i32,
    /// Total C2 residency reported by firmware.
    total_time: zx::BootDuration,
    /// Number of C2 entries reported by firmware.
    entry_count: u64,
    /// Portion of `total_time` accrued while the system was awake.
    awake_time: zx::MonotonicDuration,
}

/// Computes C1 residency from Zircon's per-CPU `total_idle` duration by subtracting the C2
/// residency accrued while the system was awake.
///
/// Zircon's `idle_time` is measured on the monotonic timeline and therefore excludes time spent in
/// system suspend, whereas firmware may or may not count system suspend as C2 residency (and this
/// may differ between cores and between suspends). Only C2 residency accrued while awake overlaps
/// with `total_idle`.
///
/// When `c2_residency` is `None` (because C2 residency reporting is unsupported on the platform or
/// not reported for the core), all of `total_idle` is reported as C1 residency.
fn compute_c1_residency(
    total_idle: zx::MonotonicDuration,
    c2_residency: Option<CoreC2Residency>,
) -> zx::MonotonicDuration {
    let awake_c2 = c2_residency.map_or(zx::MonotonicDuration::ZERO, |c2| c2.awake_time);
    (total_idle - awake_c2).max(zx::MonotonicDuration::ZERO)
}

/// Per-core accounting state for [`C2SuspendAccounting`].
#[derive(Clone, Copy, Default)]
struct CoreC2Accounting {
    /// Accumulated suspended time at the previous sample for this core.
    last_suspended: zx::BootDuration,
    /// Firmware C2 residency at the previous sample for this core.
    last_c2: zx::BootDuration,
    /// Accumulated C2 residency accrued while the system was awake.
    awake_c2: zx::MonotonicDuration,
}

/// Tracks per-core firmware C2 residency accrued while the system was awake, excluding C2
/// residency accrued during system suspend.
///
/// Between two samples for a core, C2 growth up to the suspended time that elapsed between those
/// samples is attributed to suspend; any excess growth (`c2_delta - suspended_delta`) was accrued
/// while awake. Cores whose firmware counts suspend as C2 thus have the suspend excluded from
/// `awake_c2`, whereas cores whose firmware does not count it retain their awake C2 growth outside
/// the suspend window.
///
/// Sampling immediately before and after container suspension (see
/// [`sample_cpu_c2_residency`]) keeps the window around each suspend tight, so little awake C2
/// time is lost on cores that do not count suspend as C2.
#[derive(Default)]
struct C2SuspendAccounting {
    /// Per-core accounting keyed by the firmware C2 state id.
    cores: HashMap<i32, CoreC2Accounting>,
}

impl C2SuspendAccounting {
    fn boot_duration_from_ms(ms: u64) -> zx::BootDuration {
        zx::BootDuration::from_millis(i64::try_from(ms).unwrap_or(i64::MAX))
    }

    /// Records a sample of accumulated suspended time and per-core C2 residencies.
    ///
    /// Samples older than a previously recorded one contribute no additional awake residency.
    fn record(&mut self, suspended: zx::BootDuration, residencies: &[fpowerstats::StateResidency]) {
        // Quantize suspended time to milliseconds to match firmware C2 residency granularity.
        let suspended = zx::BootDuration::from_millis(suspended.into_millis().max(0));
        for residency in residencies {
            let total_c2 = Self::boot_duration_from_ms(residency.total_time_ms);
            let core = self.cores.entry(residency.id).or_default();
            let suspended_delta = (suspended - core.last_suspended).max(zx::BootDuration::ZERO);
            let c2_delta = (total_c2 - core.last_c2).max(zx::BootDuration::ZERO);
            let awake_delta =
                zx::MonotonicDuration::from_nanos((c2_delta - suspended_delta).into_nanos().max(0));
            core.awake_c2 += awake_delta;
            core.last_suspended = core.last_suspended.max(suspended);
            core.last_c2 = core.last_c2.max(total_c2);
        }
    }

    /// Returns the C2 residency accrued while awake for `core_id`.
    fn awake_c2(&self, core_id: i32) -> zx::MonotonicDuration {
        self.cores.get(&core_id).map_or(zx::MonotonicDuration::ZERO, |core| core.awake_c2)
    }

    /// Pairs raw firmware residencies with the current per-core awake C2 accounting.
    fn snapshot(&self, residencies: Vec<fpowerstats::StateResidency>) -> Vec<CoreC2Residency> {
        residencies
            .into_iter()
            .map(|state| CoreC2Residency {
                id: state.id,
                total_time: Self::boot_duration_from_ms(state.total_time_ms),
                entry_count: u64::from(state.entry_count),
                awake_time: self.awake_c2(state.id),
            })
            .collect()
    }
}

/// Statistic type and state for a `/sys/devices/system/cpu/cpuN/cpuidle/state0/{time,usage}` node.
enum CpuIdleC1Stat {
    Time {
        c2_provider: Arc<CpuC2Provider>,
        /// Highest C1 residency (in microseconds) reported so far.
        ///
        /// Zircon idle time and firmware C2 residency are sampled at slightly different instants
        /// (and C2 at millisecond granularity), so the computed C1 residency may transiently
        /// regress. Userspace treats `time` as a monotonic counter, so reads never report a value
        /// lower than a previously reported one.
        last_time_us: AtomicU64,
    },
    Usage,
}

/// Backing [`BytesFileOps`] implementation for `/sys/devices/system/cpu/cpuN/cpuidle/state0/time`
/// and `state0/usage`, reporting per-core C1 residency (in microseconds) and reschedule count
/// from Zircon's `PerCpuStats`, subtracting awake C2 residency when available.
struct CpuIdleStatFile {
    core_id: u64,
    stat: CpuIdleC1Stat,
}

impl CpuIdleStatFile {
    fn new_time(core_id: u64, c2_provider: Arc<CpuC2Provider>) -> Self {
        Self { core_id, stat: CpuIdleC1Stat::Time { c2_provider, last_time_us: AtomicU64::new(0) } }
    }

    fn new_usage(core_id: u64) -> Self {
        Self { core_id, stat: CpuIdleC1Stat::Usage }
    }

    fn clamp_c1_residency_us(last_time_us: &AtomicU64, c1: zx::MonotonicDuration) -> u64 {
        let c1_us = u64::try_from(c1.into_micros()).unwrap_or(0);
        last_time_us.fetch_max(c1_us, Ordering::Relaxed).max(c1_us)
    }
}

impl BytesFileOps for CpuIdleStatFile {
    fn read(&self, current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let cpu_stats = current_task
            .kernel()
            .stats
            .get()
            .get_cpu_stats(zx::MonotonicInstant::INFINITE)
            .map_err(|_| errno!(EINVAL))?;
        let per_cpu_stats = cpu_stats.per_cpu_stats.ok_or_else(|| errno!(EINVAL))?;
        let per_cpu = per_cpu_stats.get(self.core_id as usize).ok_or_else(|| errno!(EINVAL))?;
        let value = match &self.stat {
            CpuIdleC1Stat::Time { c2_provider, last_time_us } => {
                let total_idle = zx::MonotonicDuration::from_nanos(
                    per_cpu.idle_time.ok_or_else(|| errno!(EINVAL))?,
                );
                let c2 = c2_provider.residency(self.core_id)?;
                let c1 = compute_c1_residency(total_idle, c2);
                Self::clamp_c1_residency_us(last_time_us, c1)
            }
            CpuIdleC1Stat::Usage => per_cpu.reschedules.ok_or_else(|| errno!(EINVAL))?,
        };
        Ok(format!("{value}\n").into_bytes().into())
    }
}

const POWER_STATS_SERVICE_DIRECTORY: &str = "/svc/fuchsia.hardware.power.stats.Service";
const CPU_C2_ENTITY_NAME: &str = "cpu_c2";

/// Maximum time to wait for a synchronous `fuchsia.hardware.power.stats.Device` call.
const POWER_STATS_FIDL_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);

/// Maximum difference between back-to-back [`suspended_time`] reads attributed to non-atomic
/// reading of the boot and monotonic clocks rather than an intervening system suspend.
const CLOCK_READ_JITTER_TOLERANCE: zx::BootDuration = zx::BootDuration::from_millis(1);

/// Connection state of [`CpuC2Provider`] to the `fuchsia.hardware.power.stats.Service` instance
/// that reports `"cpu_c2"`.
#[derive(Default)]
enum ProviderState {
    /// No initial connection has been attempted yet.
    #[default]
    Unconnected,
    /// Connected to a `fuchsia.hardware.power.stats.Service` instance that reports `"cpu_c2"`.
    Connected(Arc<fpowerstats::DeviceSynchronousProxy>),
    /// A previous connection failed with a transport error; subsequent reads attempt to reconnect
    /// and fail with `EIO` until the service recovers, avoiding non-monotonic `0` counter drops.
    Disconnected,
    /// No `fuchsia.hardware.power.stats.Service` instance in `/svc` reported `"cpu_c2"` during
    /// initial discovery, so C2 residency is unsupported on this platform and reads report `0`.
    Unavailable,
}

/// Provides per-core CPU C2 ("deep idle") state residency from
/// `fuchsia.hardware.power.stats.Service`.
///
/// A single `CpuC2Provider` instance is shared via `Arc` across all CPU cores and across the
/// `state0/time`, `state1/time`, and `state1/usage` sysfs nodes.
///
/// # Caching and Recovery
/// Connection discovery in `/svc/fuchsia.hardware.power.stats.Service` is performed lazily on
/// the first read or suspend sample:
/// - If an instance reporting the `"cpu_c2"` entity is found, its proxy is cached in
///   [`ProviderState::Connected`].
/// - If no such instance exists during initial discovery, [`ProviderState::Unavailable`] is cached
///   so subsequent reads return `Ok(None)` (`0` residency) without repeatedly scanning `/svc`.
/// - If a `GetStats` call fails after a connection was established, the state transitions to
///   [`ProviderState::Disconnected`] and the read fails with `EIO` so userspace does not observe
///   monotonic counters dropping to `0`. Subsequent reads attempt to reconnect without permanently
///   disabling C2 reporting if the driver is still restarting.
///
/// # Concurrency
/// `state` is locked only briefly to inspect or update [`ProviderState`] and clone the `Arc`
/// wrapping the synchronous proxy. The lock is released before scanning `/svc` or issuing FIDL
/// calls so concurrent sysfs reads across CPU cores do not serialize behind the mutex.
/// `suspend_accounting` is likewise locked only to record a sample and snapshot the accounting;
/// the two locks are never held at the same time.
#[derive(Default)]
struct CpuC2Provider {
    state: LockDepMutex<ProviderState, CpuC2ProxyCacheLock>,
    suspend_accounting: LockDepMutex<C2SuspendAccounting, CpuC2SuspendAccountingLock>,
}

impl CpuC2Provider {
    fn connect() -> Option<fpowerstats::DeviceSynchronousProxy> {
        let dir = std::fs::read_dir(POWER_STATS_SERVICE_DIRECTORY).ok()?;
        dir.filter_map(|entry| entry.ok()).find_map(|entry| {
            let path = entry.path().join("device").into_os_string().into_string().ok()?;
            let (client, server) = zx::Channel::create();
            fdio::service_connect(&path, server).ok()?;
            let proxy = fpowerstats::DeviceSynchronousProxy::new(client);
            let deadline = zx::MonotonicInstant::after(POWER_STATS_FIDL_TIMEOUT);
            let entities = proxy.get_entities(deadline).ok()?;
            entities.iter().any(|entity| entity.name == CPU_C2_ENTITY_NAME).then_some(proxy)
        })
    }

    fn proxy(&self) -> Result<Option<Arc<fpowerstats::DeviceSynchronousProxy>>, Errno> {
        if matches!(*self.state.lock(), ProviderState::Unconnected | ProviderState::Disconnected) {
            let connected_proxy = Self::connect().map(Arc::new);
            let mut state = self.state.lock();
            match (&*state, connected_proxy) {
                (ProviderState::Unconnected | ProviderState::Disconnected, Some(proxy)) => {
                    *state = ProviderState::Connected(proxy);
                }
                (ProviderState::Unconnected, None) => {
                    *state = ProviderState::Unavailable;
                }
                (ProviderState::Disconnected, None)
                | (ProviderState::Connected(_) | ProviderState::Unavailable, _) => {}
            }
        }
        match &*self.state.lock() {
            ProviderState::Connected(proxy) => Ok(Some(proxy.clone())),
            ProviderState::Unavailable | ProviderState::Unconnected => Ok(None),
            ProviderState::Disconnected => error!(EIO),
        }
    }

    /// Updates `suspend_accounting` if the sample did not race a system suspend, and returns a
    /// consistent [`CoreC2Residency`] snapshot while holding the accounting lock.
    fn record_sample(
        &self,
        suspended_before: zx::BootDuration,
        suspended_after: zx::BootDuration,
        residencies: Vec<fpowerstats::StateResidency>,
    ) -> Vec<CoreC2Residency> {
        let mut accounting = self.suspend_accounting.lock();
        // If the system suspended while the stats were being read, it is unknown whether the
        // residencies include that suspend, so do not use them for attribution. The tolerance
        // absorbs jitter from reading the boot and monotonic clocks non-atomically.
        if suspended_after - suspended_before <= CLOCK_READ_JITTER_TOLERANCE {
            accounting.record(suspended_after, &residencies);
        }
        accounting.snapshot(residencies)
    }

    /// Reads the per-core C2 residencies from `proxy`, updates `suspend_accounting`, and returns
    /// the resulting [`CoreC2Residency`] snapshot.
    fn sample(
        &self,
        proxy: &Arc<fpowerstats::DeviceSynchronousProxy>,
    ) -> Result<Vec<CoreC2Residency>, Errno> {
        let suspended_before = suspended_time();
        let deadline = zx::MonotonicInstant::after(POWER_STATS_FIDL_TIMEOUT);
        let reports = match proxy.get_stats(deadline) {
            Ok(reports) => reports,
            Err(e) => {
                log_warn!("Failed to read power stats for {CPU_C2_ENTITY_NAME}: {e}");
                let mut state = self.state.lock();
                if matches!(
                    &*state,
                    ProviderState::Connected(current) if Arc::ptr_eq(current, proxy)
                ) {
                    *state = ProviderState::Disconnected;
                }
                return error!(EIO);
            }
        };
        let suspended_after = suspended_time();
        let residencies = reports
            .into_iter()
            .find(|stats| stats.entity == CPU_C2_ENTITY_NAME)
            .map_or_else(Vec::new, |stats| stats.state_residency_data);
        Ok(self.record_sample(suspended_before, suspended_after, residencies))
    }

    /// Connects if needed and records a C2 residency sample when C2 reporting is available.
    fn sample_all(&self) {
        if let Ok(Some(proxy)) = self.proxy() {
            let _ = self.sample(&proxy);
        }
    }

    fn residency(&self, core_id: u64) -> Result<Option<CoreC2Residency>, Errno> {
        let core_id = i32::try_from(core_id).map_err(|_| errno!(EINVAL))?;
        let Some(proxy) = self.proxy()? else {
            return Ok(None);
        };
        Ok(self.sample(&proxy)?.into_iter().find(|s| s.id == core_id))
    }
}

/// Samples per-core CPU C2 residency so that C2 residency accrued during system suspend is
/// excluded from awake C2 residency rather than subtracted from C1 idle time.
///
/// Call immediately before and after suspending the container so the sampling window tightly
/// brackets each suspend.
pub fn sample_cpu_c2_residency(kernel: &Kernel) {
    kernel.expando.get::<CpuC2Provider>().sample_all();
}

/// Backing [`BytesFileOps`] implementation for `/sys/devices/system/cpu/cpuN/cpuidle/state1/time`
/// and `state1/usage`, reporting per-core C2 residency (in microseconds) and entry count from
/// [`CpuC2Provider`], or `0` when C2 residency is unsupported on the platform.
struct CpuIdleC2StatFile {
    core_id: u64,
    stat_type: CpuIdleStatType,
    provider: Arc<CpuC2Provider>,
}

impl BytesFileOps for CpuIdleC2StatFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let value =
            self.provider.residency(self.core_id)?.map_or(0, |state| match self.stat_type {
                CpuIdleStatType::Time => u64::try_from(state.total_time.into_micros()).unwrap_or(0),
                CpuIdleStatType::Usage => state.entry_count,
            });
        Ok(format!("{value}\n").into_bytes().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::RequestStream;
    use futures::TryStreamExt;

    #[::fuchsia::test]
    fn cpu_c2_provider_reads_firmware_residencies() {
        let (client, server) = zx::Channel::create();
        let server_thread = std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutor::default();
            executor.run_singlethreaded(async move {
                let mut stream = fpowerstats::DeviceRequestStream::from_channel(
                    fuchsia_async::Channel::from_channel(server),
                );
                if let Some(fpowerstats::DeviceRequest::GetStats { responder }) =
                    stream.try_next().await.expect("failed to read request")
                {
                    let reports = vec![fpowerstats::Stats {
                        entity: CPU_C2_ENTITY_NAME.to_string(),
                        state_residency_data: vec![
                            fpowerstats::StateResidency {
                                id: 0,
                                name: "C2_CPU0".to_string(),
                                total_time_ms: 34,
                                entry_count: 12,
                                last_entry_timestamp_ms: 0,
                            },
                            fpowerstats::StateResidency {
                                id: 1,
                                name: "C2_CPU1".to_string(),
                                total_time_ms: 78,
                                entry_count: 56,
                                last_entry_timestamp_ms: 0,
                            },
                        ],
                    }];
                    responder.send(&reports).expect("failed to send response");
                }
            });
        });

        let provider = CpuC2Provider {
            state: LockDepMutex::new(ProviderState::Connected(Arc::new(
                fpowerstats::DeviceSynchronousProxy::new(client),
            ))),
            suspend_accounting: Default::default(),
        };
        let core1 = provider
            .residency(/* core_id = */ 1)
            .expect("residency call must succeed")
            .expect("core 1 residency must be present");
        assert_eq!(core1.entry_count, 56);
        assert_eq!(core1.total_time, zx::BootDuration::from_millis(78));

        server_thread.join().expect("server thread panicked");

        // After the server closes the channel, reads return EIO and transition to Disconnected;
        // subsequent reads while the service is still absent stay Disconnected (rather than
        // permanently transitioning to Unavailable) and continue returning EIO.
        assert_eq!(provider.residency(/* core_id = */ 0), error!(EIO));
        assert!(matches!(*provider.state.lock(), ProviderState::Disconnected));
        assert_eq!(provider.residency(/* core_id = */ 0), error!(EIO));
        assert!(matches!(*provider.state.lock(), ProviderState::Disconnected));

        // When no service is present on initial discovery, the provider transitions from
        // Unconnected to Unavailable and returns Ok(None).
        let unconfigured = CpuC2Provider::default();
        assert_eq!(unconfigured.residency(/* core_id = */ 0), Ok(None));
        assert!(matches!(*unconfigured.state.lock(), ProviderState::Unavailable));
    }

    fn c2_residency(id: i32, total_time_ms: u64) -> fpowerstats::StateResidency {
        fpowerstats::StateResidency {
            id,
            name: format!("C2_CPU{id}"),
            total_time_ms,
            entry_count: 1,
            last_entry_timestamp_ms: 0,
        }
    }

    #[::fuchsia::test]
    fn c1_time_subtracts_awake_c2_residency_and_saturates_at_zero() {
        let c2 = CoreC2Residency {
            id: 0,
            total_time: zx::BootDuration::from_millis(1_899),
            entry_count: 1,
            awake_time: zx::MonotonicDuration::from_millis(1_899),
        };

        // 5,000,000 us idle - 1,899,000 us awake C2 = 3,101,000 us C1.
        assert_eq!(
            compute_c1_residency(zx::MonotonicDuration::from_seconds(5), Some(c2)),
            zx::MonotonicDuration::from_micros(3_101_000)
        );

        // When C2 residency is unavailable, report all of total_idle as C1.
        assert_eq!(
            compute_c1_residency(zx::MonotonicDuration::from_seconds(5), None),
            zx::MonotonicDuration::from_seconds(5)
        );

        // Saturate at 0 if awake C2 residency exceeds idle time due to sampling skew.
        assert_eq!(
            compute_c1_residency(zx::MonotonicDuration::from_seconds(1), Some(c2)),
            zx::MonotonicDuration::ZERO
        );

        // C2 residency accrued during suspend (excluded from awake_time) is not subtracted.
        let c2 = CoreC2Residency {
            id: 0,
            total_time: zx::BootDuration::from_millis(154_160),
            entry_count: 2,
            awake_time: zx::MonotonicDuration::from_millis(1_860),
        };
        assert_eq!(
            compute_c1_residency(zx::MonotonicDuration::from_millis(252_370), Some(c2)),
            zx::MonotonicDuration::from_micros(250_510_000)
        );
    }

    #[::fuchsia::test]
    fn c2_suspend_accounting_attributes_suspend_per_core() {
        let mut accounting = C2SuspendAccounting::default();

        // Before suspend: core 0 accrued 10 s of awake C2, core 1 accrued 2 s.
        accounting.record(
            zx::BootDuration::from_millis(0),
            &[c2_residency(0, 10_000), c2_residency(1, 2_000)],
        );
        assert_eq!(accounting.awake_c2(0), zx::MonotonicDuration::from_millis(10_000));
        assert_eq!(accounting.awake_c2(1), zx::MonotonicDuration::from_millis(2_000));

        // A 152 s suspend. Firmware counts it as C2 on core 1 (plus 0.5 s of awake C2 within the
        // sampling window) but not on core 0, which only accrued 0.2 s of awake C2.
        accounting.record(
            zx::BootDuration::from_millis(152_000),
            &[c2_residency(0, 10_200), c2_residency(1, 154_500)],
        );
        assert_eq!(accounting.awake_c2(0), zx::MonotonicDuration::from_millis(10_000));
        assert_eq!(accounting.awake_c2(1), zx::MonotonicDuration::from_millis(2_500));

        // Awake C2 accrued without further suspend is added in full. Core 2 appears for the first
        // time after the 152 s suspend with 153 s of total C2; its per-core last_suspended starts
        // at 0 so the 152 s suspend is still deducted.
        accounting.record(
            zx::BootDuration::from_millis(152_000),
            &[c2_residency(0, 70_000), c2_residency(1, 160_000), c2_residency(2, 153_000)],
        );
        assert_eq!(accounting.awake_c2(0), zx::MonotonicDuration::from_millis(69_800));
        assert_eq!(accounting.awake_c2(1), zx::MonotonicDuration::from_millis(8_000));
        assert_eq!(accounting.awake_c2(2), zx::MonotonicDuration::from_millis(1_000));

        // A stale sample (older suspended time and residencies) adds no awake residency.
        accounting.record(
            zx::BootDuration::from_millis(100_000),
            &[c2_residency(0, 10_000), c2_residency(1, 100_000)],
        );
        assert_eq!(accounting.awake_c2(0), zx::MonotonicDuration::from_millis(69_800));
        assert_eq!(accounting.awake_c2(1), zx::MonotonicDuration::from_millis(8_000));

        // Unknown cores have no awake C2 residency.
        assert_eq!(accounting.awake_c2(7), zx::MonotonicDuration::ZERO);
    }

    #[::fuchsia::test]
    fn record_sample_discards_sample_that_raced_suspend() {
        let provider = CpuC2Provider::default();

        // Initial sample while awake is recorded.
        let snapshot = provider.record_sample(
            zx::BootDuration::from_millis(0),
            zx::BootDuration::from_millis(0),
            vec![c2_residency(0, 10_000)],
        );
        assert_eq!(
            snapshot,
            vec![CoreC2Residency {
                id: 0,
                total_time: zx::BootDuration::from_millis(10_000),
                entry_count: 1,
                awake_time: zx::MonotonicDuration::from_millis(10_000),
            }]
        );

        // A sample across a 50 s suspend exceeds CLOCK_READ_JITTER_TOLERANCE: accounting is not
        // updated, and the snapshot preserves the previously recorded awake_time rather than
        // misattributing the ambiguous sample.
        let raced = provider.record_sample(
            zx::BootDuration::from_millis(0),
            zx::BootDuration::from_millis(50_000),
            vec![c2_residency(0, 60_000)],
        );
        assert_eq!(
            raced,
            vec![CoreC2Residency {
                id: 0,
                total_time: zx::BootDuration::from_millis(60_000),
                entry_count: 1,
                awake_time: zx::MonotonicDuration::from_millis(10_000),
            }]
        );

        // The next un-raced sample (with 1 ms of clock-read jitter within
        // CLOCK_READ_JITTER_TOLERANCE) attributes the 50 s suspend and records the 500 ms of
        // awake C2 accrued since the initial sample.
        let after = provider.record_sample(
            zx::BootDuration::from_millis(49_999),
            zx::BootDuration::from_millis(50_000),
            vec![c2_residency(0, 60_500)],
        );
        assert_eq!(
            after,
            vec![CoreC2Residency {
                id: 0,
                total_time: zx::BootDuration::from_millis(60_500),
                entry_count: 1,
                awake_time: zx::MonotonicDuration::from_millis(10_500),
            }]
        );
    }

    #[::fuchsia::test]
    fn c1_residency_clamps_to_high_water_mark() {
        let last_time_us = AtomicU64::new(0);
        assert_eq!(
            CpuIdleStatFile::clamp_c1_residency_us(
                &last_time_us,
                zx::MonotonicDuration::from_micros(3_101_000),
            ),
            3_101_000
        );
        // Transient regression from sampling skew is clamped to the previous high-water mark.
        assert_eq!(
            CpuIdleStatFile::clamp_c1_residency_us(
                &last_time_us,
                zx::MonotonicDuration::from_micros(3_100_500),
            ),
            3_101_000
        );
        // Subsequent growth advances the high-water mark.
        assert_eq!(
            CpuIdleStatFile::clamp_c1_residency_us(
                &last_time_us,
                zx::MonotonicDuration::from_micros(3_200_000),
            ),
            3_200_000
        );
    }

    #[::fuchsia::test]
    fn min_freq_votes_manage_domain_controller_leases() {
        let (client, server) = zx::Channel::create();
        let (lease_tx, lease_rx) = std::sync::mpsc::channel();
        let (prev_token_tx, prev_token_rx) = std::sync::mpsc::channel::<zx::EventPair>();
        let server_thread = std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutor::default();
            executor.run_singlethreaded(async move {
                let mut stream = fcpu::DomainControllerRequestStream::from_channel(
                    fuchsia_async::Channel::from_channel(server),
                );
                while let Some(request) =
                    stream.try_next().await.expect("failed to read DomainController request")
                {
                    match request {
                        fcpu::DomainControllerRequest::GetMaxFrequency { domain_id, responder } => {
                            assert_eq!(domain_id, 0);
                            responder.send(Ok(2)).expect("failed to send max frequency");
                        }
                        fcpu::DomainControllerRequest::LeaseMinFrequency {
                            domain_id,
                            frequency_index,
                            lease_name,
                            responder,
                        } => {
                            assert_eq!(domain_id, 0);
                            if let Ok(prev_token) = prev_token_rx.try_recv() {
                                assert!(is_lease_open(&prev_token));
                            }
                            let (client_token, server_token) = zx::EventPair::create();
                            responder.send(Ok(client_token)).expect("failed to send lease token");
                            lease_tx
                                .send((lease_name, frequency_index, server_token))
                                .expect("failed to forward lease token");
                        }
                        other => panic!("unexpected request: {other:?}"),
                    }
                }
            });
        });

        let proxy = fcpu::DomainControllerSynchronousProxy::new(client);
        let domain = CpuFreqDomain::new(
            &fcpu::DomainInfo {
                id: Some(0),
                core_ids: Some(vec![0, 1]),
                available_frequencies_hz: Some(vec![820_000_000, 1_200_000_000, 1_632_000_000]),
                name: Some("cluster0".to_string()),
                ..Default::default()
            },
            Some(&proxy),
        );
        domain.state.lock().domain_controller = Some(proxy);

        // Voting above cpuinfo_min_freq acquires a per-voter LeaseMinFrequency token.
        assert_eq!(
            domain.update_min_vote("powerhint_min_freq", domain.opp_index_for_min_khz(1_632_000)),
            Ok(())
        );
        assert_eq!(domain.min_voter_frequency_khz("powerhint_min_freq"), Some(1_632_000));
        assert_eq!(domain.min_voter_frequency_khz("soft_min_freq"), Some(820_000));
        let (name1, opp1, token1) = lease_rx.recv().expect("expected first lease token");
        assert_eq!(name1, "starnix:powerhint_min_freq");
        assert_eq!(opp1, 2);
        assert!(is_lease_open(&token1));

        // Re-applying the same OPP index while the token is open does not issue a duplicate
        // LeaseMinFrequency call or drop the existing lease.
        assert_eq!(
            domain.update_min_vote("powerhint_min_freq", domain.opp_index_for_min_khz(1_632_000)),
            Ok(())
        );
        assert!(lease_rx.try_recv().is_err());
        assert!(is_lease_open(&token1));

        // A second voter acquires an independent LeaseMinFrequency token while keeping the first alive.
        assert_eq!(
            domain.update_min_vote("soft_min_freq", domain.opp_index_for_min_khz(1_200_000)),
            Ok(())
        );
        assert_eq!(domain.min_voter_frequency_khz("soft_min_freq"), Some(1_200_000));
        let (name2, opp2, token2) = lease_rx.recv().expect("expected second lease token");
        assert_eq!(name2, "starnix:soft_min_freq");
        assert_eq!(opp2, 1);
        assert!(is_lease_open(&token1));
        assert!(is_lease_open(&token2));

        // Updating an existing voter from one active OPP to another acquires the new lease before
        // token1 observes EVENTPAIR_PEER_CLOSED, and then drops token1.
        prev_token_tx
            .send(token1.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate token1"))
            .expect("send previous token");
        assert_eq!(
            domain.update_min_vote("powerhint_min_freq", domain.opp_index_for_min_khz(1_200_000)),
            Ok(())
        );
        assert_eq!(domain.min_voter_frequency_khz("powerhint_min_freq"), Some(1_200_000));
        let (name3, opp3, token3) = lease_rx.recv().expect("expected replacement lease token");
        assert_eq!(name3, "starnix:powerhint_min_freq");
        assert_eq!(opp3, 1);
        assert_eq!(
            token1.wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE),
            zx::WaitResult::Ok(zx::Signals::EVENTPAIR_PEER_CLOSED)
        );
        assert!(is_lease_open(&token3));
        assert!(is_lease_open(&token2));

        // Dropping the server peer token invalidates the cached vote until re-applied, at which
        // point re-applying the same OPP index acquires a new lease.
        drop(token3);
        assert_eq!(domain.get_min_vote("powerhint_min_freq"), None);
        assert_eq!(domain.min_voter_frequency_khz("powerhint_min_freq"), Some(820_000));
        assert_eq!(
            domain.update_min_vote("powerhint_min_freq", domain.opp_index_for_min_khz(1_200_000)),
            Ok(())
        );
        assert_eq!(domain.min_voter_frequency_khz("powerhint_min_freq"), Some(1_200_000));
        let (name4, opp4, token4) = lease_rx.recv().expect("expected re-leased token");
        assert_eq!(name4, "starnix:powerhint_min_freq");
        assert_eq!(opp4, 1);
        assert!(is_lease_open(&token4));

        // Lowering powerhint_min_freq to cpuinfo_min_freq drops only its lease while soft_min_freq stays active.
        assert_eq!(
            domain.update_min_vote("powerhint_min_freq", domain.opp_index_for_min_khz(820_000)),
            Ok(())
        );
        assert_eq!(domain.min_voter_frequency_khz("powerhint_min_freq"), Some(820_000));
        assert_eq!(
            token4.wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE),
            zx::WaitResult::Ok(zx::Signals::EVENTPAIR_PEER_CLOSED)
        );
        assert!(is_lease_open(&token2));

        // Clearing soft_min_freq drops the remaining lease.
        assert_eq!(
            domain.update_min_vote("soft_min_freq", domain.opp_index_for_min_khz(0)),
            Ok(())
        );
        assert_eq!(domain.min_voter_frequency_khz("soft_min_freq"), Some(820_000));
        assert_eq!(
            token2.wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE),
            zx::WaitResult::Ok(zx::Signals::EVENTPAIR_PEER_CLOSED)
        );

        drop(domain);
        server_thread.join().expect("server thread panicked");
    }
}
