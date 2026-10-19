// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context as _;
use fidl_fuchsia_gpu_magma as fmagma;
use fidl_fuchsia_power_broker as fpbroker;
use fuchsia_async as fasync;
use fuchsia_component::client::{self as fclient, connect_to_protocol_sync};
use futures::TryStreamExt;
use itertools::Itertools;
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps, parse_unsigned_file};
use starnix_logging::{log_info, log_warn};
use starnix_sync::{LockDepMutex, MagmaPowerDomainLock};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::{errno, error};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

/// Sysfs name of the PowerVR GPU device (`0x34f00000`).
const DEFAULT_GPU_DEVICE_NAME: &str = "34f00000.gpu0";

/// Sysfs name of the GPU runtime PM genpd device (`GPUPowerOn`).
const DEFAULT_GPU_GENPD_DEVICE_NAME: &str = "genpd:0:34f00000.gpu0";

/// Maximum time to wait for synchronous FIDL calls to the Magma driver or Power Broker.
const FIDL_CALL_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);

fn fidl_deadline() -> zx::MonotonicInstant {
    zx::MonotonicInstant::after(FIDL_CALL_TIMEOUT)
}

// TODO(https://fxbug.dev/564595867): Query the operating point frequency table from
// `fuchsia.gpu.magma.PowerElementProvider` once a frequency-table FIDL method is added.
/// 15-state GPU operating point frequency table sorted ascending in Hz (`PF14` .. `PF0`).
const DEFAULT_GPU_AVAILABLE_FREQUENCIES_HZ: &[u64] = &[
    198_000_000,   // PF14
    253_000_000,   // PF13
    300_000_000,   // PF12
    396_000_000,   // PF11 (boot default)
    448_000_000,   // PF10
    512_000_000,   // PF9
    576_000_000,   // PF8  (LAUNCH / EXPENSIVE_RENDERING)
    633_000_000,   // PF7
    691_000_000,   // PF6
    748_000_000,   // PF5
    806_000_000,   // PF4
    864_000_000,   // PF3
    921_000_000,   // PF2
    998_000_000,   // PF1
    1_094_000_000, // PF0
];

const DEFAULT_GPU_FREQ_HZ: u64 = 396_000_000;
const DEFAULT_POLLING_INTERVAL_MS: u64 = 20;

/// Power Broker level when the GPU is powered off (`OFF`).
#[cfg_attr(not(test), allow(dead_code))]
const GPU_POWER_LEVEL_OFF: fpbroker::PowerLevel = 0;

/// Power Broker level when the GPU is powered on and ready for work (`ON_READY_FOR_WORK`).
const GPU_POWER_LEVEL_ON: fpbroker::PowerLevel = 1;

/// Container-provided configuration for GPU sysfs device registration and operating point
/// frequencies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuDeviceConfig {
    /// Linux platform/devfreq sysfs device name (e.g. `"34f00000.gpu0"`).
    pub device_name: String,

    /// Linux runtime PM genpd sysfs device name (e.g. `"genpd:0:34f00000.gpu0"`).
    pub genpd_device_name: String,

    // TODO(https://fxbug.dev/564595867): Query `available_frequencies_hz` from
    // `fuchsia.gpu.magma.PowerElementProvider` once a frequency-table FIDL method is added.
    /// Operating point frequencies sorted ascending in Hz.
    pub available_frequencies_hz: Vec<u64>,

    /// Default / baseline GPU clock frequency in Hz when no floor vote exceeds it.
    pub default_freq_hz: u64,
}

impl Default for GpuDeviceConfig {
    fn default() -> Self {
        Self {
            device_name: DEFAULT_GPU_DEVICE_NAME.to_string(),
            genpd_device_name: DEFAULT_GPU_GENPD_DEVICE_NAME.to_string(),
            available_frequencies_hz: DEFAULT_GPU_AVAILABLE_FREQUENCIES_HZ.to_vec(),
            default_freq_hz: DEFAULT_GPU_FREQ_HZ,
        }
    }
}

impl GpuDeviceConfig {
    fn min_freq_hz(&self) -> u64 {
        self.available_frequencies_hz.first().copied().unwrap_or(self.default_freq_hz)
    }

    fn max_freq_hz(&self) -> u64 {
        self.available_frequencies_hz.last().copied().unwrap_or(self.default_freq_hz)
    }

    /// Snaps a floor vote in Hz up to the smallest GPU operating point `>= hz` (clamped to the
    /// maximum operating point).
    fn snap_min_freq_hz(&self, hz: u64) -> Option<u64> {
        if hz == 0 {
            return None;
        }
        Some(
            self.available_frequencies_hz
                .iter()
                .copied()
                .find(|&opp_hz| opp_hz >= hz)
                .unwrap_or_else(|| self.max_freq_hz()),
        )
    }

    /// Snaps a ceiling vote in Hz down to the highest GPU operating point `<= hz` (or `None` when
    /// `hz == 0` or unconstrained at or above the maximum operating point).
    fn snap_max_freq_hz(&self, hz: u64) -> Option<u64> {
        let max_hz = self.max_freq_hz();
        if hz == 0 || hz >= max_hz {
            return None;
        }
        Some(
            self.available_frequencies_hz
                .iter()
                .rev()
                .copied()
                .find(|&opp_hz| opp_hz <= hz)
                .unwrap_or_else(|| self.min_freq_hz()),
        )
    }
}

/// Threshold separating kHz values (e.g. `396000` / `576000` in kHz-based `powerhint.json`) from
/// Hz values (e.g. `396000000` / `576000000` in Linux `devfreq` and Hz-based `powerhint.json`).
const KHZ_TO_HZ_THRESHOLD: u64 = 10_000_000;

const VOTE_MANAGER_MIN_FREQ_NODES: &[&str] =
    &["powerhint_min_freq", "soft_min_freq", "debug_min_freq"];

const VOTE_MANAGER_MAX_FREQ_NODES: &[&str] =
    &["powerhint_max_freq", "thermal_max_freq", "soft_max_freq", "debug_max_freq"];

