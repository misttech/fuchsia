// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_power_battery::{self as fpower, ChargeStatus};
use fuchsia_async as fasync;
use fuchsia_inspect::{
    self as inspect, BoolProperty, DoubleProperty, ExponentialHistogramParams, HistogramProperty,
    IntProperty, Node, NumericProperty, Property, StringProperty, UintExponentialHistogramProperty,
    UintProperty,
};
use log::{error, warn};
use state_recorder::{
    EnumStateRecorder, NumericStateRecorder, PersistenceOptions, RecordableEnum,
    RecordableNumericType, RecorderOptions, Units, units,
};
use std::cell::RefCell;
use std::rc::Rc;
use strum_macros::{Display, EnumIter, FromRepr};

const MAX_BATTERY_LEVEL_MEASUREMENTS: usize = 200;
const MAX_CHARGE_STATUS_MEASUREMENTS: usize = 20;
const MAX_FAULT_MEASUREMENTS: usize = 20;
const MAX_HEALTH_MEASUREMENTS: usize = 20;
const MAX_POWER_CONSUMPTION_MEASUREMENTS: usize = 20;
const STALE_DATA_TIMER: zx::Duration<zx::MonotonicTimeline> = zx::Duration::from_minutes(10);

#[derive(Copy, Clone, Debug, Display, EnumIter, Eq, PartialEq, Hash, FromRepr)]
#[repr(u8)]
pub enum FaultState {
    None = 0,
    NoUpdate = 1,
    DriverDisconnected = 2,
}

impl From<FaultState> for u64 {
    fn from(value: FaultState) -> Self {
        value as Self
    }
}

#[derive(Copy, Clone, Debug, Display, EnumIter, Eq, PartialEq, Hash, FromRepr)]
#[repr(u8)]
pub enum BatteryHealth {
    Unknown = 0,
    Good = 1,
    Cold = 2,
    Hot = 3,
    Dead = 4,
    OverVoltage = 5,
    UnspecifiedFailure = 6,
    Cool = 7,
    Warm = 8,
    Overheat = 9,
}

impl From<BatteryHealth> for u64 {
    fn from(value: BatteryHealth) -> Self {
        value as Self
    }
}

impl From<fidl_fuchsia_power_battery::HealthStatus> for BatteryHealth {
    fn from(status: fidl_fuchsia_power_battery::HealthStatus) -> Self {
        match status {
            fidl_fuchsia_power_battery::HealthStatus::Unknown => BatteryHealth::Unknown,
            fidl_fuchsia_power_battery::HealthStatus::Good => BatteryHealth::Good,
            fidl_fuchsia_power_battery::HealthStatus::Cold => BatteryHealth::Cold,
            fidl_fuchsia_power_battery::HealthStatus::Hot => BatteryHealth::Hot,
            fidl_fuchsia_power_battery::HealthStatus::Dead => BatteryHealth::Dead,
            fidl_fuchsia_power_battery::HealthStatus::OverVoltage => BatteryHealth::OverVoltage,
            fidl_fuchsia_power_battery::HealthStatus::UnspecifiedFailure => {
                BatteryHealth::UnspecifiedFailure
            }
            fidl_fuchsia_power_battery::HealthStatus::Cool => BatteryHealth::Cool,
            fidl_fuchsia_power_battery::HealthStatus::Warm => BatteryHealth::Warm,
            fidl_fuchsia_power_battery::HealthStatus::Overheat => BatteryHealth::Overheat,
        }
    }
}

/// Local representation of `fidl_fuchsia_power_battery::ChargeStatus` defined
/// for compatibility with the `state_recorder` library.
#[derive(Copy, Clone, Debug, Display, EnumIter, Eq, PartialEq, Hash, FromRepr)]
#[repr(u8)]
pub enum BatteryChargeStatus {
    Unknown = 0,
    Charging = 1,
    Discharging = 2,
    NotCharging = 3,
    Full = 4,
}

impl From<BatteryChargeStatus> for u64 {
    fn from(value: BatteryChargeStatus) -> Self {
        value as Self
    }
}

