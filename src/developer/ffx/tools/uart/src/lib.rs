// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Host-side tooling and subtool dispatch for `ffx uart`.
//!
//! Provides the subtool suite [`UartSuite`] and [`UartSuiteTool`],
//! command argument parsing, and coordination with background driver daemons.

/// Command-line argument definitions for the `ffx uart` subcommands.
pub mod args;
/// Diagnostic check framework and automated repair for UART connections.
pub mod doctor;
/// Process supervision and execution helpers for the `ffx-uart-driver` daemon binary.
pub mod driver;
/// Connection metadata resolution and persistence for active UART targets.
pub mod metadata;
/// Asynchronous stream abstractions and connection helpers for UART devices.
pub mod stream;
/// Subtool implementations for each `ffx uart` subcommand.
pub mod subtools;
/// Host process inspection, peer PID discovery, and daemon liveness checking.
pub mod sys;

use ffx_config::EnvironmentContext;
use ffx_writer::ToolIO;
use fho::subtool_suite::{FfxSubtoolSuite, Subtool, SubtoolBox, SubtoolSuite, ToolSuiteCommand};
use fho::{FfxTool, FhoEnvironment, Result, user_error};
use sha2 as _;

pub use args::*;
pub use doctor::*;
pub use driver::*;
pub use metadata::*;
pub use stream::*;
pub use subtools::*;
pub use sys::*;

impl ToolSuiteCommand for UartCommand {
    type SubCommand = UartSubCommand;
    fn into_subcommand(self) -> Self::SubCommand {
        self.sub_cmd
    }
}

pub struct UartSuite;

impl SubtoolSuite for UartSuite {
    type Command = UartCommand;

    async fn new_subtool(
        env: FhoEnvironment,
        subcommand: UartSubCommand,
    ) -> Result<Box<dyn SubtoolBox>> {
        Ok(match subcommand {
            UartSubCommand::Connect(cmd) => {
                Subtool::new(subtools::ConnectTool::from_env(env, cmd).await?)
            }
            UartSubCommand::Disconnect(cmd) => {
                Subtool::new(subtools::DisconnectTool::from_env(env, cmd).await?)
            }
            UartSubCommand::List(cmd) => {
                Subtool::new(subtools::ListTool::from_env(env, cmd).await?)
            }
            UartSubCommand::Probe(cmd) => {
                Subtool::new(subtools::ProbeTool::from_env(env, cmd).await?)
            }
            UartSubCommand::Status(cmd) => {
                Subtool::new(subtools::StatusTool::from_env(env, cmd).await?)
            }
            UartSubCommand::Doctor(cmd) => {
                Subtool::new(subtools::DoctorTool::from_env(env, cmd).await?)
            }
        })
    }
}

pub type UartSuiteTool = FfxSubtoolSuite<UartSuite>;
pub type UartTool = UartSuiteTool;

#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedTarget {
    Active { meta_path: std::path::PathBuf, meta: crate::metadata::ConnectionMetadata },
    Inactive { target_path: String },
}