fn spawn_power_service_watcher(kernel: &Kernel, domain: Arc<GpuPowerDomain>) {
    kernel.kthreads.spawn_future(
        move || async move {
            let service = match fclient::Service::open(fmagma::PowerServiceMarker) {
                Ok(service) => service,
                Err(err) => {
                    log_info!("GPU PowerService directory unavailable: {err:#}");
                    return;
                }
            };
            if let Err(err) = watch_power_service(domain, service).await {
                log_info!("GPU PowerService watcher exited: {err:#}");
            }
        },
        "gpu_power_service_watcher",
    );
}

/// Watches `service` (`fuchsia.gpu.magma.PowerService`) for provider instances and installs each
/// new `PowerElementProvider` proxy into `domain`, reconciling any active power or frequency votes.
async fn watch_power_service(
    domain: Arc<GpuPowerDomain>,
    service: fclient::Service<fmagma::PowerServiceMarker>,
) -> Result<(), anyhow::Error> {
    let mut instances = service.watch().await.context("failed to watch GPU PowerService")?;
    while let Some(instance) = instances.try_next().await? {
        let name = instance.instance_name().to_string();
        let provider = match instance.connect_to_power_element_provider_sync() {
            Ok(provider) => Arc::new(provider),
            Err(err) => {
                log_warn!("Failed to connect to GPU PowerElementProvider {name}: {err:#}");
                continue;
            }
        };
        let domain_clone = Arc::clone(&domain);
        fasync::unblock(move || {
            domain_clone.set_provider(provider);
        })
        .await;
    }
    Ok(())
}

/// Registers GPU devfreq (`/sys/class/devfreq/<device_name>` and
/// `/sys/devices/platform/<device_name>/devfreq/<device_name>`) and genpd runtime PM
/// (`/sys/devices/<genpd_device_name>/power/control`) sysfs nodes.
pub(crate) fn register_gpu_devices(kernel: &Kernel, config: GpuDeviceConfig) {
    let device_name = config.device_name.clone();
    let genpd_device_name = config.genpd_device_name.clone();
    let domain = Arc::new(GpuPowerDomain::new(config));
    spawn_power_service_watcher(kernel, Arc::clone(&domain));
    let registry = &kernel.device_registry;
    let objects = &registry.objects;

    // 1. `/sys/devices/<genpd_device_name>/power/control` (`GPUPowerOn`)
    let domain_for_genpd = domain.clone();
    registry.add_subsystemless_device(
        genpd_device_name.as_str().into(),
        /* parent = */ None,
        move |device, dir| {
            build_device_directory(device, dir);
            build_gpu_power_directory(dir, &domain_for_genpd);
        },
    );

    // 2. `/sys/devices/platform/<device_name>` + `/sys/class/devfreq/<device_name>` (`GPUMinFreq`)
    let domain_for_platform = domain.clone();
    let gpu_platform_device = registry.add_bus_device(
        device_name.as_str().into(),
        Some(objects.platform_device()),
        objects.platform_bus(),
        move |device, dir| {
            build_device_directory(device, dir);
            build_gpu_power_directory(dir, &domain_for_platform);
        },
    );

    let devfreq_class = objects.get_or_create_class("devfreq".into());
    registry.add_numberless_device(
        device_name.as_str().into(),
        Some(gpu_platform_device),
        devfreq_class,
        move |device, dir| {
            build_device_directory(device, dir);
            build_gpu_devfreq_directory(dir, &domain);
        },
    );
}

fn build_gpu_power_directory(dir: &SimpleDirectoryMutator, domain: &Arc<GpuPowerDomain>) {
    dir.subdir("power", 0o755, |dir| {
        dir.entry("control", GpuPowerControlFile::new_node(domain.clone()), mode!(IFREG, 0o644));
        dir.entry(
            "runtime_status",
            GpuRuntimeStatusFile::new_node(domain.clone()),
            mode!(IFREG, 0o444),
        );
    });
}

fn build_gpu_devfreq_directory(dir: &SimpleDirectoryMutator, domain: &Arc<GpuPowerDomain>) {
    dir.entry(
        "available_frequencies",
        BytesFile::new_node(
            (domain.config.available_frequencies_hz.iter().join(" ") + "\n").into_bytes(),
        ),
        mode!(IFREG, 0o444),
    );
    dir.entry("cur_freq", GpuCurFreqFile::new_node(domain.clone()), mode!(IFREG, 0o444));
    dir.entry(
        "min_freq",
        GpuFreqVoteFile::<false>::new_node(domain.clone(), "min_freq"),
        mode!(IFREG, 0o644),
    );
    dir.entry(
        "max_freq",
        GpuFreqVoteFile::<true>::new_node(domain.clone(), "max_freq"),
        mode!(IFREG, 0o644),
    );
    dir.entry(
        "polling_interval",
        GpuPollingIntervalFile::new_node(domain.clone()),
        mode!(IFREG, 0o644),
    );
    dir.entry("governor", BytesFile::new_node(b"simple_ondemand\n".to_vec()), mode!(IFREG, 0o444));

    dir.subdir("vote_manager", 0o755, |dir| {
        for &voter in VOTE_MANAGER_MIN_FREQ_NODES {
            dir.entry(
                voter,
                GpuFreqVoteFile::<false>::new_node(domain.clone(), voter),
                mode!(IFREG, 0o644),
            );
        }
        for &voter in VOTE_MANAGER_MAX_FREQ_NODES {
            dir.entry(
                voter,
                GpuFreqVoteFile::<true>::new_node(domain.clone(), voter),
                mode!(IFREG, 0o644),
            );
        }
    });
}

/// Normalizes a GPU frequency input written in either Hz (`>= 10_000_000`, e.g. `396000000`) or
/// kHz (`< 10_000_000`, e.g. `396000` / `576000`) to Hz.
fn normalize_gpu_freq_to_hz(raw_value: u64) -> u64 {
    if raw_value == 0 {
        0
    } else if raw_value < KHZ_TO_HZ_THRESHOLD {
        raw_value.saturating_mul(1_000)
    } else {
        raw_value
    }
}

#[derive(Debug)]
struct GpuPowerDomainState {
    /// Whether `GPUPowerOn` (`/sys/devices/genpd:0:34f00000.gpu0/power/control`) is set to `"on"`.
    power_control_on: bool,

