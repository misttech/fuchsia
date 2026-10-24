// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "256"]

mod cooling;
mod family;
mod powercap;
mod thermal_zone;

use crate::thermal_zone::{SensorProps, ThermalZone};
use anyhow::{Error, anyhow, bail};
use cooling::cooling_device_init;
use family::ThermalFamily;
use fidl_fuchsia_hardware_temperature as ftemperature;
use fidl_fuchsia_hardware_trippoint as ftrippoint;
use fidl_fuchsia_thermal as fthermal;
use powercap::powercap_device_init;
use starnix_core::device::kobject::Device;
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::{ArgNameAndValue, CurrentTask, Kernel};
use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use starnix_logging::{log_error, log_warn};

use starnix_sync::{LockDepMutex, ThermalTripPointStateLock};
use starnix_uapi::errors::{Errno, errno, error};
use starnix_uapi::file_mode::mode;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use thermal_netlink::{celsius_to_millicelsius, millicelsius_to_celsius};
use zx::MonotonicInstant;

fn build_thermal_zone_directory(
    device: &Device,
    proxy: ftemperature::DeviceSynchronousProxy,
    trippoint_proxy: ftrippoint::TripPointSynchronousProxy,
    sensor_manager: fthermal::SensorManagerSynchronousProxy,
    device_type: String,
    dir: &SimpleDirectoryMutator,
) {
    build_device_directory(device, dir);
    let trip_point = Arc::new(ThermalZoneTripPoint::new(device_type.clone(), trippoint_proxy));
    dir.entry(
        "emul_temp",
        EmulTempFile::new_node(device_type.clone(), sensor_manager),
        mode!(IFREG, 0o200),
    );
    dir.entry("temp", TemperatureFile::new_node(proxy), mode!(IFREG, 0o664));
    dir.entry(
        "trip_point_0_temp",
        TripPointTempFile::new_node(trip_point.clone()),
        mode!(IFREG, 0o664),
    );
    dir.entry("trip_point_0_hyst", TripPointHystFile::new_node(trip_point), mode!(IFREG, 0o664));
    dir.entry(
        "type",
        BytesFile::new_node(format!("{}\n", device_type).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry("policy", BytesFile::new_node(b"step_wise\n".to_vec()), mode!(IFREG, 0o444));
    dir.entry(
        "available_policies",
        BytesFile::new_node(b"step_wise\n".to_vec()),
        mode!(IFREG, 0o444),
    );
}

struct TemperatureFile {
    proxy: ftemperature::DeviceSynchronousProxy,
}

impl TemperatureFile {
    pub fn new_node(proxy: ftemperature::DeviceSynchronousProxy) -> impl FsNodeOps {
        BytesFile::new_node(Self { proxy })
    }
}

impl BytesFileOps for TemperatureFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let (zx_status, temp) =
            self.proxy.get_temperature_celsius(MonotonicInstant::INFINITE).map_err(|e| {
                log_error!("get_temperature_celsius failed: {}", e);
                errno!(ENOENT)
            })?;
        let _ = zx::Status::ok(zx_status).map_err(|e| {
            log_error!("get_temperature_celsius driver returned error: {}", e);
            errno!(ENOENT)
        })?;

        let out = format!("{}\n", celsius_to_millicelsius(temp) as i32);
        Ok(out.as_bytes().to_owned().into())
    }
}

fn parse_i32_sysfs_input(data: &[u8], attr_name: &str) -> Result<i32, Errno> {
    let num_str = str::from_utf8(data).map_err(|e| {
        log_warn!("Failed to convert {} input to utf-8: {:?}", attr_name, e);
        errno!(EINVAL)
    })?;
    num_str.trim().parse().map_err(|e| {
        log_warn!("Failed to parse {} input as i32: {:?}", attr_name, e);
        errno!(EINVAL)
    })
}

struct EmulTempFile {
    device_type: String,
    proxy: fthermal::SensorManagerSynchronousProxy,
}

impl EmulTempFile {
    pub fn new_node(
        device_type: String,
        proxy: fthermal::SensorManagerSynchronousProxy,
    ) -> impl FsNodeOps {
        BytesFile::new_node(Self { device_type, proxy })
    }
}

impl BytesFileOps for EmulTempFile {
    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let temp_milli_c = parse_i32_sysfs_input(&data, "emul_temp")?;

