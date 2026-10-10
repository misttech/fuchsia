// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Battery Client and Telemetry Formatter (supporting both
//! fuchsia.hardware.power.battery and fuchsia.power.battery.Battery).

use crate::common::{
    MEMBER_BATTERY, MEMBER_DEVICE, MicroUnit, WatchEvent, WatchEventStream, append_member_suffix,
    discover_instances, report_instance, run_watch_streams,
};
use anyhow::{Context, Result, anyhow};
use fidl::endpoints::ServiceMarker;
use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_fuchsia_power_battery as fpowerbattery;
use fuchsia_component::client::connect_to_protocol_at_path;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use std::fmt;

#[derive(Debug, PartialEq)]
pub struct DisplayHealthStatus(pub fbattery::HealthStatus);

impl fmt::Display for DisplayHealthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.0 {
            fbattery::HealthStatus::Good => "Good",
            fbattery::HealthStatus::Cold => "Cold",
            fbattery::HealthStatus::Cool => "Cool",
            fbattery::HealthStatus::Warm => "Warm",
            fbattery::HealthStatus::Hot => "Hot",
            fbattery::HealthStatus::Dead => "Dead",
            fbattery::HealthStatus::OverVoltage => "Over Voltage",
            fbattery::HealthStatus::UnspecifiedFailure => "Unspecified Failure",
            _ => "Unknown",
        };
        f.write_str(s)
    }
}

pub struct DisplayBatterySpec<'a>(pub &'a fbattery::Spec);

impl fmt::Display for DisplayBatterySpec<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spec = self.0;
        if let Some(model) = &spec.model {
            writeln!(f, "Model: {}", model)?;
        }
        if let Some(chemistry) = &spec.chemistry {
            writeln!(f, "Chemistry: {}", chemistry)?;
        }
        if let Some(design_capacity_uah) = spec.design_capacity_uah {
            writeln!(f, "Design Capacity: {}", MicroUnit(design_capacity_uah, "Ah"))?;
        }
        if let Some(design_voltage_uv) = spec.design_voltage_uv {
            writeln!(f, "Design Voltage: {}", MicroUnit(design_voltage_uv, "V"))?;
        }
        if let Some(opts) = &spec.supported_options {
            let format_trigger_list = |status_mask: &Option<fbattery::Status>| -> String {
                let Some(s) = status_mask else {
                    return "None".to_string();
                };
                let mut triggers = Vec::new();
                if s.present.is_some() {
                    triggers.push("present");
                }
                if s.level_percent.is_some() {
                    triggers.push("level_percent");
                }
                if s.remaining_capacity_uah.is_some() {
                    triggers.push("remaining_capacity");
                }
                if s.full_charge_capacity_uah.is_some() {
                    triggers.push("full_charge_capacity");
                }
                if s.health.is_some() {
                    triggers.push("health");
                }
                if s.cycle_count.is_some() {
                    triggers.push("cycle_count");
                }
                if s.time_remaining.is_some() {
                    triggers.push("time_remaining");
                }
                if s.voltage_uv.is_some() {
                    triggers.push("voltage");
                }
                if s.current_ua.is_some() {
                    triggers.push("current");
                }
                if s.temp_celsius.is_some() {
                    triggers.push("temperature");
                }
                if triggers.is_empty() { "None".to_string() } else { triggers.join(", ") }
            };

            writeln!(f, "Supported Triggers: {}", format_trigger_list(&opts.interest))?;
            writeln!(f, "Supported Wake Triggers: {}", format_trigger_list(&opts.wake_on))?;
        }
        Ok(())
    }
}

pub struct DisplayBatteryStatus<'a>(pub &'a fbattery::Status);

