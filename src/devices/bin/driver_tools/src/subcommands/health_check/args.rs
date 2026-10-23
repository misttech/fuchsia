// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};
use std::str::FromStr;

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "health-check",
    description = "Run an active health check on a bound driver or all bound drivers.",
    example = "To run a fast health check on the display driver:

    $ driver health-check dev/sys/platform/display --level fast",
    note = "Condition ages (for example, last observed 1.5s ago) are only computed when the tool runs on the target; from a host ffx the raw boot-timeline nanoseconds are printed instead."
)]
pub struct HealthCheckCommand {
    /// exact component moniker or topological path of the target node.
    #[argh(positional)]
    pub moniker: Option<String>,

    /// health check depth level: ping, fast (default), or deep.
    #[argh(option, short = 'l', long = "level", default = "HealthCheckLevelArg::Fast")]
    pub level: HealthCheckLevelArg,

    /// run health checks across all bound driver nodes.
    #[argh(switch, short = 'a', long = "all")]
    pub all: bool,
}

/// The depth of a health check, see `fuchsia.driver.framework/HealthCheckRequest`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HealthCheckLevelArg {
    Ping,
    Fast,
    Deep,
}

impl FromStr for HealthCheckLevelArg {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "ping" => Ok(HealthCheckLevelArg::Ping),
            "fast" => Ok(HealthCheckLevelArg::Fast),
            "deep" => Ok(HealthCheckLevelArg::Deep),
            _ => Err("Invalid health check level. Must be 'ping', 'fast', or 'deep'"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_args_defaults_and_flags() {
        let cmd = HealthCheckCommand::from_args(&["health-check"], &["dev/sys/platform/display"])
            .unwrap();
        assert_eq!(cmd.moniker.as_deref(), Some("dev/sys/platform/display"));
        assert_eq!(cmd.level, HealthCheckLevelArg::Fast);
        assert!(!cmd.all);

        let all_cmd =
            HealthCheckCommand::from_args(&["health-check"], &["--all", "-l", "deep"]).unwrap();
        assert_eq!(all_cmd.moniker, None);
        assert_eq!(all_cmd.level, HealthCheckLevelArg::Deep);
        assert!(all_cmd.all);
    }

    #[test]
    fn test_parse_level() {
        assert_eq!("ping".parse::<HealthCheckLevelArg>().unwrap(), HealthCheckLevelArg::Ping);
        assert_eq!("FAST".parse::<HealthCheckLevelArg>().unwrap(), HealthCheckLevelArg::Fast);
        assert_eq!("Deep".parse::<HealthCheckLevelArg>().unwrap(), HealthCheckLevelArg::Deep);
        assert!("invalid".parse::<HealthCheckLevelArg>().is_err());
        assert!(HealthCheckCommand::from_args(&["health-check"], &["--level", "slow"]).is_err());
    }
}
