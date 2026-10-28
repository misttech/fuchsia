// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Connection metadata management for UART targets.
//!
//! Provides functions to resolve, write, read, and delete connection metadata
//! which associates active UART target connections with their driver processes.

use ffx_config::EnvironmentContext;
use std::fs::{self, File};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

pub use uart_driver_api::{
    CONTROL_SOCKET_EXTENSION, ConnectionError, ConnectionMetadata, ConnectionStatus,
    METADATA_FILE_EXTENSION, UNIX_SOCKET_EXTENSION, UartProtocol, get_client_socket_path,
    get_control_socket_path,
};

/// Diagnostic and operational metrics exported by an active UART driver daemon.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct DaemonMetrics {
    /// Cumulative count of corrupted frames or CRC checksum failures detected.
    pub checksum_errors: u64,
    /// Cumulative count of frame retransmission timeouts triggered by the sender.
    pub retransmissions: u64,
    /// Currently active framing protocol variant (e.g. `ResendSP`).
    pub active_protocol: UartProtocol,
    /// Smoothed round-trip time estimate in milliseconds measured from ACK arrivals.
    pub estimated_rtt_ms: u32,
    /// Cumulative count of physical connection drops or TTY I/O disconnections.
    pub connection_drops: u64,
    /// Cumulative count of failed Channel 0 protocol negotiation attempts.
    pub handshake_failures: u64,
    /// Unix timestamp in milliseconds when data was last received from the UART port.
    pub last_read_timestamp_ms: u64,
    /// Unix timestamp in milliseconds when data was last written to the UART port.
    pub last_write_timestamp_ms: u64,
    /// Number of outbound frames currently buffered in the transmission queue.
    pub outgoing_queue_len: u32,
    /// Most recent channel error reported by the target over the UART link, if any.
    pub last_target_error: Option<String>,
}

impl Default for DaemonMetrics {
    fn default() -> Self {
        Self {
            checksum_errors: 0,
            retransmissions: 0,
            active_protocol: UartProtocol::ResendSP,
            estimated_rtt_ms: 0,
            connection_drops: 0,
            handshake_failures: 0,
            last_read_timestamp_ms: 0,
            last_write_timestamp_ms: 0,
            outgoing_queue_len: 0,
            last_target_error: None,
        }
    }
}

/// Normalizes and canonicalizes a target string into a standard format.
///
/// Normalization rules include:
/// 1. Stripping trailing slashes (`/` or `\`) from the target string.
/// 2. Treating targets as filesystem paths:
///    - If the path is relative, it is joined to the current working directory to make it absolute.
///    - Path components are cleaned by resolving relative indicators like `.` (current directory)
///      and `..` (parent directory).
pub fn canonicalize_target(target: &str) -> String {
    if target.is_empty() {
        return String::new();
    }
    let trimmed_target = target.trim_end_matches(&['/', '\\']);
    let target = if trimmed_target.is_empty() { &target[..1] } else { trimmed_target };
    let path = std::path::Path::new(target);
    let absolute = if path.is_relative() {
        std::env::current_dir().map(|cwd| cwd.join(path)).unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let normalized = uart_driver_api::clean_path(&absolute);
    normalized.to_string_lossy().into_owned()
}

/// Computes a unique ID for the given target.
///
/// Delegates directly to [`uart_driver_api::get_target_id_from_target_path`].
pub fn get_target_id(target: &str) -> String {
    uart_driver_api::get_target_id_from_target_path(Path::new(target))
}

/// Retrieves the Unix socket path corresponding to the given UART target.
///
/// The socket path is derived from the canonical target filesystem path.
///
/// # Errors
/// Returns an error if the socket path cannot be resolved or retrieved from target configuration.
pub fn get_socket_path(
    context: &EnvironmentContext,
    target: &str,
) -> std::result::Result<PathBuf, fho::Error> {
    let target_path = uart_driver_api::parse_target_endpoint(target, context)
        .map_err(|e| fho::user_error!("{e}"))?;
    uart_driver_api::get_socket_path_from_target_path(&target_path, context)
        .map_err(|e| fho::user_error!("{e}"))
}

/// Returns the path to the connection metadata file for a given target.
///
/// Connection metadata is stored as JSON companion files alongside the UNIX domain socket
/// under the `shared_data/ffx_uart/` directory (e.g., `ffx_uart_<target_id>.json`).
///
/// # Errors
/// Returns an error if querying the environment context for the shared data directory fails.
pub fn get_metadata_path(
    context: &EnvironmentContext,
    target: &str,
) -> std::result::Result<PathBuf, fho::Error> {
    Ok(uart_driver_api::get_metadata_path(&get_socket_path(context, target)?))
}

/// Returns the path to the daemon control socket for a given target.
///
/// # Errors
/// Returns an error if querying the environment context for the shared data directory fails.
pub fn get_control_path(
    context: &EnvironmentContext,
    target: &str,
) -> std::result::Result<PathBuf, fho::Error> {
    Ok(get_control_socket_path(&get_socket_path(context, target)?))
}

/// Atomically writes serializable metadata to `path` using a temporary file in the same parent directory.
fn atomic_write_json<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> std::result::Result<(), fho::Error> {
    let parent = path.parent().ok_or_else(|| {
        fho::user_error!("Invalid path without parent directory: {}", path.display())
    })?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temp, value).map_err(|e| fho::bug!("{e}"))?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| fho::Error::IoError(e.error))?;
    Ok(())
}