impl fmt::Display for DisplayBatteryStatus<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let info = self.0;
        if let Some(present) = info.present {
            writeln!(f, "Present: {present}")?;
        }
        if let Some(level_percent) = info.level_percent {
            writeln!(f, "Level: {:.1}%", level_percent)?;
        }
        if let Some(remaining_capacity_uah) = info.remaining_capacity_uah {
            writeln!(f, "Remaining Capacity: {}", MicroUnit(remaining_capacity_uah, "Ah"))?;
        }
        if let Some(full_charge_capacity_uah) = info.full_charge_capacity_uah {
            writeln!(f, "Full Charge Capacity: {}", MicroUnit(full_charge_capacity_uah, "Ah"))?;
        }
        if let Some(health) = info.health {
            writeln!(f, "Health: {}", DisplayHealthStatus(health))?;
        }
        if let Some(temp_celsius) = info.temp_celsius {
            writeln!(f, "Temperature: {:.1} C", temp_celsius)?;
        }
        if let Some(voltage_uv) = info.voltage_uv {
            writeln!(f, "Voltage: {}", MicroUnit(voltage_uv, "V"))?;
        }
        if let Some(current_ua) = info.current_ua {
            writeln!(f, "Current: {}", MicroUnit(current_ua, "A"))?;
        }
        if let Some(cycle_count) = info.cycle_count {
            writeln!(f, "Cycle Count: {}", cycle_count)?;
        }
        if let Some(time_remaining_ns) = info.time_remaining {
            // Convert `zx.Duration` (nanoseconds) to seconds.
            let total_secs = (time_remaining_ns as f64) / 1_000_000_000.0;
            if total_secs >= 60.0 {
                let mins = (total_secs / 60.0).floor() as u64;
                let secs = (total_secs % 60.0).floor() as u64;
                writeln!(f, "Time Remaining: {mins}m {secs:02}s ({total_secs:.1}s)")?;
            } else {
                writeln!(f, "Time Remaining: {total_secs:.1}s")?;
            }
        }
        Ok(())
    }
}

pub struct DisplayLegacyStatus<'a>(pub &'a fpowerbattery::BatteryInfo);

impl fmt::Display for DisplayLegacyStatus<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let info = self.0;
        if let Some(status) = info.status {
            let s = match status {
                fpowerbattery::BatteryStatus::Ok => "OK",
                fpowerbattery::BatteryStatus::NotAvailable => "Not Available",
                fpowerbattery::BatteryStatus::NotPresent => "Not Present",
                fpowerbattery::BatteryStatus::Unknown => "Unknown",
            };
            writeln!(f, "Status: {s}")?;
        }
        if let Some(charge_status) = info.charge_status {
            let s = match charge_status {
                fpowerbattery::ChargeStatus::NotCharging => "Not Charging",
                fpowerbattery::ChargeStatus::Charging => "Charging",
                fpowerbattery::ChargeStatus::Discharging => "Discharging",
                fpowerbattery::ChargeStatus::Full => "Full",
                fpowerbattery::ChargeStatus::Unknown => "Unknown",
            };
            writeln!(f, "Charge Status: {s}")?;
        }
        if let Some(charge_source) = info.charge_source {
            let s = match charge_source {
                fpowerbattery::ChargeSource::None => "None",
                fpowerbattery::ChargeSource::AcAdapter => "AC Adapter",
                fpowerbattery::ChargeSource::Usb => "USB",
                fpowerbattery::ChargeSource::Wireless => "Wireless",
                fpowerbattery::ChargeSource::Unknown => "Unknown",
            };
            writeln!(f, "Charge Source: {s}")?;
        }
        if let Some(level_percent) = info.level_percent {
            writeln!(f, "Level: {:.1}%", level_percent)?;
        }
        if let Some(level_status) = info.level_status {
            let s = match level_status {
                fpowerbattery::LevelStatus::Ok => "OK",
                fpowerbattery::LevelStatus::Warning => "Warning",
                fpowerbattery::LevelStatus::Low => "Low",
                fpowerbattery::LevelStatus::Critical => "Critical",
                fpowerbattery::LevelStatus::Unknown => "Unknown",
            };
            writeln!(f, "Level Status: {s}")?;
        }
        if let Some(health) = info.health {
            let s = match health {
                fpowerbattery::HealthStatus::Good => "Good",
                fpowerbattery::HealthStatus::Cold => "Cold",
                fpowerbattery::HealthStatus::Hot => "Hot",
                fpowerbattery::HealthStatus::Dead => "Dead",
                fpowerbattery::HealthStatus::OverVoltage => "Over Voltage",
                fpowerbattery::HealthStatus::UnspecifiedFailure => "Unspecified Failure",
                fpowerbattery::HealthStatus::Cool => "Cool",
                fpowerbattery::HealthStatus::Warm => "Warm",
                fpowerbattery::HealthStatus::Overheat => "Overheat",
                fpowerbattery::HealthStatus::Unknown => "Unknown",
            };
            writeln!(f, "Health: {s}")?;
        }
        if let Some(present_voltage_mv) = info.present_voltage_mv {
            writeln!(f, "Voltage: {}", MicroUnit((present_voltage_mv as i64) * 1000, "V"))?;
        }
        if let Some(remaining_charge_uah) = info.remaining_charge_uah {
            writeln!(f, "Remaining Charge: {}", MicroUnit(remaining_charge_uah, "Ah"))?;
        }
        if let Some(full_capacity_uah) = info.full_capacity_uah {
            writeln!(f, "Full Capacity: {}", MicroUnit(full_capacity_uah, "Ah"))?;
        }
        if let Some(temperature_mc) = info.temperature_mc {
            writeln!(f, "Temperature: {:.1} C", (temperature_mc as f64) / 1000.0)?;
        }
        if let Some(present_charging_current_ua) = info.present_charging_current_ua {
            writeln!(f, "Current Draw: {}", MicroUnit(present_charging_current_ua, "A"))?;
        }
        if let Some(average_charging_current_ua) = info.average_charging_current_ua {
            writeln!(f, "Average Current: {}", MicroUnit(average_charging_current_ua, "A"))?;
        }
        Ok(())
    }
}

