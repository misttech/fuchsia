// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::Write;

use anyhow::{Result, anyhow};
use argh::FromArgs;
use fidl_fuchsia_hardware_google_odpm as fgoogle_odpm;
use fidl_fuchsia_io as fio;
use fuchsia_component::client as fclient;

#[derive(FromArgs, Debug, PartialEq)]
/// Read ODPM power rails and configure polling settings.
///
/// Workflow:
/// # Start a shell in the scope of the ODPM driver.
/// $ ffx component explore <odpm_moniker>
/// # Read all rails.
/// $ odpm_util read
/// # Read only the "cpu_big" rail.
/// $ odpm_util read cpu_big
/// # Read instantaneous power (Watts).
/// $ odpm_util read -i
/// # Read current (Amperes) instead of power (Watts).
/// $ odpm_util read -c
/// # Read energy (Joules) instead of power (Watts).
/// $ odpm_util read -e
/// # Query or multiplex ODPM power rails.
/// $ odpm_util rails available
/// $ odpm_util rails enabled
/// $ odpm_util rails set 11 buck_13m
/// # Configure ODPM driver polling settings.
/// $ odpm_util config get
/// # Set the polling interval to 100ms, and record a series entry every 1s.
/// $ odpm_util config set --poll-interval-ms 100
/// $ odpm_util config set --polls-per-series-entry 10
/// # Restore the default configuration.
/// $ odpm_util config reset
struct Args {
    #[argh(subcommand)]
    command: Option<SubCommands>,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
enum SubCommands {
    Read(ReadArgs),
    Config(ConfigArgs),
    Rails(RailsArgs),
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "read")]
/// Read ODPM power rails.
struct ReadArgs {
    /// optional name of a specific rail to read (e.g. cpu_big)
    #[argh(positional)]
    rail: Option<String>,

    /// read instantaneous power (Watts) instead of average power
    #[argh(switch, short = 'i')]
    instantaneous: bool,

    /// read current (Amperes) instead of power (Watts)
    #[argh(switch, short = 'c')]
    current: bool,

    /// read energy (Joules) instead of power (Watts)
    #[argh(switch, short = 'e')]
    energy: bool,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "config")]
/// Query or configure ODPM driver polling settings.
struct ConfigArgs {
    #[argh(subcommand)]
    subcommand: ConfigSubCommands,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
enum ConfigSubCommands {
    Set(ConfigSetArgs),
    Get(ConfigGetArgs),
    Reset(ConfigResetArgs),
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "set")]
/// Set ODPM driver polling configuration parameters.
struct ConfigSetArgs {
    /// hardware polling interval in milliseconds (e.g. 100, 500, 1000)
    #[argh(option, short = 'i')]
    poll_interval_ms: Option<u32>,

    /// number of hardware polls per Inspect time-series entry (e.g. 10, 60)
    #[argh(option, short = 'p')]
    polls_per_series_entry: Option<u32>,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "get")]
/// Display current ODPM driver polling configuration.
struct ConfigGetArgs {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "reset")]
/// Reset ODPM driver polling configuration to driver defaults.
struct ConfigResetArgs {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "rails")]
/// Query or multiplex ODPM power rails.
struct RailsArgs {
    #[argh(subcommand)]
    subcommand: RailsSubCommands,
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
enum RailsSubCommands {
    Available(RailsAvailableArgs),
    Enabled(RailsEnabledArgs),
    Set(RailsSetArgs),
}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "available")]
/// List all available rails on the platform, their active channel status, and swapped-out energy/timestamps.
struct RailsAvailableArgs {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "enabled")]
/// List currently enabled/monitored rails, their channel IDs, and measurement start timestamps.
struct RailsEnabledArgs {}

#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "set")]
/// Dynamically swap a hardware channel to monitor a new internal PMIC rail.
struct RailsSetArgs {
    /// hardware channel ID (e.g. 0..11 for main PMIC, 41..52 for sub PMIC)
    #[argh(positional)]
    channel_id: u32,

    /// target rail name, schematic name, or regulator name (e.g. ldo_1m, L1M_ALIVE, buck_12m)
    #[argh(positional)]
    rail: String,
}

/// Directories to look for the service in: the provider's outgoing directory when run from within
/// its namespace, then the incoming one.
const SVC_DIRS: &[&str] = &["/out/svc", "/svc"];

async fn connect_to_odpm_device() -> Result<fgoogle_odpm::OdpmDeviceProxy> {
    for &path in SVC_DIRS {
        let Ok(dir) = fuchsia_fs::directory::open_in_namespace(path, fio::PERM_READABLE) else {
            continue;
        };
        let Ok(service) = fclient::Service::open_from_dir(dir, fgoogle_odpm::ServiceMarker) else {
            continue;
        };
        if let Some(instance) = service.enumerate().await?.into_iter().next() {
            return Ok(instance.connect_to_device()?);
        }
    }

    Err(anyhow!("Failed to find an ODPM service provider in {}", SVC_DIRS.join(" or ")))
}

