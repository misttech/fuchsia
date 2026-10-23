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
use starnix_sync::{LockDepMutex, Mutex, ThermalChargeLevelLock};
use starnix_uapi::errors::{Errno, errno};
use starnix_uapi::file_mode::mode;
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

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
        let device_registry = &kernel.device_registry;
        let device_class = device_registry.objects.thermal_class();

        let cooling_device =
            Arc::new(CoolingDevice::<T> { device_id: self.get_next_id(), device_type, ops });

        device_registry.add_numberless_device(
            cooling_device.get_device_name().as_str().into(),
            /* parent = */ None,
            device_class,
            |device, dir| cooling_device.build_device_dir(device, dir),
        )
    }
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

const CHARGER_SYNC_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);

struct FccCoolingOps {
    proxy: Mutex<Option<Arc<fbattery::ChargerSynchronousProxy>>>,
    proxy_ready: zx::Event,
    timeout: zx::MonotonicDuration,
    max_charge_level: u32,
    charge_level: AtomicU32,
    /// Serializes `set_state` transitions across the `enable()` FIDL call and `charge_level`
    /// update so concurrent writers cannot interleave charger state changes.
    write_lock: LockDepMutex<(), ThermalChargeLevelLock>,
}

impl FccCoolingOps {
    fn new(max_charge_level: u32, charge_level: u32) -> Self {
        Self::new_with_timeout(max_charge_level, charge_level, CHARGER_SYNC_TIMEOUT)
    }

    fn new_with_timeout(
        max_charge_level: u32,
        charge_level: u32,
        timeout: zx::MonotonicDuration,
    ) -> Self {
        Self {
            proxy: Mutex::new(None),
            proxy_ready: zx::Event::create(),
            timeout,
            max_charge_level,
            charge_level: AtomicU32::new(charge_level),
            write_lock: ().into(),
        }
    }

    fn set_proxy(&self, proxy: Arc<fbattery::ChargerSynchronousProxy>) {
        let mut guard = self.proxy.lock();
        *guard = Some(proxy);
        let _ = self.proxy_ready.signal(zx::Signals::NONE, zx::Signals::USER_0);
    }

    fn clear_proxy(&self, disconnected: &Arc<fbattery::ChargerSynchronousProxy>) {
        let mut guard = self.proxy.lock();
        if guard.as_ref().is_some_and(|current| Arc::ptr_eq(current, disconnected)) {
            *guard = None;
            let _ = self.proxy_ready.signal(zx::Signals::USER_0, zx::Signals::NONE);
        }
    }

    fn proxy(
        &self,
        deadline: zx::MonotonicInstant,
    ) -> Result<Arc<fbattery::ChargerSynchronousProxy>, Errno> {
        loop {
            if let Some(proxy) = self.proxy.lock().clone() {
                return Ok(proxy);
            }
            self.proxy_ready
                .wait_one(zx::Signals::USER_0, deadline)
                .to_result()
                .map_err(|e| errno!(ENODEV, e))?;
        }
    }
}

impl CoolingOps for FccCoolingOps {
    fn get_max_state(&self) -> u32 {
        self.max_charge_level
    }

    fn get_state(&self) -> Result<u32, Errno> {
        Ok(self.charge_level.load(Ordering::Relaxed))
    }

    fn set_state(&self, state: u32) -> Result<(), Errno> {
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

        let _guard = self.write_lock.lock();
        let prev_charge_level = self.charge_level.load(Ordering::Relaxed);

        // When the charge level transitions to the maximum, disable charging. Otherwise, when
        // dropping below the maximum, enable charging.
        let enable = if charge_level == self.max_charge_level
            && prev_charge_level != self.max_charge_level
        {
            Some(false)
        } else if charge_level != self.max_charge_level
            && prev_charge_level == self.max_charge_level
        {
            Some(true)
        } else {
            None
        };
        if let Some(enable) = enable {
            let deadline = zx::MonotonicInstant::after(self.timeout);
            self.proxy(deadline)?
                .enable(enable, deadline)
                .map_err(|e| errno!(EIO, e))?
                .map_err(|e| errno!(EIO, e))?;
        }

        self.charge_level.store(charge_level, Ordering::Relaxed);
        Ok(())
    }
}

