// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Common API definitions, schema data structures, and socket path resolution utilities
//! for the `ffx` UART transport driver and tooling.

/// Error types emitted during UART driver connection establishment and lifecycle.
pub mod errors;
pub use errors::*;

pub type Result<T> = std::result::Result<T, ConnectionError>;

use emulator_instance::EmulatorInstanceInfo as _;
use ffx_config::EnvironmentContext;
use sha2::{Digest, Sha256};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

/// Standard file extension for the driver's UNIX domain data socket.
pub const UNIX_SOCKET_EXTENSION: &str = "sock";

/// Standard file extension for companion connection metadata JSON files.
pub const METADATA_FILE_EXTENSION: &str = "json";
/// Standard file extension for the driver's companion UNIX control socket.
pub const CONTROL_SOCKET_EXTENSION: &str = "control";

/// Standard shared data subdirectory for ffx UART driver sockets, metadata, and logs.
pub const UART_SHARED_SUBDIR: &str = "ffx_uart";

/// Standard log file prefix for driver daemon logs.
pub const LOG_FILE_PREFIX: &str = "ffx_uart";

/// Standard log file extension.
pub const LOG_FILE_EXTENSION: &str = "log";

/// Resolves the UNIX control socket path corresponding to a driver's client socket path.
pub fn get_control_socket_path(socket_path: &Path) -> PathBuf {
    socket_path.with_extension(CONTROL_SOCKET_EXTENSION)
}

/// Resolves the companion metadata file path corresponding to a driver's client socket path.
pub fn get_metadata_path(socket_path: &Path) -> PathBuf {
    socket_path.with_extension(METADATA_FILE_EXTENSION)
}

/// Resolves the client UNIX domain socket path corresponding to a companion metadata file path.
pub fn get_client_socket_path(meta_path: &Path) -> PathBuf {
    meta_path.with_extension(UNIX_SOCKET_EXTENSION)
}

/// Computes a unique 64-bit log ID from a driver's socket path.
pub fn get_log_id_from_socket_path(socket_path: &Path) -> u64 {
    let absolute = if socket_path.is_relative() {
        std::env::current_dir()
            .map(|cwd| cwd.join(socket_path))
            .unwrap_or_else(|_| socket_path.to_path_buf())
    } else {
        socket_path.to_path_buf()
    };
    let cleaned = clean_path(&absolute);
    let path_sha2 = Sha256::digest(cleaned.as_os_str().as_encoded_bytes());
    u64::from_be_bytes(path_sha2[..8].try_into().expect("sha256 digest is at least 32 bytes"))
}

/// Formats a driver log file path for the given rotation index.
pub fn get_driver_log_file_path(log_dir: &Path, log_id: u64, rotation: usize) -> PathBuf {
    log_dir.join(format!("{LOG_FILE_PREFIX}.{log_id:016x}.{rotation}.{LOG_FILE_EXTENSION}"))
}

fn validate_and_resolve_socket_path(path: PathBuf) -> Result<PathBuf> {
    let absolute_path = if path.is_relative() {
        std::env::current_dir().map(|cwd| cwd.join(&path)).unwrap_or_else(|_| path.clone())
    } else {
        path
    };

    let resolved_path = if let Ok(abs) = std::fs::canonicalize(&absolute_path) {
        abs
    } else if let Some(parent) = absolute_path.parent() {
        if let Ok(parent_abs) = std::fs::canonicalize(parent) {
            if let Some(file_name) = absolute_path.file_name() {
                parent_abs.join(file_name)
            } else {
                absolute_path
            }
        } else {
            absolute_path
        }
    } else {
        absolute_path
    };

    let path_len = resolved_path.to_string_lossy().as_bytes().len();
    let max_len = if cfg!(target_os = "macos") { 104 } else { 108 };
    if path_len > max_len {
        return Err(ConnectionError::SocketPathTooLong {
            path: resolved_path.display().to_string(),
            len: path_len,
            max: max_len,
        });
    }
    Ok(resolved_path)
}

/// Normalizes a filesystem path by stripping redundant current directory (`.`) components
/// and resolving parent directory (`..`) components without accessing the filesystem.
pub fn clean_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            c => {
                clean.push(c.as_os_str());
            }
        }
    }
    clean
}

