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
use fidl_fuchsia_power_cpu as fcpu;
use fidl_fuchsia_power_cpu_manager as fcpumanager;
use fuchsia_component::client::connect_to_protocol_sync;
use itertools::Itertools;
use starnix_logging::{bug_ref, log_warn};
use starnix_sync::{CpuC2ProxyCacheLock, CpuFreqProxyCacheLock, LockDepMutex};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::{errno, error, from_status_like_fdio};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
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
    let c2_provider = Arc::new(CpuC2Provider::default());
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
            BytesFile::new_node(CpuIdleStatFile {
                core_id,
                stat_type: CpuIdleStatType::Time,
                c2_provider: state0_c2_provider.clone(),
            }),
            mode!(IFREG, 0o444),
        );
        dir.entry(
            "usage",
            BytesFile::new_node(CpuIdleStatFile {
                core_id,
                stat_type: CpuIdleStatType::Usage,
                c2_provider: state0_c2_provider,
            }),
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
    // Mirror `add_max_freq_entry`: only expose active min-frequency voting when the domain is
    // tunable, falling back to an inert node reporting `lowest_khz()` otherwise.
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
    // max-frequency node (including `scaling_max_freq`) registers its own persistent vote in
    // `CpuFreqDomain`, which enforces the most restrictive ceiling across all active voters.
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
    /// Active ceiling votes keyed by voter name (`voter -> opp_index`), aggregated using Linux
    /// `freq_qos` / `vote_manager` semantics (the lowest maximum frequency wins).
    votes: HashMap<&'static str, u64>,

    /// Active floor votes in kHz keyed by voter name (`voter -> khz` where `khz > lowest_khz`),
    /// aggregated using Linux `freq_qos` / `vote_manager` semantics (the highest minimum
    /// frequency wins).
    min_votes: HashMap<&'static str, u64>,

    /// The effective ceiling (`Some(opp_index)` or `None` when uncapped) last applied to
    /// `DomainController`, or `None` when unknown, either because no vote has been applied yet
    /// or because the last call failed.
    applied_opp: Option<Option<u64>>,

    /// Lazily established connection to the server, dropped if a call fails at the transport
    /// level so that the next caller reconnects.
    domain_controller: Option<fcpu::DomainControllerSynchronousProxy>,

    /// Lazily established connection to `fuchsia.power.cpu.manager.Boost`, dropped if a call
    /// fails at the transport level so that the next caller reconnects.
    boost_proxy: Option<fcpumanager::BoostSynchronousProxy>,

    /// Active boost token held while `min_votes` is non-empty. Dropping this token signals
    /// `ZX_EVENTPAIR_PEER_CLOSED` to `cpu-manager`, releasing this domain's boost vote.
    boost_token: Option<zx::EventPair>,
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
                Some(hz) if Some(hz) == max_hz => {
                    Self { applied_opp: Some(None), ..Self::default() }
                }
                Some(_) => {
                    let mut votes = HashMap::new();
                    votes.insert(SOFT_MAX_FREQ_VOTER, opp_index);
                    Self { votes, applied_opp: Some(Some(opp_index)), ..Self::default() }
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
    /// This is operating point order: `DomainController.SetMaxFrequency` takes an index into
    /// this list. It is deliberately kept apart from `sorted_khz` below, which is what
    /// `scaling_available_frequencies` reports, because sorting reorders the entries. The two
    /// lists are not positionally related, so a frequency must be matched by value to find its
    /// operating point.
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
        self.state.lock().votes.get(voter).copied()
    }

    /// Returns `voter`'s active ceiling in kHz, or the domain's highest frequency when `voter` has
    /// no active cap.
    fn voter_frequency_khz(&self, voter: &'static str) -> Option<u64> {
        self.get_vote(voter)
            .and_then(|index| self.opps_hz.get(index as usize))
            .map(|&hz| hz_to_khz(hz))
            .or_else(|| self.highest_khz())
    }

    /// Whether the domain is backed by `DomainController` and reports operating points, and can
    /// therefore have its maximum frequency controlled.
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

    /// Updates `voter`'s ceiling vote (`Some(opp_index)` or `None` to release) and applies the
    /// most restrictive ceiling across all active voters on this domain.
    pub fn update_vote(&self, voter: &'static str, opp_index: Option<u64>) -> Result<(), Errno> {
        if let Some(index) = opp_index {
            if index as usize >= self.opps_hz.len() {
                return error!(EINVAL);
            }
        }
        let mut state = self.state.lock();
        let previous = match opp_index {
            Some(index) => state.votes.insert(voter, index),
            None => state.votes.remove(voter),
        };
        let result = self.apply_effective_vote(&mut state);
        if result.is_err() {
            match previous {
                Some(prev) => {
                    state.votes.insert(voter, prev);
                }
                None => {
                    state.votes.remove(voter);
                }
            }
        }
        result
    }

    fn apply_effective_vote(&self, state: &mut CpuFreqDomainState) -> Result<(), Errno> {
        let effective_opp = state
            .votes
            .values()
            .copied()
            .min_by_key(|&index| (self.opps_hz[index as usize], std::cmp::Reverse(index)));
        if state.applied_opp == Some(effective_opp) {
            return Ok(());
        }
        state.applied_opp = None;
        match effective_opp {
            Some(index) => self.set_max_frequency(state, index)?,
            None => self.clear_max_frequency(state)?,
        }
        state.applied_opp = Some(effective_opp);
        Ok(())
    }

    /// Runs `call` against a lazily connected `DomainController`.
    ///
    /// A transport failure invalidates the cached connection and `applied_opp`; a protocol-level
    /// error leaves the connection in place for the caller to interpret.
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
                state.applied_opp = None;
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

    fn set_max_frequency(&self, state: &mut CpuFreqDomainState, index: u64) -> Result<(), Errno> {
        self.with_domain_controller(state, |dc| {
            dc.set_max_frequency(self.domain_id, index, zx::MonotonicInstant::INFINITE)
        })?
        .map_err(|e| {
            log_warn!("set_max_frequency failed for domain {}: {:?}", self.domain_id, e);
            errno!(EINVAL)
        })
    }

    fn clear_max_frequency(&self, state: &mut CpuFreqDomainState) -> Result<(), Errno> {
        self.with_domain_controller(state, |dc| {
            dc.clear_max_frequency(self.domain_id, zx::MonotonicInstant::INFINITE)
        })?
        .map_err(|e| {
            log_warn!("clear_max_frequency failed for domain {}: {:?}", self.domain_id, e);
            errno!(EINVAL)
        })
    }

    /// Returns the effective minimum frequency in kHz for `scaling_min_freq`, or `voter`'s active
    /// floor in kHz for `vote_manager/*_min_freq`, falling back to the domain's lowest frequency.
    fn min_frequency_khz(&self, voter: &'static str) -> Option<u64> {
        let state = self.state.lock();
        let active_vote = if voter == "scaling_min_freq" {
            state.min_votes.values().copied().max()
        } else {
            state.min_votes.get(voter).copied()
        };
        active_vote.or_else(|| self.lowest_khz())
    }

    /// Updates `voter`'s floor vote in kHz and acquires or releases a `cpu-manager` `Boost` token
    /// for this domain when the set of active floor votes transitions between empty and non-empty.
    pub fn update_min_vote(
        &self,
        kernel: Option<&Kernel>,
        voter: &'static str,
        khz: u64,
    ) -> Result<(), Errno> {
        let lowest_khz = self.lowest_khz().unwrap_or(0);
        let highest_khz = self.highest_khz().unwrap_or(u64::MAX);
        let mut state = self.state.lock();
        if khz <= lowest_khz {
            state.min_votes.remove(voter);
        } else {
            state.min_votes.insert(voter, khz.min(highest_khz));
        }

        if !state.min_votes.is_empty() && state.boost_token.is_none() {
            if state.boost_proxy.is_none() {
                state.boost_proxy = kernel
                    .and_then(|k| {
                        k.connect_to_protocol_at_container_svc::<fcpumanager::BoostMarker>()
                            .ok()
                            .map(|c| c.into_sync_proxy())
                    })
                    .or_else(|| connect_to_protocol_sync::<fcpumanager::BoostMarker>().ok());
            }
            if let Some(ref proxy) = state.boost_proxy {
                match proxy.boost(zx::MonotonicInstant::INFINITE) {
                    Ok(Ok(token)) => {
                        state.boost_token = Some(token);
                    }
                    Ok(Err(e)) => {
                        log_warn!(
                            "cpu-manager Boost returned error for domain {}: {:?}",
                            self.domain_id,
                            e
                        );
                    }
                    Err(e) => {
                        log_warn!(
                            "cpu-manager Boost FIDL call failed for domain {}: {:?}",
                            self.domain_id,
                            e
                        );
                        state.boost_proxy = None;
                    }
                }
            }
        } else if state.min_votes.is_empty() && state.boost_token.is_some() {
            state.boost_token = None;
        }
        Ok(())
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
/// `fuchsia.power.cpu.manager/Boost`.
///
/// While any voter on a domain requests a floor above `lowest_khz`, the domain holds a
/// ref-counted `Boost` token with `cpu-manager`; when all floor votes are cleared
/// (`<= lowest_khz`), the token is dropped.
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
        Ok(format_freq_khz(self.domain.min_frequency_khz(self.voter)).into_bytes().into())
    }

    fn write(&self, current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let khz = parse_unsigned_file::<u64>(&data)?;
        self.domain.update_min_vote(Some(current_task.kernel()), self.voter, khz)
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

/// Computes C1 residency in microseconds from Zircon's aggregate `idle_time` (in nanoseconds)
/// by subtracting C2 residency (reported in milliseconds) when available.
fn c1_time_us(zircon_idle_ns: i64, c2_residency: Option<&fpowerstats::StateResidency>) -> u64 {
    let total_idle_us = u64::try_from(zircon_idle_ns).unwrap_or(0) / 1000;
    let c2_time_us = c2_residency.map_or(0, |state| state.total_time_ms.saturating_mul(1000));
    total_idle_us.saturating_sub(c2_time_us)
}

/// Backing [`BytesFileOps`] implementation for `/sys/devices/system/cpu/cpuN/cpuidle/state0/time`
/// and `state0/usage`, reporting per-core C1 residency (in microseconds) and reschedule count
/// from Zircon's `PerCpuStats`, subtracting C2 residency when available.
struct CpuIdleStatFile {
    core_id: u64,
    stat_type: CpuIdleStatType,
    c2_provider: Arc<CpuC2Provider>,
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
        let value = match self.stat_type {
            CpuIdleStatType::Time => {
                let idle_ns = per_cpu.idle_time.ok_or_else(|| errno!(EINVAL))?;
                let c2 = self.c2_provider.residency(self.core_id)?;
                c1_time_us(idle_ns, c2.as_ref())
            }
            CpuIdleStatType::Usage => per_cpu.reschedules.ok_or_else(|| errno!(EINVAL))?,
        };
        Ok(format!("{value}\n").into_bytes().into())
    }
}

const POWER_STATS_SERVICE_DIRECTORY: &str = "/svc/fuchsia.hardware.power.stats.Service";
const CPU_C2_ENTITY_NAME: &str = "cpu_c2";

/// Maximum time to wait for a synchronous `fuchsia.hardware.power.stats.Device` call.
const POWER_STATS_FIDL_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);

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
/// the first read:
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
#[derive(Default)]
struct CpuC2Provider {
    state: LockDepMutex<ProviderState, CpuC2ProxyCacheLock>,
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