async fn run_read<W: Write>(args: ReadArgs, writer: &mut W) -> Result<()> {
    let proxy = connect_to_odpm_device().await?;
    run_read_with_proxy(args, &proxy, writer).await
}

fn write_readings<W: Write, R>(
    writer: &mut W,
    matched_rails: &[(Option<u32>, String)],
    readings: &[R],
    metric_name: &str,
    unit_suffix: &str,
    extract: impl Fn(&R) -> (Option<u32>, Option<i64>, Option<i64>),
) -> Result<()> {
    for (channel_id, rail_label) in matched_rails {
        let reading = channel_id.and_then(|id| readings.iter().find(|r| extract(r).0 == Some(id)));
        if let Some(reading) = reading {
            let (_, micro_value, timestamp) = extract(reading);
            if let Some(micro_value) = micro_value {
                let value = micro_value as f64 / 1e6;
                if let Some(timestamp) = timestamp {
                    writeln!(
                        writer,
                        "Rail {}: {:>8.6} {} (timestamp: {} nanos)",
                        rail_label, value, unit_suffix, timestamp
                    )?;
                } else {
                    writeln!(writer, "Rail {}: {:>8.6} {}", rail_label, value, unit_suffix)?;
                }
            } else {
                eprintln!("Rail {}: No {} data returned in reading", rail_label, metric_name);
            }
        } else {
            eprintln!("Rail {}: No reading returned for channel", rail_label);
        }
    }
    Ok(())
}

async fn run_read_with_proxy<W: Write>(
    args: ReadArgs,
    proxy: &fgoogle_odpm::OdpmDeviceProxy,
    writer: &mut W,
) -> Result<()> {
    let metadata_list = proxy
        .get_rail_metadata()
        .await
        .map_err(|e| anyhow!("FIDL error getting rail metadata: {:?}", e))?
        .map_err(|s| anyhow!("Failed to get rail metadata: {:?}", zx::Status::err_from_raw(s)))?;

    let mut matched_rails = Vec::new();
    for metadata in metadata_list {
        let rail_name = metadata.name.unwrap_or_default();
        let schematic_name = metadata.schematic_name.unwrap_or_default();
        let regulator_name = metadata.regulator_name.unwrap_or_default();

        if let Some(ref target) = args.rail {
            if !rail_name.eq_ignore_ascii_case(target)
                && !schematic_name.eq_ignore_ascii_case(target)
                && !regulator_name.eq_ignore_ascii_case(target)
            {
                continue;
            }
        }

        let channel_id_str =
            metadata.channel_id.map(|id| id.to_string()).unwrap_or_else(|| "N/A".to_string());
        let rail_label =
            format!("{:<17} [{:<23},Ch{:>2}]", rail_name, schematic_name, channel_id_str);
        matched_rails.push((metadata.channel_id, rail_label));
    }

    if let Some(ref target) = args.rail {
        if matched_rails.is_empty() {
            return Err(anyhow!("Rail '{}' not found", target));
        }
    }

    if args.energy {
        let options = fgoogle_odpm::Options {
            measurement_type: Some(fgoogle_odpm::MeasurementType::Cumulative),
            ..Default::default()
        };
        let readings = proxy
            .get_energy_joules(&options)
            .await
            .map_err(|e| anyhow!("FIDL error getting energy readings: {:?}", e))?
            .map_err(|s| {
                anyhow!(
                    "Error from driver getting energy readings: {:?}",
                    zx::Status::err_from_raw(s)
                )
            })?;

        write_readings(writer, &matched_rails, &readings, "energy", "J", |r| {
            (r.channel_id, r.energy_uj, r.timestamp)
        })?;
    } else if args.current {
        let options = fgoogle_odpm::Options::default();
        let readings = proxy
            .get_current_amperes(&options)
            .await
            .map_err(|e| anyhow!("FIDL error getting current readings: {:?}", e))?
            .map_err(|s| {
                anyhow!(
                    "Error from driver getting current readings: {:?}",
                    zx::Status::err_from_raw(s)
                )
            })?;

        write_readings(writer, &matched_rails, &readings, "current", "A", |r| {
            (r.channel_id, r.current_ua.map(i64::from), r.timestamp)
        })?;
    } else {
        let options = if args.instantaneous {
            fgoogle_odpm::Options {
                measurement_type: Some(fgoogle_odpm::MeasurementType::Instantaneous),
                ..Default::default()
            }
        } else {
            fgoogle_odpm::Options::default()
        };
        let readings = proxy
            .get_power_watts(&options)
            .await
            .map_err(|e| anyhow!("FIDL error getting power readings: {:?}", e))?
            .map_err(|s| {
                anyhow!(
                    "Error from driver getting power readings: {:?}",
                    zx::Status::err_from_raw(s)
                )
            })?;

        let unit_suffix = if args.instantaneous { "W (instant)" } else { "W" };
        write_readings(writer, &matched_rails, &readings, "power", unit_suffix, |r| {
            (r.channel_id, r.power_uw.map(i64::from), r.timestamp)
        })?;
    }
    Ok(())
}