/// A discovered or user-specified battery service instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatteryInstance {
    /// A `fuchsia.hardware.power.battery.Service` instance path.
    Modern(String),
    /// A legacy `fuchsia.power.battery.InfoService` instance path.
    Legacy(String),
}

impl BatteryInstance {
    /// Discovers modern (`fuchsia.hardware.power.battery.Service`) and, if none are present,
    /// legacy (`fuchsia.power.battery.InfoService`) battery instances under `/svc`.
    pub fn discover() -> Vec<Self> {
        let modern = discover_instances(fbattery::ServiceMarker::SERVICE_NAME);
        if !modern.is_empty() {
            return modern.into_iter().map(Self::Modern).collect();
        }
        discover_instances(fpowerbattery::InfoServiceMarker::SERVICE_NAME)
            .into_iter()
            .map(Self::Legacy)
            .collect()
    }

    /// Queries and prints telemetry for this battery instance.
    pub async fn query_info(&self) -> Result<()> {
        match self {
            Self::Modern(p) => Self::get_modern_info_at(p).await,
            Self::Legacy(p) => Self::get_legacy_info_at(p).await,
        }
    }

    /// Queries and prints telemetry for this instance, framing its output with a section header
    /// when `show_header` is set. Returns whether the query succeeded.
    pub async fn report(&self, show_header: bool) -> bool {
        let (title, path) = match self {
            Self::Modern(p) => ("Battery Telemetry", p.as_str()),
            Self::Legacy(p) => ("Legacy Battery Telemetry", p.as_str()),
        };
        report_instance(title, path, show_header, self.query_info()).await
    }

    /// Queries and prints telemetry for `instances`, returning whether at least one instance
    /// succeeded.
    pub async fn report_all(instances: &[Self], show_headers: bool) -> bool {
        let mut succeeded = false;
        for inst in instances {
            succeeded |= inst.report(show_headers).await;
        }
        succeeded
    }