/// Writes connection metadata (PID and target name) to the metadata file for the given target.
///
/// Creates parent directories as needed if they do not exist.
///
/// # Errors
/// Returns an error if the directories cannot be created or writing to the file fails.
pub fn write_metadata(
    context: &EnvironmentContext,
    socket_path: &std::path::Path,
    target: &str,
    pid: u32,
    baud: Option<NonZeroU32>,
) -> std::result::Result<(), fho::Error> {
    let path = uart_driver_api::get_metadata_path(socket_path);
    let id = get_target_id(target);
    let query =
        context.build().name(Some("log.level")).level(Some(ffx_config::ConfigLevel::Runtime));
    let log_level = context.get::<String, _>(query).ok();
    let metadata = ConnectionMetadata {
        pid,
        target: canonicalize_target(target),
        status: ConnectionStatus::Connecting,
        id: Some(id),
        baud,
        protocol: UartProtocol::ResendSP,
        log_level,
        nodename: None,
        serial: None,
    };
    atomic_write_json(&path, &metadata)
}
/// Reads connection metadata from a file path.
///
/// Returns `None` if the file does not exist (`ErrorKind::NotFound`).
/// If the metadata file contains invalid JSON, returns `Some` with a default fallback metadata
/// struct with `pid` set to 0 and status set to `Corrupt JSON`. If reading the file fails for
/// any other reason, returns `Some` with status set to `Unreadable`.
pub fn load_metadata_from_path(
    path: &std::path::Path,
    fallback_target: &str,
    fallback_id: Option<String>,
) -> Option<ConnectionMetadata> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            log::warn!("Failed to read metadata file at {}: {e}", path.display());
            return Some(ConnectionMetadata {
                pid: 0,
                target: fallback_target.to_string(),
                status: ConnectionStatus::Error(ConnectionError::raw("Unreadable")),
                id: fallback_id,
                baud: None,
                protocol: UartProtocol::Unknown,
                log_level: None,
                nodename: None,
                serial: None,
            });
        }
    };
    Some(serde_json::from_str(&content).unwrap_or_else(|e| {
        log::warn!("Failed to parse metadata JSON at {}: {e}", path.display());
        ConnectionMetadata {
            pid: 0,
            target: fallback_target.to_string(),
            status: ConnectionStatus::Error(ConnectionError::raw("Corrupt JSON")),
            id: fallback_id,
            baud: None,
            protocol: UartProtocol::Unknown,
            log_level: None,
            nodename: None,
            serial: None,
        }
    }))
}

/// Reads connection metadata from the metadata file for the given target.
///
/// Returns `None` if no metadata file exists. If the metadata file contains invalid JSON,
/// returns a default fallback metadata struct with `pid` set to 0.
///
/// # Errors
/// Returns an error if resolving the metadata path fails.
pub fn read_metadata(
    context: &EnvironmentContext,
    target: &str,
) -> std::result::Result<Option<ConnectionMetadata>, fho::Error> {
    let path = get_metadata_path(context, target)?;
    let id = get_target_id(target);
    Ok(load_metadata_from_path(&path, target, Some(id)))
}