impl From<fidl_fuchsia_power_battery::ChargeStatus> for BatteryChargeStatus {
    fn from(status: fidl_fuchsia_power_battery::ChargeStatus) -> Self {
        match status {
            fidl_fuchsia_power_battery::ChargeStatus::Unknown => BatteryChargeStatus::Unknown,
            fidl_fuchsia_power_battery::ChargeStatus::Charging => BatteryChargeStatus::Charging,
            fidl_fuchsia_power_battery::ChargeStatus::Discharging => {
                BatteryChargeStatus::Discharging
            }
            fidl_fuchsia_power_battery::ChargeStatus::NotCharging => {
                BatteryChargeStatus::NotCharging
            }
            fidl_fuchsia_power_battery::ChargeStatus::Full => BatteryChargeStatus::Full,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FaultRecoveryEvent {
    /// For the transition from NoUpdate to None.
    Recovered,
    /// Not a transition from NoUpdate to None.
    None,
}

struct FaultDetectorState {
    /// Monotonic timestamp of the last successful activity from the target.
    last_activity_time: fasync::MonotonicInstant,

    /// Handle to the currently active watchdog task. Used for cancellation (dropping the old task).
    watchdog_task: Option<fasync::Task<()>>,

    /// Tracks the current fault state to prevent redundant Inspect writes and report state.
    current_fault: FaultState,

    /// Inspect recorder for the fault state.
    fault_state_recorder: EnumStateRecorder<FaultState>,

    /// Store the previous level to detect changes.
    previous_raw_level: Option<f32>,

    /// Store the previous charge status to detect changes.
    previous_charge_status: Option<ChargeStatus>,
}

struct FaultDetector {
    /// The maximum allowed duration between activity reports.
    timeout_duration: zx::Duration<zx::MonotonicTimeline>,

    state: RefCell<FaultDetectorState>,
}

impl FaultDetector {
    /// Creates a new FaultDetector.
    fn new(
        timeout: zx::Duration<zx::MonotonicTimeline>,
        test_dirs: Option<PersistenceDirs>,
    ) -> Rc<Self> {
        let persistence = match test_dirs {
            Some(PersistenceDirs { storage_dir, volatile_dir }) => {
                PersistenceOptions::new("battery_level_fault".to_string())
                    .storage_dir(&storage_dir)
                    .volatile_dir(&volatile_dir)
            }
            None => PersistenceOptions::new("battery_level_fault".to_string()),
        };

        let mut fault_state_recorder = EnumStateRecorder::new(
            "battery_level_fault".into(),
            c"power",
            RecorderOptions {
                capacity: MAX_FAULT_MEASUREMENTS,
                lazy_record: true,
                manager: None,
                persistence: Some(persistence),
            },
        )
        .expect("fault_state_recorder construction failed");

        fault_state_recorder.record(FaultState::None);
        Rc::new(Self {
            timeout_duration: timeout,
            state: RefCell::new(FaultDetectorState {
                last_activity_time: fasync::MonotonicInstant::now(),
                watchdog_task: None,
                current_fault: FaultState::None,
                fault_state_recorder,
                previous_raw_level: None,
                previous_charge_status: None,
            }),
        })
    }

    /// Called by the BatteryInfoRecorders when a level update occurs.
    /// Returns FaultRecoveryEvent(Recovered) if the fault was cleared otherwise None.
    // TODO(https://fxbug.dev/467405155): Modify the code if driver only report fresh data.
    // For now, just detect changes of level. When timeout happens with the following conditions:
    // 1. if Charging and battery level <= 95% (charging is expected to be slow above 95%)
    // 2. Discharging
    // then we honor the timeout value and record NoUpdate.
    fn update(
        self: &Rc<Self>,
        new_raw_level: Option<f32>,
        new_charge_status: Option<ChargeStatus>,
    ) -> FaultRecoveryEvent {
        if new_raw_level.is_none() {
            return FaultRecoveryEvent::None;
        }

        let prev_raw_level = self.state.borrow().previous_raw_level;
        let prev_charge_status = self.state.borrow().previous_charge_status;
        let mut recovery_event = FaultRecoveryEvent::None;

        match new_charge_status {
            Some(ChargeStatus::Charging) => {
                if let Some(level) = new_raw_level {
                    if level <= 95.0 {
                        if new_raw_level != prev_raw_level {
                            recovery_event = self.notify_state_change();
                        } else if new_charge_status != prev_charge_status {
                            // Reset the watchdog timer and activity baseline when transitioning to
                            // Charging to allow a full fresh timeout window for level changes.
                            // Do not call notify_state_change so we don't clear existing faults.
                            self.state.borrow_mut().last_activity_time =
                                fasync::MonotonicInstant::now();
                            self.reset_watchdog();
                        }
                    } else {
                        recovery_event = self.stop();
                    }
                }
            }
            Some(ChargeStatus::Discharging) => {
                if new_raw_level.is_some() && new_raw_level != prev_raw_level {
                    recovery_event = self.notify_state_change();
                } else if new_charge_status != prev_charge_status {
                    // Reset the watchdog timer and activity baseline when transitioning to
                    // Discharging to allow a full fresh timeout window for level changes.
                    // Do not call notify_state_change so we don't clear existing faults.
                    self.state.borrow_mut().last_activity_time = fasync::MonotonicInstant::now();
                    self.reset_watchdog();
                }
            }
            Some(ChargeStatus::Full) | Some(ChargeStatus::NotCharging) => {
                recovery_event = self.stop();
            }
            _ => {}
        }
        self.state.borrow_mut().previous_raw_level = new_raw_level;
        self.state.borrow_mut().previous_charge_status = new_charge_status;
        recovery_event
    }

    /// Returns FaultRecoveryEvent.
    fn notify_state_change(self: &Rc<Self>) -> FaultRecoveryEvent {
        // 1. Update the "freshness" timestamp
        self.state.borrow_mut().last_activity_time = fasync::MonotonicInstant::now();

        // 2. Report Fault::None (since we just got an update)
        let recovery_event = self.record_fault_change(FaultState::None);

        // 3. Reset the watchdog timer
        self.reset_watchdog();

        recovery_event
    }

    /// Stops the watchdog. Call this when the battery is FULL or NOT_CHARGING.
    /// Returns FaultRecoveryEvent.
    fn stop(self: &Rc<Self>) -> FaultRecoveryEvent {
        // Dropping the task cancels the internal timer
        self.state.borrow_mut().watchdog_task = None;

        // Reset our internal fault state back to None
        self.record_fault_change(FaultState::None)
    }

    /// Internal function to stop the old timer and start a new one.
    fn reset_watchdog(self: &Rc<Self>) {
        let self_clone = Rc::clone(self);
        let timeout = self.timeout_duration;

        self.state.borrow_mut().watchdog_task = Some(fasync::Task::local(async move {
            fasync::Timer::new(fasync::MonotonicInstant::after(timeout)).await;
            let now = fasync::MonotonicInstant::now();
            let last_update = self_clone.state.borrow().last_activity_time;

            if now - last_update >= timeout {
                self_clone.record_fault_change(FaultState::NoUpdate);
                warn!(
                    "FAULT DETECTED: No battery updates received for more than {}s",
                    (now - last_update).into_seconds()
                );
            }
        }));
    }

    pub fn record_disconnected(&self) -> FaultRecoveryEvent {
        self.state.borrow_mut().watchdog_task = None;
        self.record_fault_change(FaultState::DriverDisconnected)
    }

    /// Synchronous method to update internal state and Inspect recorder only on change.
    /// Returns FaultRecoveryEvent.
    fn record_fault_change(&self, new_fault: FaultState) -> FaultRecoveryEvent {
        let mut state = self.state.borrow_mut();
        let old_fault = state.current_fault;

        if old_fault != new_fault {
            state.current_fault = new_fault;

            // Lock the recorder and record the state change synchronously
            state.fault_state_recorder.record(new_fault);
        }

        if old_fault != FaultState::None && new_fault == FaultState::None {
            FaultRecoveryEvent::Recovered
        } else {
            FaultRecoveryEvent::None
        }
    }
}

#[derive(Clone)]
pub struct PersistenceDirs {
    pub storage_dir: String,
    pub volatile_dir: String,
}

#[derive(Clone, Default)]
pub struct RecorderConfig {
    /// If None, default system persistence locations are used.
    /// If Some, use specific (String, String) for storage and volatile dirs.
    pub persistence_dirs: Option<PersistenceDirs>,
}

pub struct BatteryInfoRecorders {
    raw_level_percent: RefCell<NumericStateRecorder<u8>>,
    level_percent: RefCell<NumericStateRecorder<u8>>,
    previous_raw_level: RefCell<Option<u8>>,
    previous_level: RefCell<Option<u8>>,
    present_voltage: RefCell<NumericStateRecorder<u32>>,
    remaining_capacity: RefCell<NumericStateRecorder<u32>>,
    present_current: RefCell<NumericStateRecorder<i32>>,
    average_current: RefCell<NumericStateRecorder<i32>>,
    time_to_full: RefCell<NumericStateRecorder<u64>>,
    temperature_mc: RefCell<NumericStateRecorder<i32>>,
    previous_temperature_mc: RefCell<Option<i32>>,
    full_capacity: RefCell<NumericStateRecorder<i32>>,
    previous_full_capacity: RefCell<Option<i32>>,
    state_of_health_percent: RefCell<NumericStateRecorder<f64>>,
    previous_state_of_health_percent: RefCell<Option<f64>>,
    health: RefCell<EnumStateRecorder<BatteryHealth>>,
    previous_health: RefCell<Option<BatteryHealth>>,
    charge_status: RefCell<EnumStateRecorder<BatteryChargeStatus>>,
    previous_charge_status: RefCell<Option<BatteryChargeStatus>>,
    fault_detector: Rc<FaultDetector>,

    // Diagnostics Inspect node for transport/FIDL reliability counters, wake-lease state,
    // state-vs-reality anomaly counters, and update processing latency histogram.
    _diagnostics_root: Node,
    _fidl_errors_node: Node,
    battery_watch_errors: UintProperty,
    battery_refresh_errors: UintProperty,
    charger_watch_errors: UintProperty,
    sag_lease_errors: UintProperty,
    watcher_send_errors: UintProperty,
    _disconnects_node: Node,
    battery_disconnect_count: UintProperty,
    charger_disconnect_count: UintProperty,
    _state_node: Node,
    latest_temperature_mc: IntProperty,
    latest_full_capacity_uah: IntProperty,
    latest_design_capacity_uah: IntProperty,
    latest_state_of_health_percent: DoubleProperty,
    charging_wake_lease_active: BoolProperty,
    wake_lease_acquisitions: UintProperty,
    last_update_timestamp_ns: IntProperty,
    _anomalies_node: Node,
    charging_zero_or_negative_current_count: UintProperty,
    premature_charge_done_count: UintProperty,
    thermal_health_degraded_count: UintProperty,
    overvoltage_count: UintProperty,
    last_anomaly_timestamp_ns: IntProperty,
    last_anomaly_reason: StringProperty,
    update_processing_latency_us: UintExponentialHistogramProperty,
}

impl BatteryInfoRecorders {
    pub fn new(config: RecorderConfig) -> Self {
        let mut raw_level_percent_opts = PersistenceOptions::new("raw_level_percent".to_string());
        let mut level_percent_opts = PersistenceOptions::new("level_percent".to_string());
        let mut present_voltage_opts = PersistenceOptions::new("present_voltage".to_string());
        let mut remaining_capacity_opts = PersistenceOptions::new("remaining_capacity".to_string());
        let mut present_current_opts = PersistenceOptions::new("charge_current".to_string());
        let mut average_current_opts = PersistenceOptions::new("average_current".to_string());
        let mut time_to_full_opts = PersistenceOptions::new("time_to_full".to_string());
        let mut temperature_mc_opts = PersistenceOptions::new("temperature_mc".to_string());
        let mut full_capacity_opts = PersistenceOptions::new("full_capacity".to_string());
        let mut state_of_health_opts =
            PersistenceOptions::new("state_of_health_percent".to_string());
        let mut health_opts = PersistenceOptions::new("health".to_string());
        let mut charge_status_opts = PersistenceOptions::new("charge_status".to_string());

        // Apply overrides and create persistence directories before initializing any recorders
        if let Some(PersistenceDirs { storage_dir, volatile_dir }) = &config.persistence_dirs {
            let _ = std::fs::create_dir_all(storage_dir);
            let _ = std::fs::create_dir_all(volatile_dir);

            raw_level_percent_opts =
                raw_level_percent_opts.storage_dir(storage_dir).volatile_dir(volatile_dir);
            level_percent_opts =
                level_percent_opts.storage_dir(storage_dir).volatile_dir(volatile_dir);
            present_voltage_opts =
                present_voltage_opts.storage_dir(storage_dir).volatile_dir(volatile_dir);
            remaining_capacity_opts =
                remaining_capacity_opts.storage_dir(storage_dir).volatile_dir(volatile_dir);
            present_current_opts =
                present_current_opts.storage_dir(storage_dir).volatile_dir(volatile_dir);
            average_current_opts =
                average_current_opts.storage_dir(storage_dir).volatile_dir(volatile_dir);
            time_to_full_opts =
                time_to_full_opts.storage_dir(storage_dir).volatile_dir(volatile_dir);
            temperature_mc_opts =
                temperature_mc_opts.storage_dir(storage_dir).volatile_dir(volatile_dir);
            full_capacity_opts =
                full_capacity_opts.storage_dir(storage_dir).volatile_dir(volatile_dir);
            state_of_health_opts =
                state_of_health_opts.storage_dir(storage_dir).volatile_dir(volatile_dir);
            health_opts = health_opts.storage_dir(storage_dir).volatile_dir(volatile_dir);
            charge_status_opts =
                charge_status_opts.storage_dir(storage_dir).volatile_dir(volatile_dir);
        }

        let fault_detector = FaultDetector::new(STALE_DATA_TIMER, config.persistence_dirs);

        let raw_level_percent_recorder = Self::create_recorder(
            "raw_level_percent",
            units!(Percent),
            MAX_BATTERY_LEVEL_MEASUREMENTS,
            raw_level_percent_opts,
        );
        let level_percent_recorder = Self::create_recorder(
            "level_percent",
            units!(Percent),
            MAX_BATTERY_LEVEL_MEASUREMENTS,
            level_percent_opts,
        );
        let present_voltage_recorder = Self::create_recorder(
            "present_voltage",
            units!(Milli, Volts),
            MAX_BATTERY_LEVEL_MEASUREMENTS,
            present_voltage_opts,
        );
        let remaining_capacity_recorder = Self::create_recorder(
            "remaining_capacity",
            units!(Micro, AmpHours),
            MAX_BATTERY_LEVEL_MEASUREMENTS,
            remaining_capacity_opts,
        );
        let present_current_recorder = Self::create_recorder(
            "charge_current",
            units!(Micro, Amps),
            MAX_POWER_CONSUMPTION_MEASUREMENTS,
            present_current_opts,
        );
        let average_current_recorder = Self::create_recorder(
            "average_current",
            units!(Micro, Amps),
            MAX_POWER_CONSUMPTION_MEASUREMENTS,
            average_current_opts,
        );
        let time_to_full_recorder = Self::create_recorder(
            "time_to_full",
            units!(Seconds),
            MAX_POWER_CONSUMPTION_MEASUREMENTS,
            time_to_full_opts,
        );
        let temperature_mc_recorder = Self::create_recorder(
            "temperature_mc",
            units!(Milli, Celsius),
            MAX_HEALTH_MEASUREMENTS,
            temperature_mc_opts,
        );
        let full_capacity_recorder = Self::create_recorder(
            "full_capacity",
            units!(Micro, AmpHours),
            MAX_HEALTH_MEASUREMENTS,
            full_capacity_opts,
        );
        let state_of_health_recorder = Self::create_recorder(
            "state_of_health_percent",
            units!(Percent),
            MAX_HEALTH_MEASUREMENTS,
            state_of_health_opts,
        );
        let health_recorder =
            Self::create_enum_recorder("health", MAX_HEALTH_MEASUREMENTS, health_opts);
        let charge_status_recorder = Self::create_enum_recorder(
            "charge_status",
            MAX_CHARGE_STATUS_MEASUREMENTS,
            charge_status_opts,
        );

        let diagnostics_root = inspect::component::inspector().root().create_child("diagnostics");

        let fidl_errors_node = diagnostics_root.create_child("fidl_errors");
        let battery_watch_errors = fidl_errors_node.create_uint("battery_watch_errors", 0);
        let battery_refresh_errors = fidl_errors_node.create_uint("battery_refresh_errors", 0);
        let charger_watch_errors = fidl_errors_node.create_uint("charger_watch_errors", 0);
        let sag_lease_errors = fidl_errors_node.create_uint("sag_lease_errors", 0);
        let watcher_send_errors = fidl_errors_node.create_uint("watcher_send_errors", 0);

        let disconnects_node = diagnostics_root.create_child("disconnects");
        let battery_disconnect_count = disconnects_node.create_uint("battery_disconnect_count", 0);
        let charger_disconnect_count = disconnects_node.create_uint("charger_disconnect_count", 0);

        let state_node = diagnostics_root.create_child("state");
        let latest_temperature_mc = state_node.create_int("temperature_mc", 0);
        let latest_full_capacity_uah = state_node.create_int("full_capacity_uah", 0);
        let latest_design_capacity_uah = state_node.create_int("design_capacity_uah", 0);
        let latest_state_of_health_percent =
            state_node.create_double("state_of_health_percent", 0.0);
        let charging_wake_lease_active =
            state_node.create_bool("charging_wake_lease_active", false);
        let wake_lease_acquisitions = state_node.create_uint("wake_lease_acquisitions", 0);
        let last_update_timestamp_ns = state_node.create_int("last_update_timestamp_ns", 0);

        let anomalies_node = diagnostics_root.create_child("anomalies");
        let charging_zero_or_negative_current_count =
            anomalies_node.create_uint("charging_zero_or_negative_current_count", 0);
        let premature_charge_done_count =
            anomalies_node.create_uint("premature_charge_done_count", 0);
        let thermal_health_degraded_count =
            anomalies_node.create_uint("thermal_health_degraded_count", 0);
        let overvoltage_count = anomalies_node.create_uint("overvoltage_count", 0);
        let last_anomaly_timestamp_ns = anomalies_node.create_int("last_anomaly_timestamp_ns", 0);
        let last_anomaly_reason = anomalies_node.create_string("last_anomaly_reason", "none");

        let update_processing_latency_us = diagnostics_root.create_uint_exponential_histogram(
            "update_processing_latency_us",
            ExponentialHistogramParams {
                floor: 10,
                initial_step: 10,
                step_multiplier: 2,
                buckets: 12,
            },
        );

        Self {
            raw_level_percent: RefCell::new(raw_level_percent_recorder),
            level_percent: RefCell::new(level_percent_recorder),
            previous_raw_level: RefCell::new(None),
            previous_level: RefCell::new(None),
            present_voltage: RefCell::new(present_voltage_recorder),
            remaining_capacity: RefCell::new(remaining_capacity_recorder),
            present_current: RefCell::new(present_current_recorder),
            average_current: RefCell::new(average_current_recorder),
            time_to_full: RefCell::new(time_to_full_recorder),
            temperature_mc: RefCell::new(temperature_mc_recorder),
            previous_temperature_mc: RefCell::new(None),
            full_capacity: RefCell::new(full_capacity_recorder),
            previous_full_capacity: RefCell::new(None),
            state_of_health_percent: RefCell::new(state_of_health_recorder),
            previous_state_of_health_percent: RefCell::new(None),
            health: RefCell::new(health_recorder),
            previous_health: RefCell::new(None),
            charge_status: RefCell::new(charge_status_recorder),
            previous_charge_status: RefCell::new(None),
            fault_detector,
            _diagnostics_root: diagnostics_root,
            _fidl_errors_node: fidl_errors_node,
            battery_watch_errors,
            battery_refresh_errors,
            charger_watch_errors,
            sag_lease_errors,
            watcher_send_errors,
            _disconnects_node: disconnects_node,
            battery_disconnect_count,
            charger_disconnect_count,
            _state_node: state_node,
            latest_temperature_mc,
            latest_full_capacity_uah,
            latest_design_capacity_uah,
            latest_state_of_health_percent,
            charging_wake_lease_active,
            wake_lease_acquisitions,
            last_update_timestamp_ns,
            _anomalies_node: anomalies_node,
            charging_zero_or_negative_current_count,
            premature_charge_done_count,
            thermal_health_degraded_count,
            overvoltage_count,
            last_anomaly_timestamp_ns,
            last_anomaly_reason,
            update_processing_latency_us,
        }
    }

    /// Records the raw (pre-polish) battery level, bundling the state-of-charge inputs
    /// alongside it.
    ///
    /// Voltage and remaining capacity are sampled here rather than on every driver update so
    /// that each one is time-aligned with the level it corresponds to. Together with a shared
    /// capacity, this keeps the three histories in lockstep, which is what makes them usable
    /// for assessing state-of-charge estimation.
    ///
    /// The raw level is the trigger rather than the polished one because the polisher
    /// rate-limits `level_percent`; sampling on polished transitions would skew the data away
    /// from the fuel gauge's actual level changes.
    pub fn record_raw_level_on_change(&self, info: &fidl_fuchsia_power_battery::BatteryInfo) {
        if let Some(level_to_publish) = info.level_percent {
            let val = level_to_publish.round() as u8;
            let mut previous_level = self.previous_raw_level.borrow_mut();
            if Some(val) != *previous_level {
                *previous_level = Some(val);
                self.raw_level_percent.borrow_mut().record(val);

                if let Some(voltage) = info.present_voltage_mv {
                    self.present_voltage.borrow_mut().record(voltage);
                }
                if let Some(capacity) = info.remaining_charge_uah {
                    self.remaining_capacity.borrow_mut().record(capacity);
                }
            }
        }
    }

    pub fn record_level_on_change(&self, info: &fidl_fuchsia_power_battery::BatteryInfo) {
        if let Some(level_to_publish) = info.level_percent {
            let val = level_to_publish.round() as u8;
            let mut previous_level = self.previous_level.borrow_mut();
            if Some(val) != *previous_level {
                *previous_level = Some(val);
                self.level_percent.borrow_mut().record(val);

                if let Some(fidl_fuchsia_power_battery::TimeRemaining::FullCharge(nanos)) =
                    info.time_remaining
                {
                    match u64::try_from(nanos) {
                        Ok(nanos_u64) => {
                            let seconds = std::time::Duration::from_nanos(nanos_u64).as_secs();
                            self.time_to_full.borrow_mut().record(seconds);
                        }
                        Err(_) => {
                            error!("Received negative time remaining: {}", nanos);
                        }
                    }
                }
            }
        }
    }

    pub fn record_present_current(&self, current: Option<i32>) {
        if let Some(current) = current {
            self.present_current.borrow_mut().record(current);
        }
    }

    pub fn record_average_current(&self, current: Option<i32>) {
        if let Some(current) = current {
            self.average_current.borrow_mut().record(current);
        }
    }

    pub fn record_temperature_on_change(&self, temp_mc: Option<i32>) {
        if let Some(temp_mc) = temp_mc {
            self.latest_temperature_mc.set(temp_mc as i64);
            let mut previous_temp = self.previous_temperature_mc.borrow_mut();
            if Some(temp_mc) != *previous_temp {
                *previous_temp = Some(temp_mc);
                self.temperature_mc.borrow_mut().record(temp_mc);
            }
        }
    }

    pub fn record_capacity_and_soh_on_change(&self, info: &fpower::BatteryInfo) {
        if let Some(full_uah) = info.full_capacity_uah {
            self.latest_full_capacity_uah.set(full_uah as i64);
            let mut previous_full = self.previous_full_capacity.borrow_mut();
            if Some(full_uah) != *previous_full {
                *previous_full = Some(full_uah);
                self.full_capacity.borrow_mut().record(full_uah);
            }
        }
        let design_uah = info.battery_spec.as_ref().and_then(|s| s.design_capacity_uah);
        if let Some(design_uah) = design_uah {
            self.latest_design_capacity_uah.set(design_uah as i64);
        }
        if let (Some(full_uah), Some(design_uah)) = (info.full_capacity_uah, design_uah) {
            if design_uah > 0 {
                let soh = ((full_uah as f64 / design_uah as f64) * 10000.0).round() / 100.0;
                self.latest_state_of_health_percent.set(soh);
                let mut previous_soh = self.previous_state_of_health_percent.borrow_mut();
                if Some(soh) != *previous_soh {
                    *previous_soh = Some(soh);
                    self.state_of_health_percent.borrow_mut().record(soh);
                }
            }
        }
    }

    pub fn record_health_on_change(
        &self,
        health: Option<fidl_fuchsia_power_battery::HealthStatus>,
    ) {
        if let Some(health) = health {
            let battery_health = BatteryHealth::from(health);
            let mut previous_health = self.previous_health.borrow_mut();
            if Some(battery_health) != *previous_health {
                *previous_health = Some(battery_health);
                self.health.borrow_mut().record(battery_health);
            }
        }
    }

    pub fn record_charge_status_on_change(
        &self,
        status: Option<fidl_fuchsia_power_battery::ChargeStatus>,
    ) {
        if let Some(status) = status {
            let battery_status = BatteryChargeStatus::from(status);
            let mut previous_status = self.previous_charge_status.borrow_mut();
            if Some(battery_status) != *previous_status {
                *previous_status = Some(battery_status);
                self.charge_status.borrow_mut().record(battery_status);
            }
        }
    }

    /// Publishes polished battery telemetry to both the `state_recorder` time-series histories
    /// and the `diagnostics` Inspect node.
    pub fn record_battery_update(&self, info: &fpower::BatteryInfo, premature_charge_done: bool) {
        self.record_level_on_change(info);
        self.record_present_current(info.present_charging_current_ua);
        self.record_average_current(info.average_charging_current_ua);
        self.record_temperature_on_change(info.temperature_mc);
        self.record_capacity_and_soh_on_change(info);
        self.record_health_on_change(info.health);
        self.record_charge_status_on_change(info.charge_status);

        let timestamp_ns = info.timestamp.unwrap_or(0);
        self.last_update_timestamp_ns.set(timestamp_ns);

        if premature_charge_done {
            self.premature_charge_done_count.add(1);
            self.last_anomaly_timestamp_ns.set(timestamp_ns);
            self.last_anomaly_reason.set("premature_charge_done_below_95_percent");
        }

        if info.charge_status == Some(fpower::ChargeStatus::Charging) {
            if let Some(current_ua) = info.present_charging_current_ua {
                if current_ua <= 0 {
                    self.charging_zero_or_negative_current_count.add(1);
                    self.last_anomaly_timestamp_ns.set(timestamp_ns);
                    self.last_anomaly_reason.set("charging_with_non_positive_current");
                }
            }
        }

        match info.health {
            Some(
                fpower::HealthStatus::Cold
                | fpower::HealthStatus::Warm
                | fpower::HealthStatus::Hot
                | fpower::HealthStatus::Overheat,
            ) => {
                self.thermal_health_degraded_count.add(1);
                self.last_anomaly_timestamp_ns.set(timestamp_ns);
                self.last_anomaly_reason.set("thermal_health_degraded");
            }
            Some(fpower::HealthStatus::OverVoltage) => {
                self.overvoltage_count.add(1);
                self.last_anomaly_timestamp_ns.set(timestamp_ns);
                self.last_anomaly_reason.set("overvoltage");
            }
            _ => {}
        }
    }

    pub fn record_battery_watch_error(&self) {
        self.battery_watch_errors.add(1);
    }

    pub fn record_battery_refresh_error(&self) {
        self.battery_refresh_errors.add(1);
    }

    pub fn record_charger_watch_error(&self) {
        self.charger_watch_errors.add(1);
    }

    pub fn record_sag_lease_error(&self) {
        self.sag_lease_errors.add(1);
    }

    pub fn record_watcher_send_error(&self) {
        self.watcher_send_errors.add(1);
    }

    pub fn record_charger_disconnect(&self) {
        self.charger_disconnect_count.add(1);
    }

    pub fn record_wake_lease_state(&self, active: bool, newly_acquired: bool) {
        self.charging_wake_lease_active.set(active);
        if newly_acquired {
            self.wake_lease_acquisitions.add(1);
        }
    }

    pub fn record_update_latency_us(&self, latency_us: u64) {
        self.update_processing_latency_us.insert(latency_us);
    }

    fn create_recorder<T>(
        name: &str,
        unit: Units,
        capacity: usize,
        persistence: PersistenceOptions,
    ) -> NumericStateRecorder<T>
    where
        T: RecordableNumericType,
    {
        NumericStateRecorder::new(
            name.to_string(),
            c"power",
            unit,
            None,
            RecorderOptions {
                lazy_record: true,
                capacity,
                manager: None,
                persistence: Some(persistence),
            },
        )
        .unwrap_or_else(|_| panic!("{} construction failed", name))
    }

    fn create_enum_recorder<T>(
        name: &str,
        capacity: usize,
        persistence: PersistenceOptions,
    ) -> EnumStateRecorder<T>
    where
        T: RecordableEnum + 'static,
    {
        EnumStateRecorder::new(
            name.to_string(),
            c"power",
            RecorderOptions {
                lazy_record: true,
                capacity,
                manager: None,
                persistence: Some(persistence),
            },
        )
        .unwrap_or_else(|_| panic!("{} construction failed", name))
    }

    /// Public method called by the BatteryManager when a level update occurs.
    /// Returns FaultRecoveryEvent.
    pub fn update(
        &self,
        new_raw_level: Option<f32>,
        new_charge_status: Option<ChargeStatus>,
    ) -> FaultRecoveryEvent {
        self.fault_detector.update(new_raw_level, new_charge_status)
    }

    pub fn record_disconnected(&self) -> FaultRecoveryEvent {
        self.battery_disconnect_count.add(1);
        self.fault_detector.record_disconnected()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::{TempDir, tempdir};

    // Helper to check if the detector is currently in a fault state
    fn get_fault_state(detector: &FaultDetector) -> FaultState {
        detector.state.borrow().current_fault
    }

    fn create_detector(
        timeout: zx::Duration<zx::MonotonicTimeline>,
    ) -> (TempDir, Rc<FaultDetector>) {
        let dir = tempdir().unwrap();
        let storage_dir = dir.path().join("data");
        let volatile_dir = dir.path().join("tmp");
        fs::create_dir(&storage_dir).unwrap();
        fs::create_dir(&volatile_dir).unwrap();
        let detector = FaultDetector::new(
            timeout,
            Some(PersistenceDirs {
                storage_dir: storage_dir.to_str().unwrap().to_string(),
                volatile_dir: volatile_dir.to_str().unwrap().to_string(),
            }),
        );
        (dir, detector)
    }

    #[fuchsia::test]
    fn test_fault_detector_trigger_and_recovery() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let timeout = zx::Duration::from_minutes(5);
        let (_dir, detector) = create_detector(timeout);

        // --- PHASE 1: Initial State ---
        assert_eq!(get_fault_state(&detector), FaultState::None);

        assert_eq!(detector.notify_state_change(), FaultRecoveryEvent::None);
        assert_eq!(get_fault_state(&detector), FaultState::None);

        // --- PHASE 2: Advance time past timeout ---
        let deadline = fasync::MonotonicInstant::now() + timeout + zx::Duration::from_seconds(1);
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        executor.set_fake_time(deadline.into());
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Now it should be in NoUpdate state
        assert_eq!(get_fault_state(&detector), FaultState::NoUpdate);

        // --- PHASE 3: Recovery ---
        // Sending a new update should immediately clear the fault
        assert_eq!(detector.notify_state_change(), FaultRecoveryEvent::Recovered);
        assert_eq!(get_fault_state(&detector), FaultState::None);

        // --- PHASE 4: Stopping ---
        // Stop the detector (simulating FULL battery)
        assert_eq!(detector.stop(), FaultRecoveryEvent::None);
        // Advance time another 10 minutes
        let future_time = fasync::MonotonicInstant::now() + zx::Duration::from_minutes(10);
        executor.set_fake_time(future_time);
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Fault should still be None because the task was canceled by stop()
        assert_eq!(get_fault_state(&detector), FaultState::None);

        // --- Phase 5: Resume from stopping ---
        assert_eq!(detector.notify_state_change(), FaultRecoveryEvent::None);
        let deadline = fasync::MonotonicInstant::now() + timeout + zx::Duration::from_seconds(1);
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        executor.set_fake_time(deadline);
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Should be NoUpdate again
        assert_eq!(get_fault_state(&detector), FaultState::NoUpdate);
    }

    #[fuchsia::test]
    fn test_fault_detector_reset_prevents_fault() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let timeout = zx::Duration::from_minutes(5);
        let (_dir, detector) = create_detector(timeout);

        // --- T = 0 minutes ---
        assert_eq!(detector.notify_state_change(), FaultRecoveryEvent::None);
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Advance time to 4 minutes (not yet timed out)
        let middle_time = fasync::MonotonicInstant::now() + zx::Duration::from_minutes(4);
        executor.set_fake_time(middle_time);
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Fault should still be None
        assert_eq!(get_fault_state(&detector), FaultState::None);

        // --- T = 4 minutes: RESET ---
        // This cancels the timer that was supposed to fire at 5m
        // and starts a new timer that will fire at 9m (4m + 5m).
        assert_eq!(detector.notify_state_change(), FaultRecoveryEvent::None);
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // --- T = 6 minutes ---
        // Advance time by 2 more minutes.
        // Total elapsed virtual time is now 6 minutes.
        let check_time = fasync::MonotonicInstant::now() + zx::Duration::from_minutes(2);
        executor.set_fake_time(check_time);
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Fault should still be None because the new deadline (9m) hasn't passed.
        // If the reset failed, the fault would have triggered at 5m.
        assert_eq!(get_fault_state(&detector), FaultState::None);

        // --- T = 10 minutes (Final verification) ---
        // Advance time past the second deadline to prove it still works.
        let final_time = fasync::MonotonicInstant::now() + zx::Duration::from_minutes(4);
        executor.set_fake_time(final_time);
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        assert_eq!(get_fault_state(&detector), FaultState::NoUpdate);
    }

    #[fuchsia::test]
    fn test_fault_detector_thresholds_and_transitions() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let timeout = zx::Duration::from_minutes(5);
        let (_dir, detector) = create_detector(timeout);

        // --- PHASE 1: Normal Charging (<= 95%) ---
        // Start at 90%
        assert_eq!(
            detector.update(Some(90.0), Some(ChargeStatus::Charging)),
            FaultRecoveryEvent::None
        );
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(detector.state.borrow().current_fault, FaultState::None);

        // Advance 6 minutes without level change -> Should Fault
        executor.set_fake_time(fasync::MonotonicInstant::now() + zx::Duration::from_minutes(6));
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(detector.state.borrow().current_fault, FaultState::NoUpdate);

        // --- PHASE 2: Crossing the 95% Threshold while Charging ---
        // Level jumps to 96%. This should call stop() and clear the fault.
        assert_eq!(
            detector.update(Some(96.0), Some(ChargeStatus::Charging)),
            FaultRecoveryEvent::Recovered
        );
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(
            detector.state.borrow().current_fault,
            FaultState::None,
            "Fault should clear > 95%"
        );

        // Advance 1 hour -> should still be None because timer is stopped
        executor.set_fake_time(fasync::MonotonicInstant::now() + zx::Duration::from_hours(1));
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(detector.state.borrow().current_fault, FaultState::None);

        assert_eq!(
            detector.update(Some(97.0), Some(ChargeStatus::Charging)),
            FaultRecoveryEvent::None
        );
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(
            detector.state.borrow().current_fault,
            FaultState::None,
            "Fault should clear > 95%"
        );

        // Advance 1 hour -> should still be None because timer is stopped
        executor.set_fake_time(fasync::MonotonicInstant::now() + zx::Duration::from_hours(1));
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(detector.state.borrow().current_fault, FaultState::None);

        // --- PHASE 3: Discharging at High Level ---
        // User unplugs at 96%. Discharging should re-enable the watchdog even if > 95%.
        assert_eq!(
            detector.update(Some(96.0), Some(ChargeStatus::Discharging)),
            FaultRecoveryEvent::None
        );
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Advance 6 minutes without level change -> Should Fault again
        executor.set_fake_time(fasync::MonotonicInstant::now() + zx::Duration::from_minutes(6));
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(
            detector.state.borrow().current_fault,
            FaultState::NoUpdate,
            "Discharge should monitor regardless of level"
        );

        // --- PHASE 4: Recovery via Discharge Progress ---
        // Level drops to 95% while discharging -> Should clear fault
        assert_eq!(
            detector.update(Some(95.0), Some(ChargeStatus::Discharging)),
            FaultRecoveryEvent::Recovered
        );
        assert_eq!(detector.state.borrow().current_fault, FaultState::None);
    }

    #[fuchsia::test]
    fn test_fault_detector_status_change_without_level_results_in_fault() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let timeout = zx::Duration::from_minutes(5);
        let (_dir, detector) = create_detector(timeout);

        // Start at 100% and Full status (watchdog is stopped)
        assert_eq!(
            detector.update(Some(100.0), Some(ChargeStatus::Full)),
            FaultRecoveryEvent::None
        );
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Advance 6 minutes and ensure no fault
        executor.set_fake_time(fasync::MonotonicInstant::now() + zx::Duration::from_minutes(6));
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(get_fault_state(&detector), FaultState::None);

        // Change status to Discharging with the same battery level. This should start the watchdog
        // monitoring window.
        assert_eq!(
            detector.update(Some(100.0), Some(ChargeStatus::Discharging)),
            FaultRecoveryEvent::None
        );
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Advance time by another 6 minutes, past the new 5-minute window. Since no levels have
        // been reported, a NoUpdate fault should be triggered.
        executor.set_fake_time(fasync::MonotonicInstant::now() + zx::Duration::from_minutes(6));
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(
            get_fault_state(&detector),
            FaultState::NoUpdate,
            "Watchdog should have started and fired after status change"
        );
    }

