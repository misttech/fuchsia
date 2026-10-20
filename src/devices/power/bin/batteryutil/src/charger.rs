// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Charger Client and Controller (supporting both
//! fuchsia.hardware.power.charger and fuchsia.power.battery.Charger).

use crate::common::{
    MEMBER_CHARGER, MEMBER_DEBUG, MEMBER_DEVICE, MicroUnit, WatchEvent, WatchEventStream,
    append_member_suffix, discover_instances, parse_micro_value, report_instance,
    run_watch_streams, select_instance,
};
use crate::spmi::{self, PowerSource};
use anyhow::{Context, Result, anyhow};
use fidl::endpoints::ServiceMarker;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fidl_fuchsia_power_battery as fpowerbattery;
use fuchsia_component::client::connect_to_protocol_at_path;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use std::fmt;

struct DisplayOperatingMode(fcharger::OperatingMode);

impl fmt::Display for DisplayOperatingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.0 {
            fcharger::OperatingMode::Charging => "Charging",
            fcharger::OperatingMode::Passthrough => "Passthrough",
            fcharger::OperatingMode::Discharging => "Discharging",
            fcharger::OperatingMode::Otg => "OTG",
            _ => "Unknown",
        };
        f.write_str(s)
    }
}

struct DisplayChargePhase(fcharger::ChargePhase);

impl fmt::Display for DisplayChargePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.0 {
            fcharger::ChargePhase::None => "None",
            fcharger::ChargePhase::Trickle => "Trickle",
            fcharger::ChargePhase::Fast => "Fast",
            fcharger::ChargePhase::Taper => "Taper",
            fcharger::ChargePhase::TopOff => "Top-Off",
            fcharger::ChargePhase::Done => "Done",
            _ => "Unknown",
        };
        f.write_str(s)
    }
}

struct DisplaySourceType(fcharger::SourceType);

impl fmt::Display for DisplaySourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.0 {
            fcharger::SourceType::Ac => "AC",
            fcharger::SourceType::Usb => "USB",
            fcharger::SourceType::Wireless => "Wireless",
            _ => "Unknown",
        };
        f.write_str(s)
    }
}

struct DisplayChargerHealth(fcharger::Health);

impl fmt::Display for DisplayChargerHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.0 {
            fcharger::Health::Good => "Good",
            fcharger::Health::InputOverVoltage => "Input Over-Voltage",
            fcharger::Health::InputUnderVoltage => "Input Under-Voltage",
            fcharger::Health::ThermalWarning => "Thermal Warning",
            fcharger::Health::ThermalShutdown => "Thermal Shutdown",
            fcharger::Health::SafetyTimerExpired => "Safety Timer Expired",
            fcharger::Health::BatteryOverVoltage => "Battery Over-Voltage",
            fcharger::Health::WatchdogTimerExpired => "Watchdog Timer Expired",
            fcharger::Health::UnspecifiedFailure => "Unspecified Failure",
            _ => "Unknown",
        };
        f.write_str(s)
    }
}

struct DisplayChargerSpec<'a>(&'a fcharger::Spec);