        if temp_milli_c == 0 {
            match self
                .proxy
                .clear_temperature_override(&self.device_type, zx::MonotonicInstant::INFINITE)
            {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(error)) => {
                    log_warn!(
                        "Failed to clear temperature override for sensor {}: {:?}",
                        self.device_type,
                        error
                    );
                    error!(EINVAL)
                }
                Err(error) => {
                    log_warn!(
                        "Failed to call clear_temperature_override for sensor {}: {:?}",
                        self.device_type,
                        error
                    );
                    error!(EIO)
                }
            }
        } else {
            match self.proxy.set_temperature_override(
                &self.device_type,
                millicelsius_to_celsius(temp_milli_c as f32).into(),
                zx::MonotonicInstant::INFINITE,
            ) {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(error)) => {
                    log_warn!(
                        "Failed to set temperature override for sensor {}: {:?}",
                        self.device_type,
                        error
                    );
                    error!(EINVAL)
                }
                Err(error) => {
                    log_warn!(
                        "Failed to call set_temperature_override for sensor {}: {:?}",
                        self.device_type,
                        error
                    );
                    error!(EIO)
                }
            }
        }
    }
}

const TRIP_POINT_FIDL_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);

fn trip_point_fidl_deadline() -> MonotonicInstant {
    MonotonicInstant::after(TRIP_POINT_FIDL_TIMEOUT)
}

fn map_trip_point_fidl_error(err: &fidl::Error) -> Errno {
    match err {
        fidl::Error::ClientCall(zx::Status::TIMED_OUT)
        | fidl::Error::ClientRead(fidl::TransportError::Status(zx::Status::TIMED_OUT))
        | fidl::Error::ClientWrite(fidl::TransportError::Status(zx::Status::TIMED_OUT)) => {
            errno!(ETIMEDOUT)
        }
        _ => errno!(EIO),
    }
}

/// Selects a programmable trip point descriptor of `target_type` from `descriptors`.
///
/// Selects the last matching descriptor of `target_type` (the programmable warning slot rather
/// than a lower-index hardware fault threshold, regardless of whether the warning slot is
/// currently cleared or already armed).
fn select_descriptor_index(
    descriptors: &[ftrippoint::TripPointDescriptor],
    target_type: ftrippoint::TripPointType,
) -> Option<u32> {
    descriptors.iter().rfind(|d| d.type_ == target_type).map(|d| d.index)
}

#[derive(Default)]
struct TripPointState {
    temp_mc: i32,
    hyst_mc: i32,
    cached_indices: Option<(u32, Option<u32>)>,
}

struct ThermalZoneTripPoint {
    device_type: String,
    proxy: ftrippoint::TripPointSynchronousProxy,
    state: LockDepMutex<TripPointState, ThermalTripPointStateLock>,
}

impl ThermalZoneTripPoint {
    fn new(device_type: String, proxy: ftrippoint::TripPointSynchronousProxy) -> Self {
        Self { device_type, proxy, state: LockDepMutex::new(TripPointState::default()) }
    }

    fn temp_mc(&self) -> i32 {
        self.state.lock().temp_mc
    }

    fn set_temp_mc(&self, temp_mc: i32) -> Result<(), Errno> {
        let mut state = self.state.lock();
        let hyst_mc = state.hyst_mc;
        self.apply_trip_points(&mut state, (temp_mc != 0).then_some(temp_mc), hyst_mc)?;
        state.temp_mc = temp_mc;
        Ok(())
    }

    fn hyst_mc(&self) -> i32 {
        self.state.lock().hyst_mc
    }

    fn set_hyst_mc(&self, hyst_mc: i32) -> Result<(), Errno> {
        if hyst_mc < 0 {
            return error!(EINVAL);
        }

        let mut state = self.state.lock();
        let temp_mc = state.temp_mc;
        if temp_mc != 0 {
            self.apply_trip_points(&mut state, Some(temp_mc), hyst_mc)?;
        } else {
            // Resolving descriptor indices when `temp_mc == 0` ensures writes to
            // `trip_point_0_hyst` fail fast (`EIO`) on sensors that do not support
            // `fuchsia.hardware.trippoint` and caches the indices for subsequent
            // `trip_point_0_temp` writes.
            let _ = self.resolve_descriptor_indices(&mut state)?;
        }
        state.hyst_mc = hyst_mc;
        Ok(())
    }

