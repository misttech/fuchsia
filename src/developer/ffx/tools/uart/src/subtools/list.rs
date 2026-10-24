// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::args::ListCommand;
use crate::metadata::ConnectionMetadata;
use ffx_config::EnvironmentContext;
use ffx_writer::{MachineWriter, ToolIO};
use fho::{FfxMain, FfxTool, Result};

#[derive(Debug, FfxTool)]
#[target(None)]
pub struct ListTool {
    #[command]
    pub(crate) cmd: ListCommand,
    pub(crate) context: EnvironmentContext,
}

impl ListTool {
    /// Lists all currently active UART driver connections.
    pub(crate) async fn list(
        &self,
        writer: &mut MachineWriter<Vec<ConnectionMetadata>>,
    ) -> Result<()> {
        let _ = &self.cmd;
        let list_entries = crate::driver::get_active_connections(&self.context).await?;

        if writer.is_machine() {
            writer.machine(&list_entries)?;
        } else if list_entries.is_empty() {
            writer.print("No active UART connections.\n")?;
        } else {
            writer.print(format!(
                "{:<30} {:<8} {:<8} {:<10} {}\n",
                "TARGET", "PID", "BAUD", "PROTOCOL", "STATUS"
            ))?;
            for entry in list_entries {
                let baud_str = entry.baud.map(|b| b.to_string()).unwrap_or_else(|| "-".to_string());
                let status_str = format!("{}", entry.status);
                writer.print(format!(
                    "{:<30} {:<8} {:<8} {:<10} {}\n",
                    entry.target, entry.pid, baud_str, entry.protocol, status_str
                ))?;
            }
        }
        Ok(())
    }
}

impl FfxMain for ListTool {
    type Writer = MachineWriter<Vec<ConnectionMetadata>>;
    type Error = fho::Error;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        self.list(&mut writer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{ConnectionMetadata, ConnectionStatus, UartProtocol};
    use ffx_writer::TestBuffers;
    use std::fs;
    use std::num::NonZeroU32;
    use tempfile::tempdir;

    fn create_test_tool(temp_dir: &std::path::Path) -> ListTool {
        let env = ffx_config::test_env()
            .runtime_config("shared_data", temp_dir.to_str().unwrap())
            .build()
            .expect("test env");
        ListTool { cmd: ListCommand {}, context: env.context }
    }

    #[fuchsia::test]
    async fn test_list_empty() {
        let temp = tempdir().unwrap();
        let tool = create_test_tool(temp.path());
        let buffers = TestBuffers::default();
        let mut writer = MachineWriter::new_test(None, &buffers);
        tool.list(&mut writer).await.unwrap();
        assert_eq!("No active UART connections.\n", buffers.stdout.into_string());
    }

    #[fuchsia::test]
    async fn test_list_machine_empty() {
        let temp = tempdir().unwrap();
        let tool = create_test_tool(temp.path());
        let buffers = TestBuffers::default();
        let mut writer = MachineWriter::new_test(Some(ffx_writer::Format::Json), &buffers);
        tool.list(&mut writer).await.unwrap();
        assert_eq!("[]\n", buffers.stdout.into_string());
    }

    #[fuchsia::test]
    async fn test_list_active_connection_formatting() {
        let temp = tempdir().unwrap();
        let tool = create_test_tool(temp.path());
        let uart_dir = temp.path().join("ffx_uart");
        fs::create_dir_all(&uart_dir).unwrap();

        // Spawn mock live process matching driver process check ("python")
        let mut child = std::process::Command::new("python3")
            .args(["-c", "import time; time.sleep(10)"])
            .spawn()
            .unwrap();
        let pid = child.id();

        let socket_path = uart_dir.join("ffx_uart_0123456789abcdef.sock");
        fs::write(&socket_path, b"").unwrap();
        let control_socket_path = uart_driver_api::get_control_socket_path(&socket_path);
        let _listener = std::os::unix::net::UnixListener::bind(&control_socket_path).unwrap();

        let meta = ConnectionMetadata {
            pid,
            target: "/dev/ttyUSB0".to_string(),
            status: ConnectionStatus::Connected,
            id: Some("0123456789abcdef".to_string()),
            baud: NonZeroU32::new(115200),
            protocol: UartProtocol::ResendSP,
            log_level: None,
            nodename: Some("fuchsia-test-node".to_string()),
            serial: Some("SN-1234".to_string()),
        };
        fs::write(
            uart_dir.join("ffx_uart_0123456789abcdef.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        let buffers = TestBuffers::default();
        let mut writer = MachineWriter::new_test(None, &buffers);
        tool.list(&mut writer).await.unwrap();
        let output = buffers.stdout.into_string();

        for expected in [
            "TARGET",
            "PID",
            "BAUD",
            "PROTOCOL",
            "STATUS",
            "/dev/ttyUSB0",
            &pid.to_string(),
            "115200",
            "ResendSP",
            "Connected",
        ] {
            assert!(output.contains(expected), "Output missing {expected}: {output}");
        }

        let _ = child.kill();
        let _ = child.wait();
    }
}