impl fmt::Display for DisplayChargerSpec<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spec = self.0;
        if let Some(model) = &spec.model {
            writeln!(f, "Charger Model: {model}")?;
        }
        if let Some(v) = spec.max_charge_current_ua {
            writeln!(f, "Max Charge Current: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = spec.max_charge_voltage_uv {
            writeln!(f, "Max Charge Voltage: {}", MicroUnit(v, "V"))?;
        }
        if let Some(v) = spec.max_input_current_ua {
            writeln!(f, "Max Input Current: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = spec.precharge_current_ua {
            writeln!(f, "Precharge Current: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = spec.charge_term_current_ua {
            writeln!(f, "Spec Charge Term Current: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = spec.recharge_voltage_uv {
            writeln!(f, "Recharge Voltage: {}", MicroUnit(v, "V"))?;
        }
        if let Some(v) = spec.min_input_voltage_uv {
            writeln!(f, "Min Input Voltage: {}", MicroUnit(v, "V"))?;
        }
        if let Some(v) = spec.max_input_voltage_uv {
            writeln!(f, "Max Input Voltage: {}", MicroUnit(v, "V"))?;
        }
        if let Some(modes) = &spec.supported_modes {
            let list: Vec<String> =
                modes.iter().map(|m| DisplayOperatingMode(*m).to_string()).collect();
            writeln!(
                f,
                "Supported Operating Modes: {}",
                if list.is_empty() { "None".to_string() } else { list.join(", ") }
            )?;
        }
        if let Some(opts) = &spec.supported_options {
            let format_trigger_list = |status_mask: &Option<fcharger::Status>| -> String {
                let Some(s) = status_mask else {
                    return "None".to_string();
                };
                let mut triggers = Vec::new();
                if s.online.is_some() {
                    triggers.push("online");
                }
                if s.source_type.is_some() {
                    triggers.push("source_type");
                }
                if s.operating_mode.is_some() {
                    triggers.push("operating_mode");
                }
                if s.charge_phase.is_some() {
                    triggers.push("charge_phase");
                }
                if s.health.is_some() {
                    triggers.push("health");
                }
                if s.input_voltage_uv.is_some() {
                    triggers.push("input_voltage");
                }
                if s.input_current_ua.is_some() {
                    triggers.push("input_current");
                }
                if s.input_current_limit_ua.is_some() {
                    triggers.push("input_current_limit");
                }
                if s.input_voltage_limit_uv.is_some() {
                    triggers.push("input_voltage_limit");
                }
                if s.charge_current_limit_ua.is_some() {
                    triggers.push("charge_current_limit");
                }
                if s.float_voltage_uv.is_some() {
                    triggers.push("float_voltage");
                }
                if s.charge_term_current_ua.is_some() {
                    triggers.push("charge_term_current");
                }
                if triggers.is_empty() { "None".to_string() } else { triggers.join(", ") }
            };

            writeln!(f, "Supported Triggers: {}", format_trigger_list(&opts.interest))?;
            writeln!(f, "Supported Wake Triggers: {}", format_trigger_list(&opts.wake_on))?;
        }
        Ok(())
    }
}

pub struct DisplayChargerStatus<'a>(pub &'a fcharger::Status);

impl fmt::Display for DisplayChargerStatus<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0;
        if let Some(online) = s.online {
            writeln!(f, "Online: {online}")?;
        }
        if let Some(src) = s.source_type {
            writeln!(f, "Source Type: {}", DisplaySourceType(src))?;
        }
        if let Some(mode) = s.operating_mode {
            writeln!(f, "Operating Mode: {}", DisplayOperatingMode(mode))?;
        }
        if let Some(phase) = s.charge_phase {
            writeln!(f, "Charge Phase: {}", DisplayChargePhase(phase))?;
        }
        if let Some(health) = s.health {
            writeln!(f, "Health: {}", DisplayChargerHealth(health))?;
        }
        if let Some(v) = s.input_voltage_uv {
            writeln!(f, "Input Voltage: {}", MicroUnit(v, "V"))?;
        }
        if let Some(v) = s.input_current_ua {
            writeln!(f, "Input Current: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = s.input_current_limit_ua {
            writeln!(f, "Input Current Limit: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = s.input_voltage_limit_uv {
            writeln!(f, "Input Voltage Limit: {}", MicroUnit(v, "V"))?;
        }
        if let Some(v) = s.charge_current_limit_ua {
            writeln!(f, "Charge Current Limit: {}", MicroUnit(v, "A"))?;
        }
        if let Some(v) = s.float_voltage_uv {
            writeln!(f, "Float Voltage: {}", MicroUnit(v, "V"))?;
        }
        if let Some(v) = s.charge_term_current_ua {
            writeln!(f, "Charge Term Current: {}", MicroUnit(v, "A"))?;
        }
        Ok(())
    }
}

/// A discovered or user-specified `fuchsia.hardware.power.charger.Service` instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargerInstance(pub String);

impl ChargerInstance {
    /// Discovers `fuchsia.hardware.power.charger.Service` instances under `/svc`.
    pub fn discover() -> Vec<Self> {
        discover_instances(fcharger::ServiceMarker::SERVICE_NAME).into_iter().map(Self).collect()
    }

    /// Queries and prints telemetry for this charger instance.
    pub async fn query_info(&self) -> Result<()> {
        Self::get_modern_info_at(&self.0).await
    }

    /// Queries and prints telemetry for this instance, framing its output with a section header
    /// when `show_header` is set. Returns whether the query succeeded.
    pub async fn report(&self, show_header: bool) -> bool {
        report_instance("Charger Telemetry", &self.0, show_header, self.query_info()).await
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

    /// Queries charger telemetry at `path` (or across all discovered charger instances when `path`
    /// is `None`).
    pub async fn get_info(path: Option<&str>) -> Result<()> {
        if let Some(p) = path {
            if p.contains(fcharger::DebugServiceMarker::SERVICE_NAME) {
                anyhow::bail!(
                    "DebugService path '{p}' is control-only; pass a {} path for telemetry, or \
                     omit --path to use discovery",
                    fcharger::ServiceMarker::SERVICE_NAME
                );
            }
            return Self::get_modern_info_at(p).await;
        }

        let instances = Self::discover();
        if instances.is_empty() {
            anyhow::bail!("No charger service instances found under /svc");
        }
        if !Self::report_all(&instances, instances.len() > 1).await {
            anyhow::bail!("Failed to query any charger instances");
        }
        Ok(())
    }

    pub(crate) async fn get_modern_info_at(path: &str) -> Result<()> {
        let charger_path = append_member_suffix(path, MEMBER_CHARGER);
        let charger_path_str = charger_path.to_str().context("invalid UTF-8 path")?;
        let proxy = connect_to_protocol_at_path::<fcharger::ChargerMarker>(charger_path_str)
            .with_context(|| format!("Failed to connect to Charger at {charger_path_str}"))?;

        if let Ok(Ok(spec)) = proxy.get_spec().await {
            print!("{}", DisplayChargerSpec(&spec));
        }

        let status = proxy
            .get_status()
            .await
            .context("Charger GetStatus call failed")?
            .map_err(|e| anyhow!("Charger GetStatus returned domain error: {:?}", e))?;

        print!("{}", DisplayChargerStatus(&status));
        Ok(())
    }

    /// Connects a watch stream for this charger instance.
    pub fn watch_stream(&self) -> Result<WatchEventStream> {
        let target_path = append_member_suffix(&self.0, MEMBER_CHARGER);
        let target_path_str = target_path.to_str().context("invalid UTF-8 path")?;
        let proxy = connect_to_protocol_at_path::<fcharger::ChargerMarker>(target_path_str)
            .with_context(|| format!("Failed to connect to Charger at {target_path_str}"))?;
        let inst = self.0.clone();
        Ok(charger_watch_stream(proxy)
            .map(move |res| WatchEvent {
                kind: "Charger",
                instance: inst.clone(),
                status: res.map(|s| DisplayChargerStatus(&s).to_string()),
            })
            .boxed())
    }

    /// Connects watch streams for all discovered `fuchsia.hardware.power.charger.Service`
    /// instances.
    pub fn discover_watch_streams() -> Result<Vec<WatchEventStream>> {
        Self::discover().iter().map(Self::watch_stream).collect()
    }

    /// Watches charger telemetry events at `path` (or across all discovered charger instances when
    /// `path` is `None`).
    pub async fn watch(path: Option<&str>) -> Result<()> {
        if let Some(p) = path {
            if p.contains(fcharger::DebugServiceMarker::SERVICE_NAME) {
                anyhow::bail!(
                    "DebugService path '{p}' is control-only; pass a {} path for telemetry, or \
                     omit --path to use discovery",
                    fcharger::ServiceMarker::SERVICE_NAME
                );
            }
            return Self::watch_at(p).await;
        }

        let streams = Self::discover_watch_streams()?;
        if streams.is_empty() {
            anyhow::bail!("No charger service instances found under /svc");
        }
        run_watch_streams(streams, "charger").await
    }

    async fn watch_at(path: &str) -> Result<()> {
        let target_path = append_member_suffix(path, MEMBER_CHARGER);
        let target_path_str = target_path.to_str().context("invalid UTF-8 path")?;
        let proxy = connect_to_protocol_at_path::<fcharger::ChargerMarker>(target_path_str)
            .with_context(|| format!("Failed to connect to Charger at {target_path_str}"))?;

        println!("Watching charger events on {} (press Ctrl+C to exit)...\n", target_path_str);

        let mut stream = charger_watch_stream(proxy);
        let mut received_any = false;
        let mut last_err = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(status) => {
                    received_any = true;
                    println!("=== Charger Telemetry Update ===");
                    print!("{}", DisplayChargerStatus(&status));
                    println!();
                }
                Err(e) => {
                    eprintln!("Warning: received charger watch error: {:#}", e);
                    last_err = Some(e);
                }
            }
        }
        if !received_any {
            return Err(
                last_err.unwrap_or_else(|| anyhow!("Charger watch stream closed immediately"))
            );
        }
        Ok(())
    }
}

