// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::args::ProbeCommand;
use crate::metadata::{ConnectionMetadata, ConnectionStatus};
use crate::stream::{UartStream, connect_uart_stream, run_host_handshake};
use crate::sys::is_driver_running;
use ffx_config::EnvironmentContext;
use ffx_writer::{ToolIO, VerifiedMachineWriter};
use fho::{FfxMain, FfxTool, Result, user_error};
use fuchsia_async::TimeoutExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;

const PROBE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const PROBE_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
const PROBE_HANDSHAKE_RETRIES: usize = 4;

/// Method through which the target probe was validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProbeMethod {
    /// Active background driver daemon was already running and connected.
    BackgroundDriver,
    /// Direct channel handshake succeeded over UART.
    DirectHandshake,
}

/// Structured result of probing a target UART connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProbeResult {
    /// Whether the target is responsive and alive.
    pub alive: bool,
    /// Target path or endpoint probed.
    pub target: String,
    /// Method used to verify target responsiveness.
    pub method: ProbeMethod,
    /// Process ID (PID) of the background driver if already active.
    pub driver_pid: Option<u32>,
    /// Negotiated or active framing protocol (e.g. "ResendSP").
    pub protocol: Option<String>,
    /// Baud rate configured or used for probing, if applicable.
    pub baud: Option<u32>,
    /// Round-trip time in milliseconds measured during direct handshake.
    pub rtt_ms: Option<u64>,
}

#[derive(Debug, FfxTool)]
#[target(None)]
pub struct ProbeTool {
    #[command]
    pub(crate) cmd: ProbeCommand,
    pub(crate) context: EnvironmentContext,
}

impl ProbeTool {
    fn report_existing_driver_status(
        writer: &mut VerifiedMachineWriter<ProbeResult>,
        meta: &ConnectionMetadata,
    ) -> Result<()> {
        match &meta.status {
            ConnectionStatus::Connected => {
                let res = ProbeResult {
                    alive: true,
                    target: meta.target.clone(),
                    method: ProbeMethod::BackgroundDriver,
                    driver_pid: Some(meta.pid),
                    protocol: Some(meta.protocol.to_string()),
                    baud: meta.baud.map(|b| b.get()),
                    rtt_ms: None,
                };
                if writer.is_machine() {
                    writer.machine(&res)?;
                } else {
                    writer.line(format!(
                        "Target UART service is ALIVE (active connection via background driver PID {}).",
                        meta.pid
                    ))?;
                }
                Ok(())
            }
            ConnectionStatus::Connecting => Err(user_error!(
                "Background driver (PID {}) is currently connecting to target.",
                meta.pid
            )),
            ConnectionStatus::Error(reason) => Err(user_error!(
                "Background driver (PID {}) is running but reporting error: {reason}",
                meta.pid
            )),
        }
    }

    /// Opens a direct UART stream to the target device for probe handshaking.
    ///
    /// Configures the serial port or UNIX domain socket with the specified baud rate,
    /// enforcing a 5-second connection timeout to avoid blocking indefinitely on inaccessible ports.
    async fn connect_probe_stream(target_str: &str, baud: NonZeroU32) -> Result<UartStream> {
        connect_uart_stream(target_str, baud)
            .on_timeout(PROBE_CONNECT_TIMEOUT, || {
                Err(uart_driver_api::ConnectionError::TtyConfigureFailed {
                    error: format!(
                        "Timeout connecting to UART stream socket after {}s",
                        PROBE_CONNECT_TIMEOUT.as_secs()
                    ),
                })
            })
            .await
            .map_err(|e| user_error!("Failed to open UART stream: {e:?}"))
    }