    fn resolve_descriptor_indices(
        &self,
        state: &mut TripPointState,
    ) -> Result<(u32, Option<u32>), Errno> {
        if let Some(indices) = state.cached_indices {
            return Ok(indices);
        }

        let descriptors = match self.proxy.get_trip_point_descriptors(trip_point_fidl_deadline()) {
            Ok(Ok(descriptors)) => descriptors,
            Ok(Err(status)) => {
                log_warn!(
                    "get_trip_point_descriptors returned status {} for sensor {}",
                    zx::Status::err_from_raw(status),
                    self.device_type
                );
                return error!(EIO);
            }
            Err(err) => {
                log_warn!(
                    "Failed to call get_trip_point_descriptors for sensor {}: {:?}",
                    self.device_type,
                    err
                );
                return Err(map_trip_point_fidl_error(&err));
            }
        };

        let Some(high_index) =
            select_descriptor_index(&descriptors, ftrippoint::TripPointType::OneshotTempAbove)
        else {
            log_warn!(
                "No ONESHOT_TEMP_ABOVE trip point descriptor found for sensor {}",
                self.device_type
            );
            return error!(EIO);
        };
        let low_index =
            select_descriptor_index(&descriptors, ftrippoint::TripPointType::OneshotTempBelow);

        let indices = (high_index, low_index);
        state.cached_indices = Some(indices);
        Ok(indices)
    }

    fn apply_trip_points(
        &self,
        state: &mut TripPointState,
        temp_mc: Option<i32>,
        hyst_mc: i32,
    ) -> Result<(), Errno> {
        let (high_index, low_index) = self.resolve_descriptor_indices(state)?;

        let mut updates = Vec::with_capacity(2);
        if let Some(temp_mc) = temp_mc {
            let high_temp_c = millicelsius_to_celsius(temp_mc as f32);
            updates.push(ftrippoint::TripPointDescriptor {
                index: high_index,
                type_: ftrippoint::TripPointType::OneshotTempAbove,
                configuration: ftrippoint::TripPointValue::OneshotTempAboveTripPoint(
                    ftrippoint::OneshotTempAboveTripPoint {
                        critical_temperature_celsius: high_temp_c,
                    },
                ),
            });
            // Programming the static `[temp - hyst, temp]` window (`OneshotTempAbove` at `temp`
            // and `OneshotTempBelow` at `temp - hyst`) together is intentional: Starnix acts as a
            // stateless sysfs configuration bridge (without running an internal
            // `WaitForAnyTripPoint` state machine loop) so that hardware/firmware trip-point
            // controllers receive both the upper trip threshold and lower hysteresis clear
            // threshold when userspace configures `trip_point_0_{temp,hyst}`.
            if let Some(low_index) = low_index {
                let low_temp_mc = temp_mc.saturating_sub(hyst_mc);
                let low_temp_c = millicelsius_to_celsius(low_temp_mc as f32);
                updates.push(ftrippoint::TripPointDescriptor {
                    index: low_index,
                    type_: ftrippoint::TripPointType::OneshotTempBelow,
                    configuration: ftrippoint::TripPointValue::OneshotTempBelowTripPoint(
                        ftrippoint::OneshotTempBelowTripPoint {
                            critical_temperature_celsius: low_temp_c,
                        },
                    ),
                });
            }
        } else {
            updates.push(ftrippoint::TripPointDescriptor {
                index: high_index,
                type_: ftrippoint::TripPointType::OneshotTempAbove,
                configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                    ftrippoint::ClearedTripPoint,
                ),
            });
            if let Some(low_index) = low_index {
                updates.push(ftrippoint::TripPointDescriptor {
                    index: low_index,
                    type_: ftrippoint::TripPointType::OneshotTempBelow,
                    configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                        ftrippoint::ClearedTripPoint,
                    ),
                });
            }
        }

        match self.proxy.set_trip_points(&updates, trip_point_fidl_deadline()) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(raw_status)) => {
                let status = zx::Status::err_from_raw(raw_status);
                log_warn!(
                    "set_trip_points returned status {} for sensor {}",
                    status,
                    self.device_type
                );
                match status {
                    zx::Status::INVALID_ARGS | zx::Status::OUT_OF_RANGE => error!(EINVAL),
                    _ => error!(EIO),
                }
            }
            Err(err) => {
                log_warn!(
                    "Failed to call set_trip_points for sensor {}: {:?}",
                    self.device_type,
                    err
                );
                Err(map_trip_point_fidl_error(&err))
            }
        }
    }
}

struct TripPointTempFile {
    trip_point: Arc<ThermalZoneTripPoint>,
}

impl TripPointTempFile {
    pub fn new_node(trip_point: Arc<ThermalZoneTripPoint>) -> impl FsNodeOps {
        BytesFile::new_node(Self { trip_point })
    }
}

impl BytesFileOps for TripPointTempFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let temp_mc = self.trip_point.temp_mc();
        Ok(format!("{}\n", temp_mc).into_bytes().into())
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let temp_mc = parse_i32_sysfs_input(&data, "trip_point_0_temp")?;
        self.trip_point.set_temp_mc(temp_mc)
    }
}