pub fn charger_watch_stream(
    proxy: fcharger::ChargerProxy,
) -> BoxStream<'static, Result<fcharger::Status>> {
    Box::pin(stream::unfold(Some(proxy), |proxy_opt| async move {
        let proxy = proxy_opt?;
        match proxy.watch(None).await {
            Ok(Ok((status, _wake_lease))) => Some((Ok(status), Some(proxy))),
            Ok(Err(e)) => Some((Err(anyhow!("Charger Watch domain error: {:?}", e)), None)),
            Err(e) => Some((Err(anyhow!("Charger Watch FIDL error: {:#}", e)), None)),
        }
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargerModeArg(pub fcharger::OperatingMode);

impl std::str::FromStr for ChargerModeArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "charging" | "charge" | "usb" => Ok(Self(fcharger::OperatingMode::Charging)),
            "passthrough" => Ok(Self(fcharger::OperatingMode::Passthrough)),
            "discharging" | "discharge" | "battery" | "batt" => {
                Ok(Self(fcharger::OperatingMode::Discharging))
            }
            "otg" => Ok(Self(fcharger::OperatingMode::Otg)),
            _ => Err(format!(
                "Invalid charger mode '{s}'. Use charging/usb, passthrough, discharging/battery, \
                 or otg."
            )),
        }
    }
}

pub async fn set_charger_mode(path: Option<&str>, mode: fcharger::OperatingMode) -> Result<()> {
    let legacy_enable = match mode {
        fcharger::OperatingMode::Charging => Some(true),
        fcharger::OperatingMode::Passthrough => Some(false),
        _ => None,
    };

    let target = match path {
        Some(p) => set_charger_mode_at_path(p, mode, legacy_enable).await?,
        None => set_default_charger_mode(mode, legacy_enable).await?,
    };
    println!(
        "Successfully set charger operating mode to {} via {target}",
        DisplayOperatingMode(mode)
    );
    Ok(())
}

async fn set_charger_mode_at_path(
    p: &str,
    mode: fcharger::OperatingMode,
    legacy_enable: Option<bool>,
) -> Result<String> {
    if p.contains(fpowerbattery::ChargerServiceMarker::SERVICE_NAME) {
        let enable =
            legacy_enable.context("legacy ChargerService does not support power source mode")?;
        return set_legacy_charger_enable(p, enable).await;
    }
    // Never fall back to `Controller`: it is single-client and reverts on close, so a mode set
    // through it would be undone as soon as batteryutil exits. Any charger `Service` or
    // `DebugService` path (instance or member, including `/controller`) maps to `DebugService`.
    if p.contains(fcharger::ServiceMarker::SERVICE_NAME)
        || p.contains(fcharger::DebugServiceMarker::SERVICE_NAME)
        || p.ends_with("/debug")
    {
        return set_debug_charger_mode(&debug_service_instance(p)?, mode).await;
    }
    // Other paths: Debug, then legacy enable.
    match set_debug_charger_mode(p, mode).await {
        Ok(target) => Ok(target),
        Err(debug_err) => match legacy_enable {
            Some(enable) => set_legacy_charger_enable(p, enable)
                .await
                .with_context(|| format!("Debug also failed: {debug_err:#}")),
            None => Err(debug_err),
        },
    }
}

async fn set_default_charger_mode(
    mode: fcharger::OperatingMode,
    legacy_enable: Option<bool>,
) -> Result<String> {
    let mut last_err = None;
    if let Ok(p) = select_instance(fcharger::DebugServiceMarker::SERVICE_NAME) {
        match set_debug_charger_mode(&p, mode).await {
            Ok(target) => return Ok(target),
            Err(e) => last_err = Some(e),
        }
    }
    match set_sorrel_fallback_mode(mode, legacy_enable).await {
        Ok(Some(target)) => return Ok(target),
        Ok(None) => {}
        Err(e) => last_err = Some(e),
    }
    Err(last_err.unwrap_or_else(|| anyhow!("No charger control service found under /svc")))
}

/// Status label for the Sorrel USB input suspend control.
const SPMI_USB_SUSPEND_TARGET: &str = "SPMI 0x2954 (USB_IN_SUSPEND)";