    /// Monotonic generation counter for `power_control_on` updates.
    power_generation: u64,

    /// Active floor votes (`voter -> snapped_hz`). Effective floor is `max(min_votes.values())`.
    min_votes: HashMap<&'static str, u64>,

    /// Monotonic generation counter for `min_votes` updates.
    min_freq_generation: u64,

    /// Active ceiling votes (`voter -> snapped_hz`). Effective ceiling is `min(max_votes.values())`.
    max_votes: HashMap<&'static str, u64>,

    /// Monotonic generation counter for `max_votes` updates.
    max_freq_generation: u64,

    /// Configured devfreq polling interval in ms.
    polling_interval_ms: u64,

    /// FIDL proxy to the Magma driver populated by `watch_power_service` (or injected in tests).
    provider: Option<Arc<fmagma::PowerElementProviderSynchronousProxy>>,

    /// Cached `PowerGoalType::OnReadyForWork` `DependencyToken` fetched when `provider` connects.
    power_on_token: Option<zx::Event>,

    /// Active Power Broker lease token keeping `gpu_power_on` at `GPU_POWER_LEVEL_ON`
    /// (`ON_READY_FOR_WORK`).
    active_power_lease: Option<zx::EventPair>,

    /// Active Power Broker lease token holding `gpu_min_freq` at the highest active floor level.
    min_freq_lease: Option<zx::EventPair>,

    /// Last effective min-frequency floor (in Hz) synced to Power Broker.
    applied_min_freq_hz: Option<u64>,

    /// Active `PowerElementProvider::SetClockLimit` handle capping the GPU clock at the lowest
    /// active ceiling vote.
    max_freq_limit_handle: Option<zx::EventPair>,

    /// Last effective max-frequency ceiling (in Hz) synced to `PowerElementProvider::SetClockLimit`.
    applied_max_freq_hz: Option<u64>,
}

impl Default for GpuPowerDomainState {
    fn default() -> Self {
        Self {
            power_control_on: false,
            power_generation: 0,
            min_votes: HashMap::new(),
            min_freq_generation: 0,
            max_votes: HashMap::new(),
            max_freq_generation: 0,
            polling_interval_ms: DEFAULT_POLLING_INTERVAL_MS,
            provider: None,
            power_on_token: None,
            active_power_lease: None,
            min_freq_lease: None,
            applied_min_freq_hz: None,
            max_freq_limit_handle: None,
            applied_max_freq_hz: None,
        }
    }
}

/// Coordinates GPU power state and devfreq frequency votes across Starnix sysfs nodes.
///
/// Aggregates floor (`min_freq`) and ceiling (`max_freq`) votes from `devfreq` and `vote_manager`
/// nodes alongside runtime PM (`power/control`) state, and synchronizes the effective power and
/// clock levels with the Magma driver (`fuchsia.gpu.magma.PowerElementProvider`) and Fuchsia Power
/// Broker (`fuchsia.power.broker.Topology`).
#[derive(Debug, Default)]
struct GpuPowerDomain {
    config: GpuDeviceConfig,
    topology: Option<fpbroker::TopologySynchronousProxy>,
    state: LockDepMutex<GpuPowerDomainState, MagmaPowerDomainLock>,
}

impl GpuPowerDomain {
    fn new(config: GpuDeviceConfig) -> Self {
        Self {
            config,
            topology: connect_to_protocol_sync::<fpbroker::TopologyMarker>().ok(),
            state: LockDepMutex::default(),
        }
    }

    /// Creates a `GpuPowerDomain` wired directly to explicit `PowerElementProvider` and
    /// `Topology` channels (used by unit/integration tests).
    #[cfg(test)]
    fn with_proxies(
        provider: fmagma::PowerElementProviderSynchronousProxy,
        topology: fpbroker::TopologySynchronousProxy,
    ) -> Self {
        let domain = Self::with_topology(topology);
        domain.set_provider(Arc::new(provider));
        domain
    }

    /// Creates a `GpuPowerDomain` wired to an explicit `Topology` channel while waiting for
    /// `watch_power_service` to supply `PowerElementProvider` instances (used by unit tests).
    #[cfg(test)]
    fn with_topology(topology: fpbroker::TopologySynchronousProxy) -> Self {
        Self {
            config: GpuDeviceConfig::default(),
            topology: Some(topology),
            state: LockDepMutex::default(),
        }
    }

    /// Installs a newly discovered `PowerElementProvider` proxy from `Service::watch`, queries its
    /// `OnReadyForWork` power goal token, and reconciles any active power-on, min-frequency, or
    /// max-frequency votes.
    fn set_provider(&self, provider: Arc<fmagma::PowerElementProviderSynchronousProxy>) {
        let power_on_token = match provider.get_power_goals(fidl_deadline()) {
            Ok(goals) => goals
                .into_iter()
                .find(|g| g.type_ == Some(fmagma::PowerGoalType::OnReadyForWork))
                .and_then(|g| g.token),
            Err(err) => {
                log_warn!("PowerElementProvider::GetPowerGoals failed: {:?}", err);
                return;
            }
        };

        let (
            reconcile_power,
            reconcile_min_freq,
            reconcile_max_freq,
            _dropped_power_lease,
            _dropped_min_freq_lease,
            _dropped_max_freq_handle,
        ) = {
            let mut state = self.state.lock();
            state.provider = Some(provider);
            state.power_on_token = power_on_token;
            let power_lease = state.active_power_lease.take();
            let min_freq_lease = state.min_freq_lease.take();
            let max_freq_handle = state.max_freq_limit_handle.take();
            state.applied_min_freq_hz = None;
            state.applied_max_freq_hz = None;
            state.power_generation = state.power_generation.wrapping_add(1);
            state.min_freq_generation = state.min_freq_generation.wrapping_add(1);
            state.max_freq_generation = state.max_freq_generation.wrapping_add(1);
            (
                state.power_control_on,
                !state.min_votes.is_empty(),
                !state.max_votes.is_empty(),
                power_lease,
                min_freq_lease,
                max_freq_handle,
            )
        };
        if reconcile_power {
            let _ = self.sync_power_control();
        }
        if reconcile_min_freq {
            let _ = self.sync_min_freq();
        }
        if reconcile_max_freq {
            let _ = self.sync_max_freq();
        }
    }