/// Searches the connection metadata directory for any metadata file that associates
/// with the given original target name.
/// Returns the path to the directory where UART connection metadata files are stored.
pub fn get_shared_uart_path(
    context: &EnvironmentContext,
) -> std::result::Result<PathBuf, fho::Error> {
    let mut dir_path = match context.get::<PathBuf, _>("shared_data") {
        Ok(p) => p,
        Err(_) => context.get_shared_data_path().map_err(|e| fho::user_error!("{e}"))?,
    };
    dir_path.push("ffx_uart");
    Ok(dir_path)
}

/// Attempts to find an existing connection metadata file for the given target name or path.
///
/// Returns the path to the metadata file and the metadata struct itself if found.
///
/// # Errors
/// Returns an error if querying the shared data directory path fails.
pub fn find_metadata_by_target(
    context: &EnvironmentContext,
    target_name: &str,
) -> std::result::Result<Option<(PathBuf, ConnectionMetadata)>, fho::Error> {
    let dir_path = get_shared_uart_path(context)?;
    let entries = match fs::read_dir(&dir_path) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    let canonical_target = canonicalize_target(target_name);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                if file_name.starts_with("ffx_uart_") && file_name.ends_with(".json") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(metadata) = serde_json::from_str::<ConnectionMetadata>(&content) {
                            if canonicalize_target(&metadata.target) == canonical_target {
                                return Ok(Some((path, metadata)));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Searches the connection metadata directory for any metadata matching an identifier
/// (canonical path, device filename, nodename, or serial number), preferring actively
/// running daemons.
fn scan_for_metadata_by_identifier(
    shared_path: &Path,
    identifier: &str,
) -> Option<(PathBuf, ConnectionMetadata)> {
    let canonical_raw = canonicalize_target(identifier);
    let entries = fs::read_dir(shared_path).ok()?;
    let mut fallback_dead = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(meta) = serde_json::from_str::<ConnectionMetadata>(&content) {
                    let canonical_meta = canonicalize_target(&meta.target);
                    if canonical_meta == canonical_raw
                        || Path::new(&meta.target).file_name().and_then(|n| n.to_str())
                            == Some(identifier)
                        || meta.nodename.as_deref() == Some(identifier)
                        || meta.serial.as_deref() == Some(identifier)
                        || meta.id.as_deref() == Some(identifier)
                    {
                        if crate::sys::is_driver_running(meta.pid) {
                            return Some((path, meta));
                        }
                        if fallback_dead.is_none() {
                            fallback_dead = Some((path, meta));
                        }
                    }
                }
            }
        }
    }
    fallback_dead
}

/// Length of a hexadecimal target/connection ID string.
pub const TARGET_ID_HEX_LEN: usize = 16;

/// Filename prefix shared by all UART metadata and socket files (`ffx_uart_<target_id>.<ext>`).
pub const UART_FILE_PREFIX: &str = "ffx_uart_";

/// Returns `true` if `s` has the shape of a target ID: exactly [`TARGET_ID_HEX_LEN`]
/// hexadecimal characters (in either case).
pub fn is_target_id(s: &str) -> bool {
    s.len() == TARGET_ID_HEX_LEN && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Resolves a target specifier to an active connection metadata file and record.
///
/// Because users and automation can refer to UART connections in different ways,
/// this function attempts resolution in order of precedence:
/// 1. 16-character hexadecimal connection ID (direct O(1) lookup; handles corrupt metadata).
/// 2. Direct device path containing a slash (computes deterministic target ID from path, e.g. "/dev/ttyUSB0").
/// 3. Single-pass linear scan of active metadata records (resolving symlinks, device filenames,
///    nodenames, or serial numbers).
///
/// # Arguments
///
/// * `context` - Environment context used to query shared directories.
/// * `target` - Target specifier string (hex ID, device path, nodename, or serial).
///
/// # Returns
///
/// Returns `Ok(Some((metadata_path, metadata)))` if an active connection matching the specifier
/// is found, `Ok(None)` if no matching active connection exists, or an error if accessing
/// filesystem paths fails unexpectedly.
pub fn find_active_connection(
    context: &EnvironmentContext,
    target: &str,
) -> std::result::Result<Option<(PathBuf, ConnectionMetadata)>, fho::Error> {
    let shared_path = get_shared_uart_path(context)?;
    if !shared_path.exists() {
        return Ok(None);
    }

    let is_target_id =
        target.len() == TARGET_ID_HEX_LEN && target.chars().all(|c| c.is_ascii_hexdigit());
    if is_target_id {
        let meta_path = shared_path.join(format!("ffx_uart_{target}.json"));
        if let Some(meta) = load_metadata_from_path(&meta_path, target, Some(target.to_string())) {
            return Ok(Some((meta_path, meta)));
        }
    }

    if target.contains('/') {
        let target_id = get_target_id(target);
        let meta_path = shared_path.join(format!("ffx_uart_{target_id}.json"));
        if let Some(meta) = load_metadata_from_path(&meta_path, target, Some(target_id)) {
            return Ok(Some((meta_path, meta)));
        }
    } else if let Ok(resolved_path) = uart_driver_api::parse_target_endpoint(target, context) {
        let resolved_str = resolved_path.to_string_lossy();
        let target_id = get_target_id(&resolved_str);
        let meta_path = shared_path.join(format!("ffx_uart_{target_id}.json"));
        if let Some(meta) = load_metadata_from_path(&meta_path, &resolved_str, Some(target_id)) {
            return Ok(Some((meta_path, meta)));
        }
    }

    Ok(scan_for_metadata_by_identifier(&shared_path, target))
}

/// Deletes the metadata file associated with the given target if it exists.
///
/// # Errors
/// Returns an error if deleting the file fails.
pub fn delete_metadata(
    context: &EnvironmentContext,
    target: &str,
) -> std::result::Result<(), fho::Error> {
    let path = get_metadata_path(context, target)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(fho::Error::IoError(e)),
    }
}