/// Computes a deterministic 16-character hexadecimal target identifier from a target path.
///
/// This identifier is used for UNIX domain socket and companion metadata filenames
/// (e.g., `ffx_uart_<id>.sock` and `ffx_uart_<id>.json`).
///
/// Hashing the target path guarantees:
/// 1. Bounded socket path lengths: UNIX domain sockets (`sockaddr_un`) have a hard
///    OS-level limit (108 bytes on Linux, 104 bytes on macOS). Hashing guarantees
///    a constant 29-byte filename regardless of target path depth or length.
/// 2. Filesystem character safety: Target paths contain slashes and colons that cannot
///    appear in flat filenames.
/// 3. Deterministic pairing: Any client can derive the exact socket and metadata paths
///    without consulting a centralized daemon.
pub fn get_target_id_from_target_path(target_path: &Path) -> String {
    let absolute = if target_path.is_relative() {
        std::env::current_dir()
            .map(|cwd| cwd.join(target_path))
            .unwrap_or_else(|_| target_path.to_path_buf())
    } else {
        target_path.to_path_buf()
    };
    let resolved_target = clean_path(&absolute);
    let target_hash = Sha256::digest(resolved_target.to_string_lossy().as_bytes());
    let hex_id = target_hash.iter().map(|b| format!("{b:02x}")).collect::<String>();
    hex_id[..16].to_string()
}

/// Resolves the UNIX domain socket path corresponding to a target path within the
/// configured `shared_data` directory.
///
/// Ensures the resulting socket path adheres to OS-specific path length limits
/// (108 bytes on Linux, 104 bytes on macOS).
///
/// # Errors
/// Returns an error if the shared data path cannot be retrieved from `context`
/// or if the resolved socket path exceeds OS limits.
pub fn get_socket_path_from_target_path(
    target_path: &Path,
    context: &EnvironmentContext,
) -> Result<PathBuf> {
    let mut path = match context.get::<PathBuf, _>("shared_data") {
        Ok(p) => p,
        Err(_) => context
            .get_shared_data_path()
            .map_err(|e| ConnectionError::SharedDataError { error: e.to_string() })?,
    };
    path.push(UART_SHARED_SUBDIR);
    let hex_id = get_target_id_from_target_path(target_path);
    path.push(format!("ffx_uart_{hex_id}.{UNIX_SOCKET_EXTENSION}"));

    validate_and_resolve_socket_path(path)
}

/// Resolves an emulator instance name to its configured serial UNIX domain socket path.
///
/// # Errors
/// Returns an error if the emulator instance directory cannot be retrieved, if reading the
/// emulator configuration fails, or if no emulator instance with `name` exists on disk.
pub fn get_emu_uart_transport_path(name: &str, context: &EnvironmentContext) -> Result<PathBuf> {
    let instance_dir: PathBuf = context
        .get(ffx_config::keys::EMU_INSTANCE_ROOT_DIR)
        .map_err(|e| ConnectionError::raw(format!("Failed to get emu.instance_dir: {e}")))?;
    let emu_instances = emulator_instance::EmulatorInstances::new(instance_dir);
    let dir = emu_instances
        .get_instance_dir(name, false)
        .map_err(|e| ConnectionError::raw(format!("Failed to get instance dir for {name}: {e}")))?;
    match emulator_instance::read_from_disk(&dir).map_err(|e| {
        ConnectionError::raw(format!("Failed to read emulator instance {name}: {e}"))
    })? {
        emulator_instance::EngineOption::DoesExist(instance_data) => {
            Ok(instance_data.get_emulator_configuration().runtime.get_serial_socket_path())
        }
        emulator_instance::EngineOption::DoesNotExist(_) => {
            Err(ConnectionError::TargetNotRecognized { target: name.to_string() })
        }
    }
}