    fn acquire_lease(
        &self,
        token: zx::Event,
        level: fpbroker::PowerLevel,
        lease_name: &str,
    ) -> Result<zx::EventPair, ()> {
        let Some(topology) = self.topology.as_ref() else {
            return Err(());
        };
        let (client_token, broker_token) = zx::EventPair::create();
        let schema = fpbroker::LeaseSchema {
            lease_token: Some(broker_token),
            lease_name: Some(lease_name.to_string()),
            dependencies: Some(vec![fpbroker::LeaseDependency {
                requires_token: Some(token),
                requires_level: Some(level),
                ..Default::default()
            }]),
            should_return_pending_lease: Some(false),
            ..Default::default()
        };
        match topology.lease(schema, fidl_deadline()) {
            Ok(Ok(())) => Ok(client_token),
            Ok(Err(err)) => {
                log_warn!("Power Broker rejected GPU lease {}: {:?}", lease_name, err);
                Err(())
            }
            Err(err) => {
                log_warn!("Transport error leasing {}: {:?}", lease_name, err);
                Err(())
            }
        }
    }

    fn set_power_control(&self, on: bool) -> Result<(), Errno> {
        let _dropped_lease = {
            let mut state = self.state.lock();
            state.power_control_on = on;
            state.power_generation = state.power_generation.wrapping_add(1);
            if !on { state.active_power_lease.take() } else { None }
        };
        if on {
            self.sync_power_control()?;
        }
        Ok(())
    }

    fn sync_power_control(&self) -> Result<(), Errno> {
        let (generation, token) = {
            let state = self.state.lock();
            if !state.power_control_on || state.active_power_lease.is_some() {
                return Ok(());
            }
            if state.provider.is_none() || self.topology.is_none() {
                return Ok(());
            }
            let Some(ref goal_token) = state.power_on_token else {
                return Ok(());
            };
            let Ok(dup_token) = goal_token.duplicate_handle(zx::Rights::SAME_RIGHTS) else {
                return error!(EIO);
            };
            (state.power_generation, dup_token)
        };

        let lease_result = self.acquire_lease(token, GPU_POWER_LEVEL_ON, "starnix-gpu-power-on");
        let mut state = self.state.lock();
        if state.power_generation != generation || !state.power_control_on {
            drop(state);
            return lease_result.map(|_| ()).map_err(|()| errno!(EIO));
        }
        if let Ok(lease) = lease_result {
            let prev = state.active_power_lease.replace(lease);
            drop(state);
            drop(prev);
            return Ok(());
        }
        drop(state);
        error!(EIO)
    }

    fn power_control_is_on(&self) -> bool {
        self.state.lock().power_control_on
    }

    fn update_min_freq_vote(&self, voter: &'static str, raw_value: u64) -> Result<(), Errno> {
        let hz = normalize_gpu_freq_to_hz(raw_value);
        let snapped = self.config.snap_min_freq_hz(hz);

        {
            let mut state = self.state.lock();
            match snapped {
                Some(freq_hz) => {
                    state.min_votes.insert(voter, freq_hz);
                }
                None => {
                    state.min_votes.remove(voter);
                }
            }
            state.min_freq_generation = state.min_freq_generation.wrapping_add(1);
        }
        self.sync_min_freq()
    }

    fn sync_min_freq(&self) -> Result<(), Errno> {
        let (generation, target_hz, provider) = {
            let mut state = self.state.lock();
            let effective_min_hz = state.min_votes.values().copied().max();
            if state.applied_min_freq_hz == effective_min_hz {
                return Ok(());
            }

            let Some(target_hz) = effective_min_hz else {
                let dropped = state.min_freq_lease.take();
                state.applied_min_freq_hz = None;
                drop(state);
                drop(dropped);
                return Ok(());
            };
            let (Some(provider), true) =
                (state.provider.as_ref().cloned(), self.topology.is_some())
            else {
                let dropped = state.min_freq_lease.take();
                state.applied_min_freq_hz = None;
                drop(state);
                drop(dropped);
                return Ok(());
            };
            (state.min_freq_generation, target_hz, provider)
        };

        let req = fmagma::PowerElementProviderGetClockSpeedLevelRequest {
            hz: Some(target_hz),
            allow_max: Some(true),
            ..Default::default()
        };
        // Acquire the new lease outside `self.state.lock()` BEFORE replacing
        // `state.min_freq_lease` so Power Broker transitions directly without a transient
        // `GPU_POWER_LEVEL_OFF` dip and without holding the state lock across FIDL calls.
        let mut reset_provider = false;
        let mut not_supported = false;
        let mut sync_result = Ok(());
        let new_lease = match provider.get_clock_speed_level(&req, fidl_deadline()) {
            Ok(Ok(resp)) => {
                if let (Some(token), Some(level)) = (resp.token, resp.level) {
                    match self.acquire_lease(token, level, "starnix-gpu-min-freq") {
                        Ok(lease) => Some(lease),
                        Err(()) => {
                            sync_result = error!(EIO);
                            None
                        }
                    }
                } else {
                    sync_result = error!(EIO);
                    None
                }
            }
            Ok(Err(status)) => {
                let zx_status = zx::Status::err_from_raw(status);
                log_warn!("GetClockSpeedLevel({}) returned status {}", target_hz, zx_status);
                if zx_status == zx::Status::NOT_SUPPORTED {
                    not_supported = true;
                } else {
                    sync_result = error!(EIO);
                }
                None
            }
            Err(err) => {
                log_warn!("GetClockSpeedLevel transport error: {:?}", err);
                reset_provider = true;
                sync_result = error!(EIO);
                None
            }
        };

        let _dropped_lease = {
            let mut state = self.state.lock();
            if reset_provider && state.provider.as_ref().is_some_and(|p| Arc::ptr_eq(p, &provider))
            {
                state.power_on_token = None;
                state.provider = None;
            }
            if state.min_freq_generation != generation {
                new_lease
            } else if let Some(lease) = new_lease {
                state.applied_min_freq_hz = Some(target_hz);
                state.min_freq_lease.replace(lease)
            } else if not_supported {
                state.applied_min_freq_hz = Some(target_hz);
                state.min_freq_lease.take()
            } else if state.applied_min_freq_hz != Some(target_hz) {
                state.applied_min_freq_hz = None;
                state.min_freq_lease.take()
            } else {
                None
            }
        };
        sync_result
    }

