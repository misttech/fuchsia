// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use argh::FromArgs;
use fidl_fuchsia_usb_policy as usb_policy;

mod config;
mod health;
mod inspect;

#[derive(FromArgs, PartialEq, Debug)]
/// USB diagnostics and configuration CLI tool.
struct UsbCliArgs {
    #[argh(subcommand)]
    subcommand: SubCommand,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
enum SubCommand {
    Health(HealthArgs),
    Inspect(InspectArgs),
    Diagnostics(DiagnosticsArgs),
    Diag(DiagArgs),
    GetConfig(GetConfigArgs),
    SetConfig(SetConfigArgs),
}

#[derive(FromArgs, PartialEq, Debug)]
/// Prints the USB policy health report.
#[argh(subcommand, name = "health")]
struct HealthArgs {
    /// prints verbose health details
    #[argh(switch, short = 'v')]
    verbose: bool,
}

#[derive(FromArgs, PartialEq, Debug)]
/// Prints the device-side USB Inspect diagnostics.
#[argh(subcommand, name = "inspect")]
struct InspectArgs {}

#[derive(FromArgs, PartialEq, Debug)]
/// Prints both USB health report and Inspect diagnostics.
#[argh(subcommand, name = "diagnostics")]
struct DiagnosticsArgs {
    /// prints verbose health details
    #[argh(switch, short = 'v')]
    verbose: bool,
}

#[derive(FromArgs, PartialEq, Debug)]
/// Prints both USB health report and Inspect diagnostics (alias for 'diagnostics').
#[argh(subcommand, name = "diag")]
struct DiagArgs {
    /// prints verbose health details
    #[argh(switch, short = 'v')]
    verbose: bool,
}

#[derive(FromArgs, PartialEq, Debug)]
/// Prints the current USB peripheral configuration in JSON format.
#[argh(subcommand, name = "get-config")]
struct GetConfigArgs {}

#[derive(FromArgs, PartialEq, Debug)]
/// Sets the USB peripheral configuration from JSON string, JSON file path, or comma-separated functions (e.g. "cdc,test").
#[argh(subcommand, name = "set-config")]
struct SetConfigArgs {
    /// JSON configuration string, JSON file path, or comma-separated functions.
    #[argh(positional)]
    config: String,
}

#[fuchsia::main(logging_tags = ["usb-cli"])]
async fn main() {
    if let Err(e) = run_cli().await {
        eprintln!("usb-cli error: {:?}", e);
        std::process::exit(1);
    }
    println!("[usb-cli:DONE]");
}

async fn get_configuration_client() -> Result<usb_policy::ConfigurationProxy, Error> {
    if std::path::Path::new("/exposed/fuchsia.usb.policy.Configuration").exists() {
        return fuchsia_component::client::connect_to_protocol_at_path::<
            usb_policy::ConfigurationMarker,
        >("/exposed/fuchsia.usb.policy.Configuration")
        .context("Failed to connect to /exposed/fuchsia.usb.policy.Configuration");
    }

    fuchsia_component::client::connect_to_protocol::<usb_policy::ConfigurationMarker>()
        .context("Failed to connect to fuchsia.usb.policy.Configuration protocol")
}

async fn run_get_config(_args: GetConfigArgs) -> Result<(), Error> {
    let config_client = get_configuration_client().await?;
    let (_device_desc, config_descriptors) = config_client
        .get_configuration()
        .await
        .context("Failed FIDL call get_configuration")?
        .map_err(zx::Status::from_raw)
        .context("GetConfiguration returned an error status")?;

    let functions: Vec<String> = config_descriptors
        .into_iter()
        .flatten()
        .map(|func| config::descriptor_to_function_name(&func))
        .collect();

    let json_output = config::UsbConfigJson { functions };

    let serialized = serde_json::to_string_pretty(&json_output).context("Failed to format JSON")?;
    println!("{}", serialized);
    Ok(())
}

async fn run_set_config(args: SetConfigArgs) -> Result<(), Error> {
    let parsed_config = config::load_config_input(&args.config)?;
    let config_client = get_configuration_client().await?;

    let func_descriptors = parsed_config
        .functions
        .iter()
        .map(|name| config::function_name_to_descriptor(name))
        .collect::<Result<Vec<_>, _>>()?;

    let (mut device_desc, _) = config_client
        .get_configuration()
        .await
        .context("Failed FIDL call get_configuration")?
        .map_err(zx::Status::from_raw)
        .context("GetConfiguration returned an error status")?;

    device_desc.b_num_configurations = 1;

    println!("Applying new configuration via Policy (functions: {:?})...", parsed_config.functions);
    config_client
        .set_configuration(&device_desc, &[func_descriptors])
        .await
        .context("Failed set_configuration FIDL call")?
        .map_err(zx::Status::from_raw)
        .context("SetConfiguration returned an error status")?;

    println!("Successfully applied USB peripheral configuration.");
    Ok(())
}

async fn get_health_report() -> Result<usb_policy::HealthReport, Error> {
    let health =
        fuchsia_component::client::connect_to_protocol_at_path::<usb_policy::HealthMarker>(
            "/exposed/fuchsia.usb.policy.Health",
        )
        .map_err(|e| {
            anyhow::format_err!("Failed to connect to Health protocol at /exposed: {:?}", e)
        })?;

    health
        .get_report()
        .await
        .map_err(|e| anyhow::format_err!("Failed to communicate (get_report): {:?}", e))?
        .map_err(|e| {
            anyhow::format_err!(
                "Failed to get report (zx status): {:?}",
                zx::Status::err_from_raw(e)
            )
        })
}

async fn run_health(args: HealthArgs) -> Result<(), Error> {
    match get_health_report().await {
        Ok(report) => {
            if args.verbose {
                println!("{}", health::format_verbose(&report));
            } else {
                println!("{}", health::format_dashboard(&report));
            }
            Ok(())
        }
        Err(e) => {
            println!(
                "USB Policy Health service not available: fuchsia.usb.policy.Health not found."
            );
            Err(e)
        }
    }
}

async fn run_diagnostics(verbose: bool) -> Result<(), Error> {
    match get_health_report().await {
        Ok(report) => {
            if verbose {
                println!("{}", health::format_verbose(&report));
            } else {
                println!("{}", health::format_dashboard(&report));
            }
        }
        Err(_) => {
            println!("USB Policy Health service not available (skipping).");
        }
    }

    if let Err(e) = inspect::print_usb_inspect_diagnostics().await {
        println!("Failed to print USB inspect diagnostics: {:?}", e);
    }

    Ok(())
}

async fn run_cli() -> Result<(), Error> {
    let args: UsbCliArgs = argh::from_env();

    match args.subcommand {
        SubCommand::Health(sc_args) => run_health(sc_args).await,
        SubCommand::Inspect(_) => inspect::print_usb_inspect_diagnostics().await,
        SubCommand::Diagnostics(sc_args) => run_diagnostics(sc_args.verbose).await,
        SubCommand::Diag(sc_args) => run_diagnostics(sc_args.verbose).await,
        SubCommand::GetConfig(sc_args) => run_get_config(sc_args).await,
        SubCommand::SetConfig(sc_args) => run_set_config(sc_args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_missing_subcommand() {
        assert!(UsbCliArgs::from_args(&["usb-cli"], &[]).is_err());
    }

    #[test]
    fn test_parse_health() {
        let args = UsbCliArgs::from_args(&["usb-cli"], &["health"]).unwrap();
        assert_eq!(
            args,
            UsbCliArgs { subcommand: SubCommand::Health(HealthArgs { verbose: false }) }
        );

        let args_v = UsbCliArgs::from_args(&["usb-cli"], &["health", "-v"]).unwrap();
        assert_eq!(
            args_v,
            UsbCliArgs { subcommand: SubCommand::Health(HealthArgs { verbose: true }) }
        );
    }

    #[test]
    fn test_parse_inspect() {
        let args = UsbCliArgs::from_args(&["usb-cli"], &["inspect"]).unwrap();
        assert_eq!(args, UsbCliArgs { subcommand: SubCommand::Inspect(InspectArgs {}) });
    }

    #[test]
    fn test_parse_diagnostics() {
        let args = UsbCliArgs::from_args(&["usb-cli"], &["diagnostics"]).unwrap();
        assert_eq!(
            args,
            UsbCliArgs { subcommand: SubCommand::Diagnostics(DiagnosticsArgs { verbose: false }) }
        );

        let args_v = UsbCliArgs::from_args(&["usb-cli"], &["diagnostics", "--verbose"]).unwrap();
        assert_eq!(
            args_v,
            UsbCliArgs { subcommand: SubCommand::Diagnostics(DiagnosticsArgs { verbose: true }) }
        );
    }

    #[test]
    fn test_parse_diag_alias() {
        let args = UsbCliArgs::from_args(&["usb-cli"], &["diag"]).unwrap();
        assert_eq!(args, UsbCliArgs { subcommand: SubCommand::Diag(DiagArgs { verbose: false }) });

        let args_v = UsbCliArgs::from_args(&["usb-cli"], &["diag", "-v"]).unwrap();
        assert_eq!(args_v, UsbCliArgs { subcommand: SubCommand::Diag(DiagArgs { verbose: true }) });

        let args_verbose = UsbCliArgs::from_args(&["usb-cli"], &["diag", "--verbose"]).unwrap();
        assert_eq!(
            args_verbose,
            UsbCliArgs { subcommand: SubCommand::Diag(DiagArgs { verbose: true }) }
        );
    }

    #[test]
    fn test_parse_get_config() {
        let args = UsbCliArgs::from_args(&["usb-cli"], &["get-config"]).unwrap();
        assert_eq!(args, UsbCliArgs { subcommand: SubCommand::GetConfig(GetConfigArgs {}) });
    }

    #[test]
    fn test_parse_set_config() {
        let args = UsbCliArgs::from_args(&["usb-cli"], &["set-config", "cdc,test"]).unwrap();
        assert_eq!(
            args,
            UsbCliArgs {
                subcommand: SubCommand::SetConfig(SetConfigArgs { config: "cdc,test".to_string() }),
            }
        );
    }
}
