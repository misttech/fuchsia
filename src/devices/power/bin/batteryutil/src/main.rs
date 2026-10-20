// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Battery CLI Diagnostic and Control Tool.

mod battery;
mod charger;
mod common;
mod spmi;

use anyhow::Result;
use argh::FromArgs;
use battery::BatteryInstance;
use charger::{ChargerInstance, ChargerModeArg, CurrentLimitArg, ModeArg, VoltageLimitArg};
use fidl::endpoints::ServiceMarker;
use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fidl_fuchsia_power_battery as fpowerbattery;

#[derive(FromArgs, Debug, PartialEq)]
/// Inspect battery telemetry and control power/charging.
pub struct Args {
    #[argh(option, short = 'p')]
    /// optional specific service instance or device path (e.g.
    /// '/svc/fuchsia.hardware.power.battery.Service/default')
    pub path: Option<String>,

    #[argh(subcommand)]
    pub command: Option<Subcommand>,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
pub enum Subcommand {
    Get(GetCommand),
    Watch(WatchCommand),
    Battery(BatteryCommand),
    Charger(ChargerCommand),
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "get")]
/// inspect battery and charger telemetry
pub struct GetCommand {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "watch")]
/// stream real-time battery and charger status updates via hanging-get
pub struct WatchCommand {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "battery")]
/// battery telemetry subcommands (e.g. 'batteryutil battery get')
pub struct BatteryCommand {
    #[argh(subcommand)]
    pub command: Option<BatterySubcommand>,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
pub enum BatterySubcommand {
    Get(BatteryGetCommand),
    Watch(BatteryWatchCommand),
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "get")]
/// inspect battery telemetry
pub struct BatteryGetCommand {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "watch")]
/// stream real-time battery status updates via hanging-get
pub struct BatteryWatchCommand {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "charger")]
/// charger telemetry and control subcommands (e.g. 'batteryutil charger mode charging')
pub struct ChargerCommand {
    #[argh(subcommand)]
    pub command: Option<ChargerSubcommand>,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
pub enum ChargerSubcommand {
    Get(ChargerGetCommand),
    Watch(ChargerWatchCommand),
    Mode(ModeCommand),
    ChargeCurrent(ChargeCurrentCommand),
    FloatVoltage(FloatVoltageCommand),
    TermCurrent(TermCurrentCommand),
    Clear(ClearCommand),
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "get")]
/// inspect charger telemetry
pub struct ChargerGetCommand {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "watch")]
/// stream real-time charger status updates via hanging-get
pub struct ChargerWatchCommand {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "mode")]
/// set charger operating mode (charging/usb, passthrough, discharging/battery, otg, auto)
pub struct ModeCommand {
    #[argh(positional)]
    /// mode: charging/usb, passthrough, discharging/battery, otg, or auto (clear mode override)
    pub mode: ModeArg,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "charge-current")]
/// set charger fast charge current limit (e.g. 2.5A, 2500mA, 2500000, or auto)
pub struct ChargeCurrentCommand {
    #[argh(positional)]
    /// charge current limit (e.g. 2.5A, 2500mA, uA) or auto (clear current limit override)
    pub current: CurrentLimitArg,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "float-voltage")]
/// set charger battery float voltage limit (e.g. 4.35V, 4350mV, 4350000, or auto)
pub struct FloatVoltageCommand {
    #[argh(positional)]
    /// float voltage limit (e.g. 4.35V, 4350mV, uV) or auto (clear float voltage override)
    pub voltage: VoltageLimitArg,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "term-current")]
/// set charger charge termination (top-off) current (e.g. 200mA, 200000, or auto)
pub struct TermCurrentCommand {
    #[argh(positional)]
    /// termination current (e.g. 0.2A, 200mA, uA) or auto (clear termination current override)
    pub current: CurrentLimitArg,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "clear")]
/// clear all sticky charger DebugService overrides
pub struct ClearCommand {}

