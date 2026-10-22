// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::platform_metrics::PlatformMetric;
use crate::types::{Celsius, ThermalLoad};
use fidl_fuchsia_hardware_trippoint as ftrippoint;

/// Defines the message types and arguments to be used for inter-node communication
#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub enum Message {
    /// Read the temperature
    ReadTemperature,

    /// Command a system shutdown due to high temperature
    HighTemperatureShutdown,

    /// Communicate a new thermal load value for the given sensor
    /// Arg0: a ThermalLoad value which represents the severity of thermal load on the given sensor
    /// Arg1: the topological path which uniquely identifies a specific temperature sensor
    UpdateThermalLoad(ThermalLoad, String),

    /// Communicate a new thermal load value specifically for CPU thermal client
    /// Arg: a ThermalLoad value which represents the severity of CPU thermal load
    UpdateCpuThermalLoad(ThermalLoad),

    /// File a crash report
    /// Arg: the crash report signature
    FileCrashReport(String),

    /// Notify that the mic enabled state has changed
    /// Arg: the new enabled state
    NotifyMicEnabledChanged(bool),

    /// Notify that the user active state has changed
    /// Arg: the new active state
    NotifyUserActiveChanged(bool),

    /// Log the given metric with the PlatformMetrics node
    /// Arg: the `PlatformMetric` to be logged
    LogPlatformMetric(PlatformMetric),

    /// Gets the sensor name of the driver associated with the target node
    GetSensorName,

    /// Sets a temperature override for the node.
    /// Arg: the new override temperature
    SetTemperatureOverride(Celsius),

    /// Clears a previously set temperature override.
    ClearTemperatureOverride,

    /// Gets the hardware trip point descriptors from the sensor driver.
    GetTripPointDescriptors,

    /// Configures hardware trip points on the sensor driver.
    SetTripPoints(Vec<ftrippoint::TripPointDescriptor>),

    /// Waits for any configured hardware trip point to be crossed on the sensor driver.
    WaitForAnyTripPoint,

    /// Send a debug command.
    /// Arg0: node-specific command as a string
    /// Arg1: args required to execute the command
    Debug(String, Vec<String>),
}

/// Defines the return values for each of the Message types from above
#[derive(Debug)]
#[allow(dead_code)]
pub enum MessageReturn {
    /// Arg: temperature in Celsius
    ReadTemperature(Celsius),

    /// There is no arg in this MessageReturn type. It only serves as an ACK.
    SystemShutdown,

    /// There is no arg in this MessageReturn type. It only serves as an ACK.
    UpdateThermalLoad,

    /// There is no arg in this MessageReturn type. It only serves as an ACK.
    UpdateCpuThermalLoad,

    /// There is no arg in this MessageReturn type. It only serves as an ACK.
    FileCrashReport,

    /// There is no arg in this MessageReturn type. It only serves as an ACK.
    NotifyMicEnabledChanged,

    /// There is no arg in this MessageReturn type. It only serves as an ACK.
    NotifyUserActiveChanged,

    /// There is no arg in this MessageReturn type. It only serves as an ACK.
    LogPlatformMetric,

    /// Arg: the sensor name of the driver associated with the target node
    GetSensorName(String),

    /// There is no arg in this MessageReturn type. It only serves as an ACK.
    SetTemperatureOverride,

    /// There is no arg in this MessageReturn type. It only serves as an ACK.
    ClearTemperatureOverride,

    /// Arg: hardware trip point descriptors or raw zx::Status error.
    GetTripPointDescriptors(Result<Vec<ftrippoint::TripPointDescriptor>, i32>),

    /// Arg: Ok(()) or raw zx::Status error from configuring hardware trip points.
    SetTripPoints(Result<(), i32>),

    /// Arg: hardware trip point result or raw zx::Status error.
    WaitForAnyTripPoint(Result<ftrippoint::TripPointResult, i32>),

    /// There is no arg in this MessageReturn type. It only serves as an ACK.
    Debug,
}