/// Parses a user-supplied target endpoint string into an absolute target filesystem path.
///
/// Accepts:
/// - Absolute device path starting with `/` (e.g. `/dev/ttyUSB0`)
/// - Emulator instance name (e.g. `fuchsia-emulator`)
///
/// # Errors
/// Returns an error if the target string is neither an absolute path nor a recognized
/// emulator instance name.
pub fn parse_target_endpoint(target: &str, context: &EnvironmentContext) -> Result<PathBuf> {
    // 1. Absolute Path
    if target.starts_with('/') {
        return Ok(PathBuf::from(target));
    }

    // 2. Emulator instance name
    if let Ok(path) = get_emu_uart_transport_path(target, context) {
        return Ok(path);
    }

    // Fallback:
    Err(ConnectionError::TargetNotRecognized { target: target.to_string() })
}

/// Converts a UNIX domain socket path back into a human-readable target device path,
/// first inspecting companion JSON metadata, then querying `/dev` serial ports, and
/// finally falling back to the raw socket path string.
pub fn socket_path_to_target_path(socket_path: &Path, context: &EnvironmentContext) -> String {
    // Try to read metadata JSON to get the real target path.
    let json_path = get_metadata_path(socket_path);
    if let Ok(content) = std::fs::read_to_string(&json_path) {
        if let Ok(metadata) = serde_json::from_str::<ConnectionMetadata>(&content) {
            return metadata.target;
        }
    }

    // Try to match ttyUSB ports
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for entry in entries {
            if let Ok(entry) = entry {
                let path = entry.path();
                if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                    if filename.starts_with("ttyUSB")
                        || filename.starts_with("ttyS")
                        || filename.starts_with("ttyACM")
                        || filename.starts_with("tty.usbserial")
                    {
                        if let Ok(expected_socket) =
                            get_socket_path_from_target_path(&path, context)
                        {
                            if expected_socket == socket_path {
                                return path.to_string_lossy().into_owned();
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback
    socket_path.to_string_lossy().into_owned()
}

/// Current operational status of a UART target driver connection.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum ConnectionStatus {
    /// Driver process is currently opening the port and negotiating protocol with the target.
    Connecting,
    /// Connection and protocol negotiation succeeded; channel is open and ready.
    Connected,
    /// Connection failed with the enclosed error details.
    Error(ConnectionError),
}

impl std::fmt::Display for ConnectionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionStatus::Connecting => write!(f, "Connecting"),
            ConnectionStatus::Connected => write!(f, "Connected"),
            ConnectionStatus::Error(e) => write!(f, "Error: {}", e),
        }
    }
}

/// Framing protocol variant active on the UART transport link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UartProtocol {
    /// ResendSP sliding-window Go-Back-N protocol with CRC-32 integrity.
    ResendSP,
    /// Test loopback protocol used in unit and integration test fixtures.
    TestProtocol,
    /// Protocol unrecognized or not yet negotiated.
    Unknown,
}

impl std::fmt::Display for UartProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UartProtocol::ResendSP => write!(f, "ResendSP"),
            UartProtocol::TestProtocol => write!(f, "TestProtocol"),
            UartProtocol::Unknown => write!(f, "Unknown"),
        }
    }
}

impl std::str::FromStr for UartProtocol {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "resend" | "resendsp" => Ok(UartProtocol::ResendSP),
            "testprotocol" => Ok(UartProtocol::TestProtocol),
            "unknown" => Ok(UartProtocol::Unknown),
            other => Err(format!("Unknown protocol: {other}")),
        }
    }
}

/// Persisted connection metadata stored in companion JSON files for an active UART driver.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConnectionMetadata {
    /// Process ID (PID) of the active driver daemon.
    pub pid: u32,
    /// Target port specification (e.g. `/dev/ttyUSB0`).
    pub target: String,
    /// Current connection status.
    pub status: ConnectionStatus,
    /// 16-character hexadecimal target identifier hash.
    pub id: Option<String>,
    /// Configured baud rate (for TTY serial ports).
    pub baud: Option<NonZeroU32>,
    /// Active framing protocol.
    pub protocol: UartProtocol,
    /// Configured log level of the driver process.
    pub log_level: Option<String>,
    /// Discovered target nodename, if available and connected.
    pub nodename: Option<String>,
    /// Discovered target serial number, if available and connected.
    pub serial: Option<String>,
}