/// Applies Sorrel fallback overrides where USB input suspend (`SPMI 0x2954`) and battery charge
/// enable (`fuchsia.power.battery.ChargerService`) are two independent controls.
async fn set_sorrel_fallback_mode(
    mode: fcharger::OperatingMode,
    legacy_enable: Option<bool>,
) -> Result<Option<String>> {
    // Quiet presence check: `set_spmi_power_source` selects (and reports) the instance itself.
    let spmi_present =
        !discover_instances(fidl_fuchsia_hardware_spmi::DebugServiceMarker::SERVICE_NAME)
            .is_empty();
    match mode {
        fcharger::OperatingMode::Discharging => {
            if spmi_present {
                spmi::set_spmi_power_source(PowerSource::Battery).await?;
                return Ok(Some(SPMI_USB_SUSPEND_TARGET.to_string()));
            }
            Ok(None)
        }
        fcharger::OperatingMode::Charging | fcharger::OperatingMode::Passthrough => {
            let mut results = Vec::new();
            if spmi_present {
                results.push(
                    spmi::set_spmi_power_source(PowerSource::Usb)
                        .await
                        .map(|()| SPMI_USB_SUSPEND_TARGET.to_string()),
                );
            }
            if let Some(enable) = legacy_enable
                && let Ok(p) = select_instance(fpowerbattery::ChargerServiceMarker::SERVICE_NAME)
            {
                results.push(set_legacy_charger_enable(&p, enable).await);
            }
            merge_fallback_results(results)
        }
        _ => Ok(None),
    }
}

/// Merges the outcomes of independent fallback controls. Succeeds with the applied targets if any
/// control was applied, or with `None` if no control was attempted. Otherwise returns the last
/// failure. Every failure that is not returned is printed as a warning, so none is lost.
fn merge_fallback_results(results: Vec<Result<String>>) -> Result<Option<String>> {
    let mut applied = Vec::new();
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok(target) => applied.push(target),
            Err(e) => errors.push(e),
        }
    }
    let returned_err = if applied.is_empty() { errors.pop() } else { None };
    for e in &errors {
        eprintln!("Warning: fallback control not applied: {e:#}");
    }
    match returned_err {
        Some(e) => Err(e),
        None if applied.is_empty() => Ok(None),
        None => Ok(Some(applied.join(" + "))),
    }
}

/// Connects to the `Debug` member of a charger `DebugService` instance, also returning a
/// description of the target for status messages.
fn connect_debug(path: &str) -> Result<(fcharger::DebugProxy, String)> {
    let debug_path = append_member_suffix(path, MEMBER_DEBUG);
    let debug_path_str = debug_path.to_str().context("invalid UTF-8 path")?;
    let proxy = connect_to_protocol_at_path::<fcharger::DebugMarker>(debug_path_str).with_context(
        || format!("Failed to connect to Debug charger protocol at {debug_path_str}"),
    )?;
    Ok((proxy, format!("fuchsia.hardware.power.charger.Debug ({debug_path_str})")))
}

async fn set_debug_control(path: &str, options: &fcharger::ControlOptions) -> Result<String> {
    let (proxy, target) = connect_debug(path)?;
    set_debug_control_with_proxy(&proxy, options).await?;
    Ok(target)
}

async fn set_debug_control_with_proxy(
    proxy: &fcharger::DebugProxy,
    options: &fcharger::ControlOptions,
) -> Result<()> {
    proxy
        .set_control(options)
        .await
        .context("Debug SetControl call failed")?
        .map_err(|status| anyhow!("Debug SetControl rejected with error: {:?}", status))
}

async fn set_debug_charger_mode(path: &str, mode: fcharger::OperatingMode) -> Result<String> {
    let options = fcharger::ControlOptions { operating_mode: Some(mode), ..Default::default() };
    set_debug_control(path, &options).await
}

async fn set_legacy_charger_enable(path: &str, enable: bool) -> Result<String> {
    let legacy_path = append_member_suffix(path, MEMBER_DEVICE);
    let legacy_path_str = legacy_path.to_str().context("invalid UTF-8 path")?;
    let proxy = connect_to_protocol_at_path::<fpowerbattery::ChargerMarker>(legacy_path_str)
        .with_context(|| {
            format!("Failed to connect to legacy Charger protocol at {legacy_path_str}")
        })?;

    proxy
        .enable(enable)
        .await
        .context("Enable call failed")?
        .map_err(|e| anyhow!("Enable rejected with domain error: {:?}", e))?;
    Ok(format!("fuchsia.power.battery.Charger ({legacy_path_str})"))
}

/// Argument for `batteryutil mode`: an operating mode to apply, or `auto` to clear the operating
/// mode override applied through the charger `DebugService`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeArg {
    /// Applies an operating mode: as a sticky `Debug` override where the charger `DebugService`
    /// is available, otherwise through the legacy or Sorrel controls.
    Set(ChargerModeArg),
    /// Clears the operating mode override applied through the charger `DebugService`.
    Auto,
}

impl std::str::FromStr for ModeArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        s.parse().map(Self::Set).map_err(|_| {
            format!(
                "Invalid mode '{s}'. Use charging/usb, passthrough, discharging/battery, otg, \
                 or auto."
            )
        })
    }
}

/// Argument for `batteryutil charger charge-current`: a fast charge current limit in microamps, or
/// `auto` to clear the fast charge current limit override applied through the charger
/// `DebugService`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentLimitArg {
    /// Applies a sticky fast charge current limit (in `uA`) through the charger `DebugService`.
    Set(u32),
    /// Clears the fast charge current limit override applied through the charger `DebugService`.
    Auto,
}

impl std::str::FromStr for CurrentLimitArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        parse_micro_value(s, "A").map(Self::Set).map_err(|e| {
            format!("Invalid charge current '{s}' ({e}). Use e.g. 2.5A, 2500mA, 2500000, or auto.")
        })
    }
}