fn format_config(cfg: &fgoogle_odpm::PollingConfig) -> String {
    let interval_nanos = cfg.poll_interval.unwrap_or(0);
    let interval_ms = interval_nanos as f64 / 1e6;
    let interval_s = interval_nanos as f64 / 1e9;
    let polls = cfg.polls_per_series_entry.unwrap_or(0);
    let series_interval_s = interval_s * (polls as f64);

    format!(
        "ODPM Polling Configuration:\n  \
         Hardware poll interval:    {:.1} ms ({:.3} s)\n  \
         Polls per series entry:    {}\n  \
         Series recording interval: {:.2} s",
        interval_ms, interval_s, polls, series_interval_s
    )
}

const MIN_POLL_INTERVAL_MS: u32 = (fgoogle_odpm::MIN_POLL_INTERVAL / 1_000_000) as u32;

async fn run_config_set_with_proxy<W: Write>(
    args: ConfigSetArgs,
    proxy: &fgoogle_odpm::OdpmDeviceProxy,
    writer: &mut W,
) -> Result<()> {
    if args.poll_interval_ms.is_none() && args.polls_per_series_entry.is_none() {
        return Err(anyhow!(
            "At least one configuration option must be specified (--poll-interval-ms or --polls-per-series-entry)"
        ));
    }

    let mut new_config = fgoogle_odpm::PollingConfig::default();
    if let Some(ms) = args.poll_interval_ms {
        if ms < MIN_POLL_INTERVAL_MS {
            return Err(anyhow!("poll-interval-ms must be at least {MIN_POLL_INTERVAL_MS} ms"));
        }
        let duration = zx::MonotonicDuration::from_millis(ms.into());
        new_config.poll_interval = Some(duration.into_nanos());
    }
    if let Some(polls) = args.polls_per_series_entry {
        if polls == 0 {
            return Err(anyhow!("polls-per-series-entry must be greater than 0"));
        }
        new_config.polls_per_series_entry = Some(polls);
    }

    match proxy.set_polling_config(&new_config).await {
        Ok(Ok(())) => {
            writeln!(writer, "Successfully updated ODPM polling configuration.")?;
        }
        Ok(Err(status)) => {
            return Err(anyhow!("Failed to set config: {:?}", zx::Status::err_from_raw(status)));
        }
        Err(e) => {
            return Err(anyhow!("FIDL error setting config: {:?}", e));
        }
    }

    let current = proxy
        .get_polling_config()
        .await
        .map_err(|e| anyhow!("FIDL error getting config: {:?}", e))?
        .map_err(|s| anyhow!("Driver error: {:?}", zx::Status::err_from_raw(s)))?;
    writeln!(writer, "{}", format_config(&current))?;
    Ok(())
}

async fn run_config_get_with_proxy<W: Write>(
    proxy: &fgoogle_odpm::OdpmDeviceProxy,
    writer: &mut W,
) -> Result<()> {
    let current = proxy
        .get_polling_config()
        .await
        .map_err(|e| anyhow!("FIDL error getting config: {:?}", e))?
        .map_err(|s| anyhow!("Driver error: {:?}", zx::Status::err_from_raw(s)))?;
    writeln!(writer, "{}", format_config(&current))?;
    Ok(())
}

async fn run_config_reset_with_proxy<W: Write>(
    proxy: &fgoogle_odpm::OdpmDeviceProxy,
    writer: &mut W,
) -> Result<()> {
    match proxy.reset_polling_config().await {
        Ok(Ok(())) => {
            writeln!(writer, "Reset ODPM polling configuration to driver defaults.")?;
        }
        Ok(Err(status)) => {
            return Err(anyhow!("Failed to reset config: {:?}", zx::Status::err_from_raw(status)));
        }
        Err(e) => {
            return Err(anyhow!("FIDL error resetting config: {:?}", e));
        }
    }
    let current = proxy
        .get_polling_config()
        .await
        .map_err(|e| anyhow!("FIDL error getting config: {:?}", e))?
        .map_err(|s| anyhow!("Driver error: {:?}", zx::Status::err_from_raw(s)))?;
    writeln!(writer, "{}", format_config(&current))?;
    Ok(())
}