/// Updates the metadata JSON file with discovered target identity (`nodename` and `serial`),
/// atomically replacing the file via a temporary file.
///
/// # Errors
/// Returns an error if reading existing metadata, serializing updated JSON, or atomic file replacement fails.
pub fn update_metadata_identity(
    socket_path: &Path,
    nodename: Option<String>,
    serial: Option<String>,
) -> Result<()> {
    let json_path = get_metadata_path(socket_path);
    let content =
        std::fs::read_to_string(&json_path).map_err(|e| ConnectionError::MetadataError {
            path: json_path.display().to_string(),
            error: e.to_string(),
        })?;
    let mut meta: ConnectionMetadata =
        serde_json::from_str(&content).map_err(|e| ConnectionError::MetadataError {
            path: json_path.display().to_string(),
            error: e.to_string(),
        })?;
    meta.nodename = nodename;
    meta.serial = serial;
    let new_content = serde_json::to_string(&meta).map_err(|e| ConnectionError::MetadataError {
        path: json_path.display().to_string(),
        error: e.to_string(),
    })?;
    let rand_suffix: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let temp_path = json_path.with_extension(format!("{}_{}.tmp", std::process::id(), rand_suffix));
    std::fs::write(&temp_path, new_content.as_bytes()).map_err(|e| {
        ConnectionError::MetadataError {
            path: temp_path.display().to_string(),
            error: e.to_string(),
        }
    })?;
    std::fs::rename(&temp_path, &json_path).map_err(|e| ConnectionError::MetadataError {
        path: json_path.display().to_string(),
        error: e.to_string(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_target_path_mapping() {
        let env = ffx_config::test_env().build().expect("test env");
        let context = &env.context;

        // Fallback for random socket path (returns absolute path as-is)
        let random_socket = PathBuf::from("/tmp/ffx_uart_random.sock");
        let target_str = socket_path_to_target_path(&random_socket, context);
        assert_eq!(target_str, "/tmp/ffx_uart_random.sock");
    }

    #[test]
    fn test_socket_path_length_validation() {
        // Test short path succeeds
        let short_path = format!("/{}", "a".repeat(9));
        let env = ffx_config::test_env()
            .runtime_config("shared_data", short_path.as_str())
            .build()
            .expect("test env");
        let context = &env.context;

        let target_path = PathBuf::from("/dev/ttyUSB0");
        let res = get_socket_path_from_target_path(&target_path, context);
        assert!(res.is_ok());

        // Test long path fails
        let long_path = format!("/{}", "a".repeat(99));
        let env = ffx_config::test_env()
            .runtime_config("shared_data", long_path.as_str())
            .build()
            .expect("test env");
        let context = &env.context;

        let res = get_socket_path_from_target_path(&target_path, context);
        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(err_msg.contains("UNIX socket path exceeds limit"), "Err message was: {}", err_msg);
    }

    #[test]
    fn test_socket_path_length_validation_boundaries() {
        let max_len = if cfg!(target_os = "macos") { 104 } else { 108 };
        let suffix_len = 40;
        let exact_fit_len = max_len - suffix_len;
        let exceeds_len = exact_fit_len + 1;

        let target_path = PathBuf::from("/dev/ttyUSB0");

        // Exact fit length succeeds
        let fit_path = format!("/{}", "a".repeat(exact_fit_len - 1));
        let env = ffx_config::test_env()
            .runtime_config("shared_data", fit_path.as_str())
            .build()
            .expect("test env");
        let res = get_socket_path_from_target_path(&target_path, &env.context);
        assert!(
            res.is_ok(),
            "Expected OK for exact fit path length {}, got: {:?}",
            exact_fit_len,
            res
        );

        // One byte over fails
        let long_path = format!("/{}", "a".repeat(exceeds_len - 1));
        let env = ffx_config::test_env()
            .runtime_config("shared_data", long_path.as_str())
            .build()
            .expect("test env");
        let res = get_socket_path_from_target_path(&target_path, &env.context);
        assert!(res.is_err(), "Expected error for exceeding path length {}", exceeds_len);
    }

    #[test]
    fn test_update_metadata_identity() {
        let temp = tempdir().unwrap();
        let socket_path = temp.path().join("ffx_uart_test.sock");
        let json_path = temp.path().join("ffx_uart_test.json");

        let meta = ConnectionMetadata {
            pid: 1234,
            target: "/dev/ttyUSB0".to_string(),
            status: ConnectionStatus::Connected,
            id: Some("testid".to_string()),
            baud: NonZeroU32::new(1000000),
            protocol: UartProtocol::ResendSP,
            log_level: None,
            nodename: None,
            serial: None,
        };
        std::fs::write(&json_path, serde_json::to_string(&meta).unwrap()).unwrap();

        update_metadata_identity(
            &socket_path,
            Some("iris-target".to_string()),
            Some("SER12345".to_string()),
        )
        .unwrap();

        let updated_content = std::fs::read_to_string(&json_path).unwrap();
        let updated_meta: ConnectionMetadata = serde_json::from_str(&updated_content).unwrap();
        assert_eq!(updated_meta.nodename.as_deref(), Some("iris-target"));
        assert_eq!(updated_meta.serial.as_deref(), Some("SER12345"));
        assert_eq!(updated_meta.baud, NonZeroU32::new(1000000));
    }

    #[test]
    fn test_get_target_id_from_target_path() {
        let p1 = Path::new("/dev/ttyUSB0");
        let p2 = Path::new("/dev/ttyUSB0/");
        let id1 = get_target_id_from_target_path(p1);
        let id2 = get_target_id_from_target_path(p2);
        assert_eq!(id1.len(), 16);
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_connection_error_serde_roundtrip() {
        let err1 = ConnectionError::PathNotFound { path: "/dev/ttyUSB0".to_string() };
        let json1 = serde_json::to_string(&err1).unwrap();
        assert_eq!(json1, r#"{"type":"PathNotFound","path":"/dev/ttyUSB0"}"#);
        let roundtrip1: ConnectionError = serde_json::from_str(&json1).unwrap();
        assert_eq!(err1, roundtrip1);

        let err2 = ConnectionError::raw("Connection lost");
        let json2 = serde_json::to_string(&err2).unwrap();
        assert_eq!(json2, r#"{"type":"Raw","message":"Connection lost"}"#);
        let roundtrip2: ConnectionError = serde_json::from_str(&json2).unwrap();
        assert_eq!(err2, roundtrip2);
    }

    #[fuchsia::test]
    fn test_driver_log_path_helpers() {
        let sock = Path::new("/tmp/test_socket.sock");
        let log_id = get_log_id_from_socket_path(sock);
        assert_ne!(log_id, 0);

        let log_dir = Path::new("/var/log/uart");
        let log_file = get_driver_log_file_path(log_dir, log_id, 0);
        let expected =
            format!("/var/log/uart/{LOG_FILE_PREFIX}.{log_id:016x}.0.{LOG_FILE_EXTENSION}");
        assert_eq!(log_file.to_string_lossy(), expected);

        let log_rot = get_driver_log_file_path(log_dir, log_id, 2);
        let expected_rot =
            format!("/var/log/uart/{LOG_FILE_PREFIX}.{log_id:016x}.2.{LOG_FILE_EXTENSION}");
        assert_eq!(log_rot.to_string_lossy(), expected_rot);
    }

    #[test]
    fn test_parse_target_endpoint_emulator() {
        let temp = tempdir().unwrap();
        let env = ffx_config::test_env()
            .runtime_config(ffx_config::keys::EMU_INSTANCE_ROOT_DIR, temp.path().to_str().unwrap())
            .build()
            .expect("test env");

        let emu_name = "test-emu-uart";
        let emu_dir = temp.path().join(emu_name);
        std::fs::create_dir_all(&emu_dir).unwrap();

        let expected_sock = temp.path().join("femu_s_abcd");
        let mut instance_data = emulator_instance::EmulatorInstanceData::new_with_state(
            emu_name,
            emulator_instance::EngineState::Running,
        );
        instance_data.get_emulator_configuration_mut().runtime.instance_directory = emu_dir.clone();
        instance_data.get_emulator_configuration_mut().runtime.serial_socket =
            expected_sock.clone();
        emulator_instance::write_to_disk(&instance_data, &emu_dir).unwrap();

        let resolved = parse_target_endpoint(emu_name, &env.context).unwrap();
        assert_eq!(resolved, expected_sock);

        let missing = parse_target_endpoint("non-existent-emu", &env.context);
        assert!(matches!(missing, Err(ConnectionError::TargetNotRecognized { .. })));
    }
}