    #[fuchsia::test]
    fn test_fault_detector_charging_status_change_without_level_restarts_watchdog() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let timeout = zx::Duration::from_minutes(5);
        let (_dir, detector) = create_detector(timeout);

        // Start at 50% and NotCharging status (watchdog is stopped)
        assert_eq!(
            detector.update(Some(50.0), Some(ChargeStatus::NotCharging)),
            FaultRecoveryEvent::None
        );
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Advance 6 minutes and ensure no fault
        executor.set_fake_time(fasync::MonotonicInstant::now() + zx::Duration::from_minutes(6));
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(get_fault_state(&detector), FaultState::None);

        // Change status to Charging with the same battery level (50%).
        // This should start the watchdog.
        assert_eq!(
            detector.update(Some(50.0), Some(ChargeStatus::Charging)),
            FaultRecoveryEvent::None
        );
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Advance time by 6 minutes. A NoUpdate fault should be triggered.
        executor.set_fake_time(fasync::MonotonicInstant::now() + zx::Duration::from_minutes(6));
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(
            get_fault_state(&detector),
            FaultState::NoUpdate,
            "Watchdog should have started and fired after status change to Charging"
        );
    }

    #[fuchsia::test]
    fn test_fault_detector_status_change_without_level_does_not_clear_existing_fault() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let timeout = zx::Duration::from_minutes(5);
        let (_dir, detector) = create_detector(timeout);

        // Start at 50% and Charging status
        assert_eq!(
            detector.update(Some(50.0), Some(ChargeStatus::Charging)),
            FaultRecoveryEvent::None
        );
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Advance 6 minutes to trigger the NoUpdate fault
        executor.set_fake_time(fasync::MonotonicInstant::now() + zx::Duration::from_minutes(6));
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(get_fault_state(&detector), FaultState::NoUpdate);

        // Change status to Discharging but keep the same battery level (50%)
        // This should NOT clear the NoUpdate fault since the battery level has not progressed.
        assert_eq!(
            detector.update(Some(50.0), Some(ChargeStatus::Discharging)),
            FaultRecoveryEvent::None
        );
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(get_fault_state(&detector), FaultState::NoUpdate);

        // Advance time by 6 minutes, the fault should still be NoUpdate
        executor.set_fake_time(fasync::MonotonicInstant::now() + zx::Duration::from_minutes(6));
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert_eq!(get_fault_state(&detector), FaultState::NoUpdate);

        // Now send a level update (e.g. 49% while discharging) to recover from the fault
        assert_eq!(
            detector.update(Some(49.0), Some(ChargeStatus::Discharging)),
            FaultRecoveryEvent::Recovered
        );
        assert_eq!(get_fault_state(&detector), FaultState::None);
    }

    #[fuchsia::test]
    fn test_fault_detector_driver_disconnected() {
        let _executor = fasync::TestExecutor::new_with_fake_time();
        let timeout = zx::Duration::from_minutes(5);
        let (_dir, detector) = create_detector(timeout);

        assert_eq!(get_fault_state(&detector), FaultState::None);

        detector.record_disconnected();
        assert_eq!(get_fault_state(&detector), FaultState::DriverDisconnected);
    }

    #[fuchsia::test]
    async fn test_recorders_diagnostics_and_state_recorder_histories() {
        use diagnostics_assertions::{AnyProperty, assert_data_tree};

        let dir = tempdir().unwrap();
        let storage_path = dir.path().join("storage");
        let volatile_path = dir.path().join("volatile");
        fs::create_dir(&storage_path).unwrap();
        fs::create_dir(&volatile_path).unwrap();

        let config = RecorderConfig {
            persistence_dirs: Some(PersistenceDirs {
                storage_dir: storage_path.to_str().unwrap().to_string(),
                volatile_dir: volatile_path.to_str().unwrap().to_string(),
            }),
        };

        let recorders = BatteryInfoRecorders::new(config);

        recorders.record_battery_watch_error();
        recorders.record_battery_watch_error();
        recorders.record_battery_refresh_error();
        recorders.record_charger_watch_error();
        recorders.record_sag_lease_error();
        recorders.record_watcher_send_error();
        recorders.record_disconnected();
        recorders.record_charger_disconnect();
        recorders.record_wake_lease_state(true, true);

        let info = fpower::BatteryInfo {
            status: Some(fpower::BatteryStatus::Ok),
            charge_status: Some(fpower::ChargeStatus::Charging),
            level_percent: Some(80.0),
            present_voltage_mv: Some(4650),
            present_charging_current_ua: Some(-50_000),
            temperature_mc: Some(48_000),
            full_capacity_uah: Some(4_500_000),
            battery_spec: Some(fpower::BatterySpec {
                design_capacity_uah: Some(5_000_000),
                ..Default::default()
            }),
            health: Some(fpower::HealthStatus::Warm),
            timestamp: Some(123_456_789),
            ..Default::default()
        };

        recorders.record_battery_update(&info, true);
        recorders.record_update_latency_us(45);

        let global_inspector = inspect::component::inspector();
        assert_data_tree!(global_inspector, root: contains {
            diagnostics: {
                fidl_errors: {
                    battery_watch_errors: 2u64,
                    battery_refresh_errors: 1u64,
                    charger_watch_errors: 1u64,
                    sag_lease_errors: 1u64,
                    watcher_send_errors: 1u64,
                },
                disconnects: {
                    battery_disconnect_count: 1u64,
                    charger_disconnect_count: 1u64,
                },
                state: {
                    temperature_mc: 48_000i64,
                    full_capacity_uah: 4_500_000i64,
                    design_capacity_uah: 5_000_000i64,
                    state_of_health_percent: 90.0f64,
                    charging_wake_lease_active: true,
                    wake_lease_acquisitions: 1u64,
                    last_update_timestamp_ns: 123_456_789i64,
                },
                anomalies: {
                    charging_zero_or_negative_current_count: 1u64,
                    premature_charge_done_count: 1u64,
                    thermal_health_degraded_count: 1u64,
                    overvoltage_count: 0u64,
                    last_anomaly_timestamp_ns: 123_456_789i64,
                    last_anomaly_reason: "thermal_health_degraded",
                },
                update_processing_latency_us: AnyProperty,
            },
            power_observability_state_recorders: contains {
                temperature_mc: contains {
                    history: contains {
                        shards: contains {
                            "0": contains {
                                values: vec![48_000i64],
                            }
                        }
                    }
                },
                full_capacity: contains {
                    history: contains {
                        shards: contains {
                            "0": contains {
                                values: vec![4_500_000i64],
                            }
                        }
                    }
                },
                state_of_health_percent: contains {
                    history: contains {
                        shards: contains {
                            "0": contains {
                                values: vec![90.0f64],
                            }
                        }
                    }
                },
            }
        });
    }
}