/// Updates only the connection status in the metadata file for a target.
///
/// # Errors
/// Returns an error if the metadata file cannot be read, updated, or written.
pub fn update_metadata_status(
    context: &EnvironmentContext,
    target: &str,
    status: ConnectionStatus,
) -> std::result::Result<(), fho::Error> {
    let path = get_metadata_path(context, target)?;
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut metadata: ConnectionMetadata =
        serde_json::from_reader(file).map_err(|e| fho::bug!("{e}"))?;
    metadata.status = status;
    if metadata.status != ConnectionStatus::Connected {
        metadata.nodename = None;
        metadata.serial = None;
    }
    atomic_write_json(&path, &metadata)
}

/// Returns true if the target string looks like a UART target (filesystem path or device node).
pub fn is_uart_target(target: &str) -> bool {
    target.starts_with("uart:") || target.starts_with('/') || target.starts_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[fuchsia::test]
    fn test_canonicalize_target() {
        assert_eq!(canonicalize_target("/dev/ttyUSB0/"), "/dev/ttyUSB0");
        assert_eq!(canonicalize_target("/dev/ttyUSB0///"), "/dev/ttyUSB0");
        assert_eq!(canonicalize_target("/"), "/");
        assert_eq!(canonicalize_target(""), "");
    }

    #[fuchsia::test]
    fn test_get_target_id() {
        let id1 = get_target_id("/dev/ttyUSB0");
        assert_eq!(id1.len(), 16);
        let id2 = get_target_id("/dev/ttyUSB0/");
        assert_eq!(id1, id2);
    }

    #[fuchsia::test]
    fn test_is_uart_target() {
        assert!(is_uart_target("uart:/dev/ttyUSB0"));
        assert!(is_uart_target("/dev/ttyUSB0"));
        assert!(is_uart_target("./ttyUSB0"));
        assert!(!is_uart_target("tcp:127.0.0.1:8080"));
        assert!(!is_uart_target("127.0.0.1:8080"));
        assert!(!is_uart_target("fe80::1"));
        assert!(!is_uart_target("some-device-name"));
    }

    #[fuchsia::test]
    fn test_metadata_roundtrip() {
        let temp = tempdir().unwrap();
        let env = ffx_config::test_env()
            .runtime_config("shared_data", temp.path().to_str().unwrap())
            .build()
            .expect("test env");

        let target = "/dev/ttyUSB0";
        let socket_path = get_socket_path(&env.context, target).unwrap();

        // Write metadata (pass trailing slashes to verify stored target is canonicalized)
        write_metadata(
            &env.context,
            &socket_path,
            "/dev/ttyUSB0///",
            12345,
            NonZeroU32::new(115200),
        )
        .unwrap();

        // Read metadata
        let meta = read_metadata(&env.context, target).unwrap().expect("metadata exists");
        assert_eq!(meta.pid, 12345);
        assert_eq!(meta.target, target);
        assert_eq!(meta.baud, NonZeroU32::new(115200));
        assert_eq!(meta.status, ConnectionStatus::Connecting);

        // Update status
        update_metadata_status(&env.context, target, ConnectionStatus::Connected).unwrap();
        let meta = read_metadata(&env.context, target).unwrap().expect("metadata exists");
        assert_eq!(meta.status, ConnectionStatus::Connected);

        // Find metadata by target
        let found = find_metadata_by_target(&env.context, target).unwrap();
        assert!(found.is_some());
        let (found_path, found_meta) = found.unwrap();
        assert_eq!(found_meta.pid, 12345);
        assert!(found_path.exists());

        // Delete metadata
        delete_metadata(&env.context, target).unwrap();
        assert!(read_metadata(&env.context, target).unwrap().is_none());
    }

    #[fuchsia::test]
    fn test_scan_for_metadata_by_identifier() {
        let temp = tempdir().unwrap();
        let shared_path = temp.path().join("ffx_uart");
        fs::create_dir_all(&shared_path).unwrap();

        let meta = ConnectionMetadata {
            pid: 4321,
            target: "/dev/ttyUSB1".to_string(),
            status: ConnectionStatus::Connected,
            id: Some("0123456789abcdef".to_string()),
            baud: NonZeroU32::new(115200),
            protocol: UartProtocol::ResendSP,
            log_level: None,
            nodename: Some("fuchsia-node-1".to_string()),
            serial: Some("SN-123456".to_string()),
        };
        let file_path = shared_path.join("ffx_uart_0123456789abcdef.json");
        fs::write(&file_path, serde_json::to_string(&meta).unwrap()).unwrap();

        // Search by nodename
        let by_nodename = scan_for_metadata_by_identifier(&shared_path, "fuchsia-node-1");
        assert!(by_nodename.is_some());
        assert_eq!(by_nodename.unwrap().1.pid, 4321);

        // Search by serial
        let by_serial = scan_for_metadata_by_identifier(&shared_path, "SN-123456");
        assert!(by_serial.is_some());
        assert_eq!(by_serial.unwrap().1.pid, 4321);

        // Search by device base filename
        let by_dev = scan_for_metadata_by_identifier(&shared_path, "ttyUSB1");
        assert!(by_dev.is_some());
        assert_eq!(by_dev.unwrap().1.pid, 4321);

        // Search by target path
        let by_path = scan_for_metadata_by_identifier(&shared_path, "/dev/ttyUSB1");
        assert!(by_path.is_some());
        assert_eq!(by_path.unwrap().1.pid, 4321);

        // Search by connection ID
        let by_id = scan_for_metadata_by_identifier(&shared_path, "0123456789abcdef");
        assert!(by_id.is_some());
        assert_eq!(by_id.unwrap().1.pid, 4321);

        // Not found
        assert!(scan_for_metadata_by_identifier(&shared_path, "unknown").is_none());

        // Prioritize active running daemon over dead metadata file with shared identifier
        let mut live_child = std::process::Command::new("python3")
            .args(["-c", "import time; time.sleep(10)"])
            .spawn()
            .expect("spawn live mock process");
        let live_pid = live_child.id();

        let dead_meta = ConnectionMetadata {
            pid: 0,
            target: "/dev/ttyUSB2".to_string(),
            status: ConnectionStatus::Connected,
            id: Some("1111222233334444".to_string()),
            baud: NonZeroU32::new(115200),
            protocol: UartProtocol::ResendSP,
            log_level: None,
            nodename: Some("fuchsia-duplicate".to_string()),
            serial: Some("SN-duplicate".to_string()),
        };
        let dead_path = shared_path.join("ffx_uart_1111222233334444.json");
        fs::write(&dead_path, serde_json::to_string(&dead_meta).unwrap()).unwrap();

        let live_meta = ConnectionMetadata {
            pid: live_pid,
            target: "/dev/ttyUSB2".to_string(),
            status: ConnectionStatus::Connected,
            id: Some("5555666677778888".to_string()),
            baud: NonZeroU32::new(115200),
            protocol: UartProtocol::ResendSP,
            log_level: None,
            nodename: Some("fuchsia-duplicate".to_string()),
            serial: Some("SN-duplicate".to_string()),
        };
        let live_path = shared_path.join("ffx_uart_5555666677778888.json");
        fs::write(&live_path, serde_json::to_string(&live_meta).unwrap()).unwrap();

        let by_dup_node = scan_for_metadata_by_identifier(&shared_path, "fuchsia-duplicate");
        assert!(by_dup_node.is_some());
        assert_eq!(by_dup_node.unwrap().1.pid, live_pid);

        let by_dup_serial = scan_for_metadata_by_identifier(&shared_path, "SN-duplicate");
        assert!(by_dup_serial.is_some());
        assert_eq!(by_dup_serial.unwrap().1.pid, live_pid);

        let _ = live_child.kill();
        let _ = live_child.wait();
    }

    #[fuchsia::test]
    fn test_find_active_connection() {
        let temp = tempdir().unwrap();
        let env = ffx_config::test_env()
            .runtime_config("shared_data", temp.path().to_str().unwrap())
            .build()
            .expect("test env");
        let shared_path = temp.path().join("ffx_uart");
        fs::create_dir_all(&shared_path).unwrap();

        let target_id = "0123456789abcdef";
        let target_path = "/dev/ttyUSB1";
        let meta = ConnectionMetadata {
            pid: 4321,
            target: target_path.to_string(),
            status: ConnectionStatus::Connected,
            id: Some(target_id.to_string()),
            baud: NonZeroU32::new(115200),
            protocol: UartProtocol::ResendSP,
            log_level: None,
            nodename: Some("fuchsia-node-1".to_string()),
            serial: Some("SN-123456".to_string()),
        };
        let file_path = shared_path.join(format!("ffx_uart_{target_id}.json"));
        fs::write(&file_path, serde_json::to_string(&meta).unwrap()).unwrap();

        // 1. Resolve by 16-hex connection ID
        let found_by_id = find_active_connection(&env.context, target_id).unwrap();
        assert!(found_by_id.is_some());
        let (path, m) = found_by_id.unwrap();
        assert_eq!(path, file_path);
        assert_eq!(m.pid, 4321);

        // 2. Resolve by target path
        let found_by_path = find_active_connection(&env.context, target_path).unwrap();
        assert!(found_by_path.is_some());
        assert_eq!(found_by_path.unwrap().1.pid, 4321);

        // 3. Resolve by nodename
        let found_by_node = find_active_connection(&env.context, "fuchsia-node-1").unwrap();
        assert!(found_by_node.is_some());
        assert_eq!(found_by_node.unwrap().1.pid, 4321);

        // 4. Resolve by serial
        let found_by_serial = find_active_connection(&env.context, "SN-123456").unwrap();
        assert!(found_by_serial.is_some());
        assert_eq!(found_by_serial.unwrap().1.pid, 4321);

        // 5. Resolve corrupt metadata by 16-hex ID
        let corrupt_id = "fedcba9876543210";
        let corrupt_path = shared_path.join(format!("ffx_uart_{corrupt_id}.json"));
        fs::write(&corrupt_path, b"invalid json content").unwrap();
        let found_corrupt = find_active_connection(&env.context, corrupt_id).unwrap();
        assert!(found_corrupt.is_some());
        let (c_path, c_meta) = found_corrupt.unwrap();
        assert_eq!(c_path, corrupt_path);
        assert_eq!(c_meta.pid, 0);

        // 6. Resolve by 16-hex serial number falling through to scan
        let hex_serial = "abcdef0123456789";
        let serial_meta = ConnectionMetadata {
            pid: 4321,
            target: "/dev/ttyUSB2".to_string(),
            status: ConnectionStatus::Connected,
            id: Some("1111222233334444".to_string()),
            baud: NonZeroU32::new(115200),
            protocol: UartProtocol::ResendSP,
            log_level: None,
            nodename: None,
            serial: Some(hex_serial.to_string()),
        };
        let serial_file_path = shared_path.join("ffx_uart_1111222233334444.json");
        fs::write(&serial_file_path, serde_json::to_string(&serial_meta).unwrap()).unwrap();
        let found_by_hex_serial = find_active_connection(&env.context, hex_serial).unwrap();
        assert!(found_by_hex_serial.is_some());
        assert_eq!(found_by_hex_serial.unwrap().0, serial_file_path);

        // 7. Unknown target
        assert!(find_active_connection(&env.context, "non-existent-device").unwrap().is_none());
    }
}
