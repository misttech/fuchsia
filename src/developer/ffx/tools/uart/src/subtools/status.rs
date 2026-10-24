// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::args::StatusCommand;
use crate::metadata::{
    ConnectionMetadata, ConnectionStatus, DaemonMetrics, get_target_id, read_metadata,
};
use crate::sys::is_driver_running;
use ffx_config::EnvironmentContext;
use ffx_writer::{ToolIO, VerifiedMachineWriter};
use fho::{FfxMain, FfxTool, Result, user_error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::path::Path;
use std::time::Duration;

const CONTROL_SOCKET_TIMEOUT: Duration = Duration::from_secs(2);
const STALL_THRESHOLD_MS: u64 = 5_000;

/// Structured telemetry and status report for an active UART connection.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ConnectionStatusInfo {
    /// Target path or specifier.
    pub target: String,
    /// 16-character hexadecimal target identifier.
    pub target_id: String,
    /// Process ID (PID) of the driver daemon.
    pub pid: u32,
    /// Target nodename, if discovered.
    pub nodename: Option<String>,
    /// Target serial number, if discovered.
    pub serial: Option<String>,
    /// Path to the driver's log file.
    pub log_file: String,
    /// Connection status (e.g. "Connected").
    pub status: String,
    /// Configured baud rate, if applicable.
    pub baud: Option<u32>,
    /// Active framing protocol name (e.g. "ResendSP").
    pub active_protocol: String,
    /// Estimated round-trip latency in milliseconds.
    pub estimated_rtt_ms: u32,
    /// Total number of retransmissions.
    pub retransmissions: u64,
    /// Total number of CRC/framing errors detected.
    pub checksum_errors: u64,
    /// Number of connection drops observed.
    pub connection_drops: u64,
    /// Number of failed handshakes.
    pub handshake_failures: u64,
    /// Relative time since last read activity (e.g. "5s ago" or "Never").
    pub last_read_activity: String,
    /// Relative time since last write activity (e.g. "2s ago" or "Never").
    pub last_write_activity: String,
    /// Milliseconds since UNIX epoch of last read activity.
    pub last_read_timestamp_ms: u64,
    /// Milliseconds since UNIX epoch of last write activity.
    pub last_write_timestamp_ms: u64,
    /// Outgoing packet queue backlog.
    pub outgoing_queue_len: u32,
    /// Whether connection appears stalled.
    pub is_stalled: bool,
    /// Most recent channel error reported by the target over the UART link, if any.
    pub last_target_error: Option<String>,
}

#[derive(Debug, FfxTool)]
#[target(None)]
pub struct StatusTool {
    #[command]
    pub(crate) cmd: StatusCommand,
    pub(crate) context: EnvironmentContext,
}

impl StatusTool {
    /// Retrieves and displays real-time connection status and telemetry metrics for a target UART daemon.
    pub(crate) async fn status(
        &self,
        target: String,
        writer: &mut VerifiedMachineWriter<ConnectionStatusInfo>,
    ) -> Result<()> {
        let _ = &self.cmd;
        let resolved = crate::resolve_target(&self.context, &target).await?;
        let (target_str, meta_path, meta) = match resolved {
            crate::ResolvedTarget::Active { meta_path, meta } => {
                let target_str = meta.target.clone();
                (target_str, meta_path, meta)
            }
            crate::ResolvedTarget::Inactive { target_path } => {
                return Err(user_error!("Target '{target_path}' has no active UART connection."));
            }
        };

        let socket_path = uart_driver_api::get_client_socket_path(&meta_path);
        let control_socket_path = uart_driver_api::get_control_socket_path(&socket_path);

        let metrics =
            fetch_daemon_metrics(&self.context, &target, &target_str, &control_socket_path)?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let last_read_str = format_activity_diff(metrics.last_read_timestamp_ms, now);
        let last_write_str = format_activity_diff(metrics.last_write_timestamp_ms, now);
        let log_path_str = get_driver_log_path_str(&socket_path);
        let target_id_str = meta.id.clone().unwrap_or_else(|| get_target_id(&target_str));
        let is_stalled = is_connection_stalled(&meta, &metrics, now);

        let status_info = ConnectionStatusInfo {
            target: target.clone(),
            target_id: target_id_str,
            pid: meta.pid,
            nodename: meta.nodename.clone(),
            serial: meta.serial.clone(),
            log_file: log_path_str,
            status: meta.status.to_string(),
            baud: meta.baud.map(|b| b.get()),
            active_protocol: metrics.active_protocol.to_string(),
            estimated_rtt_ms: metrics.estimated_rtt_ms,
            retransmissions: metrics.retransmissions,
            checksum_errors: metrics.checksum_errors,
            connection_drops: metrics.connection_drops,
            handshake_failures: metrics.handshake_failures,
            last_read_activity: last_read_str,
            last_write_activity: last_write_str,
            last_read_timestamp_ms: metrics.last_read_timestamp_ms,
            last_write_timestamp_ms: metrics.last_write_timestamp_ms,
            outgoing_queue_len: metrics.outgoing_queue_len,
            is_stalled,
            last_target_error: metrics.last_target_error,
        };

        if writer.is_machine() {
            writer.machine(&status_info)?;
        } else {
            print_status_report(writer, &status_info)?;
        }
        Ok(())
    }
}

