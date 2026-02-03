// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use fidl_fuchsia_power_battery::ChargeStatus;
use fuchsia_async as fasync;
use fuchsia_inspect::{self as inspect};
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use log::{error, warn};
use state_recorder::{
    EnumStateRecorder, NumericStateRecorder, PersistenceOptions, RecordableNumericType,
    RecorderOptions, Units, units,
};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write;
use std::fs::{self as fs, OpenOptions, read_to_string};
use std::io::Write as OtherWrite;
use std::rc::Rc;
use std::str::FromStr;
use strum_macros::{Display, EnumIter, FromRepr};

static BATTERY_LEVEL_HEADER: &str = "# BATTERY LEVEL";
static CHARGE_STATUS_HEADER: &str = "# CHARGE STATUS";
static BATTERY_HISTORY_FILE_FOR_RENAME: &str = "/data/history_before_rename.txt";

const MAX_BATTERY_LEVEL_MEASUREMENTS: usize = 200;
const MAX_FAULT_MEASUREMENTS: usize = 20;
const MAX_POWER_CONSUMPTION_MEASUREMENTS: usize = 20;
const STALE_DATA_TIMER: zx::Duration<zx::MonotonicTimeline> = zx::Duration::from_minutes(10);

#[derive(Copy, Clone, Debug, Display, EnumIter, Eq, PartialEq, Hash, FromRepr)]
#[repr(u8)]
pub enum FaultState {
    None = 0,
    NoUpdate = 1,
}

impl From<FaultState> for u64 {
    fn from(value: FaultState) -> Self {
        value as Self
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
        let prev_raw_level = self.state.borrow().previous_raw_level;
        let mut recovery_event = FaultRecoveryEvent::None;

        match new_charge_status {
            Some(ChargeStatus::Charging) => {
                if let Some(level) = new_raw_level {
                    if level <= 95.0 {
                        // Only notify if the level is actually new
                        if new_raw_level != prev_raw_level {
                            recovery_event = self.notify_level_change();
                        }
                    } else {
                        recovery_event = self.stop();
                    }
                }
            }
            Some(ChargeStatus::Discharging) => {
                if new_raw_level.is_some() && new_raw_level != prev_raw_level {
                    recovery_event = self.notify_level_change();
                }
            }
            Some(ChargeStatus::Full) | Some(ChargeStatus::NotCharging) => {
                recovery_event = self.stop();
            }
            _ => {}
        }
        self.state.borrow_mut().previous_raw_level = new_raw_level;
        recovery_event
    }

    /// Returns FaultRecoveryEvent.
    fn notify_level_change(self: &Rc<Self>) -> FaultRecoveryEvent {
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
                    "FAULT DETECTED: No battery updates received for more than {:?}. (Last update: {:?}) nanos",
                    timeout, last_update
                );
            }
        }));
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

        if old_fault == FaultState::NoUpdate && new_fault == FaultState::None {
            FaultRecoveryEvent::Recovered
        } else {
            FaultRecoveryEvent::None
        }
    }
}

#[derive(Clone)]
pub struct HistoryLoggerConfig {
    pub curr_boot_path: String,
    pub prev_boot_path: String,
    pub battery_level_buffer_capacity: usize,
    pub charge_status_buffer_capacity: usize,
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
    fault_detector: Rc<FaultDetector>,
}