#[fuchsia::main]
async fn main() -> Result<()> {
    let args: Args = argh::from_env();
    let path = args.path.as_deref();

    match args.command {
        None | Some(Subcommand::Get(_)) => get_all_info(path).await,
        Some(Subcommand::Watch(_)) => watch_all(path).await,
        Some(Subcommand::Battery(BatteryCommand { command })) => match command {
            None | Some(BatterySubcommand::Get(_)) => BatteryInstance::get_info(path).await,
            Some(BatterySubcommand::Watch(_)) => BatteryInstance::watch(path).await,
        },
        Some(Subcommand::Charger(ChargerCommand { command })) => match command {
            None | Some(ChargerSubcommand::Get(_)) => ChargerInstance::get_info(path).await,
            Some(ChargerSubcommand::Watch(_)) => ChargerInstance::watch(path).await,
            Some(ChargerSubcommand::Mode(cmd)) => match cmd.mode {
                ModeArg::Set(ChargerModeArg(mode)) => charger::set_charger_mode(path, mode).await,
                ModeArg::Auto => charger::clear_charger_mode_override(path).await,
            },
            Some(ChargerSubcommand::ChargeCurrent(cmd)) => match cmd.current {
                CurrentLimitArg::Set(ua) => charger::set_charge_current(path, ua).await,
                CurrentLimitArg::Auto => charger::clear_charge_current_override(path).await,
            },
            Some(ChargerSubcommand::FloatVoltage(cmd)) => match cmd.voltage {
                VoltageLimitArg::Set(uv) => charger::set_float_voltage(path, uv).await,
                VoltageLimitArg::Auto => charger::clear_float_voltage_override(path).await,
            },
            Some(ChargerSubcommand::TermCurrent(cmd)) => match cmd.current {
                CurrentLimitArg::Set(ua) => charger::set_term_current(path, ua).await,
                CurrentLimitArg::Auto => charger::clear_term_current_override(path).await,
            },
            Some(ChargerSubcommand::Clear(_)) => charger::clear_all_charger_overrides(path).await,
        },
    }
}

/// Queries both battery and charger telemetry when `--path` is omitted, or dispatches `--path` to
/// the matching battery or charger protocol.
async fn get_all_info(path: Option<&str>) -> Result<()> {
    if let Some(p) = path {
        if p.contains(fcharger::DebugServiceMarker::SERVICE_NAME) {
            anyhow::bail!(
                "DebugService path '{p}' is control-only; pass a {} or {} path for telemetry, or \
                 omit --path to use discovery",
                fbattery::ServiceMarker::SERVICE_NAME,
                fcharger::ServiceMarker::SERVICE_NAME
            );
        }
        if p.contains(fcharger::ServiceMarker::SERVICE_NAME) || p.ends_with("/charger") {
            return ChargerInstance::get_info(Some(p)).await;
        }
        if p.contains(fbattery::ServiceMarker::SERVICE_NAME)
            || p.contains(fpowerbattery::InfoServiceMarker::SERVICE_NAME)
            || p.ends_with("/battery")
            || p.ends_with("/device")
        {
            return BatteryInstance::get_info(Some(p)).await;
        }
        // Probe an unlabeled path newest-first: modern Battery, modern Charger, then legacy.
        if BatteryInstance::get_modern_info_at(p).await.is_ok() {
            return Ok(());
        }
        if ChargerInstance::get_modern_info_at(p).await.is_ok() {
            return Ok(());
        }
        return BatteryInstance::get_legacy_info_at(p).await;
    }

    let battery_instances = BatteryInstance::discover();
    let charger_instances = ChargerInstance::discover();
    let total = battery_instances.len() + charger_instances.len();
    if total == 0 {
        anyhow::bail!("No battery or charger service instances found under /svc");
    }

    let show_headers = total > 1;
    let mut succeeded = false;
    succeeded |= BatteryInstance::report_all(&battery_instances, show_headers).await;
    succeeded |= ChargerInstance::report_all(&charger_instances, show_headers).await;
    if !succeeded {
        anyhow::bail!("Failed to query any battery or charger instances");
    }
    Ok(())
}

