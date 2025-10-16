// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use fidl_fuchsia_power_battery::ChargeStatus;
use fuchsia_inspect::{self as inspect};
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use log::{error, warn};
use std::collections::VecDeque;
use std::fmt::Write;
use std::fs::{OpenOptions, read_to_string};
use std::io::Write as OtherWrite;
use std::str::FromStr;

static BATTERY_LEVEL_HEADER: &str = "# BATTERY LEVEL";
static CHARGE_STATUS_HEADER: &str = "# CHARGE STATUS";

#[derive(Clone)]
pub struct HistoryLoggerConfig {
    pub curr_boot_path: String,
    pub prev_boot_path: String,
    pub battery_level_buffer_capacity: usize,
    pub charge_status_buffer_capacity: usize,
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
        }
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
        if let Err(e) =
            write_history(&self.config.curr_boot_path, &self.battery_history, &self.charge_history)
        {
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
        if let Err(e) =
            write_history(&self.config.curr_boot_path, &self.battery_history, &self.charge_history)
        {
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

    let mut file =
        OpenOptions::new().write(true).create(true).truncate(true).open(curr_boot_path)?;
    write!(file, "{}", content)?;
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