/// Argument for `batteryutil charger float-voltage`: a battery float voltage limit in microvolts,
/// or `auto` to clear the float voltage override applied through the charger `DebugService`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoltageLimitArg {
    /// Applies a sticky battery float voltage limit (in `uV`) through the charger `DebugService`.
    Set(u32),
    /// Clears the battery float voltage override applied through the charger `DebugService`.
    Auto,
}

impl std::str::FromStr for VoltageLimitArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        parse_micro_value(s, "V").map(Self::Set).map_err(|e| {
            format!("Invalid float voltage '{s}' ({e}). Use e.g. 4.35V, 4350mV, 4350000, or auto.")
        })
    }
}

fn resolve_debug_instance(path: Option<&str>, command: &str) -> Result<String> {
    match path {
        Some(p) => debug_service_instance(p),
        None => select_instance(fcharger::DebugServiceMarker::SERVICE_NAME).with_context(|| {
            format!("'{command}' requires {}", fcharger::DebugServiceMarker::SERVICE_NAME)
        }),
    }
}

/// Applies a sticky fast charge current limit (in `uA`) through the charger `DebugService`.
pub async fn set_charge_current(path: Option<&str>, current_ua: u32) -> Result<()> {
    let instance = resolve_debug_instance(path, "charger charge-current")?;
    let options = fcharger::ControlOptions {
        charge_current_limit_ua: Some(current_ua),
        ..Default::default()
    };
    let target = set_debug_control(&instance, &options).await?;
    println!(
        "Successfully set charger fast charge current limit to {} via {target}",
        MicroUnit(current_ua, "A")
    );
    Ok(())
}

/// Withdraws the sticky fast charge current limit override applied through the charger
/// `DebugService`, leaving any other debug overrides intact.
pub(crate) async fn clear_charge_current_override(path: Option<&str>) -> Result<()> {
    let options =
        fcharger::ControlOptions { charge_current_limit_ua: Some(0), ..Default::default() };
    clear_charger_overrides(
        path,
        &options,
        "charger charge-current auto",
        "charger fast charge current limit override",
    )
    .await
}

/// Applies a sticky battery float voltage limit (in `uV`) through the charger `DebugService`.
pub async fn set_float_voltage(path: Option<&str>, voltage_uv: u32) -> Result<()> {
    let instance = resolve_debug_instance(path, "charger float-voltage")?;
    let options =
        fcharger::ControlOptions { float_voltage_uv: Some(voltage_uv), ..Default::default() };
    let target = set_debug_control(&instance, &options).await?;
    println!(
        "Successfully set charger float voltage to {} via {target}",
        MicroUnit(voltage_uv, "V")
    );
    Ok(())
}

/// Withdraws the sticky battery float voltage override applied through the charger `DebugService`,
/// leaving any other debug overrides intact.
pub(crate) async fn clear_float_voltage_override(path: Option<&str>) -> Result<()> {
    let options = fcharger::ControlOptions { float_voltage_uv: Some(0), ..Default::default() };
    clear_charger_overrides(
        path,
        &options,
        "charger float-voltage auto",
        "charger float voltage override",
    )
    .await
}

/// Applies a sticky charge termination (top-off) current (in `uA`) through the charger
/// `DebugService`.
pub async fn set_term_current(path: Option<&str>, current_ua: u32) -> Result<()> {
    let instance = resolve_debug_instance(path, "charger term-current")?;
    let options =
        fcharger::ControlOptions { charge_term_current_ua: Some(current_ua), ..Default::default() };
    let target = set_debug_control(&instance, &options).await?;
    println!(
        "Successfully set charger termination current to {} via {target}",
        MicroUnit(current_ua, "A")
    );
    Ok(())
}

/// Withdraws the sticky charge termination current override applied through the charger
/// `DebugService`, leaving any other debug overrides intact.
pub(crate) async fn clear_term_current_override(path: Option<&str>) -> Result<()> {
    let options =
        fcharger::ControlOptions { charge_term_current_ua: Some(0), ..Default::default() };
    clear_charger_overrides(
        path,
        &options,
        "charger term-current auto",
        "charger termination current override",
    )
    .await
}

/// Withdraws sticky overrides matching `options` (or all overrides when `options` is empty)
/// through the charger `DebugService`, handing control back to the production policy client or
/// the charger's autonomous defaults.
pub(crate) async fn clear_charger_overrides(
    path: Option<&str>,
    options: &fcharger::ControlOptions,
    command: &str,
    description: &str,
) -> Result<()> {
    let instance = resolve_debug_instance(path, command)?;
    let target = clear_debug_overrides(&instance, options).await?;
    println!("Successfully cleared {description} via {target}");
    Ok(())
}

/// Withdraws the sticky operating mode override applied through the charger `DebugService`,
/// handing operating mode control back to the production policy client or the charger's autonomous
/// defaults while leaving any other debug overrides intact.
pub(crate) async fn clear_charger_mode_override(path: Option<&str>) -> Result<()> {
    // `ClearControl` only inspects field presence; `Charging` is a placeholder to mark
    // `operating_mode` as set in the mask.
    let options = fcharger::ControlOptions {
        operating_mode: Some(fcharger::OperatingMode::Charging),
        ..Default::default()
    };
    clear_charger_overrides(path, &options, "charger mode auto", "charger operating mode override")
        .await
}

/// Withdraws all sticky overrides applied through the charger `DebugService`, handing control of
/// all fields back to the production policy client or the charger's autonomous defaults.
pub(crate) async fn clear_all_charger_overrides(path: Option<&str>) -> Result<()> {
    clear_charger_overrides(
        path,
        &fcharger::ControlOptions::default(),
        "charger clear",
        "all charger overrides",
    )
    .await
}