/// Multiplexes both battery and charger watch streams when `--path` is omitted, or dispatches
/// `--path` to the matching battery or charger watcher.
async fn watch_all(path: Option<&str>) -> Result<()> {
    if let Some(p) = path {
        if p.contains(fcharger::DebugServiceMarker::SERVICE_NAME) {
            anyhow::bail!(
                "DebugService path '{p}' is control-only; pass a {} or {} path for telemetry, or \
                 omit --path to use discovery",
                fbattery::ServiceMarker::SERVICE_NAME,
                fcharger::ServiceMarker::SERVICE_NAME
            );
        }
        if p.contains(fcharger::ServiceMarker::SERVICE_NAME) || p.ends_with("/charger") {
            return ChargerInstance::watch(Some(p)).await;
        }
        return BatteryInstance::watch(Some(p)).await;
    }

    let mut streams = BatteryInstance::discover_watch_streams()?;
    streams.extend(ChargerInstance::discover_watch_streams()?);
    if streams.is_empty() {
        anyhow::bail!(
            "No modern fuchsia.hardware.power.battery or charger instances found under /svc"
        );
    }
    common::run_watch_streams(streams, "battery and charger").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_argh_args_parsing() {
        let args = Args::from_args(&["batteryutil"], &["get"]).unwrap();
        assert_eq!(args.command, Some(Subcommand::Get(GetCommand {})));

        let args = Args::from_args(&["batteryutil"], &["watch"]).unwrap();
        assert_eq!(args.command, Some(Subcommand::Watch(WatchCommand {})));

        let args = Args::from_args(&["batteryutil"], &["battery"]).unwrap();
        assert_eq!(args.command, Some(Subcommand::Battery(BatteryCommand { command: None })));

        let args = Args::from_args(&["batteryutil"], &["battery", "get"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Battery(BatteryCommand {
                command: Some(BatterySubcommand::Get(BatteryGetCommand {}))
            }))
        );

        let args = Args::from_args(&["batteryutil"], &["battery", "watch"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Battery(BatteryCommand {
                command: Some(BatterySubcommand::Watch(BatteryWatchCommand {}))
            }))
        );

        let args = Args::from_args(&["batteryutil"], &["charger"]).unwrap();
        assert_eq!(args.command, Some(Subcommand::Charger(ChargerCommand { command: None })));

        let args = Args::from_args(&["batteryutil"], &["charger", "get"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::Get(ChargerGetCommand {}))
            }))
        );

        let args = Args::from_args(&["batteryutil"], &["charger", "watch"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::Watch(ChargerWatchCommand {}))
            }))
        );

        let args =
            Args::from_args(&["batteryutil"], &["-p", "/svc/test", "charger", "mode", "charging"])
                .unwrap();
        assert_eq!(args.path, Some("/svc/test".to_string()));
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::Mode(ModeCommand {
                    mode: ModeArg::Set(ChargerModeArg(fcharger::OperatingMode::Charging))
                }))
            }))
        );

        let args = Args::from_args(&["batteryutil"], &["charger", "mode", "battery"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::Mode(ModeCommand {
                    mode: ModeArg::Set(ChargerModeArg(fcharger::OperatingMode::Discharging))
                }))
            }))
        );

        let args = Args::from_args(&["batteryutil"], &["charger", "mode", "auto"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::Mode(ModeCommand { mode: ModeArg::Auto }))
            }))
        );

        let args = Args::from_args(&["batteryutil"], &["charger", "mode", "passthrough"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::Mode(ModeCommand {
                    mode: ModeArg::Set(ChargerModeArg(fcharger::OperatingMode::Passthrough))
                }))
            }))
        );

        let args =
            Args::from_args(&["batteryutil"], &["charger", "charge-current", "2.5A"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::ChargeCurrent(ChargeCurrentCommand {
                    current: CurrentLimitArg::Set(2_500_000)
                }))
            }))
        );

        let args =
            Args::from_args(&["batteryutil"], &["charger", "charge-current", "auto"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::ChargeCurrent(ChargeCurrentCommand {
                    current: CurrentLimitArg::Auto
                }))
            }))
        );

        let args =
            Args::from_args(&["batteryutil"], &["charger", "float-voltage", "4.35V"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::FloatVoltage(FloatVoltageCommand {
                    voltage: VoltageLimitArg::Set(4_350_000)
                }))
            }))
        );

        let args =
            Args::from_args(&["batteryutil"], &["charger", "float-voltage", "auto"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::FloatVoltage(FloatVoltageCommand {
                    voltage: VoltageLimitArg::Auto
                }))
            }))
        );

        let args =
            Args::from_args(&["batteryutil"], &["charger", "term-current", "200mA"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::TermCurrent(TermCurrentCommand {
                    current: CurrentLimitArg::Set(200_000)
                }))
            }))
        );

        let args = Args::from_args(&["batteryutil"], &["charger", "term-current", "auto"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::TermCurrent(TermCurrentCommand {
                    current: CurrentLimitArg::Auto
                }))
            }))
        );

        let args = Args::from_args(&["batteryutil"], &["charger", "clear"]).unwrap();
        assert_eq!(
            args.command,
            Some(Subcommand::Charger(ChargerCommand {
                command: Some(ChargerSubcommand::Clear(ClearCommand {}))
            }))
        );
    }
}
