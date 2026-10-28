// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Command-line argument definitions for the `ffx uart` tool.
//!
//! Defines the [`UartCommand`] struct and [`UartSubCommand`] enums parsed by `argh`.

use std::num::NonZeroU32;

use argh::{ArgsInfo, FromArgs};

/// Default serial baud rate (1,000,000 baud).
pub const DEFAULT_BAUD_RATE: NonZeroU32 = match NonZeroU32::new(1_000_000) {
    Some(v) => v,
    None => unreachable!(),
};

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
/// Interact with the UART subsystem.
#[argh(
    subcommand,
    name = "uart",
    note = "Most commands require a target (via the global 'ffx -t <target> uart ...' flag).
If no target is specified, commands (except 'connect') will implicitly target
the active connection if exactly one is running."
)]
pub struct UartCommand {
    #[argh(subcommand)]
    /// specific UART subcommand to execute
    pub sub_cmd: UartSubCommand,
}

/// Available subcommands for `ffx uart`.
#[derive(ArgsInfo, FromArgs, PartialEq, Clone, Debug)]
#[argh(subcommand)]
pub enum UartSubCommand {
    /// Start a background driver daemon to connect to a UART target
    Connect(ConnectCommand),
    /// Stop the driver daemon and disconnect from a UART target
    Disconnect(DisconnectCommand),
    /// List active UART connections
    List(ListCommand),
    /// Probe a UART target connection health
    Probe(ProbeCommand),
    /// Show real-time connection status and metrics for a UART target
    Status(StatusCommand),
    /// Diagnose and repair UART connection issues
    Doctor(DoctorCommand),
}

/// Arguments for the `ffx uart connect` subcommand.
///
/// Starts a background driver daemon connected to the specified UART target device.
#[derive(ArgsInfo, FromArgs, PartialEq, Clone, Debug)]
#[argh(
    subcommand,
    name = "connect",
    description = "Start a background driver daemon to connect to a UART target",
    note = r#"ffx-uart uses "-t <target>" just as with any other ffx command, although the "uart:" prefix is optional.

Examples:
  ffx -t uart:/dev/ttyUSB0 uart connect
  ffx -t /dev/ttyUSB0 uart connect

If you have configured a default target (e.g. via 'ffx target default set uart:...'), you can omit the '-t' flag:
  ffx uart connect

Supported target formats:
  <path>           - Connect directly to a TTY device or UNIX socket path (e.g. "/dev/ttyUSB0", "/tmp/socket")

Target strings may optionally be prefixed with "uart:" (e.g. "uart:/dev/ttyUSB0"), which will be stripped."#
)]
pub struct ConnectCommand {
    #[argh(option)]
    /// baud rate for the UART port (default: 1000000, only used for TTY targets)
    pub baud: Option<NonZeroU32>,

    #[argh(option)]
    /// custom UNIX socket path to use
    pub socket: Option<String>,

    #[argh(switch)]
    /// reconnect if already connected (reclaims stale daemon)
    pub reconnect: bool,

    #[argh(option)]
    /// protocol to use: "resend" (ResendSP)
    pub protocol: Option<String>,

    #[argh(switch)]
    /// do not retry connection if it fails or is lost
    pub no_retry: bool,
}

#[derive(ArgsInfo, FromArgs, PartialEq, Clone, Debug)]
/// Stop the driver daemon and disconnect from a UART target
#[argh(subcommand, name = "disconnect")]
pub struct DisconnectCommand {}

#[derive(ArgsInfo, FromArgs, PartialEq, Clone, Debug)]
/// List active UART connections
#[argh(subcommand, name = "list")]
pub struct ListCommand {}

#[derive(ArgsInfo, FromArgs, PartialEq, Clone, Debug)]
/// Probe a UART target connection health
#[argh(subcommand, name = "probe")]
pub struct ProbeCommand {
    #[argh(option, default = "DEFAULT_BAUD_RATE")]
    /// baud rate for the UART port (default: 1000000, only used for TTY targets)
    pub baud: NonZeroU32,
}

#[derive(ArgsInfo, FromArgs, PartialEq, Clone, Debug)]
/// Show real-time connection status and metrics for a UART target
#[argh(subcommand, name = "status")]
pub struct StatusCommand {}

#[derive(ArgsInfo, FromArgs, PartialEq, Clone, Debug)]
/// Diagnose and repair UART connection issues
#[argh(subcommand, name = "doctor")]
pub struct DoctorCommand {
    #[argh(switch)]
    /// automatically fix detected issues where possible
    pub fix: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_connect_command() {
        let cmd = UartCommand::from_args(
            &["uart"],
            &["connect", "--baud", "115200", "--reconnect", "--no-retry"],
        )
        .expect("Failed to parse connect args");
        match cmd.sub_cmd {
            UartSubCommand::Connect(connect) => {
                assert_eq!(connect.baud, NonZeroU32::new(115200));
                assert!(connect.reconnect);
                assert!(connect.no_retry);
                assert_eq!(connect.socket, None);
            }
            other => panic!("Expected Connect subcommand, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_disconnect_command() {
        let cmd = UartCommand::from_args(&["uart"], &["disconnect"])
            .expect("Failed to parse disconnect args");
        assert_eq!(cmd.sub_cmd, UartSubCommand::Disconnect(DisconnectCommand {}));
    }

    #[test]
    fn test_parse_list_command() {
        let cmd = UartCommand::from_args(&["uart"], &["list"]).expect("Failed to parse list args");
        assert_eq!(cmd.sub_cmd, UartSubCommand::List(ListCommand {}));
    }

    #[test]
    fn test_parse_probe_command() {
        let cmd = UartCommand::from_args(&["uart"], &["probe", "--baud", "115200"])
            .expect("Failed to parse probe args");
        assert_eq!(
            cmd.sub_cmd,
            UartSubCommand::Probe(ProbeCommand { baud: NonZeroU32::new(115200).unwrap() })
        );

        let cmd_default =
            UartCommand::from_args(&["uart"], &["probe"]).expect("Failed to parse probe args");
        assert_eq!(
            cmd_default.sub_cmd,
            UartSubCommand::Probe(ProbeCommand { baud: DEFAULT_BAUD_RATE })
        );
    }

    #[test]
    fn test_parse_invalid_baud() {
        let err_connect = UartCommand::from_args(&["uart"], &["connect", "--baud", "0"]);
        assert!(err_connect.is_err());

        let err_probe = UartCommand::from_args(&["uart"], &["probe", "--baud", "0"]);
        assert!(err_probe.is_err());
    }

    #[test]
    fn test_parse_status_command() {
        let cmd =
            UartCommand::from_args(&["uart"], &["status"]).expect("Failed to parse status args");
        assert_eq!(cmd.sub_cmd, UartSubCommand::Status(StatusCommand {}));
    }

    #[test]
    fn test_parse_doctor_command() {
        let cmd = UartCommand::from_args(&["uart"], &["doctor", "--fix"])
            .expect("Failed to parse doctor args");
        assert_eq!(cmd.sub_cmd, UartSubCommand::Doctor(DoctorCommand { fix: true }));
    }
}
