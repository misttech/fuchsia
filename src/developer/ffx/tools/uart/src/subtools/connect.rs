// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of the `ffx uart connect` subtool.
//!
//! Handles connection parameter resolution, daemon process launching,
//! socket verification, and reconnect / cleanup logic.

use crate::args::{ConnectCommand, DEFAULT_BAUD_RATE};
use crate::driver::{
    cleanup_target_files, get_driver_log_path, get_or_create_log_dir, locate_driver,
    spawn_driver_daemon, validate_socket_path_len, wait_for_daemon_started,
};
use crate::metadata::{ConnectionMetadata, get_socket_path};
use crate::sys::{get_peer_pid, is_driver_running};
use ffx_config::EnvironmentContext;
use ffx_writer::{SimpleWriter, ToolIO};
use fho::{FfxMain, FfxTool, Result, bug, user_error};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

const LOG_LEVEL_CONFIG_KEY: &str = "log.level";

pub(crate) fn canonicalize_or_parent(path: &Path) -> PathBuf {
    if let Ok(abs) = std::fs::canonicalize(path) {
        return abs;
    }
    if let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) {
        if let Ok(parent_abs) = std::fs::canonicalize(parent) {
            return parent_abs.join(file_name);
        }
    }

    path.to_path_buf()
}

#[derive(Debug)]
struct LaunchParameters {
    baud: NonZeroU32,
    log_level: Option<String>,
    socket_path: PathBuf,
    log_dir: PathBuf,
    log_path: PathBuf,
}

#[derive(Debug, FfxTool)]
#[target(None)]
pub struct ConnectTool {
    #[command]
    pub(crate) cmd: ConnectCommand,
    pub(crate) context: EnvironmentContext,
}

impl ConnectTool {
    fn prepare_launch_params(
        &self,
        target: &str,
        args: &ConnectCommand,
        meta: Option<&ConnectionMetadata>,
    ) -> Result<LaunchParameters> {
        let (baud, log_level) = self.resolve_args(args, meta, args.protocol.as_deref())?;
        let socket_path = self.resolve_socket_path(target, args.socket.as_deref())?;
        let log_dir = get_or_create_log_dir(&self.context)?;
        let log_path = get_driver_log_path(&log_dir, &socket_path);
        Ok(LaunchParameters { baud, log_level, socket_path, log_dir, log_path })
    }

    pub(crate) async fn connect(&self, target: String, writer: &mut SimpleWriter) -> Result<()> {
        let raw_target = target.strip_prefix("uart:").unwrap_or(&target).to_string();
        let original_target = raw_target.clone();
        let resolved = crate::resolve_target(&self.context, &raw_target).await?;
        let (target, existing_meta) = match resolved {
            crate::ResolvedTarget::Active { meta, .. } => (meta.target.clone(), Some(meta)),
            crate::ResolvedTarget::Inactive { target_path } => (target_path, None),
        };

        self.handle_reconnect_and_cleanup(&target, self.cmd.reconnect, existing_meta.as_ref())
            .await?;

        let params = self.prepare_launch_params(&target, &self.cmd, existing_meta.as_ref())?;
        let driver_path = locate_driver(&self.context);
        spawn_driver_daemon(
            &driver_path,
            &self.context,
            params.log_level.as_deref(),
            &target,
            params.baud,
            &params.socket_path,
            &params.log_dir,
            self.cmd.no_retry,
        )
        .await?;

        let _pid = wait_for_daemon_started(&self.context, &target, &params.log_path).await?;

        let action = if self.cmd.reconnect { "Reconnect" } else { "Connect" };
        writer.line(format!("{action} called for {original_target}"))?;
        Ok(())
    }