    /// Queries battery telemetry at `path` (or across all discovered battery instances when `path`
    /// is `None`).
    pub async fn get_info(path: Option<&str>) -> Result<()> {
        if let Some(p) = path {
            if p.contains(fpowerbattery::InfoServiceMarker::SERVICE_NAME) || p.ends_with("/device")
            {
                return Self::get_legacy_info_at(p).await;
            }
            if p.contains(fbattery::ServiceMarker::SERVICE_NAME) || p.ends_with("/battery") {
                return Self::get_modern_info_at(p).await;
            }
            if Self::get_modern_info_at(p).await.is_ok() {
                return Ok(());
            }
            return Self::get_legacy_info_at(p).await;
        }

        let instances = Self::discover();
        if instances.is_empty() {
            anyhow::bail!("No battery service instances found under /svc");
        }
        if !Self::report_all(&instances, instances.len() > 1).await {
            anyhow::bail!("Failed to query any battery instances");
        }
        Ok(())
    }

    pub(crate) async fn get_modern_info_at(path: &str) -> Result<()> {
        let modern_path = append_member_suffix(path, MEMBER_BATTERY);
        let modern_path_str = modern_path.to_str().context("invalid UTF-8 path")?;
        let proxy = connect_to_protocol_at_path::<fbattery::BatteryMarker>(modern_path_str)
            .with_context(|| format!("Failed to connect to Battery at {modern_path_str}"))?;

        if let Ok(Ok(spec)) = proxy.get_spec().await {
            print!("{}", DisplayBatterySpec(&spec));
        }

        let status = proxy
            .get_status()
            .await
            .context("GetStatus call failed")?
            .map_err(|e| anyhow!("GetStatus returned domain error: {:?}", e))?;

        print!("{}", DisplayBatteryStatus(&status));
        Ok(())
    }

    pub(crate) async fn get_legacy_info_at(path: &str) -> Result<()> {
        let legacy_path = append_member_suffix(path, MEMBER_DEVICE);
        let legacy_path_str = legacy_path.to_str().context("invalid UTF-8 path")?;
        let proxy = connect_to_protocol_at_path::<fpowerbattery::BatteryInfoProviderMarker>(
            legacy_path_str,
        )
        .with_context(|| {
            format!("Failed to connect to legacy BatteryInfoProvider at {legacy_path_str}")
        })?;
        let info = proxy.get_battery_info().await.context("GetBatteryInfo call failed")?;
        print!("{}", DisplayLegacyStatus(&info));
        Ok(())
    }

    /// Connects a watch stream for this instance if it supports `Watch` (`Modern`).
    pub fn watch_stream(&self) -> Result<Option<WatchEventStream>> {
        let Self::Modern(inst) = self else {
            return Ok(None);
        };
        let target_path = append_member_suffix(inst, MEMBER_BATTERY);
        let target_path_str = target_path.to_str().context("invalid UTF-8 path")?;
        let proxy = connect_to_protocol_at_path::<fbattery::BatteryMarker>(target_path_str)
            .with_context(|| format!("Failed to connect to Battery at {target_path_str}"))?;
        let inst = inst.clone();
        Ok(Some(
            battery_watch_stream(proxy)
                .map(move |res| WatchEvent {
                    kind: "Battery",
                    instance: inst.clone(),
                    status: res.map(|s| DisplayBatteryStatus(&s).to_string()),
                })
                .boxed(),
        ))
    }

    /// Connects watch streams for all discovered `fuchsia.hardware.power.battery.Service`
    /// instances.
    pub fn discover_watch_streams() -> Result<Vec<WatchEventStream>> {
        Self::discover().iter().filter_map(|inst| inst.watch_stream().transpose()).collect()
    }

    /// Watches battery telemetry events at `path` (or across all discovered battery instances when
    /// `path` is `None`).
    pub async fn watch(path: Option<&str>) -> Result<()> {
        if let Some(p) = path {
            return Self::watch_at(p).await;
        }
        let streams = Self::discover_watch_streams()?;
        if streams.is_empty() {
            anyhow::bail!("No modern fuchsia.hardware.power.battery instances found under /svc");
        }
        run_watch_streams(streams, "battery").await
    }