    /// Executes a direct Channel 0 ResendSP handshake over the UART stream and measures round-trip latency.
    async fn probe_direct_handshake(
        target_str: &str,
        baud: NonZeroU32,
        writer: &mut VerifiedMachineWriter<ProbeResult>,
    ) -> Result<()> {
        let mut stream = Self::connect_probe_stream(target_str, baud).await?;
        let start = std::time::Instant::now();
        let proposed = vec![uart_fpl::ProtocolId::ResendSP];
        match run_host_handshake(
            &mut stream,
            proposed,
            PROBE_HANDSHAKE_TIMEOUT,
            PROBE_HANDSHAKE_RETRIES,
        )
        .await
        {
            Ok((protocol, _session_id, _unconsumed)) => {
                let rtt = start.elapsed();
                let res = ProbeResult {
                    alive: true,
                    target: target_str.to_string(),
                    method: ProbeMethod::DirectHandshake,
                    driver_pid: None,
                    protocol: Some(format!("{:?}", protocol)),
                    baud: Some(baud.get()),
                    rtt_ms: Some(rtt.as_millis() as u64),
                };
                if writer.is_machine() {
                    writer.machine(&res)?;
                } else {
                    writer.line(format!(
                        "Target UART service is ALIVE (direct probe handshake succeeded, Protocol: {:?}, RTT: {:?}).",
                        protocol,
                        rtt
                    ))?;
                }
                Ok(())
            }
            Err(e) => Err(user_error!("Target did not respond: {e:?}")),
        }
    }

    /// Probes a target UART connection non-destructively to verify device responsiveness.
    pub(crate) async fn probe(
        &self,
        target: &str,
        writer: &mut VerifiedMachineWriter<ProbeResult>,
    ) -> Result<()> {
        let resolved = crate::resolve_target(&self.context, target).await?;
        let target_str = match resolved {
            crate::ResolvedTarget::Active { meta, .. } => {
                if is_driver_running(meta.pid) {
                    return Self::report_existing_driver_status(writer, &meta);
                }
                meta.target
            }
            crate::ResolvedTarget::Inactive { target_path } => target_path,
        };

        if !writer.is_machine() {
            writer.line(format!("Probing target UART connection at {}...", target_str))?;
        }

        Self::probe_direct_handshake(&target_str, self.cmd.baud, writer).await
    }
}

impl FfxMain for ProbeTool {
    type Writer = VerifiedMachineWriter<ProbeResult>;
    type Error = fho::Error;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        let spec = crate::get_spec(&self.context, false, &mut writer).await?;
        self.probe(&spec, &mut writer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::UartProtocol;
    use ffx_writer::TestBuffers;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use uart_fpl::{FrameParser, FrameType, ProtocolId, TargetHandshake, encode_frame};

    #[fuchsia::test]
    fn test_report_existing_driver_status_variants() {
        let mut meta = ConnectionMetadata {
            pid: 4321,
            target: "/dev/ttyUSB0".to_string(),
            status: ConnectionStatus::Connected,
            id: None,
            baud: NonZeroU32::new(115200),
            protocol: UartProtocol::ResendSP,
            log_level: None,
            nodename: None,
            serial: None,
        };

        let buffers = TestBuffers::default();
        let mut writer = VerifiedMachineWriter::new_test(None, &buffers);
        assert!(ProbeTool::report_existing_driver_status(&mut writer, &meta).is_ok());
        assert!(buffers.into_stdout_str().contains("Target UART service is ALIVE"));

        // Verify machine JSON output and schema
        let buffers = TestBuffers::default();
        let mut writer = VerifiedMachineWriter::new_test(Some(ffx_writer::Format::Json), &buffers);
        assert!(ProbeTool::report_existing_driver_status(&mut writer, &meta).is_ok());
        let json_str = buffers.into_stdout_str();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        VerifiedMachineWriter::<ProbeResult>::verify_schema(&parsed).unwrap();
        assert_eq!(parsed["alive"], true);
        assert_eq!(parsed["method"], "background_driver");
        assert_eq!(parsed["driver_pid"], 4321);
        assert_eq!(parsed["protocol"], "ResendSP");
        assert_eq!(parsed["baud"], 115200);

        meta.status = ConnectionStatus::Connecting;
        let buffers = TestBuffers::default();
        let mut writer = VerifiedMachineWriter::new_test(None, &buffers);
        let err = ProbeTool::report_existing_driver_status(&mut writer, &meta).unwrap_err();
        assert!(err.to_string().contains("currently connecting to target"));

        meta.status =
            ConnectionStatus::Error(uart_driver_api::ConnectionError::TtyConfigureFailed {
                error: "hardware disconnected".to_string(),
            });
        let buffers = TestBuffers::default();
        let mut writer = VerifiedMachineWriter::new_test(None, &buffers);
        let err = ProbeTool::report_existing_driver_status(&mut writer, &meta).unwrap_err();
        assert!(err.to_string().contains("hardware disconnected"));
    }

    #[fuchsia::test]
    async fn test_probe_direct_handshake_success() {
        let temp = tempdir().unwrap();
        let sock_path = temp.path().join("probe_target.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

        let server_task = fuchsia_async::Task::local(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut parser = FrameParser::new();
                let mut buf = [0u8; 1024];
                let target = TargetHandshake::new(vec![ProtocolId::ResendSP]);
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    parser.feed(&buf[..n]);
                    if let Some(req) = parser.next_frame() {
                        let (_, payload) = target.handle_request(&req.payload).unwrap();
                        let resp =
                            encode_frame(req.session_id, 0, 0, FrameType::NegotiateResp, &payload)
                                .unwrap();
                        let _ = stream.write_all(&resp).await;
                        return;
                    }
                }
            }
        });