/// Connects to the driver's UNIX control socket and reads live [`DaemonMetrics`].
///
/// Sets a read timeout on the control socket to prevent hanging if the daemon
/// is unresponsive. If the socket cannot be connected, inspects driver process liveness
/// to provide a clear error message distinguishing an unreachable socket from a stopped daemon.
///
/// # Arguments
///
/// * `context` - Environment context used for metadata lookups.
/// * `target` - Original target string requested by the user.
/// * `target_str` - Canonicalized target path string.
/// * `control_socket_path` - Path to the UNIX control socket (typically `<socket>.control`).
///
/// # Errors
///
/// Returns an error if the daemon is not running, the control socket is unreachable,
/// reading times out, or the returned metrics cannot be parsed as JSON.
fn fetch_daemon_metrics(
    context: &EnvironmentContext,
    target: &str,
    target_str: &str,
    control_socket_path: &Path,
) -> Result<DaemonMetrics> {
    let mut stream = match std::os::unix::net::UnixStream::connect(control_socket_path) {
        Ok(s) => s,
        Err(e) => {
            if let Ok(Some(meta)) = read_metadata(context, target_str) {
                if is_driver_running(meta.pid) {
                    return Err(user_error!(
                        "Daemon (PID {}) is running but control socket is unreachable: {}",
                        meta.pid,
                        e
                    ));
                }
            }
            return Err(user_error!("Daemon is not running for target '{}'", target));
        }
    };

    stream.set_read_timeout(Some(CONTROL_SOCKET_TIMEOUT)).ok();
    let mut content = String::new();
    stream
        .read_to_string(&mut content)
        .map_err(|e| user_error!("Failed to read metrics from daemon: {}", e))?;
    serde_json::from_str(&content).map_err(|e| user_error!("Failed to parse metrics: {}", e))
}

/// Determines whether an active UART connection appears stalled based on telemetry timestamps.
///
/// A connection is considered stalled if it is in [`ConnectionStatus::Connected`] state,
/// write activity occurred more recently than read activity, and more than [`STALL_THRESHOLD_MS`]
/// milliseconds have elapsed since the last write without any subsequent read activity.
///
/// # Arguments
///
/// * `meta` - Connection metadata indicating current status.
/// * `metrics` - Live daemon telemetry metrics.
/// * `now` - Current system time in milliseconds since UNIX epoch.
fn is_connection_stalled(meta: &ConnectionMetadata, metrics: &DaemonMetrics, now: u64) -> bool {
    if meta.status != ConnectionStatus::Connected {
        return false;
    }
    let last_read = metrics.last_read_timestamp_ms;
    let last_write = metrics.last_write_timestamp_ms;
    if last_write > last_read { now.saturating_sub(last_write) > STALL_THRESHOLD_MS } else { false }
}

fn format_activity_diff(timestamp_ms: u64, now: u64) -> String {
    if timestamp_ms == 0 {
        "Never".to_string()
    } else {
        format!("{}s ago", now.saturating_sub(timestamp_ms) / 1000)
    }
}

/// Derives the expected log file path for a driver instance from its socket path.
fn get_driver_log_path_str(socket_path: &Path) -> String {
    if let Some(parent) = socket_path.parent() {
        crate::driver::get_driver_log_path(parent, socket_path).to_string_lossy().into_owned()
    } else {
        "Unknown".to_string()
    }
}