impl BatteryInfoRecorders {
    pub fn new(config: RecorderConfig) -> Self {
        let mut raw_level_percent_opts = PersistenceOptions::new("raw_level_percent".to_string());
        let mut level_percent_opts = PersistenceOptions::new("level_percent".to_string());
        let mut present_voltage_opts = PersistenceOptions::new("present_voltage".to_string());
        let mut remaining_capacity_opts = PersistenceOptions::new("remaining_capacity".to_string());
        let mut present_current_opts = PersistenceOptions::new("charge_current".to_string());
        let mut average_current_opts = PersistenceOptions::new("average_current".to_string());

        let fault_detector = FaultDetector::new(STALE_DATA_TIMER, config.persistence_dirs.clone());

        // Apply overrides if they exist
        if let Some(PersistenceDirs { storage_dir, volatile_dir }) = &config.persistence_dirs {
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
        }

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
            MAX_POWER_CONSUMPTION_MEASUREMENTS,
            present_voltage_opts,
        );
        let remaining_capacity_recorder = Self::create_recorder(
            "remaining_capacity",
            units!(Micro, AmpHours),
            MAX_POWER_CONSUMPTION_MEASUREMENTS,
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

        Self {
            raw_level_percent: RefCell::new(raw_level_percent_recorder),
            level_percent: RefCell::new(level_percent_recorder),
            previous_raw_level: RefCell::new(None),
            previous_level: RefCell::new(None),
            present_voltage: RefCell::new(present_voltage_recorder),
            remaining_capacity: RefCell::new(remaining_capacity_recorder),
            present_current: RefCell::new(present_current_recorder),
            average_current: RefCell::new(average_current_recorder),
            fault_detector,
        }
    }

    pub fn record_raw_level_on_change(&self, level: Option<f32>) {
        if let Some(level_to_publish) = level {
            let val = level_to_publish.round() as u8;
            let mut previous_level = self.previous_raw_level.borrow_mut();
            if Some(val) != *previous_level {
                *previous_level = Some(val);
                self.raw_level_percent.borrow_mut().record(val);
            }
        }
    }

    pub fn record_level_on_change(&self, level: Option<f32>) {
        if let Some(level_to_publish) = level {
            let val = level_to_publish.round() as u8;
            let mut previous_level = self.previous_level.borrow_mut();
            if Some(val) != *previous_level {
                *previous_level = Some(val);
                self.level_percent.borrow_mut().record(val);
            }
        }
    }

    pub fn record_present_voltage(&self, voltage: Option<u32>) {
        if let Some(voltage) = voltage {
            self.present_voltage.borrow_mut().record(voltage);
        }
    }