    pub(crate) async fn terminate_peer(socket_path: &Path) -> Result<()> {
        if socket_path.exists() {
            if let Ok(Ok(stream)) = tokio::time::timeout(
                crate::driver::SOCKET_PROBE_TIMEOUT,
                tokio::net::UnixStream::connect(socket_path),
            )
            .await
            {
                if let Ok(pid) = get_peer_pid(&stream) {
                    crate::driver::terminate_process(pid).await?;
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn handle_reconnect_and_cleanup(
        &self,
        target: &str,
        reconnect: bool,
        existing_meta: Option<&ConnectionMetadata>,
    ) -> Result<()> {
        if !reconnect {
            if let Some(meta) = existing_meta {
                if is_driver_running(meta.pid)
                    && meta.status == crate::metadata::ConnectionStatus::Connected
                {
                    return Err(user_error!(
                        "Already connected to target {} (via resolved path {}). Run with '--reconnect' if you want to force a restart and apply new arguments.",
                        meta.target,
                        target
                    ));
                }
            }
        }

        if let Some(meta) = existing_meta {
            if is_driver_running(meta.pid) {
                log::info!("Terminating driver process (PID {}) for target {}", meta.pid, target);
                crate::driver::terminate_process(meta.pid).await?;
            }
        }

        if let Ok(socket_path) = get_socket_path(&self.context, target) {
            Self::terminate_peer(&socket_path).await?;
        }

        cleanup_target_files(&self.context, target);
        Ok(())
    }

    pub(crate) fn validate_protocol(protocol: Option<&str>) -> Result<()> {
        match protocol {
            None => Ok(()),
            Some(p) => match p.to_lowercase().as_str() {
                "resend" | "resendsp" => Ok(()),
                other => Err(user_error!(
                    "Unknown protocol '{}'. Supported protocols: 'resend' (ResendSP)",
                    other
                )),
            },
        }
    }

    pub(crate) fn resolve_log_level(
        &self,
        reconnect: bool,
        existing_meta: Option<&ConnectionMetadata>,
    ) -> Option<String> {
        let query = self
            .context
            .build()
            .name(Some(LOG_LEVEL_CONFIG_KEY))
            .level(Some(ffx_config::ConfigLevel::Runtime));
        self.context.get(query).ok().or_else(|| {
            if reconnect { existing_meta.and_then(|m| m.log_level.clone()) } else { None }
        })
    }

    pub(crate) fn resolve_args(
        &self,
        cmd: &ConnectCommand,
        existing_meta: Option<&ConnectionMetadata>,
        protocol: Option<&str>,
    ) -> Result<(NonZeroU32, Option<String>)> {
        Self::validate_protocol(protocol)?;

        let baud = match (cmd.baud, cmd.reconnect) {
            (Some(b), _) => b,
            (None, true) => existing_meta.and_then(|m| m.baud).unwrap_or(DEFAULT_BAUD_RATE),
            (None, false) => DEFAULT_BAUD_RATE,
        };

        let log_level = self.resolve_log_level(cmd.reconnect, existing_meta);
        Ok((baud, log_level))
    }

    pub(crate) fn resolve_socket_path(
        &self,
        target: &str,
        custom_socket: Option<&str>,
    ) -> Result<PathBuf> {
        let (raw_socket_path, is_custom) = match custom_socket {
            Some(s) => (PathBuf::from(s), true),
            None => (get_socket_path(&self.context, target)?, false),
        };

        let absolute_socket_path = if raw_socket_path.is_relative() {
            std::env::current_dir().map_err(|e| bug!("{e:?}"))?.join(&raw_socket_path)
        } else {
            raw_socket_path
        };

        let resolved_socket_path = canonicalize_or_parent(&absolute_socket_path);

        if is_custom {
            if let Some(parent) = resolved_socket_path.parent() {
                if !parent.exists() {
                    return Err(user_error!(
                        "Parent directory for custom socket path '{}' does not exist: {}",
                        resolved_socket_path.display(),
                        parent.display()
                    ));
                }
            }
        }

        validate_socket_path_len(&resolved_socket_path)?;
        Ok(resolved_socket_path)
    }
}

impl FfxMain for ConnectTool {
    type Writer = SimpleWriter;
    type Error = fho::Error;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        let spec = crate::get_spec(&self.context, true, &mut writer).await?;
        self.connect(spec, &mut writer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{ConnectionStatus, UartProtocol, read_metadata, write_metadata};
    use tempfile::tempdir;

    fn create_test_tool(temp_dir: &std::path::Path) -> ConnectTool {
        let env = ffx_config::test_env()
            .runtime_config("shared_data", temp_dir.to_str().unwrap())
            .build()
            .expect("test env");
        ConnectTool {
            cmd: ConnectCommand {
                baud: None,
                reconnect: false,
                no_retry: false,
                socket: None,
                protocol: None,
            },
            context: env.context,
        }
    }

    #[fuchsia::test]
    fn test_validate_protocol() {
        assert!(ConnectTool::validate_protocol(None).is_ok());
        assert!(ConnectTool::validate_protocol(Some("resendsp")).is_ok());
        assert!(ConnectTool::validate_protocol(Some("unknown")).is_err());
        assert!(ConnectTool::validate_protocol(Some("invalid")).is_err());
    }

    #[fuchsia::test]
    fn test_resolve_args_defaults() {
        let temp = tempdir().unwrap();
        let tool = create_test_tool(temp.path());
        let cmd = ConnectCommand {
            baud: None,
            reconnect: false,
            no_retry: false,
            socket: None,
            protocol: None,
        };
        let (baud, log_level) = tool.resolve_args(&cmd, None, None).unwrap();
        assert_eq!(baud, DEFAULT_BAUD_RATE);
        assert!(log_level.is_none());
    }

    #[fuchsia::test]
    fn test_resolve_args_custom_cli_override() {
        let temp = tempdir().unwrap();
        let tool = create_test_tool(temp.path());
        let cmd = ConnectCommand {
            baud: NonZeroU32::new(115200),
            reconnect: false,
            no_retry: false,
            socket: None,
            protocol: None,
        };
        let (baud_custom, _) = tool.resolve_args(&cmd, None, None).unwrap();
        assert_eq!(baud_custom, NonZeroU32::new(115200).unwrap());
    }

    #[fuchsia::test]
    fn test_resolve_args_reconnect_inherits_metadata() {
        let temp = tempdir().unwrap();
        let tool = create_test_tool(temp.path());
        let existing = ConnectionMetadata {
            pid: 100,
            target: "/dev/ttyUSB0".to_string(),
            status: ConnectionStatus::Connected,
            id: None,
            baud: NonZeroU32::new(57600),
            protocol: UartProtocol::ResendSP,
            log_level: Some("debug".to_string()),
            nodename: None,
            serial: None,
        };
        let cmd_reconnect = ConnectCommand {
            baud: None,
            reconnect: true,
            no_retry: false,
            socket: None,
            protocol: None,
        };
        let (baud_reconnect, log_level_reconnect) =
            tool.resolve_args(&cmd_reconnect, Some(&existing), None).unwrap();
        assert_eq!(baud_reconnect, NonZeroU32::new(57600).unwrap());
        assert_eq!(log_level_reconnect, Some("debug".to_string()));
    }

    #[fuchsia::test]
    fn test_resolve_socket_path() {
        let temp = tempdir().unwrap();
        let tool = create_test_tool(temp.path());

        // Default socket path for target
        let path = tool.resolve_socket_path("/dev/ttyUSB0", None).unwrap();
        assert!(path.to_string_lossy().contains("ffx_uart"));

        // Custom socket path in existing dir
        let custom = temp.path().join("my_custom.sock");
        let path_custom = tool.resolve_socket_path("/dev/ttyUSB0", custom.to_str()).unwrap();
        assert_eq!(path_custom, custom);
    }

    #[fuchsia::test]
    fn test_canonicalize_or_parent() {
        let temp = tempdir().unwrap();
        let non_existent_file = temp.path().join("subfile.sock");
        let resolved = canonicalize_or_parent(&non_existent_file);
        assert_eq!(resolved.file_name(), non_existent_file.file_name());
        assert_eq!(resolved.parent().unwrap(), temp.path().canonicalize().unwrap());
    }

    #[fuchsia::test]
    async fn test_handle_reconnect_already_connected() {
        let temp = tempdir().unwrap();
        let tool = create_test_tool(temp.path());
        let current_pid = std::process::id();
        let target = "/dev/ttyUSB0";
        let socket_path = get_socket_path(&tool.context, target).unwrap();
        write_metadata(&tool.context, &socket_path, target, current_pid, NonZeroU32::new(115200))
            .unwrap();
        crate::metadata::update_metadata_status(&tool.context, target, ConnectionStatus::Connected)
            .unwrap();

        let meta = read_metadata(&tool.context, target).unwrap();
        assert!(meta.is_some());

        // Without reconnect: should fail because driver process is alive and status is Connected
        let res = tool.handle_reconnect_and_cleanup(target, false, meta.as_ref()).await;
        assert!(res.is_err());
        let err_msg = format!("{}", res.unwrap_err());
        assert!(err_msg.contains("Already connected to target"));
    }

    #[fuchsia::test]
    async fn test_handle_reconnect_stale_socket_cleaned() {
        let temp = tempdir().unwrap();
        let tool = create_test_tool(temp.path());
        let target = "/dev/ttyUSB0";
        let socket_path = get_socket_path(&tool.context, target).unwrap();
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&socket_path, b"").unwrap();
        assert!(socket_path.exists());

        let res = tool.handle_reconnect_and_cleanup(target, false, None).await;
        assert!(res.is_ok());
        assert!(!socket_path.exists());
    }
}