    async fn watch_at(path: &str) -> Result<()> {
        let target_path = append_member_suffix(path, MEMBER_BATTERY);
        let target_path_str = target_path.to_str().context("invalid UTF-8 path")?;
        let proxy = connect_to_protocol_at_path::<fbattery::BatteryMarker>(target_path_str)
            .with_context(|| format!("Failed to connect to Battery at {target_path_str}"))?;

        println!("Watching battery events on {} (press Ctrl+C to exit)...\n", target_path_str);

        let mut stream = battery_watch_stream(proxy);
        let mut received_any = false;
        let mut last_err = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(status) => {
                    received_any = true;
                    println!("=== Battery Telemetry Update ===");
                    print!("{}", DisplayBatteryStatus(&status));
                    println!();
                }
                Err(e) => {
                    eprintln!("Warning: received watch error: {:#}", e);
                    last_err = Some(e);
                }
            }
        }
        if !received_any {
            return Err(
                last_err.unwrap_or_else(|| anyhow!("Battery watch stream closed immediately"))
            );
        }
        Ok(())
    }
}

pub fn battery_watch_stream(
    proxy: fbattery::BatteryProxy,
) -> BoxStream<'static, Result<fbattery::Status>> {
    Box::pin(stream::unfold(Some(proxy), |proxy_opt| async move {
        let proxy = proxy_opt?;
        match proxy.watch(None).await {
            Ok(Ok((status, _wake_lease))) => Some((Ok(status), Some(proxy))),
            Ok(Err(e)) => Some((Err(anyhow!("Battery Watch domain error: {:?}", e)), None)),
            Err(e) => Some((Err(anyhow!("Battery Watch FIDL error: {:#}", e)), None)),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_health_status() {
        assert_eq!(DisplayHealthStatus(fbattery::HealthStatus::Good).to_string(), "Good");
        assert_eq!(DisplayHealthStatus(fbattery::HealthStatus::Hot).to_string(), "Hot");
        assert_eq!(DisplayHealthStatus(fbattery::HealthStatus::Cold).to_string(), "Cold");
    }

    #[test]
    fn test_display_battery_status() {
        let status = fbattery::Status {
            level_percent: Some(85.5),
            remaining_capacity_uah: Some(4_200_000),
            full_charge_capacity_uah: Some(5_000_000),
            health: Some(fbattery::HealthStatus::Good),
            temp_celsius: Some(31.2),
            voltage_uv: Some(3_850_000),
            current_ua: Some(1_200_000),
            cycle_count: Some(12),
            ..Default::default()
        };
        let output = DisplayBatteryStatus(&status).to_string();
        assert!(output.contains("Level: 85.5%"));
        assert!(output.contains("Remaining Capacity: 4.200 Ah"));
        assert!(output.contains("Full Charge Capacity: 5.000 Ah"));
        assert!(output.contains("Health: Good"));
        assert!(output.contains("Temperature: 31.2 C"));
        assert!(output.contains("Voltage: 3.850 V"));
        assert!(output.contains("Current: 1.200 A"));
        assert!(output.contains("Cycle Count: 12"));
    }

    #[test]
    fn test_display_battery_spec() {
        let spec = fbattery::Spec {
            model: Some("MAX77779".to_string()),
            chemistry: Some("Li-Ion".to_string()),
            design_capacity_uah: Some(5_000_000),
            design_voltage_uv: Some(3_850_000),
            supported_options: Some(fbattery::WatchOptions {
                interest: Some(fbattery::Status {
                    present: Some(true),
                    level_percent: Some(0.0),
                    cycle_count: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let output = DisplayBatterySpec(&spec).to_string();
        assert!(output.contains("Model: MAX77779"));
        assert!(output.contains("Chemistry: Li-Ion"));
        assert!(output.contains("Design Capacity: 5.000 Ah"));
        assert!(output.contains("Design Voltage: 3.850 V"));
        let expected = "Supported Triggers: present, level_percent, cycle_count";
        assert!(output.contains(expected));
        assert!(output.contains("Supported Wake Triggers: None"));
    }
}