    fn min_voter_freq_hz(&self, voter: &'static str) -> u64 {
        let state = self.state.lock();
        if voter == "min_freq" {
            state.min_votes.values().copied().max().unwrap_or(0)
        } else {
            state.min_votes.get(voter).copied().unwrap_or(0)
        }
    }

    fn update_max_freq_vote(&self, voter: &'static str, raw_value: u64) -> Result<(), Errno> {
        let hz = normalize_gpu_freq_to_hz(raw_value);
        let snapped = self.config.snap_max_freq_hz(hz);
        {
            let mut state = self.state.lock();
            match snapped {
                Some(freq_hz) => {
                    state.max_votes.insert(voter, freq_hz);
                }
                None => {
                    state.max_votes.remove(voter);
                }
            }
            state.max_freq_generation = state.max_freq_generation.wrapping_add(1);
        }
        self.sync_max_freq()
    }

    fn sync_max_freq(&self) -> Result<(), Errno> {
        let (generation, target_hz, provider) = {
            let mut state = self.state.lock();
            let effective_max_hz = state.max_votes.values().copied().min();
            if state.applied_max_freq_hz == effective_max_hz {
                return Ok(());
            }

            let Some(target_hz) = effective_max_hz else {
                let dropped = state.max_freq_limit_handle.take();
                state.applied_max_freq_hz = None;
                drop(state);
                drop(dropped);
                return Ok(());
            };
            let Some(provider) = state.provider.as_ref().cloned() else {
                let dropped = state.max_freq_limit_handle.take();
                state.applied_max_freq_hz = None;
                drop(state);
                drop(dropped);
                return Ok(());
            };
            (state.max_freq_generation, target_hz, provider)
        };

        let req = fmagma::PowerElementProviderSetClockLimitRequest {
            hz: Some(target_hz),
            ..Default::default()
        };
        let mut reset_provider = false;
        let mut not_supported = false;
        let mut sync_result = Ok(());
        let new_handle = match provider.set_clock_limit(&req, fidl_deadline()) {
            Ok(Ok(resp)) => {
                if let Some(handle) = resp.handle {
                    Some(handle)
                } else {
                    sync_result = error!(EIO);
                    None
                }
            }
            Ok(Err(status)) => {
                let zx_status = zx::Status::err_from_raw(status);
                log_warn!("SetClockLimit({}) returned status {}", target_hz, zx_status);
                if zx_status == zx::Status::NOT_SUPPORTED {
                    not_supported = true;
                } else {
                    sync_result = error!(EIO);
                }
                None
            }
            Err(err) => {
                log_warn!("SetClockLimit transport error: {:?}", err);
                reset_provider = true;
                sync_result = error!(EIO);
                None
            }
        };

        let _dropped_handle = {
            let mut state = self.state.lock();
            if reset_provider && state.provider.as_ref().is_some_and(|p| Arc::ptr_eq(p, &provider))
            {
                state.power_on_token = None;
                state.provider = None;
            }
            if state.max_freq_generation != generation {
                new_handle
            } else if let Some(handle) = new_handle {
                state.applied_max_freq_hz = Some(target_hz);
                state.max_freq_limit_handle.replace(handle)
            } else if not_supported {
                state.applied_max_freq_hz = Some(target_hz);
                state.max_freq_limit_handle.take()
            } else if state.applied_max_freq_hz != Some(target_hz) {
                state.applied_max_freq_hz = None;
                state.max_freq_limit_handle.take()
            } else {
                None
            }
        };
        sync_result
    }

    fn max_voter_freq_hz(&self, voter: &'static str) -> u64 {
        let max_hz = self.config.max_freq_hz();
        let state = self.state.lock();
        if voter == "max_freq" {
            state.max_votes.values().copied().min().unwrap_or(max_hz)
        } else {
            state.max_votes.get(voter).copied().unwrap_or(max_hz)
        }
    }

    fn cur_freq_hz(&self) -> u64 {
        let default_hz = self.config.default_freq_hz;
        let max_hz = self.config.max_freq_hz();
        let state = self.state.lock();
        let floor = state.min_votes.values().copied().max().unwrap_or(default_hz);
        let ceil = state.max_votes.values().copied().min().unwrap_or(max_hz);
        floor.max(default_hz).min(ceil)
    }

    fn set_polling_interval_ms(&self, ms: u64) {
        self.state.lock().polling_interval_ms = ms;
    }

    fn polling_interval_ms(&self) -> u64 {
        self.state.lock().polling_interval_ms
    }
}

struct GpuPowerControlFile {
    domain: Arc<GpuPowerDomain>,
}

impl GpuPowerControlFile {
    fn new_node(domain: Arc<GpuPowerDomain>) -> impl FsNodeOps {
        BytesFile::new_node(Self { domain })
    }
}

impl BytesFileOps for GpuPowerControlFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let s: &[u8] = if self.domain.power_control_is_on() { b"on\n" } else { b"auto\n" };
        Ok(s.into())
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let s = std::str::from_utf8(&data).map_err(|_| errno!(EINVAL))?.trim();
        match s {
            "on" => self.domain.set_power_control(true),
            "auto" => self.domain.set_power_control(false),
            _ => error!(EINVAL),
        }
    }
}

struct GpuRuntimeStatusFile {
    domain: Arc<GpuPowerDomain>,
}

impl GpuRuntimeStatusFile {
    fn new_node(domain: Arc<GpuPowerDomain>) -> impl FsNodeOps {
        BytesFile::new_node(Self { domain })
    }
}

impl BytesFileOps for GpuRuntimeStatusFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let s: &[u8] = if self.domain.power_control_is_on() { b"active\n" } else { b"suspended\n" };
        Ok(s.into())
    }
}