struct TripPointHystFile {
    trip_point: Arc<ThermalZoneTripPoint>,
}

impl TripPointHystFile {
    pub fn new_node(trip_point: Arc<ThermalZoneTripPoint>) -> impl FsNodeOps {
        BytesFile::new_node(Self { trip_point })
    }
}

impl BytesFileOps for TripPointHystFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let hyst_mc = self.trip_point.hyst_mc();
        Ok(format!("{}\n", hyst_mc).into_bytes().into())
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let hyst_mc = parse_i32_sysfs_input(&data, "trip_point_0_hyst")?;
        self.trip_point.set_hyst_mc(hyst_mc)
    }
}

fn thermal_device_init(kernel: &Kernel) -> Result<(), Error> {
    let sensor_manager =
        fuchsia_component::client::connect_to_protocol_sync::<fthermal::SensorManagerMarker>()
            .map_err(|error| anyhow!("Failed to connect to SensorManager: {:?}", error))?;

    let sensors = sensor_manager.list_sensors(zx::MonotonicInstant::INFINITE)?;

    let registry = &kernel.device_registry;
    let thermal_class = registry.objects.thermal_class();
    let mut sensor_proxies = HashMap::new();

    for (thermal_zone_id, sensor_info) in sensors.into_iter().enumerate() {
        let Some(sensor_name) = sensor_info.name else {
            log_warn!("No sensor name for thermal zone {}, skipping.", thermal_zone_id);
            continue;
        };
        let sensor_name_clone = sensor_name.clone();

        let thermal_zone_id = thermal_zone_id as u32;
        let thermal_zone = format!("thermal_zone{}", thermal_zone_id);

        // Create a synchronous proxy for file requests.
        // Reads and writes are expected to block until data or an error is returned.
        let (sensor_sync, sensor_server_sync) = fidl::endpoints::create_sync_proxy();

        match sensor_manager.connect(
            fthermal::SensorManagerConnectRequest {
                name: Some(sensor_name.clone()),
                server_end: Some(fthermal::SensorServer_::Temperature(sensor_server_sync)),
                ..Default::default()
            },
            zx::MonotonicInstant::INFINITE,
        ) {
            Ok(Ok(())) => {}
            Ok(Err(connect_err)) => {
                log_error!("Failed to connect to sensor {} (sync): {:?}", sensor_name, connect_err);
                continue;
            }
            Err(fidl_err) => {
                log_error!(
                    "Failed to call connect for sensor {} (sync): {:?}",
                    sensor_name,
                    fidl_err
                );
                continue;
            }
        }

        let (trippoint_sync, trippoint_server_sync) = fidl::endpoints::create_sync_proxy();

        match sensor_manager.connect(
            fthermal::SensorManagerConnectRequest {
                name: Some(sensor_name.clone()),
                server_end: Some(fthermal::SensorServer_::Trippoint(trippoint_server_sync)),
                ..Default::default()
            },
            zx::MonotonicInstant::INFINITE,
        ) {
            Ok(Ok(())) => {}
            Ok(Err(connect_err)) => {
                log_warn!(
                    "Failed to connect to sensor {} trip point server (sync): {:?}",
                    sensor_name,
                    connect_err
                );
            }
            Err(fidl_err) => {
                log_warn!(
                    "Failed to call connect for sensor {} trip point server (sync): {:?}",
                    sensor_name,
                    fidl_err
                );
            }
        }

        registry.add_numberless_device(
            thermal_zone.as_str().into(),
            /* parent = */ None,
            thermal_class.clone(),
            move |device, dir| {
                match fuchsia_component::client::connect_to_protocol_sync::<fthermal::SensorManagerMarker>() {
                    Ok(sensor_manager) => build_thermal_zone_directory(device, sensor_sync, trippoint_sync, sensor_manager, sensor_name_clone, dir),
                    Err(error) => log_warn!("Failed to connect to SensorManager when building thermal zone for sensor {}: {:?}", sensor_name_clone, error),
                }
            },
        );

        // Create an asynchronous proxy for netlink calls.
        // The thermal netlink server periodically gets temperature data from the thermal sensors in
        // the background.
        let (sensor, sensor_server) = fidl::endpoints::create_proxy();

        match sensor_manager.connect(
            fthermal::SensorManagerConnectRequest {
                name: Some(sensor_name.clone()),
                server_end: Some(fthermal::SensorServer_::Temperature(sensor_server)),
                ..Default::default()
            },
            zx::MonotonicInstant::INFINITE,
        ) {
            Ok(Ok(())) => {}
            Ok(Err(connect_err)) => {
                log_error!(
                    "Failed to connect to sensor {} (async): {:?}",
                    sensor_name,
                    connect_err
                );
                continue;
            }
            Err(fidl_err) => {
                log_error!(
                    "Failed to call connect for sensor {} (async): {:?}",
                    sensor_name,
                    fidl_err
                );
                continue;
            }
        }

        sensor_proxies.insert(
            SensorProps { name: sensor_name },
            ThermalZone { id: thermal_zone_id, proxy: sensor },
        );
    }

    let (thermal_family, thermal_family_worker) = ThermalFamily::new(sensor_proxies);
    kernel.generic_netlink().add_family(Arc::new(thermal_family));
    kernel
        .kthreads
        .spawn_future(move || async move { thermal_family_worker.await }, "thermal_netlink_worker");

    Ok(())
}