/// Maps a charger service instance path to the matching `DebugService` instance, since sticky
/// overrides are applied and cleared exclusively through `Debug`.
fn debug_service_instance(path: &str) -> Result<String> {
    if path.contains(fpowerbattery::ChargerServiceMarker::SERVICE_NAME) {
        return Err(anyhow!(
            "legacy ChargerService has no DebugService overrides; use 'charger mode charging' instead"
        ));
    }
    Ok(path
        .replace(fcharger::ServiceMarker::SERVICE_NAME, fcharger::DebugServiceMarker::SERVICE_NAME))
}

async fn clear_debug_overrides(path: &str, options: &fcharger::ControlOptions) -> Result<String> {
    let (proxy, target) = connect_debug(path)?;
    clear_debug_overrides_with_proxy(&proxy, options).await?;
    Ok(target)
}

async fn clear_debug_overrides_with_proxy(
    proxy: &fcharger::DebugProxy,
    options: &fcharger::ControlOptions,
) -> Result<()> {
    proxy.clear_control(options).await.context("Debug ClearControl call failed")?.map_err(
        |domain_error| anyhow!("Debug ClearControl rejected with error: {:?}", domain_error),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_charger_mode_arg() {
        for s in ["charging", "charge", "usb"] {
            assert_eq!(
                s.parse::<ChargerModeArg>(),
                Ok(ChargerModeArg(fcharger::OperatingMode::Charging))
            );
        }
        assert_eq!(
            "passthrough".parse::<ChargerModeArg>(),
            Ok(ChargerModeArg(fcharger::OperatingMode::Passthrough))
        );
        for s in ["battery", "batt", "discharging", "discharge"] {
            assert_eq!(
                s.parse::<ChargerModeArg>(),
                Ok(ChargerModeArg(fcharger::OperatingMode::Discharging))
            );
        }
        assert_eq!(
            "otg".parse::<ChargerModeArg>(),
            Ok(ChargerModeArg(fcharger::OperatingMode::Otg))
        );
        for invalid in ["1", "0", "true", "false", "on", "off", "invalid"] {
            assert!(invalid.parse::<ChargerModeArg>().is_err());
        }
    }

    #[test]
    fn test_display_charger_spec_and_status() {
        let spec = fcharger::Spec {
            model: Some("MAX77779".to_string()),
            max_charge_current_ua: Some(4_500_000),
            max_charge_voltage_uv: Some(4_400_000),
            max_input_current_ua: Some(3_200_000),
            supported_modes: Some(vec![
                fcharger::OperatingMode::Charging,
                fcharger::OperatingMode::Passthrough,
                fcharger::OperatingMode::Discharging,
                fcharger::OperatingMode::Otg,
            ]),
            supported_options: Some(fcharger::WatchOptions {
                interest: Some(fcharger::Status {
                    online: Some(false),
                    operating_mode: Some(fcharger::OperatingMode::Charging),
                    input_current_limit_ua: Some(0),
                    ..Default::default()
                }),
                wake_on: Some(fcharger::Status { online: Some(false), ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let spec_out = DisplayChargerSpec(&spec).to_string();
        assert!(spec_out.contains("Charger Model: MAX77779"));
        assert!(spec_out.contains("Max Charge Current: 4.500 A"));
        assert!(spec_out.contains("Max Charge Voltage: 4.400 V"));
        assert!(spec_out.contains("Max Input Current: 3.200 A"));
        assert!(
            spec_out.contains("Supported Operating Modes: Charging, Passthrough, Discharging, OTG")
        );
        assert!(
            spec_out.contains("Supported Triggers: online, operating_mode, input_current_limit")
        );
        assert!(spec_out.contains("Supported Wake Triggers: online"));

        let status = fcharger::Status {
            online: Some(true),
            source_type: Some(fcharger::SourceType::Usb),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            charge_phase: Some(fcharger::ChargePhase::Fast),
            health: Some(fcharger::Health::Good),
            input_current_limit_ua: Some(1_500_000),
            input_current_ua: Some(1_200_000),
            charge_current_limit_ua: Some(2_500_000),
            float_voltage_uv: Some(4_350_000),
            ..Default::default()
        };
        let status_out = DisplayChargerStatus(&status).to_string();
        assert!(status_out.contains("Online: true"));
        assert!(status_out.contains("Source Type: USB"));
        assert!(status_out.contains("Operating Mode: Charging"));
        assert!(status_out.contains("Charge Phase: Fast"));
        assert!(status_out.contains("Health: Good"));
        assert!(status_out.contains("Input Current Limit: 1.500 A"));
        assert!(status_out.contains("Input Current: 1.200 A"));
        assert!(status_out.contains("Charge Current Limit: 2.500 A"));
        assert!(status_out.contains("Float Voltage: 4.350 V"));
    }

    #[fuchsia::test]
    async fn test_charger_watch_stream_stops_on_domain_error() {
        let (charger_proxy, mut charger_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::ChargerMarker>();
        let server_task = fuchsia_async::Task::local(async move {
            let Some(Ok(fcharger::ChargerRequest::Watch { responder, .. })) =
                charger_stream.next().await
            else {
                panic!("expected a first Watch request");
            };
            responder
                .send(Ok((
                    &fcharger::Status {
                        online: Some(true),
                        operating_mode: Some(fcharger::OperatingMode::Passthrough),
                        ..Default::default()
                    },
                    None,
                )))
                .expect("send Watch status");
            let Some(Ok(fcharger::ChargerRequest::Watch { responder, .. })) =
                charger_stream.next().await
            else {
                panic!("expected a second Watch request");
            };
            responder.send(Err(fcharger::Error::Io)).expect("send Watch error");
        });

        let mut watch_stream = charger_watch_stream(charger_proxy);
        let first = watch_stream.next().await.expect("first item").expect("ok status");
        assert_eq!(first.online, Some(true));
        assert_eq!(first.operating_mode, Some(fcharger::OperatingMode::Passthrough));

        let second = watch_stream.next().await.expect("error item");
        assert!(second.is_err());
        assert!(watch_stream.next().await.is_none());
        server_task.await;
    }

    #[fuchsia::test]
    async fn test_set_debug_charger_mode_sends_operating_mode() {
        let (debug_proxy, mut debug_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::DebugMarker>();
        let debug_server = fuchsia_async::Task::local(async move {
            let Some(Ok(fcharger::DebugRequest::SetControl { options, responder })) =
                debug_stream.next().await
            else {
                panic!("expected a SetControl request");
            };
            assert_eq!(options.operating_mode, Some(fcharger::OperatingMode::Discharging));
            responder.send(Ok(())).expect("send SetControl response");
        });
        set_debug_control_with_proxy(
            &debug_proxy,
            &fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Discharging),
                ..Default::default()
            },
        )
        .await
        .expect("set_debug_control_with_proxy succeeded");
        debug_server.await;
    }

    #[fuchsia::test]
    async fn test_set_debug_control_sends_charge_current_and_float_voltage() {
        let (debug_proxy, mut debug_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::DebugMarker>();
        let debug_server = fuchsia_async::Task::local(async move {
            let Some(Ok(fcharger::DebugRequest::SetControl { options, responder })) =
                debug_stream.next().await
            else {
                panic!("expected first SetControl request");
            };
            assert_eq!(options.charge_current_limit_ua, Some(2_500_000));
            assert!(options.operating_mode.is_none());
            assert!(options.float_voltage_uv.is_none());
            responder.send(Ok(())).expect("send first SetControl response");

            let Some(Ok(fcharger::DebugRequest::SetControl { options, responder })) =
                debug_stream.next().await
            else {
                panic!("expected second SetControl request");
            };
            assert_eq!(options.float_voltage_uv, Some(4_350_000));
            assert!(options.operating_mode.is_none());
            assert!(options.charge_current_limit_ua.is_none());
            responder.send(Ok(())).expect("send second SetControl response");
        });
        set_debug_control_with_proxy(
            &debug_proxy,
            &fcharger::ControlOptions {
                charge_current_limit_ua: Some(2_500_000),
                ..Default::default()
            },
        )
        .await
        .expect("set charge current succeeded");
        set_debug_control_with_proxy(
            &debug_proxy,
            &fcharger::ControlOptions { float_voltage_uv: Some(4_350_000), ..Default::default() },
        )
        .await
        .expect("set float voltage succeeded");
        debug_server.await;
    }

    /// Verifies that a termination current override is sent as `charge_term_current_ua` alone.
    #[fuchsia::test]
    async fn test_set_debug_control_sends_term_current() {
        let (debug_proxy, mut debug_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::DebugMarker>();
        let debug_server = fuchsia_async::Task::local(async move {
            let Some(Ok(fcharger::DebugRequest::SetControl { options, responder })) =
                debug_stream.next().await
            else {
                panic!("expected a SetControl request");
            };
            assert_eq!(
                options,
                fcharger::ControlOptions {
                    charge_term_current_ua: Some(200_000),
                    ..Default::default()
                }
            );
            responder.send(Ok(())).expect("send SetControl response");
        });
        set_debug_control_with_proxy(
            &debug_proxy,
            &fcharger::ControlOptions {
                charge_term_current_ua: Some(200_000),
                ..Default::default()
            },
        )
        .await
        .expect("set termination current succeeded");
        debug_server.await;
    }

    #[test]
    fn test_parse_mode_arg() {
        for s in ["auto", "AUTO", "Auto"] {
            assert_eq!(s.parse::<ModeArg>(), Ok(ModeArg::Auto));
        }
        assert_eq!(
            "usb".parse::<ModeArg>(),
            Ok(ModeArg::Set(ChargerModeArg(fcharger::OperatingMode::Charging)))
        );
        assert_eq!(
            "otg".parse::<ModeArg>(),
            Ok(ModeArg::Set(ChargerModeArg(fcharger::OperatingMode::Otg)))
        );
        let err = "automatic".parse::<ModeArg>().unwrap_err();
        assert!(err.contains("or auto"), "unexpected error: {err}");
    }

    #[test]
    fn test_parse_current_and_voltage_limit_args() {
        for s in ["auto", "AUTO", "Auto"] {
            assert_eq!(s.parse::<CurrentLimitArg>(), Ok(CurrentLimitArg::Auto));
            assert_eq!(s.parse::<VoltageLimitArg>(), Ok(VoltageLimitArg::Auto));
        }
        assert_eq!("2.5A".parse::<CurrentLimitArg>(), Ok(CurrentLimitArg::Set(2_500_000)));
        assert_eq!("2500mA".parse::<CurrentLimitArg>(), Ok(CurrentLimitArg::Set(2_500_000)));
        assert_eq!("2500000".parse::<CurrentLimitArg>(), Ok(CurrentLimitArg::Set(2_500_000)));
        assert!("invalid".parse::<CurrentLimitArg>().is_err());

        assert_eq!("4.35V".parse::<VoltageLimitArg>(), Ok(VoltageLimitArg::Set(4_350_000)));
        assert_eq!("4350mV".parse::<VoltageLimitArg>(), Ok(VoltageLimitArg::Set(4_350_000)));
        assert_eq!("4350000".parse::<VoltageLimitArg>(), Ok(VoltageLimitArg::Set(4_350_000)));
        assert!("invalid".parse::<VoltageLimitArg>().is_err());
    }

    #[test]
    fn test_debug_service_instance_mapping() {
        let service = format!("/svc/{}/default", fcharger::ServiceMarker::SERVICE_NAME);
        let debug = format!("/svc/{}/default", fcharger::DebugServiceMarker::SERVICE_NAME);
        assert_eq!(debug_service_instance(&service).unwrap(), debug);
        assert_eq!(debug_service_instance(&debug).unwrap(), debug);

        // A `Controller` member path resolves to the `Debug` member, never to `Controller`.
        let controller = debug_service_instance(&format!("{service}/controller")).unwrap();
        assert_eq!(
            append_member_suffix(controller, MEMBER_DEBUG),
            std::path::PathBuf::from(format!("{debug}/debug"))
        );

        let legacy = format!("/svc/{}/default", fpowerbattery::ChargerServiceMarker::SERVICE_NAME);
        assert!(debug_service_instance(&legacy).is_err());
    }

    #[fuchsia::test]
    async fn test_clear_debug_overrides_sends_clear_control() {
        let (debug_proxy, mut debug_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::DebugMarker>();
        let debug_server = fuchsia_async::Task::local(async move {
            // 1. Selective mode override clear.
            let Some(Ok(fcharger::DebugRequest::ClearControl { options, responder })) =
                debug_stream.next().await
            else {
                panic!("expected first ClearControl request");
            };
            assert!(options.operating_mode.is_some());
            assert!(options.input_current_limit_ua.is_none());
            assert!(options.charge_current_limit_ua.is_none());
            assert!(options.float_voltage_uv.is_none());
            responder.send(Ok(())).expect("send ClearControl response");
            let Some(Ok(fcharger::DebugRequest::ClearControl { options, responder })) =
                debug_stream.next().await
            else {
                panic!("expected second ClearControl request");
            };
            assert!(options.charge_current_limit_ua.is_some());
            assert!(options.operating_mode.is_none());
            assert!(options.float_voltage_uv.is_none());
            responder.send(Ok(())).expect("send ClearControl response");

            let Some(Ok(fcharger::DebugRequest::ClearControl { options, responder })) =
                debug_stream.next().await
            else {
                panic!("expected third ClearControl request");
            };
            assert!(options.float_voltage_uv.is_some());
            assert!(options.operating_mode.is_none());
            assert!(options.charge_current_limit_ua.is_none());
            responder.send(Ok(())).expect("send ClearControl response");

            let Some(Ok(fcharger::DebugRequest::ClearControl { options, responder })) =
                debug_stream.next().await
            else {
                panic!("expected fourth ClearControl request");
            };
            assert!(options.charge_term_current_ua.is_some());
            assert!(options.operating_mode.is_none());
            assert!(options.charge_current_limit_ua.is_none());
            assert!(options.float_voltage_uv.is_none());
            responder.send(Ok(())).expect("send ClearControl response");

            let Some(Ok(fcharger::DebugRequest::ClearControl { options, responder })) =
                debug_stream.next().await
            else {
                panic!("expected fifth ClearControl request");
            };
            assert_eq!(options, fcharger::ControlOptions::default());
            responder.send(Ok(())).expect("send ClearControl response");
        });
        let mode_mask = fcharger::ControlOptions {
            operating_mode: Some(fcharger::OperatingMode::Charging),
            ..Default::default()
        };
        clear_debug_overrides_with_proxy(&debug_proxy, &mode_mask)
            .await
            .expect("clear mode override succeeded");
        let cc_mask =
            fcharger::ControlOptions { charge_current_limit_ua: Some(0), ..Default::default() };
        clear_debug_overrides_with_proxy(&debug_proxy, &cc_mask)
            .await
            .expect("clear charge current override succeeded");
        let fv_mask = fcharger::ControlOptions { float_voltage_uv: Some(0), ..Default::default() };
        clear_debug_overrides_with_proxy(&debug_proxy, &fv_mask)
            .await
            .expect("clear float voltage override succeeded");
        let term_mask =
            fcharger::ControlOptions { charge_term_current_ua: Some(0), ..Default::default() };
        clear_debug_overrides_with_proxy(&debug_proxy, &term_mask)
            .await
            .expect("clear termination current override succeeded");
        clear_debug_overrides_with_proxy(&debug_proxy, &fcharger::ControlOptions::default())
            .await
            .expect("clear all overrides succeeded");
        debug_server.await;
    }

    #[fuchsia::test]
    async fn test_clear_debug_overrides_propagates_driver_error() {
        let (debug_proxy, mut debug_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::DebugMarker>();
        let debug_server = fuchsia_async::Task::local(async move {
            let Some(Ok(fcharger::DebugRequest::ClearControl { options, responder })) =
                debug_stream.next().await
            else {
                panic!("expected a ClearControl request");
            };
            assert!(options.operating_mode.is_some());
            responder.send(Err(fcharger::Error::Io)).expect("send ClearControl error");
        });
        let mask = fcharger::ControlOptions {
            operating_mode: Some(fcharger::OperatingMode::Charging),
            ..Default::default()
        };
        let err = clear_debug_overrides_with_proxy(&debug_proxy, &mask).await.unwrap_err();
        assert!(format!("{err:#}").contains("Io"), "unexpected error: {err:#}");
        debug_server.await;
    }

    #[test]
    fn test_merge_fallback_results() {
        assert!(merge_fallback_results(vec![]).unwrap().is_none());
        assert_eq!(
            merge_fallback_results(vec![Ok("spmi".to_string()), Ok("legacy".to_string())])
                .unwrap()
                .as_deref(),
            Some("spmi + legacy")
        );
        // A partial success still succeeds; the failure is only reported as a warning.
        assert_eq!(
            merge_fallback_results(vec![Ok("spmi".to_string()), Err(anyhow!("legacy failed"))])
                .unwrap()
                .as_deref(),
            Some("spmi")
        );
        // With nothing applied, the last failure is returned and earlier ones become warnings.
        let err =
            merge_fallback_results(vec![Err(anyhow!("spmi failed")), Err(anyhow!("no enable"))])
                .unwrap_err();
        assert_eq!(err.to_string(), "no enable");
    }
}
