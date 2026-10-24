// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of the `ffx uart disconnect` subtool.
//!
//! Handles terminating driver processes and cleaning up socket and metadata files.

use crate::args::DisconnectCommand;
use crate::metadata::ConnectionMetadata;
use crate::sys::{get_peer_pid, is_driver_running};
use ffx_config::EnvironmentContext;
use ffx_writer::{SimpleWriter, ToolIO};
use fho::{FfxMain, FfxTool, Result, user_error};
use std::path::Path;

fn remove_file_if_exists(path: &Path, label: &str) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("Failed to remove {label} file at {}: {:?}", path.display(), e);
        }
    }
}

#[derive(Debug, FfxTool)]
#[target(None)]
pub struct DisconnectTool {
    #[command]
    pub(crate) cmd: DisconnectCommand,
    pub(crate) context: EnvironmentContext,
}

impl DisconnectTool {
    pub(crate) async fn disconnect(&self, target: &str, writer: &mut SimpleWriter) -> Result<()> {
        let resolved = crate::resolve_target(&self.context, target).await?;
        match resolved {
            crate::ResolvedTarget::Active { meta_path, meta } => {
                self.terminate_and_clean_target(&meta_path, &meta).await?;
                writer.line(format!("Disconnect called for {}", meta.target))?;
                Ok(())
            }
            crate::ResolvedTarget::Inactive { target_path } => {
                Err(user_error!("Not connected to target {target_path}"))
            }
        }
    }

    pub(crate) async fn terminate_and_clean_target(
        &self,
        meta_path: &Path,
        meta: &ConnectionMetadata,
    ) -> Result<()> {
        let socket_path = uart_driver_api::get_client_socket_path(meta_path);
        // `is_driver_running` verifies both process liveness and that the command line / binary
        // matches `ffx-uart-driver` via /proc, preventing accidental termination if the PID was reused.
        if is_driver_running(meta.pid) {
            if let Err(e) = crate::driver::terminate_process(meta.pid).await {
                log::warn!("Failed to terminate driver process {}: {:?}", meta.pid, e);
            }
        } else if socket_path.exists() {
            if let Ok(Ok(stream)) = tokio::time::timeout(
                crate::driver::SOCKET_PROBE_TIMEOUT,
                tokio::net::UnixStream::connect(&socket_path),
            )
            .await
            {
                if let Ok(pid) = get_peer_pid(&stream) {
                    if is_driver_running(pid) {
                        if let Err(e) = crate::driver::terminate_process(pid).await {
                            log::warn!("Failed to terminate peer driver process {pid}: {e:?}");
                        }
                    }
                }
            }
        }
        let control_socket_path = uart_driver_api::get_control_socket_path(&socket_path);
        remove_file_if_exists(meta_path, "metadata");
        remove_file_if_exists(&socket_path, "socket");
        remove_file_if_exists(&control_socket_path, "control socket");
        Ok(())
    }
}