fn parse_args(args: &[&str]) -> Result<Vec<String>, Error> {
    let mut cooling_devices = Vec::new();
    for &arg in args {
        match ArgNameAndValue::from(arg) {
            ArgNameAndValue { name: "cooling", value: Some(cooling) } if !cooling.is_empty() => {
                cooling_devices = cooling.split(',').map(String::from).collect();
            }
            _ => bail!("invalid thermal argument: {arg}"),
        }
    }
    Ok(cooling_devices)
}

/// Validates `thermal` module configuration `args`.
pub fn validate_args(args: &[&str]) -> Result<(), Error> {
    parse_args(args).map(|_| ())
}

/// Initializes thermal zones, powercap zones, and optional cooling devices from module
/// configuration `args`.
pub fn module_init(kernel: &Kernel, args: &[&str]) -> Result<(), Error> {
    let cooling_devices = parse_args(args)?;
    powercap_device_init(kernel);
    thermal_device_init(kernel)?;
    cooling_device_init(kernel, cooling_devices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_async as fasync;
    use futures::TryStreamExt;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[fuchsia::test]
    fn test_select_descriptor_index() {
        let descriptors = vec![
            ftrippoint::TripPointDescriptor {
                index: 0,
                type_: ftrippoint::TripPointType::OneshotTempAbove,
                configuration: ftrippoint::TripPointValue::OneshotTempAboveTripPoint(
                    ftrippoint::OneshotTempAboveTripPoint { critical_temperature_celsius: 95.0 },
                ),
            },
            ftrippoint::TripPointDescriptor {
                index: 1,
                type_: ftrippoint::TripPointType::OneshotTempAbove,
                configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                    ftrippoint::ClearedTripPoint,
                ),
            },
            ftrippoint::TripPointDescriptor {
                index: 2,
                type_: ftrippoint::TripPointType::OneshotTempAbove,
                configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                    ftrippoint::ClearedTripPoint,
                ),
            },
            ftrippoint::TripPointDescriptor {
                index: 3,
                type_: ftrippoint::TripPointType::OneshotTempBelow,
                configuration: ftrippoint::TripPointValue::OneshotTempBelowTripPoint(
                    ftrippoint::OneshotTempBelowTripPoint { critical_temperature_celsius: 80.0 },
                ),
            },
            ftrippoint::TripPointDescriptor {
                index: 4,
                type_: ftrippoint::TripPointType::OneshotTempBelow,
                configuration: ftrippoint::TripPointValue::OneshotTempBelowTripPoint(
                    ftrippoint::OneshotTempBelowTripPoint { critical_temperature_celsius: 70.0 },
                ),
            },
        ];

        // 1. Selects the last matching descriptor of `target_type` (`rfind`).
        assert_eq!(
            select_descriptor_index(&descriptors, ftrippoint::TripPointType::OneshotTempAbove),
            Some(2)
        );
        assert_eq!(
            select_descriptor_index(&descriptors[..2], ftrippoint::TripPointType::OneshotTempAbove),
            Some(1)
        );
        assert_eq!(
            select_descriptor_index(&descriptors, ftrippoint::TripPointType::OneshotTempBelow),
            Some(4)
        );

        // 2. Startup layout where a lower-index hardware fault descriptor (`index 0`) and the
        // programmable warning high/low descriptors (`index 1`, `index 2`) all start as
        // `ClearedTripPoint`, or where `index 1` is already armed while `index 0` is cleared:
        // `OneshotTempAbove` always selects `Some(1)` and `OneshotTempBelow` selects `Some(2)`.
        let all_cleared_startup = vec![
            ftrippoint::TripPointDescriptor {
                index: 0,
                type_: ftrippoint::TripPointType::OneshotTempAbove,
                configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                    ftrippoint::ClearedTripPoint,
                ),
            },
            ftrippoint::TripPointDescriptor {
                index: 1,
                type_: ftrippoint::TripPointType::OneshotTempAbove,
                configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                    ftrippoint::ClearedTripPoint,
                ),
            },
            ftrippoint::TripPointDescriptor {
                index: 2,
                type_: ftrippoint::TripPointType::OneshotTempBelow,
                configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                    ftrippoint::ClearedTripPoint,
                ),
            },
        ];
        assert_eq!(
            select_descriptor_index(
                &all_cleared_startup,
                ftrippoint::TripPointType::OneshotTempAbove
            ),
            Some(1)
        );
        assert_eq!(
            select_descriptor_index(
                &all_cleared_startup,
                ftrippoint::TripPointType::OneshotTempBelow
            ),
            Some(2)
        );

        let warning_already_armed = vec![
            ftrippoint::TripPointDescriptor {
                index: 0,
                type_: ftrippoint::TripPointType::OneshotTempAbove,
                configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                    ftrippoint::ClearedTripPoint,
                ),
            },
            ftrippoint::TripPointDescriptor {
                index: 1,
                type_: ftrippoint::TripPointType::OneshotTempAbove,
                configuration: ftrippoint::TripPointValue::OneshotTempAboveTripPoint(
                    ftrippoint::OneshotTempAboveTripPoint { critical_temperature_celsius: 85.0 },
                ),
            },
        ];
        assert_eq!(
            select_descriptor_index(
                &warning_already_armed,
                ftrippoint::TripPointType::OneshotTempAbove
            ),
            Some(1)
        );

        // 3. Returns `None` when no descriptor matches `target_type`.
        assert_eq!(
            select_descriptor_index(&descriptors[..3], ftrippoint::TripPointType::OneshotTempBelow),
            None
        );
        assert_eq!(select_descriptor_index(&[], ftrippoint::TripPointType::OneshotTempAbove), None);
    }

    #[fuchsia::test]
    async fn test_thermal_zone_trip_point_caching_and_errors() {
        let scope = fasync::Scope::new();
        let (client_end, mut stream) =
            fidl::endpoints::create_request_stream::<ftrippoint::TripPointMarker>();
        let get_descriptors_calls = Arc::new(AtomicUsize::new(0));
        let get_descriptors_calls_clone = get_descriptors_calls.clone();

        // Single-descriptor (`OneshotTempAbove`-only) sensor where `SetTripPoints` rejects
        // temperatures > 120.0°C with `ZX_ERR_INVALID_ARGS`, temperatures <= -273.15°C with
        // `ZX_ERR_OUT_OF_RANGE`, and temperatures == 100.0°C with `ZX_ERR_INTERNAL`.
        scope.spawn(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    ftrippoint::TripPointRequest::GetTripPointDescriptors { responder } => {
                        get_descriptors_calls_clone.fetch_add(1, Ordering::SeqCst);
                        let _ = responder.send(Ok(&[ftrippoint::TripPointDescriptor {
                            index: 1,
                            type_: ftrippoint::TripPointType::OneshotTempAbove,
                            configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                                ftrippoint::ClearedTripPoint,
                            ),
                        }]));
                    }
                    ftrippoint::TripPointRequest::SetTripPoints { descriptors, responder } => {
                        assert_eq!(descriptors.len(), 1);
                        assert_eq!(descriptors[0].index, 1);
                        assert_eq!(
                            descriptors[0].type_,
                            ftrippoint::TripPointType::OneshotTempAbove
                        );
                        let status = match &descriptors[0].configuration {
                            ftrippoint::TripPointValue::ClearedTripPoint(_) => Ok(()),
                            ftrippoint::TripPointValue::OneshotTempAboveTripPoint(tp) => {
                                if tp.critical_temperature_celsius > 120.0 {
                                    Err(zx::Status::INVALID_ARGS.into_raw())
                                } else if tp.critical_temperature_celsius <= -273.15 {
                                    Err(zx::Status::OUT_OF_RANGE.into_raw())
                                } else if (tp.critical_temperature_celsius - 100.0).abs() < 0.01 {
                                    Err(zx::Status::INTERNAL.into_raw())
                                } else {
                                    Ok(())
                                }
                            }
                            _ => Err(zx::Status::INVALID_ARGS.into_raw()),
                        };
                        let _ = responder.send(status);
                    }
                    _ => unreachable!(),
                }
            }
        });

        let get_descriptors_calls_check = get_descriptors_calls.clone();
        fasync::unblock(move || {
            let proxy = ftrippoint::TripPointSynchronousProxy::new(client_end.into_channel());
            let trip_point = ThermalZoneTripPoint::new("test-sensor".to_string(), proxy);

            // 1. Negative `hyst_mc` is rejected before any FIDL call.
            assert_eq!(trip_point.set_hyst_mc(-1), error!(EINVAL));
            assert_eq!(get_descriptors_calls_check.load(Ordering::SeqCst), 0);

            // 2. Setting `hyst_mc` while `temp_mc == 0` resolves and caches descriptor indices
            // (`Some((1, None))` for single-descriptor sensor) without arming a trip point.
            assert_eq!(trip_point.set_hyst_mc(2_000), Ok(()));
            assert_eq!(trip_point.hyst_mc(), 2_000);
            assert_eq!(trip_point.state.lock().cached_indices, Some((1, None)));
            assert_eq!(get_descriptors_calls_check.load(Ordering::SeqCst), 1);

            // 3. Subsequent `set_temp_mc` and `set_hyst_mc` calls reuse cached indices without
            // calling `GetTripPointDescriptors` again, including when clearing (`0`).
            assert_eq!(trip_point.set_temp_mc(75_000), Ok(()));
            assert_eq!(trip_point.temp_mc(), 75_000);
            assert_eq!(trip_point.set_hyst_mc(3_000), Ok(()));
            assert_eq!(trip_point.hyst_mc(), 3_000);
            assert_eq!(trip_point.set_temp_mc(0), Ok(()));
            assert_eq!(trip_point.temp_mc(), 0);
            assert_eq!(get_descriptors_calls_check.load(Ordering::SeqCst), 1);

            // 4. `SetTripPoints` error mapping: `INVALID_ARGS` and `OUT_OF_RANGE` map to `EINVAL`,
            // while other driver statuses (`INTERNAL`) map to `EIO`, preserving previous `temp_mc`.
            assert_eq!(trip_point.set_temp_mc(80_000), Ok(()));
            assert_eq!(trip_point.set_temp_mc(150_000), error!(EINVAL));
            assert_eq!(trip_point.temp_mc(), 80_000);
            assert_eq!(trip_point.set_temp_mc(-300_000), error!(EINVAL));
            assert_eq!(trip_point.temp_mc(), 80_000);
            assert_eq!(trip_point.set_temp_mc(100_000), error!(EIO));
            assert_eq!(trip_point.temp_mc(), 80_000);
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_thermal_zone_trip_point_dual_descriptors_and_hyst_rollback() {
        assert_eq!(parse_i32_sysfs_input(b" 75000\n", "trip_point_0_temp"), Ok(75_000));
        assert_eq!(parse_i32_sysfs_input(b"-5000\n", "emul_temp"), Ok(-5_000));
        assert_eq!(parse_i32_sysfs_input(b"invalid\n", "trip_point_0_temp"), error!(EINVAL));
        assert_eq!(parse_i32_sysfs_input(&[0xff, 0xfe], "trip_point_0_hyst"), error!(EINVAL));

        let scope = fasync::Scope::new();
        let (client_end, mut stream) =
            fidl::endpoints::create_request_stream::<ftrippoint::TripPointMarker>();
        let recorded_batches = Arc::new(Mutex::new(Vec::new()));
        let recorded_batches_clone = recorded_batches.clone();

        // Dual-descriptor (`OneshotTempAbove` at index 0 + `OneshotTempBelow` at index 1) sensor
        // that rejects any trip point <= -273.15°C with `ZX_ERR_OUT_OF_RANGE`.
        scope.spawn(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    ftrippoint::TripPointRequest::GetTripPointDescriptors { responder } => {
                        let _ = responder.send(Ok(&[
                            ftrippoint::TripPointDescriptor {
                                index: 0,
                                type_: ftrippoint::TripPointType::OneshotTempAbove,
                                configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                                    ftrippoint::ClearedTripPoint,
                                ),
                            },
                            ftrippoint::TripPointDescriptor {
                                index: 1,
                                type_: ftrippoint::TripPointType::OneshotTempBelow,
                                configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                                    ftrippoint::ClearedTripPoint,
                                ),
                            },
                        ]));
                    }
                    ftrippoint::TripPointRequest::SetTripPoints { descriptors, responder } => {
                        let out_of_range = descriptors.iter().any(|d| match &d.configuration {
                            ftrippoint::TripPointValue::OneshotTempAboveTripPoint(tp) => {
                                tp.critical_temperature_celsius <= -273.15
                            }
                            ftrippoint::TripPointValue::OneshotTempBelowTripPoint(tp) => {
                                tp.critical_temperature_celsius <= -273.15
                            }
                            _ => false,
                        });
                        if out_of_range {
                            let _ = responder.send(Err(zx::Status::OUT_OF_RANGE.into_raw()));
                        } else {
                            recorded_batches_clone.lock().unwrap().push(descriptors);
                            let _ = responder.send(Ok(()));
                        }
                    }
                    _ => unreachable!(),
                }
            }
        });

        fasync::unblock(move || {
            let proxy = ftrippoint::TripPointSynchronousProxy::new(client_end.into_channel());
            let trip_point = ThermalZoneTripPoint::new("dual-sensor".to_string(), proxy);

            // 1. Configure `hyst_mc = 3_000` then `temp_mc = 75_000`: caches `Some((0, Some(1)))`
            // and programs `OneshotTempAbove(75.0)` and `OneshotTempBelow(72.0)`.
            assert_eq!(trip_point.set_hyst_mc(3_000), Ok(()));
            assert_eq!(trip_point.state.lock().cached_indices, Some((0, Some(1))));
            assert_eq!(trip_point.set_temp_mc(75_000), Ok(()));

            // 2. Updating `hyst_mc` to `5_000` while `temp_mc != 0` reprograms the window to
            // `[70.0°C, 75.0°C]`.
            assert_eq!(trip_point.set_hyst_mc(5_000), Ok(()));
            assert_eq!(trip_point.hyst_mc(), 5_000);

            // 3. If `set_hyst_mc` triggers a `SetTripPoints` error while `temp_mc != 0` (e.g.
            // `75_000 - 400_000 = -325_000` m°C <= -273.15°C -> `OUT_OF_RANGE`), `set_hyst_mc`
            // returns `EINVAL` and preserves the previous `hyst_mc` (`5_000`).
            assert_eq!(trip_point.set_hyst_mc(400_000), error!(EINVAL));
            assert_eq!(trip_point.hyst_mc(), 5_000);

            // 4. Clearing `temp_mc` (`0`) clears both `OneshotTempAbove` and `OneshotTempBelow`.
            assert_eq!(trip_point.set_temp_mc(0), Ok(()));
            assert_eq!(trip_point.temp_mc(), 0);

            let batches = recorded_batches.lock().unwrap();
            assert_eq!(
                *batches,
                vec![
                    vec![
                        ftrippoint::TripPointDescriptor {
                            index: 0,
                            type_: ftrippoint::TripPointType::OneshotTempAbove,
                            configuration: ftrippoint::TripPointValue::OneshotTempAboveTripPoint(
                                ftrippoint::OneshotTempAboveTripPoint {
                                    critical_temperature_celsius: 75.0,
                                },
                            ),
                        },
                        ftrippoint::TripPointDescriptor {
                            index: 1,
                            type_: ftrippoint::TripPointType::OneshotTempBelow,
                            configuration: ftrippoint::TripPointValue::OneshotTempBelowTripPoint(
                                ftrippoint::OneshotTempBelowTripPoint {
                                    critical_temperature_celsius: 72.0,
                                },
                            ),
                        },
                    ],
                    vec![
                        ftrippoint::TripPointDescriptor {
                            index: 0,
                            type_: ftrippoint::TripPointType::OneshotTempAbove,
                            configuration: ftrippoint::TripPointValue::OneshotTempAboveTripPoint(
                                ftrippoint::OneshotTempAboveTripPoint {
                                    critical_temperature_celsius: 75.0,
                                },
                            ),
                        },
                        ftrippoint::TripPointDescriptor {
                            index: 1,
                            type_: ftrippoint::TripPointType::OneshotTempBelow,
                            configuration: ftrippoint::TripPointValue::OneshotTempBelowTripPoint(
                                ftrippoint::OneshotTempBelowTripPoint {
                                    critical_temperature_celsius: 70.0,
                                },
                            ),
                        },
                    ],
                    vec![
                        ftrippoint::TripPointDescriptor {
                            index: 0,
                            type_: ftrippoint::TripPointType::OneshotTempAbove,
                            configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                                ftrippoint::ClearedTripPoint,
                            ),
                        },
                        ftrippoint::TripPointDescriptor {
                            index: 1,
                            type_: ftrippoint::TripPointType::OneshotTempBelow,
                            configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                                ftrippoint::ClearedTripPoint,
                            ),
                        },
                    ],
                ]
            );
        })
        .await;
    }
}