fn print_status_report(
    writer: &mut VerifiedMachineWriter<ConnectionStatusInfo>,
    info: &ConnectionStatusInfo,
) -> Result<()> {
    writer.print(format!("Connection Status for target '{}':\n", info.target))?;
    writer.print(format!("  Target ID:           {}\n", info.target_id))?;
    writer.print(format!("  Daemon PID:          {}\n", info.pid))?;
    writer.print(format!(
        "  Node Name:           {}\n",
        info.nodename.as_deref().unwrap_or("Unknown")
    ))?;
    writer.print(format!(
        "  Serial Number:       {}\n",
        info.serial.as_deref().unwrap_or("Unknown")
    ))?;
    writer.print(format!("  Log File:            {}\n", info.log_file))?;
    writer.print(format!("  Status:              {}\n", info.status))?;
    writer.print(format!("  Active Protocol:     {}\n", info.active_protocol))?;
    writer.print(format!("  Estimated RTT:       {} ms\n", info.estimated_rtt_ms))?;
    writer.print(format!("  Retransmissions:     {}\n", info.retransmissions))?;
    writer.print(format!("  Checksum Errors:     {}\n", info.checksum_errors))?;
    writer.print(format!("  Connection Drops:    {}\n", info.connection_drops))?;
    writer.print(format!("  Handshake Failures:  {}\n", info.handshake_failures))?;
    writer.print(format!("  Last Read Activity:  {}\n", info.last_read_activity))?;
    writer.print(format!("  Last Write Activity: {}\n", info.last_write_activity))?;
    writer.print(format!("  Queue Backlog:       {} messages\n", info.outgoing_queue_len))?;
    if let Some(err) = &info.last_target_error {
        writer.print(format!(
            "  Last Target Error:   {}\n",
            safe_string::TermSafe::from_str_escaped(err)
        ))?;
    }

    if info.is_stalled {
        writer
            .print("\n[WARNING] Connection appears stalled. The target may not be responding.\n")?;
    }

    Ok(())
}

impl FfxMain for StatusTool {
    type Writer = VerifiedMachineWriter<ConnectionStatusInfo>;
    type Error = fho::Error;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        let spec = crate::get_spec(&self.context, false, &mut writer).await?;
        self.status(spec, &mut writer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    use uart_driver_api::UartProtocol;

    #[test]
    fn test_is_connection_stalled() {
        let mut meta = ConnectionMetadata {
            pid: 1234,
            baud: NonZeroU32::new(115200),
            protocol: UartProtocol::ResendSP,
            target: "/dev/ttyUSB0".to_string(),
            status: ConnectionStatus::Connected,
            id: None,
            nodename: None,
            serial: None,
            log_level: None,
        };
        let mut metrics = DaemonMetrics::default();

        // Connected, no activity
        assert!(!is_connection_stalled(&meta, &metrics, 10_000));

        // Connected, read > write
        metrics.last_write_timestamp_ms = 5_000;
        metrics.last_read_timestamp_ms = 6_000;
        assert!(!is_connection_stalled(&meta, &metrics, 20_000));

        // Connected, write > read, but elapsed <= STALL_THRESHOLD_MS
        metrics.last_write_timestamp_ms = 10_000;
        metrics.last_read_timestamp_ms = 5_000;
        assert!(!is_connection_stalled(&meta, &metrics, 10_000 + STALL_THRESHOLD_MS));

        // Connected, write > read, elapsed > STALL_THRESHOLD_MS
        assert!(is_connection_stalled(&meta, &metrics, 10_000 + STALL_THRESHOLD_MS + 1));

        // Not connected -> never stalled even if elapsed > STALL_THRESHOLD_MS
        meta.status = ConnectionStatus::Connecting;
        assert!(!is_connection_stalled(&meta, &metrics, 10_000 + STALL_THRESHOLD_MS + 1));
    }

    #[test]
    fn test_status_print_human() {
        let info = ConnectionStatusInfo {
            target: "/dev/ttyUSB0".to_string(),
            target_id: "6fa7dcf897145d2e".to_string(),
            pid: 1234,
            nodename: Some("my-node".to_string()),
            serial: Some("SN123".to_string()),
            log_file: "/path/to/log".to_string(),
            status: "Connected".to_string(),
            baud: Some(115200),
            active_protocol: "ResendSP".to_string(),
            estimated_rtt_ms: 5,
            retransmissions: 1,
            checksum_errors: 0,
            connection_drops: 0,
            handshake_failures: 0,
            last_read_activity: "2s ago".to_string(),
            last_write_activity: "1s ago".to_string(),
            last_read_timestamp_ms: 1000,
            last_write_timestamp_ms: 2000,
            outgoing_queue_len: 0,
            is_stalled: false,
            last_target_error: Some("Failed to register channel with RCS: PEER_CLOSED".to_string()),
        };
        let buffers = ffx_writer::TestBuffers::default();
        let mut writer = VerifiedMachineWriter::<ConnectionStatusInfo>::new_test(None, &buffers);
        print_status_report(&mut writer, &info).unwrap();
        let output = buffers.stdout.into_string();
        assert!(output.contains("Connection Status for target '/dev/ttyUSB0':"));
        assert!(output.contains("Daemon PID:          1234"));
        assert!(output.contains("Node Name:           my-node"));
        assert!(output.contains("Serial Number:       SN123"));
        assert!(output.contains("Active Protocol:     ResendSP"));
        assert!(
            output
                .contains("Last Target Error:   Failed to register channel with RCS: PEER_CLOSED")
        );
        assert!(!output.contains("[WARNING] Connection appears stalled"));

        let mut info_with_ansi = info;
        info_with_ansi.last_target_error = Some("err \x1b[31mred\nspoof".to_string());
        let buffers_ansi = ffx_writer::TestBuffers::default();
        let mut writer_ansi =
            VerifiedMachineWriter::<ConnectionStatusInfo>::new_test(None, &buffers_ansi);
        print_status_report(&mut writer_ansi, &info_with_ansi).unwrap();
        let output_ansi = buffers_ansi.stdout.into_string();
        assert!(output_ansi.contains("Last Target Error:   err \\u{1b}[31mred\\nspoof"));
    }

    #[test]
    fn test_status_print_human_stalled() {
        let info = ConnectionStatusInfo {
            target: "/dev/ttyUSB0".to_string(),
            target_id: "6fa7dcf897145d2e".to_string(),
            pid: 1234,
            nodename: None,
            serial: None,
            log_file: "/path/to/log".to_string(),
            status: "Connected".to_string(),
            baud: None,
            active_protocol: "ResendSP".to_string(),
            estimated_rtt_ms: 0,
            retransmissions: 0,
            checksum_errors: 0,
            connection_drops: 0,
            handshake_failures: 0,
            last_read_activity: "Never".to_string(),
            last_write_activity: "Never".to_string(),
            last_read_timestamp_ms: 0,
            last_write_timestamp_ms: 0,
            outgoing_queue_len: 0,
            is_stalled: true,
            last_target_error: None,
        };
        let buffers = ffx_writer::TestBuffers::default();
        let mut writer = VerifiedMachineWriter::<ConnectionStatusInfo>::new_test(None, &buffers);
        print_status_report(&mut writer, &info).unwrap();
        let output = buffers.stdout.into_string();
        assert!(output.contains("Node Name:           Unknown"));
        assert!(output.contains("Serial Number:       Unknown"));
        assert!(output.contains("[WARNING] Connection appears stalled"));
    }

    #[test]
    fn test_status_machine_json() {
        let info = ConnectionStatusInfo {
            target: "/dev/ttyUSB0".to_string(),
            target_id: "6fa7dcf897145d2e".to_string(),
            pid: 1234,
            nodename: Some("my-node".to_string()),
            serial: None,
            log_file: "/path/to/log".to_string(),
            status: "Connected".to_string(),
            baud: Some(115200),
            active_protocol: "ResendSP".to_string(),
            estimated_rtt_ms: 5,
            retransmissions: 0,
            checksum_errors: 0,
            connection_drops: 0,
            handshake_failures: 0,
            last_read_activity: "1s ago".to_string(),
            last_write_activity: "1s ago".to_string(),
            last_read_timestamp_ms: 1000,
            last_write_timestamp_ms: 1000,
            outgoing_queue_len: 0,
            is_stalled: false,
            last_target_error: None,
        };
        let buffers = ffx_writer::TestBuffers::default();
        let mut writer = VerifiedMachineWriter::<ConnectionStatusInfo>::new_test(
            Some(ffx_writer::Format::Json),
            &buffers,
        );
        writer.machine(&info).unwrap();
        let output = buffers.stdout.into_string();
        let parsed: ConnectionStatusInfo = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed, info);
    }