impl FfxMain for DisconnectTool {
    type Writer = SimpleWriter;
    type Error = fho::Error;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        let _ = self.cmd;
        let spec = crate::get_spec(&self.context, false, &mut writer).await?;
        self.disconnect(&spec, &mut writer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{
        ConnectionStatus, UartProtocol, find_active_connection, get_socket_path, write_metadata,
    };
    use std::num::NonZeroU32;
    use tempfile::tempdir;

    fn create_test_tool(temp_dir: &Path) -> DisconnectTool {
        let env = ffx_config::test_env()
            .runtime_config("shared_data", temp_dir.to_str().unwrap())
            .build()
            .expect("test env");
        DisconnectTool { cmd: DisconnectCommand {}, context: env.context }
    }

    #[fuchsia::test]
    async fn test_disconnect_not_connected() {
        let temp = tempdir().unwrap();
        let tool = create_test_tool(temp.path());
        let buffers = ffx_writer::TestBuffers::default();
        let mut writer = SimpleWriter::new_test(&buffers);
        let res = tool.disconnect("/dev/ttyUSB99", &mut writer).await;
        assert!(res.is_err());
        let err_msg = format!("{}", res.unwrap_err());
        assert!(err_msg.contains("Not connected to target /dev/ttyUSB99"));
    }

    #[fuchsia::test]
    async fn test_disconnect_success() {
        let temp = tempdir().unwrap();
        let tool = create_test_tool(temp.path());
        let buffers = ffx_writer::TestBuffers::default();
        let mut writer = SimpleWriter::new_test(&buffers);
        let target = "/dev/ttyUSB0";
        let socket_path = get_socket_path(&tool.context, target).unwrap();
        write_metadata(&tool.context, &socket_path, target, 999999, NonZeroU32::new(115200))
            .unwrap();
        let meta_path = socket_path.with_extension(uart_driver_api::METADATA_FILE_EXTENSION);
        assert!(meta_path.exists());
        let control_path = socket_path.with_extension(uart_driver_api::CONTROL_SOCKET_EXTENSION);
        std::fs::write(&control_path, b"").unwrap();
        assert!(control_path.exists());

        let res = tool.disconnect(target, &mut writer).await;
        assert!(res.is_ok());
        assert!(!meta_path.exists());
        assert!(!control_path.exists());
        assert_eq!(buffers.into_stdout_str(), "Disconnect called for /dev/ttyUSB0\n");
    }

    #[fuchsia::test]
    async fn test_disconnect_find_active_connection() {
        let temp = tempdir().unwrap();
        let tool = create_test_tool(temp.path());
        let shared_path = temp.path().join("ffx_uart");
        std::fs::create_dir_all(&shared_path).unwrap();

        let target_id = "0123456789abcdef";
        let meta = ConnectionMetadata {
            pid: 1234,
            target: "/dev/ttyUSB2".to_string(),
            status: ConnectionStatus::Connected,
            id: Some(target_id.to_string()),
            baud: NonZeroU32::new(115200),
            protocol: UartProtocol::ResendSP,
            log_level: None,
            nodename: Some("fuchsia-node-disc".to_string()),
            serial: None,
        };
        let file_path = shared_path.join(format!("ffx_uart_{target_id}.json"));
        std::fs::write(&file_path, serde_json::to_string(&meta).unwrap()).unwrap();

        let found = find_active_connection(&tool.context, target_id).unwrap();
        assert!(found.is_some());
        let (path, found_meta) = found.unwrap();
        assert_eq!(path, file_path);
        assert_eq!(found_meta.pid, 1234);

        let found_by_nodename = find_active_connection(&tool.context, "fuchsia-node-disc").unwrap();
        assert!(found_by_nodename.is_some());
        assert_eq!(found_by_nodename.unwrap().1.pid, 1234);
    }

    #[fuchsia::test]
    async fn test_disconnect_main_with_target_id() {
        let temp = tempdir().unwrap();
        let target_id = "0123456789abcdef";
        let mut env = ffx_config::test_env()
            .runtime_config("shared_data", temp.path().to_str().unwrap())
            .build()
            .expect("test env");
        let shared_path = temp.path().join("ffx_uart");
        std::fs::create_dir_all(&shared_path).unwrap();

        let meta = ConnectionMetadata {
            pid: 999999,
            target: "/dev/ttyUSB0".to_string(),
            status: ConnectionStatus::Connected,
            id: Some(target_id.to_string()),
            baud: NonZeroU32::new(115200),
            protocol: UartProtocol::ResendSP,
            log_level: None,
            nodename: None,
            serial: None,
        };
        let file_path = shared_path.join(format!("ffx_uart_{target_id}.json"));
        std::fs::write(&file_path, serde_json::to_string(&meta).unwrap()).unwrap();
        assert!(file_path.exists());

        env.context.override_target_specifier(&Some(format!("uart:{target_id}")));

        let tool = DisconnectTool { cmd: DisconnectCommand {}, context: env.context };
        let buffers = ffx_writer::TestBuffers::default();
        let writer = SimpleWriter::new_test(&buffers);
        let res = tool.main(writer).await;
        assert!(res.is_ok());
        assert!(!file_path.exists());
    }
}