    pub fn record_remaining_capacity(&self, capacity: Option<u32>) {
        if let Some(capacity) = capacity {
            self.remaining_capacity.borrow_mut().record(capacity);
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

    /// Public method called by the BatteryManager when a level update occurs.
    /// Returns FaultRecoveryEvent.
    pub fn update(
        &self,
        new_raw_level: Option<f32>,
        new_charge_status: Option<ChargeStatus>,
    ) -> FaultRecoveryEvent {
        self.fault_detector.update(new_raw_level, new_charge_status)
    }
}

/// Manages publishing historical battery data to Inspect.
pub struct HistoryLogger {
    /// Inspect node for historical battery measurements.
    root: inspect::Node,

    config: HistoryLoggerConfig,

    battery_history: VecDeque<(zx::BootInstant, i64)>,
    charge_history: VecDeque<(zx::BootInstant, String)>,

    battery_history_inspect: BoundedListNode,
    charge_history_inspect: BoundedListNode,

    _prev_battery_history_inspect: BoundedListNode,
    _prev_charge_history_inspect: BoundedListNode,

    should_persist: bool,

    prev_charge_status: ChargeStatus,

    temporary_file_for_renaming: String,
}

impl HistoryLogger {
    pub fn from_file(root: &inspect::Node, config: HistoryLoggerConfig) -> Self {
        let (curr_battery_level, curr_charge_status) = read_history(&config.curr_boot_path);
        let (prev_battery_level, prev_charge_status) = read_history(&config.prev_boot_path);

        let root = root.create_child("historical_data");
        let battery_history_inspect = init_battery_level_node(
            &root,
            "battery_level",
            &curr_battery_level,
            config.battery_level_buffer_capacity,
        );
        let charge_history_inspect = init_charge_status_node(
            &root,
            "charge_status_changes",
            &curr_charge_status,
            config.charge_status_buffer_capacity,
        );
        let _prev_battery_history_inspect = init_battery_level_node(
            &root,
            "previous_boot_battery_level",
            &prev_battery_level,
            prev_battery_level.len(),
        );
        let _prev_charge_history_inspect = init_charge_status_node(
            &root,
            "previous_boot_charge_status_changes",
            &prev_charge_status,
            prev_charge_status.len(),
        );

        Self {
            root,
            config,
            battery_history: VecDeque::from(curr_battery_level),
            charge_history: VecDeque::from(curr_charge_status),
            battery_history_inspect,
            charge_history_inspect,
            _prev_battery_history_inspect,
            _prev_charge_history_inspect,
            should_persist: true,
            prev_charge_status: ChargeStatus::Unknown,
            temporary_file_for_renaming: BATTERY_HISTORY_FILE_FOR_RENAME.to_string(),
        }
    }

    #[cfg(test)]
    pub fn change_temporary_file_for_renaming_for_test(&mut self, path: String) {
        self.temporary_file_for_renaming = path;
    }

    /// Adds a new battery level entry, rotating the buffer if need be. The new value is published
    /// to inspect and persisted.
    pub fn add_battery_level(&mut self, timestamp: zx::BootInstant, level: i32) {
        self.battery_history_inspect.add_entry(|node| {
            node.record_int("@time", timestamp.into_nanos());
            node.record_int("level", level.into());
        });

        if !self.should_persist {
            return;
        }

        self.battery_history.push_back((timestamp, level.into()));
        if self.battery_history.len() > self.config.battery_level_buffer_capacity {
            self.battery_history.pop_front();
        }
        if let Err(e) = write_history(
            &self.config.curr_boot_path,
            &self.battery_history,
            &self.charge_history,
            &self.temporary_file_for_renaming,
        ) {
            self.handle_write_error(e);
        }
    }

    pub fn update_charge_status(
        &mut self,
        timestamp: zx::BootInstant,
        new_status: Option<ChargeStatus>,
    ) {
        if let Some(status) = new_status {
            if status != self.prev_charge_status {
                self.add_charge_status(timestamp, status);
                self.prev_charge_status = status;
            }
        }
    }

    /// Adds a new charge status entry, rotating the buffer if need be. The new value is published
    /// to inspect and persisted.
    pub fn add_charge_status(&mut self, timestamp: zx::BootInstant, status: ChargeStatus) {
        // Each Android charge status maps 1-to-1 to a Fuchsia charges status. However,
        // Fuchsia charge statuses should be used if more are introduced and mapped to
        // Android statuses.
        let status = match status {
            ChargeStatus::NotCharging => "NOT_CHARGING",
            ChargeStatus::Charging => "CHARGING",
            ChargeStatus::Discharging => "DISCHARGING",
            ChargeStatus::Full => "FULL",
            ChargeStatus::Unknown => "UNKNOWN",
        };

        self.charge_history_inspect.add_entry(|node| {
            node.record_int("@time", timestamp.into_nanos());
            node.record_string("status", status);
        });

        if !self.should_persist {
            return;
        }

        self.charge_history.push_back((timestamp, status.to_string()));
        if self.charge_history.len() > self.config.charge_status_buffer_capacity {
            self.charge_history.pop_front();
        }
        if let Err(e) = write_history(
            &self.config.curr_boot_path,
            &self.battery_history,
            &self.charge_history,
            &self.temporary_file_for_renaming,
        ) {
            self.handle_write_error(e);
        }
    }

    /// Handles a write error by logging an error, cleaning up in-memory state, and stopping future
    /// writes.
    fn handle_write_error(&mut self, error: Error) {
        error!("error persisting history, stopping: {:?}", error);
        self.root.record_bool("not_persisting", true);
        self.battery_history.clear();
        self.charge_history.clear();
        self.should_persist = false;
    }
}

fn init_battery_level_node(
    root: &inspect::Node,
    name: &str,
    values: &Vec<(zx::BootInstant, i64)>,
    capacity: usize,
) -> BoundedListNode {
    let mut node = BoundedListNode::new(root.create_child(name), capacity);
    for (timestamp, level) in values.iter() {
        node.add_entry(|node| {
            node.record_int("@time", timestamp.into_nanos());
            node.record_int("level", *level);
        });
    }
    return node;
}

fn init_charge_status_node(
    root: &inspect::Node,
    name: &str,
    values: &Vec<(zx::BootInstant, String)>,
    capacity: usize,
) -> BoundedListNode {
    let mut node = BoundedListNode::new(root.create_child(name), capacity);
    for (timestamp, status) in values.iter() {
        node.add_entry(|node| {
            node.record_int("@time", timestamp.into_nanos());
            node.record_string("status", status);
        });
    }
    return node;
}

/// Writes battery levels and charge status changes to a file in a format like:
///
/// # BATTERY LEVEL
/// $TIMESTAMP,$LEVEL
/// $TIMESTAMP,$LEVEL
/// $TIMESTAMP,$LEVEL
/// ...
///
/// # CHARGE STATUS
/// $TIMESTAMP,$STATUS
/// $TIMESTAMP,$STATUS
/// $TIMESTAMP,$STATUS
fn write_history(
    curr_boot_path: &str,
    battery_history: &VecDeque<(zx::BootInstant, i64)>,
    charge_history: &VecDeque<(zx::BootInstant, String)>,
    temporary_file_for_renaming: &str,
) -> Result<(), Error> {
    let mut content = String::new();

    // Write battery levels to their own section
    writeln!(&mut content, "{}", BATTERY_LEVEL_HEADER)?;
    for (timestamp, level) in battery_history.iter() {
        writeln!(&mut content, "{},{}", timestamp.into_nanos(), level)?;
    }

    // Add a new line
    writeln!(&mut content, "")?;

    // Write charge status changes to their own section
    writeln!(&mut content, "{}", CHARGE_STATUS_HEADER)?;
    for (timestamp, status) in charge_history.iter() {
        writeln!(&mut content, "{},{}", timestamp.into_nanos(), status)?;
    }

    // Write to temporary_file_for_renaming and then rename to curr_boot_path
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temporary_file_for_renaming)?;
    write!(file, "{}", content)?;

    file.sync_data()?;
    fs::rename(temporary_file_for_renaming, curr_boot_path)?;
    Ok(())
}

/// Reads battery levels and charge status changes from a file in a format like:
///
/// # BATTERY LEVEL
/// $TIMESTAMP,$LEVEL
/// $TIMESTAMP,$LEVEL
/// $TIMESTAMP,$LEVEL
/// ...
///
/// # CHARGE STATUS
/// $TIMESTAMP,$STATUS
/// $TIMESTAMP,$STATUS
/// $TIMESTAMP,$STATUS
fn read_history(
    curr_boot_path: &str,
) -> (Vec<(zx::BootInstant, i64)>, Vec<(zx::BootInstant, String)>) {
    let content = match read_to_string(curr_boot_path) {
        Ok(c) => c,
        Err(e) => {
            warn!("Error reading history from {}: {}", curr_boot_path, e);
            return (Vec::new(), Vec::new());
        }
    };

    let mut lines = content.lines();
    if let Some(l) = lines.next() {
        if l.trim() != BATTERY_LEVEL_HEADER {
            warn!("Error reading history from {}: battery level header missing", curr_boot_path);
            return (Vec::new(), Vec::new());
        }
    } else {
        warn!("Error reading history from {}: empty file", curr_boot_path);
        return (Vec::new(), Vec::new());
    }

    let mut battery_level = Vec::new();
    while let Some(l) = lines.next() {
        if l.trim().is_empty() {
            break;
        }

        if let Some(v) = parse_line::<i64>(l) {
            battery_level.push(v);
        } else {
            warn!("Error reading battery level line '{}', skipping", l);
        }
    }

    if let Some(l) = lines.next() {
        if l.trim() != CHARGE_STATUS_HEADER {
            warn!("Error reading history from {}: charge status header missing", curr_boot_path);
            return (battery_level, Vec::new());
        }
    } else {
        warn!("Error reading history from {}: charge status header missing", curr_boot_path);
        return (battery_level, Vec::new());
    }

    let mut charge_status = Vec::new();
    while let Some(l) = lines.next() {
        if l.trim().is_empty() {
            break;
        }

        if let Some(v) = parse_line::<String>(l) {
            charge_status.push(v);
        } else {
            warn!("Error reading charge status line '{}', skipping", l);
        }
    }

    return (battery_level, charge_status);
}

fn parse_line<T: FromStr>(line: &str) -> Option<(zx::BootInstant, T)> {
    let mut parts = line.splitn(2, ',');
    if let (Some(ts), Some(val)) = (parts.next(), parts.next()) {
        let ts = ts.trim().parse::<i64>();
        let val = val.trim().parse::<T>();

        if let (Ok(ts), Ok(val)) = (ts, val) {
            return Some((zx::BootInstant::from_nanos(ts), val));
        }
    }
    return None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::assert_data_tree;
    use std::fs::write;
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

        assert_eq!(detector.notify_level_change(), FaultRecoveryEvent::None);
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
        assert_eq!(detector.notify_level_change(), FaultRecoveryEvent::Recovered);
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
        assert_eq!(detector.notify_level_change(), FaultRecoveryEvent::None);
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
        assert_eq!(detector.notify_level_change(), FaultRecoveryEvent::None);
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
        assert_eq!(detector.notify_level_change(), FaultRecoveryEvent::None);
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

    fn create_config(
        dir: &TempDir,
        battery_level_buffer_capacity: usize,
        charge_status_buffer_capacity: usize,
        curr_boot_file: &str,
        prev_boot_file: &str,
    ) -> HistoryLoggerConfig {
        HistoryLoggerConfig {
            curr_boot_path: dir.path().join(curr_boot_file).to_str().unwrap().to_string(),
            prev_boot_path: dir.path().join(prev_boot_file).to_str().unwrap().to_string(),
            battery_level_buffer_capacity,
            charge_status_buffer_capacity,
        }
    }

    #[fuchsia::test]
    async fn test_inspect_schema() {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();

        let config = create_config(&dir, 1, 1, "curr_data.txt", "prev_data.txt");
        let _logger = HistoryLogger::from_file(inspector.root(), config);

        // Please notify an OWNER of //src/developer/forensics if this assertion is changed. Some
        // tools might depend on this inspect.
        assert_data_tree!(
            inspector,
            root: {
                historical_data: {
                    battery_level: {},
                    charge_status_changes: {},
                    previous_boot_battery_level: {},
                    previous_boot_charge_status_changes: {},
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_limits_battery_levels_inspect() {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();

        let config = create_config(&dir, 2, 1, "curr_data.txt", "prev_data.txt");
        let mut logger = HistoryLogger::from_file(inspector.root(), config);
        logger.change_temporary_file_for_renaming_for_test(
            dir.path().join("tmp.txt").to_str().unwrap().to_string(),
        );

        logger.add_battery_level(zx::BootInstant::from_nanos(1234), 12);
        logger.add_battery_level(zx::BootInstant::from_nanos(2345), 13);
        logger.add_battery_level(zx::BootInstant::from_nanos(3456), 14);

        assert_data_tree!(
            inspector,
            root: {
                historical_data: {
                    battery_level: {
                       "1": {
                                "@time": 2345,
                                "level": 13,
                        },
                        "2": {
                                "@time": 3456,
                                "level": 14,
                        },
                    },
                    charge_status_changes: {},
                    previous_boot_battery_level: {},
                    previous_boot_charge_status_changes: {},
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_limits_battery_levels_file() {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();

        let config = create_config(&dir, 2, 1, "curr_data.txt", "prev_data.txt");
        let mut logger = HistoryLogger::from_file(inspector.root(), config.clone());
        logger.change_temporary_file_for_renaming_for_test(
            dir.path().join("tmp.txt").to_str().unwrap().to_string(),
        );

        logger.add_battery_level(zx::BootInstant::from_nanos(1234), 12);
        logger.add_battery_level(zx::BootInstant::from_nanos(2345), 13);
        logger.add_battery_level(zx::BootInstant::from_nanos(3456), 14);

        assert_eq!(
            read_to_string(config.curr_boot_path).unwrap(),
            "# BATTERY LEVEL\n\
            2345,13\n\
            3456,14\n\
            \n\
            # CHARGE STATUS\n"
        );
    }

    #[fuchsia::test]
    async fn test_limits_charge_status_inspect() {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();

        let config = create_config(&dir, 1, 2, "curr_data.txt", "prev_data.txt");
        let mut logger = HistoryLogger::from_file(inspector.root(), config);
        logger.change_temporary_file_for_renaming_for_test(
            dir.path().join("tmp.txt").to_str().unwrap().to_string(),
        );

        logger.add_charge_status(zx::BootInstant::from_nanos(1234), ChargeStatus::NotCharging);
        logger.add_charge_status(zx::BootInstant::from_nanos(2345), ChargeStatus::Charging);
        logger.add_charge_status(zx::BootInstant::from_nanos(3456), ChargeStatus::Full);

        assert_data_tree!(
            inspector,
            root: {
                historical_data: {
                    battery_level: {},
                    charge_status_changes: {
                       "1": {
                                "@time": 2345,
                                "status": "CHARGING",
                        },
                        "2": {
                                "@time": 3456,
                                "status": "FULL",
                        },
                    },
                    previous_boot_battery_level: {},
                    previous_boot_charge_status_changes: {},
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_limits_charge_status_file() {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();

        let config = create_config(&dir, 1, 2, "curr_data.txt", "prev_data.txt");
        let mut logger = HistoryLogger::from_file(inspector.root(), config.clone());
        logger.change_temporary_file_for_renaming_for_test(
            dir.path().join("tmp.txt").to_str().unwrap().to_string(),
        );

        logger.add_charge_status(zx::BootInstant::from_nanos(1234), ChargeStatus::NotCharging);
        logger.add_charge_status(zx::BootInstant::from_nanos(2345), ChargeStatus::Charging);
        logger.add_charge_status(zx::BootInstant::from_nanos(3456), ChargeStatus::Full);

        assert_eq!(
            read_to_string(config.curr_boot_path).unwrap(),
            "# BATTERY LEVEL\n\
            \n\
            # CHARGE STATUS\n\
            2345,CHARGING\n\
            3456,FULL\n"
        );
    }

    #[fuchsia::test]
    async fn test_inspect_limits_are_independent() {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();

        let config = create_config(&dir, 3, 2, "curr_data.txt", "prev_data.txt");
        let mut logger = HistoryLogger::from_file(inspector.root(), config);
        logger.change_temporary_file_for_renaming_for_test(
            dir.path().join("tmp.txt").to_str().unwrap().to_string(),
        );

        logger.add_battery_level(zx::BootInstant::from_nanos(1234), 12);
        logger.add_battery_level(zx::BootInstant::from_nanos(2345), 13);
        logger.add_battery_level(zx::BootInstant::from_nanos(3456), 14);
        logger.add_battery_level(zx::BootInstant::from_nanos(4567), 15);

        logger.add_charge_status(zx::BootInstant::from_nanos(1234), ChargeStatus::NotCharging);
        logger.add_charge_status(zx::BootInstant::from_nanos(2345), ChargeStatus::Charging);
        logger.add_charge_status(zx::BootInstant::from_nanos(3456), ChargeStatus::Full);

        assert_data_tree!(
            inspector,
            root: {
                historical_data: {
                    battery_level: {
                       "1": {
                                "@time": 2345,
                                "level": 13,
                        },
                        "2": {
                                "@time": 3456,
                                "level": 14,
                        },
                        "3": {
                                "@time": 4567,
                                "level": 15,
                        },
                    },
                    charge_status_changes: {
                       "1": {
                                "@time": 2345,
                                "status": "CHARGING",
                        },
                        "2": {
                                "@time": 3456,
                                "status": "FULL",
                        },
                    },
                    previous_boot_battery_level: {},
                    previous_boot_charge_status_changes: {},
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_file_limits_are_independent() {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();

        let config = create_config(&dir, 3, 2, "curr_data.txt", "prev_data.txt");
        let mut logger = HistoryLogger::from_file(inspector.root(), config.clone());
        logger.change_temporary_file_for_renaming_for_test(
            dir.path().join("tmp.txt").to_str().unwrap().to_string(),
        );

        logger.add_battery_level(zx::BootInstant::from_nanos(1234), 12);
        logger.add_battery_level(zx::BootInstant::from_nanos(2345), 13);
        logger.add_battery_level(zx::BootInstant::from_nanos(3456), 14);
        logger.add_battery_level(zx::BootInstant::from_nanos(4567), 15);

        logger.add_charge_status(zx::BootInstant::from_nanos(1234), ChargeStatus::NotCharging);
        logger.add_charge_status(zx::BootInstant::from_nanos(2345), ChargeStatus::Charging);
        logger.add_charge_status(zx::BootInstant::from_nanos(3456), ChargeStatus::Full);

        assert_eq!(
            read_to_string(config.curr_boot_path).unwrap(),
            "# BATTERY LEVEL\n\
            2345,13\n\
            3456,14\n\
            4567,15\n\
            \n\
            # CHARGE STATUS\n\
            2345,CHARGING\n\
            3456,FULL\n"
        );
    }

    #[fuchsia::test]
    async fn test_from_file_emits_previous_boo_data() {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();
        let config = create_config(&dir, 5, 5, "curr_data.txt", "prev_data.txt");

        let content = "# BATTERY LEVEL\n\
                       1234,12\n\
                       2345,13\n\
                       3456,14\n\
                       \n\
                       # CHARGE STATUS\n\
                       1234,NOT_CHARGING\n\
                       2345,CHARGING\n\
                       3456,FULL\n";
        write(&config.prev_boot_path, content).unwrap();

        let _logger = HistoryLogger::from_file(inspector.root(), config);

        assert_data_tree!(
            inspector,
            root: {
                historical_data: {
                    battery_level: {},
                    charge_status_changes: {},
                    previous_boot_battery_level: {
                       "0": {
                                "@time": 1234,
                                "level": 12,
                        },
                       "1": {
                                "@time": 2345,
                                "level": 13,
                        },
                        "2": {
                                "@time": 3456,
                                "level": 14,
                        },

                    },
                    previous_boot_charge_status_changes: {
                       "0": {
                                "@time": 1234,
                                "status": "NOT_CHARGING",
                        },
                       "1": {
                                "@time": 2345,
                                "status": "CHARGING",
                        },
                        "2": {
                                "@time": 3456,
                                "status": "FULL",
                        },

                    },
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_from_file_empty_history_on_bad_battery_header() {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();
        let config = create_config(&dir, 5, 5, "curr_data.txt", "prev_data.txt");

        let content = "INVALID HEADER\n1234,12\n";
        write(&config.curr_boot_path, content).unwrap();

        let _logger = HistoryLogger::from_file(inspector.root(), config);

        assert_data_tree!(
            inspector,
            root: {
                historical_data: {
                    battery_level: {},
                    charge_status_changes: {},
                    previous_boot_battery_level: {},
                    previous_boot_charge_status_changes: {},
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_from_file_empty_charge_history_on_bad_charge_header() {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();
        let config = create_config(&dir, 5, 5, "curr_data.txt", "prev_data.txt");

        let content = "# BATTERY LEVEL\n\
                       1234,12\n\
                       \n\
                       INVALID HEADER\n";
        write(&config.curr_boot_path, content).unwrap();

        let _logger = HistoryLogger::from_file(inspector.root(), config);

        assert_data_tree!(
            inspector,
            root: {
                historical_data: {
                    battery_level: {
                        "0": { "@time": 1234, "level": 12 },
                    },
                    charge_status_changes: {},
                    previous_boot_battery_level: {},
                    previous_boot_charge_status_changes: {},
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_from_file_skips_bad_lines() {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();
        let config = create_config(&dir, 5, 5, "curr_data.txt", "prev_data.txt");

        let content = "# BATTERY LEVEL\n\
                       1234,12\n\
                       bad-line\n\
                       3456,14\n\
                       \n\
                       # CHARGE STATUS\n\
                       1234,FULL\n\
                       bad-line\n";
        write(&config.curr_boot_path, content).unwrap();

        let _logger = HistoryLogger::from_file(inspector.root(), config);

        assert_data_tree!(
            inspector,
            root: {
                historical_data: {
                    battery_level: {
                        "0": { "@time": 1234, "level": 12 },
                        "1": { "@time": 3456, "level": 14 },
                    },
                    charge_status_changes: {
                        "0": { "@time": 1234, "status": "FULL" },
                    },
                    previous_boot_battery_level: {},
                    previous_boot_charge_status_changes: {},
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_from_file_load_and_add_smoke_test() {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();
        let config = create_config(&dir, 3, 3, "curr_data.txt", "prev_data.txt");

        {
            // Log 4 values for battery level and charging such that the first is dropped.
            let mut logger = HistoryLogger::from_file(inspector.root(), config.clone());
            logger.change_temporary_file_for_renaming_for_test(
                dir.path().join("tmp.txt").to_str().unwrap().to_string(),
            );

            logger.add_battery_level(zx::BootInstant::from_nanos(1234), 12);
            logger.add_battery_level(zx::BootInstant::from_nanos(2345), 13);
            logger.add_battery_level(zx::BootInstant::from_nanos(3456), 14);
            logger.add_battery_level(zx::BootInstant::from_nanos(4567), 15);

            logger.add_charge_status(zx::BootInstant::from_nanos(1234), ChargeStatus::NotCharging);
            logger.add_charge_status(zx::BootInstant::from_nanos(2345), ChargeStatus::Charging);
            logger.add_charge_status(zx::BootInstant::from_nanos(3456), ChargeStatus::Full);
            logger.add_charge_status(zx::BootInstant::from_nanos(4567), ChargeStatus::Discharging);
        }

        // Log 2 values for battery level and charging such that only the last values from above
        // remain.
        let mut logger = HistoryLogger::from_file(inspector.root(), config);
        logger.change_temporary_file_for_renaming_for_test(
            dir.path().join("tmp.txt").to_str().unwrap().to_string(),
        );

        logger.add_battery_level(zx::BootInstant::from_nanos(5678), 16);
        logger.add_battery_level(zx::BootInstant::from_nanos(6789), 17);

        logger.add_charge_status(zx::BootInstant::from_nanos(5678), ChargeStatus::NotCharging);
        logger.add_charge_status(zx::BootInstant::from_nanos(6789), ChargeStatus::Charging);

        assert_data_tree!(
            inspector,
            root: {
                historical_data: {
                    battery_level: {
                        "2": { "@time": 4567, "level": 15 },
                        "3": { "@time": 5678, "level": 16 },
                        "4": { "@time": 6789, "level": 17 },
                    },
                    charge_status_changes: {
                        "2": { "@time": 4567, "status": "DISCHARGING" },
                        "3": { "@time": 5678, "status": "NOT_CHARGING" },
                        "4": { "@time": 6789, "status": "CHARGING" },
                    },
                    previous_boot_battery_level: {},
                    previous_boot_charge_status_changes: {},
                }
            }
        );
    }
}