    #[test]
    fn test_status_schema_verification() {
        let info = ConnectionStatusInfo {
            target: "/dev/ttyUSB0".to_string(),
            target_id: "6fa7dcf897145d2e".to_string(),
            pid: 1234,
            nodename: None,
            serial: None,
            log_file: "/path/to/log".to_string(),
            status: "Connected".to_string(),
            baud: None,
            active_protocol: "ResendSP".to_string(),
            estimated_rtt_ms: 0,
            retransmissions: 0,
            checksum_errors: 0,
            connection_drops: 0,
            handshake_failures: 0,
            last_read_activity: "Never".to_string(),
            last_write_activity: "Never".to_string(),
            last_read_timestamp_ms: 0,
            last_write_timestamp_ms: 0,
            outgoing_queue_len: 0,
            is_stalled: false,
            last_target_error: None,
        };
        let val = serde_json::to_value(&info).unwrap();
        VerifiedMachineWriter::<ConnectionStatusInfo>::verify_schema(&val)
            .expect("schema verification should succeed");
    }

    #[test]
    fn test_status_schema_output() {
        let buffers = ffx_writer::TestBuffers::default();
        let mut writer = VerifiedMachineWriter::<ConnectionStatusInfo>::new_test(
            Some(ffx_writer::Format::Json),
            &buffers,
        );
        writer.try_print_schema().expect("schema printing should succeed");
        let schema_output = buffers.stdout.into_string();
        assert!(schema_output.contains("ConnectionStatusInfo"));
    }
}