struct GpuFreqVoteFile<const IS_MAX: bool> {
    domain: Arc<GpuPowerDomain>,
    voter: &'static str,
}

impl<const IS_MAX: bool> GpuFreqVoteFile<IS_MAX> {
    fn new_node(domain: Arc<GpuPowerDomain>, voter: &'static str) -> impl FsNodeOps {
        BytesFile::new_node(Self { domain, voter })
    }
}

impl<const IS_MAX: bool> BytesFileOps for GpuFreqVoteFile<IS_MAX> {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let freq_hz = if IS_MAX {
            self.domain.max_voter_freq_hz(self.voter)
        } else {
            self.domain.min_voter_freq_hz(self.voter)
        };
        Ok(format!("{freq_hz}\n").into_bytes().into())
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let val = parse_unsigned_file::<u64>(&data)?;
        if IS_MAX {
            self.domain.update_max_freq_vote(self.voter, val)
        } else {
            self.domain.update_min_freq_vote(self.voter, val)
        }
    }
}

struct GpuCurFreqFile {
    domain: Arc<GpuPowerDomain>,
}

impl GpuCurFreqFile {
    fn new_node(domain: Arc<GpuPowerDomain>) -> impl FsNodeOps {
        BytesFile::new_node(Self { domain })
    }
}

impl BytesFileOps for GpuCurFreqFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        Ok(format!("{}\n", self.domain.cur_freq_hz()).into_bytes().into())
    }
}

struct GpuPollingIntervalFile {
    domain: Arc<GpuPowerDomain>,
}

impl GpuPollingIntervalFile {
    fn new_node(domain: Arc<GpuPowerDomain>) -> impl FsNodeOps {
        BytesFile::new_node(Self { domain })
    }
}