async fn run_config<W: Write>(args: ConfigArgs, writer: &mut W) -> Result<()> {
    let proxy = connect_to_odpm_device().await?;
    match args.subcommand {
        ConfigSubCommands::Set(set_args) => {
            run_config_set_with_proxy(set_args, &proxy, writer).await
        }
        ConfigSubCommands::Get(_) => run_config_get_with_proxy(&proxy, writer).await,
        ConfigSubCommands::Reset(_) => run_config_reset_with_proxy(&proxy, writer).await,
    }
}

async fn run_rails_available_with_proxy<W: Write>(
    proxy: &fgoogle_odpm::OdpmDeviceProxy,
    writer: &mut W,
) -> Result<()> {
    let rails = proxy
        .get_available_rails()
        .await
        .map_err(|e| anyhow!("FIDL error getting available rails: {:?}", e))?
        .map_err(|s| anyhow!("Failed to get available rails: {:?}", zx::Status::err_from_raw(s)))?;

    writeln!(
        writer,
        "{:<17} {:<23} {:<12} {:<10} {:>12} {:<20} {:<20}",
        "NAME", "SCHEMATIC_NAME", "REGULATOR", "CHANNEL", "ENERGY_J", "START_NS", "STOP_NS"
    )?;
    for rail in rails {
        let name = rail.name.unwrap_or_default();
        let schematic = rail.schematic_name.unwrap_or_default();
        let regulator = rail.regulator_name.unwrap_or_default();
        let channel_str =
            rail.channel_id.map(|id| format!("Ch{id}")).unwrap_or_else(|| "inactive".to_string());
        let energy_str = rail
            .energy_uj
            .map(|e| format!("{:.6}", e as f64 / 1e6))
            .unwrap_or_else(|| "-".to_string());
        let start_str = rail
            .measurement_start
            .map(|t| t.into_nanos().to_string())
            .unwrap_or_else(|| "-".to_string());
        let stop_str = rail
            .measurement_stop
            .map(|t| t.into_nanos().to_string())
            .unwrap_or_else(|| "-".to_string());
        writeln!(
            writer,
            "{:<17} {:<23} {:<12} {:<10} {:>12} {:<20} {:<20}",
            name, schematic, regulator, channel_str, energy_str, start_str, stop_str
        )?;
    }
    Ok(())
}

async fn run_rails_enabled_with_proxy<W: Write>(
    proxy: &fgoogle_odpm::OdpmDeviceProxy,
    writer: &mut W,
) -> Result<()> {
    let rails = proxy
        .get_rail_metadata()
        .await
        .map_err(|e| anyhow!("FIDL error getting enabled rails: {:?}", e))?
        .map_err(|s| anyhow!("Failed to get enabled rails: {:?}", zx::Status::err_from_raw(s)))?;

    writeln!(
        writer,
        "{:<8} {:<17} {:<23} {:<12} {:<20}",
        "CHANNEL", "NAME", "SCHEMATIC_NAME", "REGULATOR", "START_NS"
    )?;
    for rail in rails {
        let channel_str =
            rail.channel_id.map(|id| format!("Ch{id}")).unwrap_or_else(|| "N/A".to_string());
        let name = rail.name.unwrap_or_default();
        let schematic = rail.schematic_name.unwrap_or_default();
        let regulator = rail.regulator_name.unwrap_or_default();
        let start_str = rail
            .measurement_start
            .map(|t| t.into_nanos().to_string())
            .unwrap_or_else(|| "-".to_string());
        writeln!(
            writer,
            "{:<8} {:<17} {:<23} {:<12} {:<20}",
            channel_str, name, schematic, regulator, start_str
        )?;
    }
    Ok(())
}

async fn run_rails_set_with_proxy<W: Write>(
    args: RailsSetArgs,
    proxy: &fgoogle_odpm::OdpmDeviceProxy,
    writer: &mut W,
) -> Result<()> {
    match proxy.set_channel_rail(args.channel_id, &args.rail).await {
        Ok(Ok(())) => {
            writeln!(
                writer,
                "Successfully set channel {} to monitor rail '{}'.",
                args.channel_id, args.rail
            )?;
        }
        Ok(Err(status)) => {
            return Err(anyhow!(
                "Failed to set channel {} to rail '{}': {:?}",
                args.channel_id,
                args.rail,
                zx::Status::err_from_raw(status)
            ));
        }
        Err(e) => {
            return Err(anyhow!("FIDL error setting channel rail: {:?}", e));
        }
    }

    let rails = proxy
        .get_rail_metadata()
        .await
        .map_err(|e| anyhow!("FIDL error getting rail metadata: {:?}", e))?
        .map_err(|s| anyhow!("Failed to get rail metadata: {:?}", zx::Status::err_from_raw(s)))?;

    if let Some(updated) = rails.iter().find(|r| r.channel_id == Some(args.channel_id)) {
        let name = updated.name.as_deref().unwrap_or("");
        let schematic = updated.schematic_name.as_deref().unwrap_or("");
        let regulator = updated.regulator_name.as_deref().unwrap_or("");
        let start_str = updated
            .measurement_start
            .map(|t| t.into_nanos().to_string())
            .unwrap_or_else(|| "-".to_string());
        writeln!(
            writer,
            "Channel {:>2}: {:<17} [{:<23}, {}] (measurement_start: {} nanos)",
            args.channel_id, name, schematic, regulator, start_str
        )?;
    }
    Ok(())
}

