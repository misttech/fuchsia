// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error, anyhow, format_err};
use fidl_fuchsia_power_battery as fbattery;
use starnix_core::device::kobject::Device;
use starnix_core::device::service_watcher::ServiceWatcher;
use starnix_core::fs::sysfs::{CpuFreqDomain, build_device_directory, get_cpu_freq_domains};
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use starnix_logging::{log_error, log_warn};
use starnix_sync::{LockDepMutex, ThermalChargeLevelLock};
use starnix_uapi::errors::{Errno, errno};
use starnix_uapi::file_mode::mode;
use std::borrow::Cow;
use std::sync::Arc;

trait CoolingOps: Send + Sync + 'static {
    fn get_max_state(&self) -> u32;
    fn get_state(&self) -> Result<u32, Errno>;
    fn set_state(&self, state: u32) -> Result<(), Errno>;
}

impl<T: CoolingOps> CoolingOps for Arc<T> {
    fn get_max_state(&self) -> u32 {
        self.as_ref().get_max_state()
    }

    fn get_state(&self) -> Result<u32, Errno> {
        self.as_ref().get_state()
    }

    fn set_state(&self, state: u32) -> Result<(), Errno> {
        self.as_ref().set_state(state)
    }
}

struct CoolingDevice<T: CoolingOps> {
    device_id: u32,
    device_type: String,
    ops: T,
}

impl<T: CoolingOps> CoolingDevice<T> {
    fn get_device_name(&self) -> String {
        format!("cooling_device{}", self.device_id)
    }

    fn build_device_dir(self: Arc<Self>, device: &Device, dir: &SimpleDirectoryMutator) {
        build_device_directory(device, dir);
        dir.entry(
            "max_state",
            BytesFile::new_node(format!("{}\n", self.ops.get_max_state()).into_bytes()),
            mode!(IFREG, 0o444),
        );
        dir.entry(
            "type",
            BytesFile::new_node(format!("{}\n", self.device_type).into_bytes()),
            mode!(IFREG, 0o444),
        );
        dir.entry("cur_state", CurStateFile::new_node(self), mode!(IFREG, 0o644));
    }
}

/// Current state file, which proxies integral reads and writes to [`CoolingOps`].
struct CurStateFile<T: CoolingOps> {
    cooling_device: Arc<CoolingDevice<T>>,
}

impl<T: CoolingOps> CurStateFile<T> {
    fn new_node(cooling_device: Arc<CoolingDevice<T>>) -> impl FsNodeOps {
        BytesFile::new_node(Self { cooling_device })
    }
}

impl<T: CoolingOps> BytesFileOps for CurStateFile<T> {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let state = self.cooling_device.ops.get_state()?;
        Ok(format!("{}\n", state).into_bytes().into())
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let input = str::from_utf8(&data).map_err(|_| errno!(EINVAL))?;
        let input_num = input.trim().parse().map_err(|_| errno!(EINVAL))?;
        self.cooling_device.ops.set_state(input_num)
    }
}

/// Registrar for cooling devices, which are sequentially numbered.
struct CoolingDeviceRegistrar {
    next_id: u32,
}

impl CoolingDeviceRegistrar {
    fn new() -> Self {
        Self { next_id: 0 }
    }

    fn get_next_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Register a device in the thermal class.
    fn register<T: CoolingOps>(&mut self, kernel: &Kernel, device_type: String, ops: T) -> Device {
        register_cooling_device(kernel, self.get_next_id(), device_type, ops)
    }
}

fn register_cooling_device<T: CoolingOps>(
    kernel: &Kernel,
    device_id: u32,
    device_type: String,
    ops: T,
) -> Device {
    let device_registry = &kernel.device_registry;
    let device_class = device_registry.objects.thermal_class();

    let cooling_device = Arc::new(CoolingDevice::<T> { device_id, device_type, ops });

    device_registry.add_numberless_device(
        cooling_device.get_device_name().as_str().into(),
        /* parent = */ None,
        device_class,
        |device, dir| cooling_device.build_device_dir(device, dir),
    )
}

const COOLING_DEVICE_VOTER: &str = "cooling_device";

struct CpuCoolingOps {
    domain: Arc<CpuFreqDomain>,
}

impl CpuCoolingOps {
    fn new(domain: Arc<CpuFreqDomain>) -> Self {
        Self { domain }
    }
}

impl CoolingOps for CpuCoolingOps {
    fn get_max_state(&self) -> u32 {
        (self.domain.opp_count() - 1) as u32
    }
    fn get_state(&self) -> Result<u32, Errno> {
        Ok(self.domain.get_vote(COOLING_DEVICE_VOTER).unwrap_or(0) as u32)
    }
    fn set_state(&self, state: u32) -> Result<(), Errno> {
        let opp_index = if state == 0 { None } else { Some(state.into()) };
        self.domain.update_vote(COOLING_DEVICE_VOTER, opp_index)
    }
}