impl BytesFileOps for GpuPollingIntervalFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        Ok(format!("{}\n", self.domain.polling_interval_ms()).into_bytes().into())
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let ms = parse_unsigned_file::<u64>(&data)?;
        self.domain.set_polling_interval_ms(ms);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_endpoints;
    use fidl_fuchsia_io as fio;
    use fuchsia_component::server::ServiceFs;
    use futures::StreamExt;
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[::fuchsia::test]
    async fn sysfs_registers_gpu_devfreq_and_genpd_nodes() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_gpu_devices(kernel, GpuDeviceConfig::default());
            let root = &kernel.device_registry.objects.root;

            assert!(root.lookup("devices/genpd:0:34f00000.gpu0/power/control".into()).is_some());
            assert!(
                root.lookup("devices/genpd:0:34f00000.gpu0/power/runtime_status".into()).is_some()
            );
            assert!(root.lookup("class/devfreq/34f00000.gpu0".into()).is_some());
            assert!(root.lookup("bus/platform/devices/34f00000.gpu0".into()).is_some());
            assert!(root.lookup("devices/platform/34f00000.gpu0/power/control".into()).is_some());
            assert!(
                root.lookup(
                    "devices/platform/34f00000.gpu0/devfreq/34f00000.gpu0/vote_manager/powerhint_min_freq"
                        .into()
                )
                .is_some()
            );
            assert!(
                root.lookup("devices/platform/34f00000.gpu0/devfreq/34f00000.gpu0/min_freq".into())
                    .is_some()
            );
            assert!(
                root.lookup("devices/platform/34f00000.gpu0/devfreq/34f00000.gpu0/max_freq".into())
                    .is_some()
            );
            assert!(
                root.lookup("devices/platform/34f00000.gpu0/devfreq/34f00000.gpu0/cur_freq".into())
                    .is_some()
            );
            assert!(
                root.lookup(
                    "devices/platform/34f00000.gpu0/devfreq/34f00000.gpu0/available_frequencies"
                        .into()
                )
                .is_some()
            );
            assert!(
                root.lookup(
                    "devices/platform/34f00000.gpu0/devfreq/34f00000.gpu0/polling_interval".into()
                )
                .is_some()
            );
        })
        .await;
    }

    struct MockPowerBackend {
        power_token_koid: zx::Koid,
        clock_token_koid: zx::Koid,
        power_token: zx::Event,
        clock_token: zx::Event,
        leased_power_koid: Arc<AtomicU64>,
        leased_clock_koid: Arc<AtomicU64>,
        leased_clock_level: Arc<AtomicU64>,
        capped_clock_hz: Arc<AtomicU64>,
        clock_speed_calls: Arc<AtomicU64>,
        active_broker_tokens: Arc<Mutex<Vec<zx::EventPair>>>,
        active_limit_tokens: Arc<Mutex<Vec<zx::EventPair>>>,
    }

    impl MockPowerBackend {
        fn new() -> Self {
            let power_token = zx::Event::create();
            let power_token_koid = power_token.koid().expect("koid");
            let clock_token = zx::Event::create();
            let clock_token_koid = clock_token.koid().expect("koid");
            Self {
                power_token_koid,
                clock_token_koid,
                power_token,
                clock_token,
                leased_power_koid: Arc::new(AtomicU64::new(0)),
                leased_clock_koid: Arc::new(AtomicU64::new(0)),
                leased_clock_level: Arc::new(AtomicU64::new(0)),
                capped_clock_hz: Arc::new(AtomicU64::new(0)),
                clock_speed_calls: Arc::new(AtomicU64::new(0)),
                active_broker_tokens: Arc::new(Mutex::new(Vec::new())),
                active_limit_tokens: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn spawn_provider(
            &self,
            scope: &fasync::Scope,
            mut stream: fmagma::PowerElementProviderRequestStream,
            reject_576mhz_not_supported: bool,
        ) {
            let power_token =
                self.power_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup");
            let clock_token =
                self.clock_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup");
            let capped_clock_recorder = self.capped_clock_hz.clone();
            let clock_speed_calls = self.clock_speed_calls.clone();
            let limit_tokens_holder = self.active_limit_tokens.clone();
            scope.spawn(async move {
                while let Some(Ok(req)) = stream.next().await {
                    match req {
                        fmagma::PowerElementProviderRequest::GetPowerGoals { responder } => {
                            let dup =
                                power_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup");
                            let _ = responder.send(vec![fmagma::PowerGoal {
                                type_: Some(fmagma::PowerGoalType::OnReadyForWork),
                                token: Some(dup),
                                ..Default::default()
                            }]);
                        }
                        fmagma::PowerElementProviderRequest::GetClockSpeedLevel {
                            payload,
                            responder,
                        } => {
                            clock_speed_calls.fetch_add(1, Ordering::SeqCst);
                            if reject_576mhz_not_supported && payload.hz == Some(576_000_000) {
                                let _ = responder.send(Err(zx::Status::NOT_SUPPORTED.into_raw()));
                            } else {
                                let dup = clock_token
                                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                    .expect("dup");
                                let level = if payload.hz == Some(576_000_000) { 6u8 } else { 3u8 };
                                let _ = responder.send(Ok(
                                    fmagma::PowerElementProviderGetClockSpeedLevelResponse {
                                        token: Some(dup),
                                        level: Some(level),
                                        actual_hz: payload.hz,
                                        ..Default::default()
                                    },
                                ));
                            }
                        }
                        fmagma::PowerElementProviderRequest::SetClockLimit {
                            payload,
                            responder,
                        } => {
                            if reject_576mhz_not_supported && payload.hz == Some(576_000_000) {
                                let _ = responder.send(Err(zx::Status::NOT_SUPPORTED.into_raw()));
                            } else if reject_576mhz_not_supported && payload.hz == Some(396_000_000)
                            {
                                let _ = responder.send(Err(zx::Status::INTERNAL.into_raw()));
                            } else {
                                let (client_ep, server_ep) = zx::EventPair::create();
                                capped_clock_recorder
                                    .store(payload.hz.unwrap_or(0), Ordering::SeqCst);
                                limit_tokens_holder.lock().push(server_ep);
                                let _ = responder.send(Ok(
                                    fmagma::PowerElementProviderSetClockLimitResponse {
                                        handle: Some(client_ep),
                                        actual_hz: payload.hz,
                                        ..Default::default()
                                    },
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            });
        }

        fn spawn_topology(
            &self,
            scope: &fasync::Scope,
            topology_server: fidl::endpoints::ServerEnd<fpbroker::TopologyMarker>,
            reject_leases: bool,
        ) {
            let power_token_raw = self.power_token_koid.raw_koid();
            let power_koid_recorder = self.leased_power_koid.clone();
            let clock_koid_recorder = self.leased_clock_koid.clone();
            let clock_level_recorder = self.leased_clock_level.clone();
            let tokens_holder = self.active_broker_tokens.clone();
            scope.spawn(async move {
                let mut stream = topology_server.into_stream();
                while let Some(Ok(req)) = stream.next().await {
                    if let fpbroker::TopologyRequest::Lease { payload, responder } = req {
                        if reject_leases {
                            let _ = responder.send(Err(fpbroker::LeaseError::Internal));
                            continue;
                        }
                        if let Some(deps) = payload.dependencies {
                            for dep in deps {
                                let koid = dep
                                    .requires_token
                                    .as_ref()
                                    .and_then(|t| t.koid().ok())
                                    .map(|k| k.raw_koid())
                                    .unwrap_or(0);
                                if koid == power_token_raw {
                                    power_koid_recorder.store(koid, Ordering::SeqCst);
                                } else {
                                    clock_koid_recorder.store(koid, Ordering::SeqCst);
                                    clock_level_recorder.store(
                                        dep.requires_level.unwrap_or(GPU_POWER_LEVEL_OFF) as u64,
                                        Ordering::SeqCst,
                                    );
                                }
                            }
                        }
                        if let Some(lt) = payload.lease_token {
                            tokens_holder.lock().push(lt);
                        }
                        let _ = responder.send(Ok(()));
                    }
                }
            });
        }
    }

    #[::fuchsia::test]
    async fn gpu_power_domain_leases_power_broker_on_hints() {
        let (provider_client, provider_server) =
            create_endpoints::<fmagma::PowerElementProviderMarker>();
        let (topology_client, topology_server) = create_endpoints::<fpbroker::TopologyMarker>();

        let backend = MockPowerBackend::new();
        let scope = fasync::Scope::new();
        backend.spawn_provider(&scope, provider_server.into_stream(), false);
        backend.spawn_topology(&scope, topology_server, false);

        let domain = fasync::unblock(move || {
            let domain = GpuPowerDomain::with_proxies(
                fmagma::PowerElementProviderSynchronousProxy::new(provider_client.into_channel()),
                fpbroker::TopologySynchronousProxy::new(topology_client.into_channel()),
            );

            // 1. Acquire `GPUPowerOn` ("on") and `GPUMinFreq` ("576000" kHz -> 576 MHz).
            domain.set_power_control(true).expect("set power control on");
            assert!(domain.power_control_is_on());
            domain.update_min_freq_vote("powerhint_min_freq", 576_000).expect("vote 576000 kHz");
            assert_eq!(domain.min_voter_freq_hz("min_freq"), 576_000_000);
            assert_eq!(domain.cur_freq_hz(), 576_000_000);

            // 2. Release votes ("auto" and "0").
            domain.update_min_freq_vote("powerhint_min_freq", 0).expect("clear min_freq");
            assert_eq!(domain.min_voter_freq_hz("min_freq"), 0);
            assert_eq!(domain.cur_freq_hz(), 396_000_000);
            domain.update_max_freq_vote("powerhint_max_freq", 300_000).expect("cap max_freq");
            assert_eq!(domain.max_voter_freq_hz("max_freq"), 300_000_000);
            assert_eq!(domain.cur_freq_hz(), 300_000_000);
            domain.update_max_freq_vote("powerhint_max_freq", 0).expect("clear max_freq with 0");
            assert_eq!(domain.max_voter_freq_hz("max_freq"), 1_094_000_000);
            assert_eq!(domain.cur_freq_hz(), 396_000_000);
            domain.set_power_control(false).expect("set power control auto");
            assert!(!domain.power_control_is_on());
            domain
        })
        .await;

        assert_eq!(
            backend.leased_power_koid.load(Ordering::SeqCst),
            backend.power_token_koid.raw_koid()
        );
        assert_eq!(
            backend.leased_clock_koid.load(Ordering::SeqCst),
            backend.clock_token_koid.raw_koid()
        );
        assert_eq!(backend.leased_clock_level.load(Ordering::SeqCst), 6);
        assert_eq!(backend.capped_clock_hz.load(Ordering::SeqCst), 300_000_000);

        // Verify both server-side lease tokens received `EVENTPAIR_PEER_CLOSED` on release!
        let broker_tokens = backend.active_broker_tokens.lock();
        assert_eq!(broker_tokens.len(), 2);
        for bt in broker_tokens.iter() {
            let signals = bt
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST);
            assert!(signals.is_ok());
        }

        // Verify the SetClockLimit handle also received `EVENTPAIR_PEER_CLOSED` when cleared!
        let limit_tokens = backend.active_limit_tokens.lock();
        assert_eq!(limit_tokens.len(), 1);
        for lt in limit_tokens.iter() {
            let signals = lt
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST);
            assert!(signals.is_ok());
        }
        drop(domain);
    }

    #[::fuchsia::test]
    async fn watch_power_service_reconciles_active_votes_on_provider_arrival() {
        let (topology_client, topology_server) = create_endpoints::<fpbroker::TopologyMarker>();
        let backend = Arc::new(MockPowerBackend::new());
        let scope = fasync::Scope::new();
        backend.spawn_topology(&scope, topology_server, false);

        let (dir_client, dir_server) = create_endpoints::<fio::DirectoryMarker>();
        let mut fs = ServiceFs::new();
        fs.add_fidl_service_instance("default", |req: fmagma::PowerServiceRequest| req);
        fs.serve_connection(dir_server).expect("serve PowerService directory");
        let backend_for_fs = Arc::clone(&backend);
        scope.spawn(async move {
            let inner_scope = fasync::Scope::new();
            while let Some(fmagma::PowerServiceRequest::PowerElementProvider(stream)) =
                fs.next().await
            {
                backend_for_fs.spawn_provider(&inner_scope, stream, false);
            }
        });

        let domain = Arc::new(GpuPowerDomain::with_topology(
            fpbroker::TopologySynchronousProxy::new(topology_client.into_channel()),
        ));

        // Record power and min-frequency votes BEFORE any `PowerElementProvider` is connected.
        domain.set_power_control(true).expect("power control before provider");
        domain.update_min_freq_vote("powerhint_min_freq", 576_000).expect("vote before provider");
        assert_eq!(backend.leased_power_koid.load(Ordering::SeqCst), 0);
        assert_eq!(backend.leased_clock_koid.load(Ordering::SeqCst), 0);

        // Now start `watch_power_service` against the ServiceFs directory and verify both active
        // votes are automatically reconciled as soon as the instance is discovered.
        let service =
            fclient::Service::open_from_dir(dir_client, fmagma::PowerServiceMarker).expect("open");
        let watcher_domain = Arc::clone(&domain);
        scope.spawn(async move {
            let _ = watch_power_service(watcher_domain, service).await;
        });

        while backend.leased_power_koid.load(Ordering::SeqCst) == 0
            || backend.leased_clock_koid.load(Ordering::SeqCst) == 0
        {
            fasync::Timer::new(zx::MonotonicDuration::from_millis(10)).await;
        }

        assert_eq!(
            backend.leased_power_koid.load(Ordering::SeqCst),
            backend.power_token_koid.raw_koid()
        );
        assert_eq!(
            backend.leased_clock_koid.load(Ordering::SeqCst),
            backend.clock_token_koid.raw_koid()
        );
        assert_eq!(backend.leased_clock_level.load(Ordering::SeqCst), 6);
    }

    #[::fuchsia::test]
    async fn gpu_power_domain_returns_eio_on_lease_error_and_allows_not_supported() {
        let (provider_client, provider_server) =
            create_endpoints::<fmagma::PowerElementProviderMarker>();
        let (topology_client, topology_server) = create_endpoints::<fpbroker::TopologyMarker>();

        let backend = MockPowerBackend::new();
        let scope = fasync::Scope::new();
        backend.spawn_provider(&scope, provider_server.into_stream(), true);
        backend.spawn_topology(&scope, topology_server, true);

        fasync::unblock(move || {
            let domain = GpuPowerDomain::with_proxies(
                fmagma::PowerElementProviderSynchronousProxy::new(provider_client.into_channel()),
                fpbroker::TopologySynchronousProxy::new(topology_client.into_channel()),
            );

            // 1. Topology::Lease rejection on `GPUPowerOn` returns EIO.
            assert_eq!(domain.set_power_control(true), error!(EIO));

            // 2. `ZX_ERR_NOT_SUPPORTED` from `GetClockSpeedLevel` is gracefully ignored (Ok(()))
            // and cached in `applied_min_freq_hz` so redundant votes at the same target Hz do not
            // make additional FIDL calls.
            assert_eq!(domain.update_min_freq_vote("powerhint_min_freq", 576_000), Ok(()));
            assert_eq!(domain.update_min_freq_vote("soft_min_freq", 576_000), Ok(()));
            assert_eq!(domain.update_min_freq_vote("soft_min_freq", 0), Ok(()));

            // 3. Topology::Lease rejection on `GetClockSpeedLevel` returns EIO.
            assert_eq!(domain.update_min_freq_vote("powerhint_min_freq", 396_000), error!(EIO));

            // 4. `ZX_ERR_NOT_SUPPORTED` from `SetClockLimit` is gracefully ignored (Ok(())), while
            // non-`NOT_SUPPORTED` errors propagate EIO.
            assert_eq!(domain.update_max_freq_vote("powerhint_max_freq", 576_000), Ok(()));
            assert_eq!(domain.update_max_freq_vote("powerhint_max_freq", 396_000), error!(EIO));
        })
        .await;
        // 576_000_000 was queried once (cached across the two redundant votes) and 396_000_000
        // was queried once.
        assert_eq!(backend.clock_speed_calls.load(Ordering::SeqCst), 2);
    }
}