impl ResolvedTarget {
    pub fn target_str(&self) -> &str {
        match self {
            Self::Active { meta, .. } => &meta.target,
            Self::Inactive { target_path } => target_path.as_str(),
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }
}

pub(crate) async fn resolve_target(
    context: &EnvironmentContext,
    spec: &str,
) -> Result<ResolvedTarget> {
    let raw_target = spec.strip_prefix("uart:").unwrap_or(spec);

    if raw_target.is_empty() {
        return Err(user_error!("Empty target specifier."));
    }

    if let Some((meta_path, meta)) = crate::metadata::find_active_connection(context, raw_target)? {
        return Ok(ResolvedTarget::Active { meta_path, meta });
    }

    if let Ok(path) = uart_driver_api::parse_target_endpoint(raw_target, context) {
        return Ok(ResolvedTarget::Inactive { target_path: path.to_string_lossy().into_owned() });
    }
    Err(user_error!(
        "Target '{}' not found or has no active UART connection.\n\
         To connect to a new UART target, please specify the device path directly:\n\
         `ffx -t <path> uart connect`\n\
         See 'ffx uart connect --help' for details.",
        raw_target
    ))
}

pub(crate) async fn get_spec(
    context: &EnvironmentContext,
    is_connect: bool,
    writer: &mut (impl ToolIO + ?Sized),
) -> Result<String> {
    let spec_opt = ffx_target::get_target_specifier(context)?;
    if let Some(spec) = spec_opt {
        return Ok(spec);
    }
    if is_connect {
        return Err(user_error!(
            "No target specified. Please specify a target using 'ffx -t uart:<target> uart connect' or configure a default target."
        ));
    }
    let active = driver::get_active_connections(context).await?;
    match active.len() {
        0 => Err(user_error!(
            "No target specified and no active UART connections found. Please specify a target."
        )),
        1 => {
            let target = active[0].target.clone();
            if !writer.is_machine() {
                writer.print(format!(
                    "No target specified. Using active connection '{}'.\n",
                    target
                ))?;
            }
            Ok(target)
        }
        _ => {
            let mut err_msg =
                "No target specified and multiple active UART connections found:\n".to_string();
            for entry in active {
                err_msg.push_str(&format!("  - {}\n", entry.target));
            }
            err_msg.push_str("Please specify a target using '-t'.");
            Err(user_error!("{err_msg}"))
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod spec_tests {
    use super::*;

    #[fuchsia::test]
    async fn test_get_spec_no_target() {
        let temp = tempfile::tempdir().unwrap();
        let env = ffx_config::test_env()
            .runtime_config("shared_data", temp.path().to_str().unwrap())
            .build()
            .unwrap();
        let buffers = ffx_writer::TestBuffers::default();
        let mut writer = ffx_writer::SimpleWriter::new_test(&buffers);
        let res_connect = get_spec(&env.context, true, &mut writer).await;
        assert!(res_connect.is_err());
        assert!(res_connect.unwrap_err().to_string().contains(
            "No target specified. Please specify a target using 'ffx -t uart:<target> uart connect'"
        ));

        let res_disconnect = get_spec(&env.context, false, &mut writer).await;
        assert!(res_disconnect.is_err());
        assert!(
            res_disconnect
                .unwrap_err()
                .to_string()
                .contains("No target specified and no active UART connections found")
        );
    }

    #[fuchsia::test]
    async fn test_get_spec_with_target() {
        let mut env = ffx_config::test_init().unwrap();
        let buffers = ffx_writer::TestBuffers::default();
        let mut writer = ffx_writer::SimpleWriter::new_test(&buffers);
        env.context.override_target_specifier(&Some("uart:/dev/ttyUSB0".to_string()));
        let spec = get_spec(&env.context, true, &mut writer).await.unwrap();
        assert_eq!(spec, "uart:/dev/ttyUSB0");
    }

    #[fuchsia::test]
    async fn test_resolve_empty_target() {
        let env = ffx_config::test_init().unwrap();
        let res = resolve_target(&env.context, "uart:").await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("Empty target specifier."));
    }
    #[fuchsia::test]
    async fn test_resolve_target_inactive() {
        let env = ffx_config::test_init().unwrap();
        let res = resolve_target(&env.context, "uart:/dev/ttyUSB0").await.unwrap();
        assert_eq!(res, ResolvedTarget::Inactive { target_path: "/dev/ttyUSB0".to_string() });
        assert_eq!(res.target_str(), "/dev/ttyUSB0");
        assert!(!res.is_active());
    }

    #[fuchsia::test]
    async fn test_resolve_target_active() {
        let temp = tempfile::tempdir().unwrap();
        let env = ffx_config::test_env()
            .runtime_config("shared_data", temp.path().to_str().unwrap())
            .build()
            .unwrap();
        let shared_path = temp.path().join("ffx_uart");
        std::fs::create_dir_all(&shared_path).unwrap();

        let target_id = "0123456789abcdef";
        let meta = crate::metadata::ConnectionMetadata {
            pid: 12345,
            target: "/dev/ttyUSB5".to_string(),
            status: crate::metadata::ConnectionStatus::Connected,
            id: Some(target_id.to_string()),
            baud: std::num::NonZeroU32::new(115200),
            protocol: crate::metadata::UartProtocol::ResendSP,
            log_level: None,
            nodename: Some("fuchsia-resolved-node".to_string()),
            serial: Some("SN-RES-1".to_string()),
        };
        let file_path = shared_path.join(format!("ffx_uart_{target_id}.json"));
        std::fs::write(&file_path, serde_json::to_string(&meta).unwrap()).unwrap();

        // 1. Resolve by nodename
        let res_node = resolve_target(&env.context, "fuchsia-resolved-node").await.unwrap();
        assert_eq!(res_node.target_str(), "/dev/ttyUSB5");
        assert!(res_node.is_active());

        // 2. Resolve by 16-hex ID
        let res_id = resolve_target(&env.context, target_id).await.unwrap();
        assert_eq!(res_id.target_str(), "/dev/ttyUSB5");

        // 3. Resolve by target path
        let res_path = resolve_target(&env.context, "/dev/ttyUSB5").await.unwrap();
        assert_eq!(res_path.target_str(), "/dev/ttyUSB5");
        assert!(res_path.is_active());
    }

    #[fuchsia::test]
    async fn test_resolve_target_invalid() {
        let env = ffx_config::test_init().unwrap();
        let res = resolve_target(&env.context, "invalid-target-name").await;
        assert!(res.is_err());
        assert!(
            res.unwrap_err().to_string().contains("not found or has no active UART connection")
        );
    }

    #[fuchsia::test]
    async fn test_get_spec_single_active_connection() {
        let temp = tempfile::tempdir().unwrap();
        let env = ffx_config::test_env()
            .runtime_config("shared_data", temp.path().to_str().unwrap())
            .build()
            .unwrap();
        let buffers = ffx_writer::TestBuffers::default();
        let mut writer = ffx_writer::SimpleWriter::new_test(&buffers);

        let target = "/dev/ttyUSB0";
        let sock_path = get_socket_path(&env.context, target).unwrap();
        if let Some(parent) = sock_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let _listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let control_path = sock_path.with_extension(uart_driver_api::CONTROL_SOCKET_EXTENSION);
        let _control_listener = tokio::net::UnixListener::bind(&control_path).unwrap();

        write_metadata(
            &env.context,
            &sock_path,
            target,
            std::process::id(),
            std::num::NonZeroU32::new(115200),
        )
        .unwrap();
        update_metadata_status(&env.context, target, ConnectionStatus::Connected).unwrap();

        let spec = get_spec(&env.context, false, &mut writer).await.unwrap();
        assert_eq!(spec, target);
        let stdout = buffers.into_stdout_str();
        assert!(stdout.contains(&format!("Using active connection '{target}'")));
    }

    #[fuchsia::test]
    async fn test_get_spec_multiple_active_connections() {
        let temp = tempfile::tempdir().unwrap();
        let env = ffx_config::test_env()
            .runtime_config("shared_data", temp.path().to_str().unwrap())
            .build()
            .unwrap();
        let buffers = ffx_writer::TestBuffers::default();
        let mut writer = ffx_writer::SimpleWriter::new_test(&buffers);

        let target1 = "/dev/ttyUSB0";
        let sock1 = get_socket_path(&env.context, target1).unwrap();
        if let Some(parent) = sock1.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let _l1 = tokio::net::UnixListener::bind(&sock1).unwrap();
        let c1 = sock1.with_extension(uart_driver_api::CONTROL_SOCKET_EXTENSION);
        let _cl1 = tokio::net::UnixListener::bind(&c1).unwrap();
        write_metadata(
            &env.context,
            &sock1,
            target1,
            std::process::id(),
            std::num::NonZeroU32::new(115200),
        )
        .unwrap();
        update_metadata_status(&env.context, target1, ConnectionStatus::Connected).unwrap();

        let target2 = "/dev/ttyUSB1";
        let sock2 = get_socket_path(&env.context, target2).unwrap();
        let _l2 = tokio::net::UnixListener::bind(&sock2).unwrap();
        let c2 = sock2.with_extension(uart_driver_api::CONTROL_SOCKET_EXTENSION);
        let _cl2 = tokio::net::UnixListener::bind(&c2).unwrap();
        write_metadata(
            &env.context,
            &sock2,
            target2,
            std::process::id(),
            std::num::NonZeroU32::new(115200),
        )
        .unwrap();
        update_metadata_status(&env.context, target2, ConnectionStatus::Connected).unwrap();

        let res = get_spec(&env.context, false, &mut writer).await;
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(err.contains("multiple active UART connections found"));
        assert!(err.contains(target1));
        assert!(err.contains(target2));
    }
}