fn register_cpu_domains(
    kernel: &Kernel,
    registrar: &mut CoolingDeviceRegistrar,
) -> Result<(), Error> {
    let domains = get_cpu_freq_domains(kernel);
    if !domains.iter().any(|d| d.is_tunable()) {
        return Err(anyhow!("No tunable CPU domains available from DomainController"));
    }

    // Expose each tunable domain as a separate cooling device.
    for domain in domains.iter().filter(|d| d.is_tunable()) {
        let device_type = domain.name().context("name not provided")?.to_string();
        let ops = CpuCoolingOps::new(domain.clone());
        registrar.register(kernel, device_type, ops);
    }
    Ok(())
}

/// Initializes the cooling devices specified in the device list.
///
/// Device strings are of the form `type[=param]`. Not all device types support a parameter.
///
/// Supported devices:
/// * `fcc=N`: Fast charge current, where N is the maximum charge level.
pub fn cooling_device_init(kernel: &Kernel, devices: Vec<String>) -> Result<(), Error> {
    let mut registrar = CoolingDeviceRegistrar::new();
    for device_spec in devices.into_iter() {
        let (device_type, device_param) = device_spec
            .split_once('=')
            .map_or_else(|| (device_spec.as_str(), None), |(t, p)| (t, Some(p)));
        match device_type {
            "fcc" => {
                // TODO(b/460321934): Return errors rather than logging them.
                if let Err(e) = register_fcc_device(
                    kernel,
                    &mut registrar,
                    device_param
                        .ok_or_else(|| format_err!("Missing parameter for 'fcc' cooling device"))?,
                ) {
                    log_error!("Failed to register 'fcc' cooling device: {e:?}");
                }
            }
            "cpu" => {
                // TODO(b/460321934): Return errors rather than logging them.
                if let Err(e) = register_cpu_domains(kernel, &mut registrar) {
                    log_error!("Failed to register 'cpu' cooling device: {e:?}");
                }
            }
            t => {
                return Err(format_err!("Unknown cooling device: {t:?}"));
            }
        };
    }

    Ok(())
}

struct FccCoolingOps {
    proxy: Arc<fbattery::ChargerSynchronousProxy>,
    max_charge_level: u32,
    charge_level: LockDepMutex<u32, ThermalChargeLevelLock>,
}

impl FccCoolingOps {
    fn new(proxy: Arc<fbattery::ChargerSynchronousProxy>, max_charge_level: u32) -> Self {
        Self { proxy, max_charge_level, charge_level: 0.into() }
    }
}

impl CoolingOps for FccCoolingOps {
    fn get_max_state(&self) -> u32 {
        self.max_charge_level
    }

    fn get_state(&self) -> Result<u32, Errno> {
        let locked_charge_level = self.charge_level.lock();
        Ok(*locked_charge_level)
    }

    fn set_state(&self, state: u32) -> Result<(), Errno> {
        let mut locked_charge_level = self.charge_level.lock();

        // Attempting to set a charge level greater than the maximum results in 0 being set.
        // This is based on observations of how this node behaves on Linux.
        // See b/446016549#comment4 for details.
        let charge_level = if state > self.max_charge_level {
            log_warn!(
                "FCC charge_level of {} exceeds {}; setting to 0",
                state,
                self.max_charge_level
            );
            0
        } else {
            state
        };

        // When the charge level goes to the maximum, disable charging. Otherwise, when dropping
        // below the maximum, enable charging.
        if charge_level == self.max_charge_level {
            self.proxy
                .enable(false, zx::MonotonicInstant::INFINITE)
                .map_err(|e| errno!(EIO, e))?
                .map_err(|e| errno!(EIO, e))?;
        } else if *locked_charge_level == self.max_charge_level {
            self.proxy
                .enable(true, zx::MonotonicInstant::INFINITE)
                .map_err(|e| errno!(EIO, e))?
                .map_err(|e| errno!(EIO, e))?;
        }

        *locked_charge_level = charge_level;
        Ok(())
    }
}

fn register_fcc_device(
    kernel: &Kernel,
    registrar: &mut CoolingDeviceRegistrar,
    param: &str,
) -> Result<(), Error> {
    let max_charge_level: u32 = param.parse().context("Invalid max_charge_level")?;
    let device_id = registrar.get_next_id();
    ServiceWatcher::register_singleton(
        kernel,
        fbattery::ChargerServiceMarker,
        move |current_task, instance, scope| {
            let proxy = match instance.connect_to_device_sync() {
                Ok(proxy) => Arc::new(proxy),
                Err(e) => {
                    log_error!("Failed to connect to battery Charger: {e:?}");
                    return;
                }
            };
            scope.monitor_channel(proxy.clone());
            let ops = FccCoolingOps::new(proxy, max_charge_level);
            let device =
                register_cooling_device(current_task.kernel(), device_id, "fcc".to_string(), ops);
            scope.add_scoped_device(device);
        },
    );
    Ok(())
}