async fn run_rails<W: Write>(args: RailsArgs, writer: &mut W) -> Result<()> {
    let proxy = connect_to_odpm_device().await?;
    match args.subcommand {
        RailsSubCommands::Available(_) => run_rails_available_with_proxy(&proxy, writer).await,
        RailsSubCommands::Enabled(_) => run_rails_enabled_with_proxy(&proxy, writer).await,
        RailsSubCommands::Set(set_args) => run_rails_set_with_proxy(set_args, &proxy, writer).await,
    }
}

async fn run(args: Args) -> Result<()> {
    run_with_writer(args, &mut std::io::stdout()).await
}

async fn run_with_writer<W: Write>(args: Args, writer: &mut W) -> Result<()> {
    if let Some(cmd) = args.command {
        match cmd {
            SubCommands::Read(read_args) => run_read(read_args, writer).await,
            SubCommands::Config(config_args) => run_config(config_args, writer).await,
            SubCommands::Rails(rails_args) => run_rails(rails_args, writer).await,
        }
    } else {
        writeln!(
            writer,
            "{}",
            Args::from_args(&["odpm_util"], &["--help"]).unwrap_err().output.trim_end()
        )?;
        Ok(())
    }
}

#[fuchsia::main]
async fn main() -> Result<()> {
    let args: Args = argh::from_env();
    if let Err(e) = run(args).await {
        eprintln!("Error: {e}");
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn test_args_parsing_no_subcommand() {
        let args = Args::from_args(&["odpm_util"], &[]).unwrap();
        assert_eq!(args, Args { command: None });

        assert!(Args::from_args(&["odpm_util"], &["cpu_big"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["-i"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["--instantaneous"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["cpu_big", "-i"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["-c"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["--current"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["cpu_big", "-c"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["-e"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["--energy"]).is_err());
        assert!(Args::from_args(&["odpm_util"], &["cpu_big", "-e"]).is_err());
    }

    #[test]
    fn test_args_parsing_read() {
        let args = Args::from_args(&["odpm_util"], &["read"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: false,
                    current: false,
                    energy: false,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "cpu_big"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: Some("cpu_big".to_string()),
                    instantaneous: false,
                    current: false,
                    energy: false,
                })),
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "-i"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: true,
                    current: false,
                    energy: false,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "--instantaneous"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: true,
                    current: false,
                    energy: false,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "cpu_big", "-i"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: Some("cpu_big".to_string()),
                    instantaneous: true,
                    current: false,
                    energy: false,
                })),
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "-c"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: false,
                    current: true,
                    energy: false,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "--current"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: false,
                    current: true,
                    energy: false,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "cpu_big", "-c"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: Some("cpu_big".to_string()),
                    instantaneous: false,
                    current: true,
                    energy: false,
                })),
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "-e"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: false,
                    current: false,
                    energy: true,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "--energy"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: None,
                    instantaneous: false,
                    current: false,
                    energy: true,
                }))
            }
        );

        let args = Args::from_args(&["odpm_util"], &["read", "cpu_big", "-e"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Read(ReadArgs {
                    rail: Some("cpu_big".to_string()),
                    instantaneous: false,
                    current: false,
                    energy: true,
                })),
            }
        );
    }

    #[test]
    fn test_args_parsing_config() {
        let args = Args::from_args(&["odpm_util"], &["config", "set"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Config(ConfigArgs {
                    subcommand: ConfigSubCommands::Set(ConfigSetArgs {
                        poll_interval_ms: None,
                        polls_per_series_entry: None,
                    }),
                })),
            }
        );

        let args =
            Args::from_args(&["odpm_util"], &["config", "set", "-i", "100", "-p", "10"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Config(ConfigArgs {
                    subcommand: ConfigSubCommands::Set(ConfigSetArgs {
                        poll_interval_ms: Some(100),
                        polls_per_series_entry: Some(10),
                    }),
                })),
            }
        );

        let args = Args::from_args(&["odpm_util"], &["config", "get"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Config(ConfigArgs {
                    subcommand: ConfigSubCommands::Get(ConfigGetArgs {}),
                })),
            }
        );

        let args = Args::from_args(&["odpm_util"], &["config", "reset"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Config(ConfigArgs {
                    subcommand: ConfigSubCommands::Reset(ConfigResetArgs {}),
                })),
            }
        );
    }

    #[fuchsia::test]
    async fn test_run_no_subcommand_displays_help() {
        let mut buf = Vec::new();
        let res = run_with_writer(Args { command: None }, &mut buf).await;
        assert!(res.is_ok());

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("Usage: odpm_util"));
        assert!(output.contains("read"));
        assert!(output.contains("config"));
    }

    #[test]
    fn test_format_config() {
        let cfg = fgoogle_odpm::PollingConfig {
            poll_interval: Some(1_000_000_000),
            polls_per_series_entry: Some(60),
            ..Default::default()
        };
        let formatted = format_config(&cfg);
        assert!(formatted.contains("1000.0 ms"));
        assert!(formatted.contains("60"));
        assert!(formatted.contains("60.00 s"));
    }

    #[fuchsia::test]
    async fn test_run_config_set_update() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::OdpmDeviceRequest::SetPollingConfig { payload, responder } => {
                        assert_eq!(payload.poll_interval, Some(250_000_000));
                        assert_eq!(payload.polls_per_series_entry, Some(10));
                        let _ = responder.send(Ok(()));
                    }
                    fgoogle_odpm::OdpmDeviceRequest::GetPollingConfig { responder } => {
                        let _ = responder.send(Ok(&fgoogle_odpm::PollingConfig {
                            poll_interval: Some(250_000_000),
                            polls_per_series_entry: Some(10),
                            ..Default::default()
                        }));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let args = ConfigSetArgs { poll_interval_ms: Some(250), polls_per_series_entry: Some(10) };
        let mut buf = Vec::new();
        let res = run_config_set_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("Successfully updated ODPM polling configuration."));
        assert!(output.contains("250.0 ms"));
        drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn test_run_config_set_no_options_error() {
        let (proxy, _stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let args = ConfigSetArgs { poll_interval_ms: None, polls_per_series_entry: None };
        let mut buf = Vec::new();
        let res = run_config_set_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_err());
    }

    #[fuchsia::test]
    async fn test_run_config_get() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::OdpmDeviceRequest::GetPollingConfig { responder } => {
                        let _ = responder.send(Ok(&fgoogle_odpm::PollingConfig {
                            poll_interval: Some(1_000_000_000),
                            polls_per_series_entry: Some(60),
                            ..Default::default()
                        }));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let mut buf = Vec::new();
        let res = run_config_get_with_proxy(&proxy, &mut buf).await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("1000.0 ms"));
        drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn test_run_config_reset() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::OdpmDeviceRequest::ResetPollingConfig { responder } => {
                        let _ = responder.send(Ok(()));
                    }
                    fgoogle_odpm::OdpmDeviceRequest::GetPollingConfig { responder } => {
                        let _ = responder.send(Ok(&fgoogle_odpm::PollingConfig {
                            poll_interval: Some(1_000_000_000),
                            polls_per_series_entry: Some(60),
                            ..Default::default()
                        }));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let mut buf = Vec::new();
        let res = run_config_reset_with_proxy(&proxy, &mut buf).await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("Reset ODPM polling configuration to driver defaults."));
        assert!(output.contains("1000.0 ms"));
        drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn test_run_config_set_below_min_interval_error() {
        let expected_msg = format!("poll-interval-ms must be at least {MIN_POLL_INTERVAL_MS} ms");

        let (proxy, _stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let args = ConfigSetArgs { poll_interval_ms: Some(0), polls_per_series_entry: None };
        let mut buf = Vec::new();
        let res = run_config_set_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains(&expected_msg));

        let args = ConfigSetArgs {
            poll_interval_ms: Some(MIN_POLL_INTERVAL_MS - 1),
            polls_per_series_entry: None,
        };
        let mut buf = Vec::new();
        let res = run_config_set_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains(&expected_msg));
    }

    #[fuchsia::test]
    async fn test_run_config_set_zero_polls_error() {
        let (proxy, _stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let args = ConfigSetArgs { poll_interval_ms: None, polls_per_series_entry: Some(0) };
        let mut buf = Vec::new();
        let res = run_config_set_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_err());
    }

    #[fuchsia::test]
    async fn test_run_read_energy() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::OdpmDeviceRequest::GetRailMetadata { responder } => {
                        let _ = responder.send(Ok(&[
                            fgoogle_odpm::RailMetadata {
                                name: Some("cpu_big".to_string()),
                                channel_id: Some(12),
                                schematic_name: Some("S2M_VDD_CPUCL2".to_string()),
                                ..Default::default()
                            },
                            fgoogle_odpm::RailMetadata {
                                name: Some("gpu".to_string()),
                                channel_id: Some(42),
                                schematic_name: Some("S2S_VDD_GPU".to_string()),
                                ..Default::default()
                            },
                        ]));
                    }
                    fgoogle_odpm::OdpmDeviceRequest::GetEnergyJoules { responder, .. } => {
                        let _ = responder.send(Ok(&[
                            fgoogle_odpm::EnergyReading {
                                channel_id: Some(12),
                                energy_uj: Some(5_000_000),
                                timestamp: Some(10_000_000_000),
                                ..Default::default()
                            },
                            fgoogle_odpm::EnergyReading {
                                channel_id: Some(42),
                                energy_uj: Some(12_345_678),
                                timestamp: Some(10_000_000_000),
                                ..Default::default()
                            },
                        ]));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let args = ReadArgs {
            rail: Some("gpu".to_string()),
            instantaneous: false,
            current: false,
            energy: true,
        };
        let mut buf = Vec::new();
        let res = run_read_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("gpu               [S2S_VDD_GPU            ,Ch42]"));
        assert!(output.contains("12.345678 J"));
        assert!(output.contains("timestamp: 10000000000 nanos"));
        assert!(!output.contains("cpu_big"));
        drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn test_run_read_power() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::OdpmDeviceRequest::GetRailMetadata { responder } => {
                        let _ = responder.send(Ok(&[fgoogle_odpm::RailMetadata {
                            name: Some("gpu".to_string()),
                            channel_id: Some(42),
                            schematic_name: Some("S2S_VDD_GPU".to_string()),
                            ..Default::default()
                        }]));
                    }
                    fgoogle_odpm::OdpmDeviceRequest::GetPowerWatts { payload, responder } => {
                        let power_uw = if payload.measurement_type
                            == Some(fgoogle_odpm::MeasurementType::Instantaneous)
                        {
                            3_500_000
                        } else {
                            2_500_000
                        };
                        let _ = responder.send(Ok(&[fgoogle_odpm::PowerReading {
                            channel_id: Some(42),
                            power_uw: Some(power_uw),
                            timestamp: Some(20_000_000_000),
                            ..Default::default()
                        }]));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let args = ReadArgs { rail: None, instantaneous: false, current: false, energy: false };
        let mut buf = Vec::new();
        let res = run_read_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("gpu               [S2S_VDD_GPU            ,Ch42]"));
        assert!(output.contains("2.500000 W (timestamp: 20000000000 nanos)"));

        let args_inst = ReadArgs { rail: None, instantaneous: true, current: false, energy: false };
        let mut buf_inst = Vec::new();
        let res_inst = run_read_with_proxy(args_inst, &proxy, &mut buf_inst).await;
        assert!(res_inst.is_ok());
        let output_inst = String::from_utf8(buf_inst).unwrap();
        assert!(output_inst.contains("3.500000 W (instant) (timestamp: 20000000000 nanos)"));

        drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn test_run_read_current() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::OdpmDeviceRequest::GetRailMetadata { responder } => {
                        let _ = responder.send(Ok(&[fgoogle_odpm::RailMetadata {
                            name: Some("gpu".to_string()),
                            channel_id: Some(42),
                            schematic_name: Some("S2S_VDD_GPU".to_string()),
                            ..Default::default()
                        }]));
                    }
                    fgoogle_odpm::OdpmDeviceRequest::GetCurrentAmperes { responder, .. } => {
                        let _ = responder.send(Ok(&[fgoogle_odpm::CurrentReading {
                            channel_id: Some(42),
                            current_ua: Some(1_250_000),
                            timestamp: Some(30_000_000_000),
                            ..Default::default()
                        }]));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let args = ReadArgs { rail: None, instantaneous: false, current: true, energy: false };
        let mut buf = Vec::new();
        let res = run_read_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("gpu               [S2S_VDD_GPU            ,Ch42]"));
        assert!(output.contains("1.250000 A (timestamp: 30000000000 nanos)"));
        drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn test_run_read_rail_not_found() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::OdpmDeviceRequest::GetRailMetadata { responder } => {
                        let _ = responder.send(Ok(&[fgoogle_odpm::RailMetadata {
                            name: Some("gpu".to_string()),
                            channel_id: Some(42),
                            schematic_name: Some("S2S_VDD_GPU".to_string()),
                            ..Default::default()
                        }]));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let args = ReadArgs {
            rail: Some("nonexistent".to_string()),
            instantaneous: false,
            current: false,
            energy: false,
        };
        let mut buf = Vec::new();
        let res = run_read_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("Rail 'nonexistent' not found"));
        drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn test_args_parsing_rails() {
        let args = Args::from_args(&["odpm_util"], &["rails", "available"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Rails(RailsArgs {
                    subcommand: RailsSubCommands::Available(RailsAvailableArgs {}),
                })),
            }
        );

        let args = Args::from_args(&["odpm_util"], &["rails", "enabled"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Rails(RailsArgs {
                    subcommand: RailsSubCommands::Enabled(RailsEnabledArgs {}),
                })),
            }
        );

        let args = Args::from_args(&["odpm_util"], &["rails", "set", "12", "cam_1v8"]).unwrap();
        assert_eq!(
            args,
            Args {
                command: Some(SubCommands::Rails(RailsArgs {
                    subcommand: RailsSubCommands::Set(RailsSetArgs {
                        channel_id: 12,
                        rail: "cam_1v8".to_string(),
                    }),
                })),
            }
        );
    }

    #[fuchsia::test]
    async fn test_run_rails_available() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::OdpmDeviceRequest::GetAvailableRails { responder } => {
                        let _ = responder.send(Ok(&[
                            fgoogle_odpm::RailMetadata {
                                name: Some("cpu_big".to_string()),
                                channel_id: Some(12),
                                schematic_name: Some("S2M_VDD_CPUCL2".to_string()),
                                regulator_name: Some("buck_2m".to_string()),
                                measurement_start: Some(zx::BootInstant::from_nanos(1_000_000_000)),
                                measurement_stop: None,
                                energy_uj: None,
                                ..Default::default()
                            },
                            fgoogle_odpm::RailMetadata {
                                name: Some("cam_1v8".to_string()),
                                channel_id: None,
                                schematic_name: Some("L4M_VDD_CAM_1V8".to_string()),
                                regulator_name: Some("ldo_4m".to_string()),
                                measurement_start: Some(zx::BootInstant::from_nanos(1_000_000_000)),
                                measurement_stop: Some(zx::BootInstant::from_nanos(2_000_000_000)),
                                energy_uj: Some(3_456_789),
                                ..Default::default()
                            },
                        ]));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let mut buf = Vec::new();
        let res = run_rails_available_with_proxy(&proxy, &mut buf).await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("NAME"));
        assert!(output.contains("cpu_big"));
        assert!(output.contains("buck_2m"));
        assert!(output.contains("Ch12"));
        assert!(output.contains("1000000000"));
        assert!(output.contains("cam_1v8"));
        assert!(output.contains("ldo_4m"));
        assert!(output.contains("inactive"));
        assert!(output.contains("3.456789"));
        assert!(output.contains("2000000000"));
        drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn test_run_rails_enabled() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::OdpmDeviceRequest::GetRailMetadata { responder } => {
                        let _ = responder.send(Ok(&[fgoogle_odpm::RailMetadata {
                            name: Some("cpu_big".to_string()),
                            channel_id: Some(12),
                            schematic_name: Some("S2M_VDD_CPUCL2".to_string()),
                            regulator_name: Some("buck_2m".to_string()),
                            measurement_start: Some(zx::BootInstant::from_nanos(1_500_000_000)),
                            ..Default::default()
                        }]));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let mut buf = Vec::new();
        let res = run_rails_enabled_with_proxy(&proxy, &mut buf).await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("Ch12"));
        assert!(output.contains("cpu_big"));
        assert!(output.contains("S2M_VDD_CPUCL2"));
        assert!(output.contains("buck_2m"));
        assert!(output.contains("1500000000"));
        drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn test_run_rails_set() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fgoogle_odpm::OdpmDeviceMarker>();
        let task = fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fgoogle_odpm::OdpmDeviceRequest::SetChannelRail {
                        channel_id,
                        rail_name,
                        responder,
                    } => {
                        assert_eq!(channel_id, 12);
                        assert_eq!(rail_name, "cam_1v8");
                        let _ = responder.send(Ok(()));
                    }
                    fgoogle_odpm::OdpmDeviceRequest::GetRailMetadata { responder } => {
                        let _ = responder.send(Ok(&[fgoogle_odpm::RailMetadata {
                            name: Some("cam_1v8".to_string()),
                            channel_id: Some(12),
                            schematic_name: Some("L4M_VDD_CAM_1V8".to_string()),
                            regulator_name: Some("ldo_4m".to_string()),
                            measurement_start: Some(zx::BootInstant::from_nanos(2_000_000_000)),
                            ..Default::default()
                        }]));
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        });

        let args = RailsSetArgs { channel_id: 12, rail: "cam_1v8".to_string() };
        let mut buf = Vec::new();
        let res = run_rails_set_with_proxy(args, &proxy, &mut buf).await;
        assert!(res.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("Successfully set channel 12 to monitor rail 'cam_1v8'."));
        assert!(output.contains("Channel 12: cam_1v8"));
        assert!(output.contains("measurement_start: 2000000000 nanos"));
        drop(proxy);
        task.await;
    }
}