    fn residency(&self, core_id: u64) -> Result<Option<fpowerstats::StateResidency>, Errno> {
        let core_id = i32::try_from(core_id).map_err(|_| errno!(EINVAL))?;
        let Some(proxy) = self.proxy()? else {
            return Ok(None);
        };
        let deadline = zx::MonotonicInstant::after(POWER_STATS_FIDL_TIMEOUT);
        let reports = match proxy.get_stats(deadline) {
            Ok(reports) => reports,
            Err(e) => {
                log_warn!("Failed to read power stats for {CPU_C2_ENTITY_NAME}: {e}");
                let mut state = self.state.lock();
                if matches!(
                    &*state,
                    ProviderState::Connected(current) if Arc::ptr_eq(current, &proxy)
                ) {
                    *state = ProviderState::Disconnected;
                }
                return error!(EIO);
            }
        };
        Ok(reports
            .into_iter()
            .find(|stats| stats.entity == CPU_C2_ENTITY_NAME)
            .and_then(|stats| stats.state_residency_data.into_iter().find(|s| s.id == core_id)))
    }
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
                CpuIdleStatType::Time => state.total_time_ms.saturating_mul(1000),
                CpuIdleStatType::Usage => u64::from(state.entry_count),
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
        };
        let core1 = provider
            .residency(/* core_id = */ 1)
            .expect("residency call must succeed")
            .expect("core 1 residency must be present");
        assert_eq!(core1.entry_count, 56);
        assert_eq!(core1.total_time_ms, 78);

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

    #[::fuchsia::test]
    fn c1_time_subtracts_c2_residency_and_saturates_at_zero() {
        let c2 = fpowerstats::StateResidency {
            id: 0,
            name: "C2_CPU0".to_string(),
            total_time_ms: 1_899,
            entry_count: 1,
            last_entry_timestamp_ms: 0,
        };

        // 5,000,000 us total idle - 1,899,000 us C2 = 3,101,000 us C1.
        assert_eq!(c1_time_us(/* zircon_idle_ns = */ 5_000_000_000, Some(&c2)), 3_101_000);

        // When C2 residency is unavailable, report aggregate idle time.
        assert_eq!(c1_time_us(/* zircon_idle_ns = */ 5_000_000_000, None), 5_000_000);

        // Saturate at 0 if C2 residency exceeds aggregate idle time due to sampling skew.
        assert_eq!(c1_time_us(/* zircon_idle_ns = */ 1_000_000_000, Some(&c2)), 0);
    }

    #[::fuchsia::test]
    fn min_freq_votes_manage_boost_token_lifecycle() {
        let (client, server) = zx::Channel::create();
        let (token_tx, token_rx) = std::sync::mpsc::channel::<zx::EventPair>();
        let server_thread = std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutor::default();
            executor.run_singlethreaded(async move {
                let mut stream = fcpumanager::BoostRequestStream::from_channel(
                    fuchsia_async::Channel::from_channel(server),
                );
                while let Some(fcpumanager::BoostRequest::Boost { responder }) =
                    stream.try_next().await.expect("failed to read Boost request")
                {
                    let (client_token, server_token) = zx::EventPair::create();
                    responder.send(Ok(client_token)).expect("failed to send Boost response");
                    token_tx.send(server_token).expect("failed to forward server token");
                }
            });
        });

        let domain = CpuFreqDomain::new(
            &fcpu::DomainInfo {
                id: Some(0),
                core_ids: Some(vec![0, 1]),
                available_frequencies_hz: Some(vec![820_000_000, 1_200_000_000, 1_632_000_000]),
                name: Some("little".to_string()),
                ..Default::default()
            },
            None,
        );
        domain.state.lock().boost_proxy = Some(fcpumanager::BoostSynchronousProxy::new(client));

        assert_eq!(domain.min_frequency_khz("powerhint_min_freq"), Some(820_000));
        assert_eq!(domain.min_frequency_khz("soft_min_freq"), Some(820_000));
        assert_eq!(domain.min_frequency_khz("scaling_min_freq"), Some(820_000));

        // 1. Writing khz > lowest_khz to powerhint_min_freq requests a Boost token and updates
        //    powerhint_min_freq and scaling_min_freq reads.
        domain
            .update_min_vote(None, "powerhint_min_freq", 1_632_000)
            .expect("update_min_vote must succeed");
        let server_token = token_rx.recv().expect("must receive a Boost token");
        assert_eq!(
            server_token
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::TimedOut(zx::Signals::NONE)
        );
        assert_eq!(domain.min_frequency_khz("powerhint_min_freq"), Some(1_632_000));
        assert_eq!(domain.min_frequency_khz("soft_min_freq"), Some(820_000));
        assert_eq!(domain.min_frequency_khz("scaling_min_freq"), Some(1_632_000));

        // 2. While another voter (soft_min_freq) also holds a vote > lowest_khz, clearing
        //    powerhint_min_freq (<= lowest_khz) keeps the boost token alive.
        domain
            .update_min_vote(None, "soft_min_freq", 1_200_000)
            .expect("update_min_vote must succeed");
        assert!(token_rx.try_recv().is_err(), "must reuse existing Boost token");
        assert_eq!(domain.min_frequency_khz("soft_min_freq"), Some(1_200_000));
        assert_eq!(domain.min_frequency_khz("scaling_min_freq"), Some(1_632_000));

        domain
            .update_min_vote(None, "powerhint_min_freq", 820_000)
            .expect("clearing powerhint_min_freq must succeed");
        assert_eq!(domain.min_frequency_khz("powerhint_min_freq"), Some(820_000));
        assert_eq!(domain.min_frequency_khz("soft_min_freq"), Some(1_200_000));
        assert_eq!(domain.min_frequency_khz("scaling_min_freq"), Some(1_200_000));
        assert_eq!(
            server_token
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::TimedOut(zx::Signals::NONE)
        );

        // 3. Clearing all min votes (<= lowest_khz or 0) drops the zx::EventPair token
        //    (ZX_EVENTPAIR_PEER_CLOSED observed by the server end) and restores scaling_min_freq
        //    to lowest_khz.
        domain
            .update_min_vote(None, "soft_min_freq", 0)
            .expect("clearing soft_min_freq must succeed");
        assert_eq!(domain.min_frequency_khz("soft_min_freq"), Some(820_000));
        assert_eq!(domain.min_frequency_khz("scaling_min_freq"), Some(820_000));
        let closed_signals = server_token
            .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
            .to_result()
            .expect("server_token must observe ZX_EVENTPAIR_PEER_CLOSED");
        assert!(closed_signals.contains(zx::Signals::EVENTPAIR_PEER_CLOSED));

        drop(domain);
        server_thread.join().expect("server thread panicked");
    }
}