fn register_fcc_device(
    kernel: &Kernel,
    registrar: &mut CoolingDeviceRegistrar,
    param: &str,
) -> Result<(), Error> {
    let max_charge_level: u32 = param.parse().context("Invalid max_charge_level")?;
    let ops = Arc::new(FccCoolingOps::new(max_charge_level, /* charge_level = */ 0));

    // Register the cooling device synchronously so sysfs scanners (such as Android's thermal
    // init scripts and Thermal HAL) find `/sys/class/thermal/cooling_device*` at startup before
    // asynchronous ChargerService discovery completes.
    registrar.register(kernel, "fcc".to_string(), ops.clone());

    ServiceWatcher::register_singleton(
        kernel,
        fbattery::ChargerServiceMarker,
        move |_current_task, instance, scope| {
            let proxy = match instance.connect_to_device_sync() {
                Ok(proxy) => Arc::new(proxy),
                Err(e) => {
                    log_error!("Failed to connect to battery Charger: {e:?}");
                    return;
                }
            };
            scope.monitor_channel(proxy.clone());
            ops.set_proxy(proxy.clone());
            let ops_for_disconnect = ops.clone();
            scope.on_disconnect(move |_current_task| {
                ops_for_disconnect.clear_proxy(&proxy);
            });
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_uapi::error;

    #[::fuchsia::test]
    async fn fcc_cooling_device_registered_before_charger_service_attaches() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            cooling_device_init(kernel, vec!["fcc=8".to_string()]).expect("cooling_device_init");

            let root = &kernel.device_registry.objects.root;
            assert!(root.lookup("class/thermal/cooling_device0".into()).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn fcc_set_state_waits_for_delayed_proxy_and_skips_duplicate_transitions() {
        let (proxy, mut stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<fbattery::ChargerMarker>();
        let ops = Arc::new(FccCoolingOps::new(8, /* charge_level = */ 0));

        let ops_for_writer = ops.clone();
        let writer_thread = std::thread::spawn(move || {
            // Transitioning to `max_charge_level` blocks on `proxy_ready` until `set_proxy` runs.
            ops_for_writer.set_state(8).expect("set_state(8)");
            assert_eq!(ops_for_writer.get_state().expect("get_state"), 8);

            // Setting `max_charge_level` again when already at `max_charge_level` must not issue a
            // duplicate `Enable(false)` FIDL call.
            ops_for_writer.set_state(8).expect("duplicate set_state(8)");
            assert_eq!(ops_for_writer.get_state().expect("get_state"), 8);

            // Dropping below `max_charge_level` re-enables charging.
            ops_for_writer.set_state(4).expect("set_state(4)");
            assert_eq!(ops_for_writer.get_state().expect("get_state"), 4);
        });

        ops.set_proxy(Arc::new(proxy));

        let mut requests = Vec::new();
        while let Some(Ok(req)) = stream.next().await {
            match req {
                fbattery::ChargerRequest::Enable { enable, responder } => {
                    requests.push(enable);
                    let _ = responder.send(Ok(()));
                }
                other => panic!("unexpected request: {other:?}"),
            }
            if requests.len() == 2 {
                break;
            }
        }
        assert_eq!(requests, vec![false, true]);
        writer_thread.join().expect("writer_thread");
    }

    #[::fuchsia::test]
    async fn fcc_set_state_returns_enodev_on_proxy_timeout_and_after_disconnect() {
        let ops = FccCoolingOps::new_with_timeout(
            8,
            /* charge_level = */ 0,
            zx::MonotonicDuration::from_millis(10),
        );

        // State changes below `max_charge_level` do not toggle charging and succeed without a
        // connected `ChargerService`.
        ops.set_state(3).expect("set_state(3)");
        assert_eq!(ops.get_state().expect("get_state"), 3);

        // Transitioning to `max_charge_level` without an attached `ChargerService` times out with
        // `ENODEV` and preserves the previous `charge_level`.
        assert_eq!(ops.set_state(8), error!(ENODEV));
        assert_eq!(ops.get_state().expect("get_state"), 3);

        // Attaching and then disconnecting a proxy resets readiness so subsequent transitions time
        // out with `ENODEV`.
        let (proxy, _stream) =
            fidl::endpoints::create_sync_proxy_and_stream::<fbattery::ChargerMarker>();
        let proxy = Arc::new(proxy);
        ops.set_proxy(proxy.clone());
        ops.clear_proxy(&proxy);
        assert_eq!(ops.set_state(8), error!(ENODEV));
        assert_eq!(ops.get_state().expect("get_state"), 3);
    }
}