        let buffers = TestBuffers::default();
        let mut writer = VerifiedMachineWriter::new_test(None, &buffers);
        let res = ProbeTool::probe_direct_handshake(
            sock_path.to_str().unwrap(),
            NonZeroU32::new(115200).unwrap(),
            &mut writer,
        )
        .await;
        assert!(res.is_ok());
        let stdout = buffers.into_stdout_str();
        assert!(stdout.contains("direct probe handshake succeeded, Protocol: ResendSP, RTT:"));
        server_task.await;
    }

    #[fuchsia::test]
    async fn test_probe_direct_handshake_machine_json() {
        let temp = tempdir().unwrap();
        let sock_path = temp.path().join("probe_target_json.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

        let server_task = fuchsia_async::Task::local(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut parser = FrameParser::new();
                let mut buf = [0u8; 1024];
                let target = TargetHandshake::new(vec![ProtocolId::ResendSP]);
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    parser.feed(&buf[..n]);
                    if let Some(req) = parser.next_frame() {
                        let (_, payload) = target.handle_request(&req.payload).unwrap();
                        let resp =
                            encode_frame(req.session_id, 0, 0, FrameType::NegotiateResp, &payload)
                                .unwrap();
                        let _ = stream.write_all(&resp).await;
                        return;
                    }
                }
            }
        });

        let buffers = TestBuffers::default();
        let mut writer = VerifiedMachineWriter::new_test(Some(ffx_writer::Format::Json), &buffers);
        let res = ProbeTool::probe_direct_handshake(
            sock_path.to_str().unwrap(),
            NonZeroU32::new(115200).unwrap(),
            &mut writer,
        )
        .await;
        assert!(res.is_ok());
        let stdout = buffers.into_stdout_str();
        let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        VerifiedMachineWriter::<ProbeResult>::verify_schema(&parsed).unwrap();
        assert_eq!(parsed["alive"], true);
        assert_eq!(parsed["method"], "direct_handshake");
        assert_eq!(parsed["protocol"], "ResendSP");
        assert_eq!(parsed["baud"], 115200);
        assert!(parsed["rtt_ms"].as_u64().is_some());
        server_task.await;
    }

    #[fuchsia::test]
    async fn test_probe_direct_handshake_failure() {
        let temp = tempdir().unwrap();
        let sock_path = temp.path().join("non_existent_probe.sock");
        let buffers = TestBuffers::default();
        let mut writer = VerifiedMachineWriter::new_test(None, &buffers);
        let res = ProbeTool::probe_direct_handshake(
            sock_path.to_str().unwrap(),
            NonZeroU32::new(115200).unwrap(),
            &mut writer,
        )
        .await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("Failed to open UART stream"));
    }
}
