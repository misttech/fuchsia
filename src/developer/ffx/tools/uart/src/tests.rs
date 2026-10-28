// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Hermetic mock driver test harness and unit test suite for `ffx_tool_uart`.
//!
//! This module verifies host CLI behaviors without requiring physical hardware or running
//! system background daemons. It simulates driver lifecycles, control sockets, and metadata
//! persistence within isolated temporary test environments.

use argh::FromArgs;

use super::*;
use crate::args::{
    ConnectCommand, DisconnectCommand, DoctorCommand, ListCommand, ProbeCommand, StatusCommand,
};
use ffx_writer::TestBuffers;
use fho::FfxMain;
use fuchsia_async as _;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use sha2::Digest;
use std::fs;
use std::io::Write as _;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use uart_driver_api::ConnectionError;

const MOCK_DRIVER_SCRIPT: &str = include_str!("../test_data/mock_driver.py");

fn write_mock_driver(temp_path: &Path, filename: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script_path = temp_path.join(filename);
    fs::write(&script_path, MOCK_DRIVER_SCRIPT).unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();
    script_path
}

fn create_test_env_with_log_levels(
    test_name: &str,
    runtime_log: Option<&str>,
    user_log: Option<&str>,
) -> (ffx_config::TestEnv, tempfile::TempDir) {
    let temp_dir = tempfile::Builder::new()
        .prefix(&format!("u_{}_{}_", std::process::id(), test_name))
        .tempdir()
        .expect("create temp dir");

    let script_path = write_mock_driver(temp_dir.path(), "mock_driver.py");

    let mut builder = ffx_config::test_env()
        .runtime_config("shared_data", temp_dir.path().to_str().unwrap())
        .runtime_config("sdk.overrides.ffx-uart-driver", script_path.to_str().unwrap());
    if let Some(rl) = runtime_log {
        builder = builder.runtime_config("log.level", rl);
    }
    if let Some(ul) = user_log {
        builder = builder.user_config("log.level", ul);
    }
    let env = builder.build().expect("test env");
    (env, temp_dir)
}

fn create_test_env(test_name: &str) -> (ffx_config::TestEnv, tempfile::TempDir) {
    create_test_env_with_log_levels(test_name, None, None)
}

fn make_connect_cmd(baud: u32, reconnect: bool) -> UartSubCommand {
    UartSubCommand::Connect(ConnectCommand {
        no_retry: false,
        baud: NonZeroU32::new(baud),
        socket: None,
        reconnect,
        protocol: None,
    })
}
async fn run_tool(
    env: &ffx_config::TestEnv,
    target: Option<&str>,
    sub_cmd: UartSubCommand,
) -> std::result::Result<(String, String), fho::Error> {
    run_tool_with_format(env, target, sub_cmd, None).await
}

async fn run_tool_with_format(
    env: &ffx_config::TestEnv,
    target: Option<&str>,
    sub_cmd: UartSubCommand,
    format: Option<ffx_writer::Format>,
) -> std::result::Result<(String, String), fho::Error> {
    let mut context_tool = env.context.clone();
    if let Some(target) = target {
        context_tool.override_target_specifier(&Some(format!("uart:{}", target)));
    }
    let buffers = TestBuffers::default();
    let writer = ffx_writer::SimpleWriter::new_test(&buffers);
    match sub_cmd {
        UartSubCommand::Connect(cmd) => {
            let tool = ConnectTool { cmd, context: context_tool };
            tool.main(writer).await?;
        }
        UartSubCommand::Disconnect(cmd) => {
            let tool = DisconnectTool { cmd, context: context_tool };
            tool.main(writer).await?;
        }
        UartSubCommand::List(cmd) => {
            let tool = ListTool { cmd, context: context_tool };
            let machine_writer = ffx_writer::MachineWriter::new_test(format, &buffers);
            tool.main(machine_writer).await?;
        }
        UartSubCommand::Probe(cmd) => {
            let tool = ProbeTool { cmd, context: context_tool };
            let machine_writer = ffx_writer::VerifiedMachineWriter::new_test(format, &buffers);
            tool.main(machine_writer).await?;
        }
        UartSubCommand::Status(cmd) => {
            let tool = StatusTool { cmd, context: context_tool };
            let machine_writer = ffx_writer::VerifiedMachineWriter::new_test(format, &buffers);
            tool.main(machine_writer).await?;
        }
        UartSubCommand::Doctor(cmd) => {
            let tool = DoctorTool { cmd, context: context_tool };
            tool.main(writer).await?;
        }
    }
    Ok((buffers.stdout.into_string(), buffers.stderr.into_string()))
}

struct DaemonCleanupGuard<'a> {
    env: &'a ffx_config::TestEnv,
    canonical_target: String,
}

impl<'a> DaemonCleanupGuard<'a> {
    fn new(env: &'a ffx_config::TestEnv, target: &str) -> Self {
        let canonical_target = canonicalize_target(target);
        Self { env, canonical_target }
    }
}

impl<'a> Drop for DaemonCleanupGuard<'a> {
    fn drop(&mut self) {
        if let Ok(Some(meta)) = read_metadata(&self.env.context, &self.canonical_target) {
            if is_running(meta.pid) && meta.pid != std::process::id() {
                let nix_pid = Pid::from_raw(meta.pid as i32);
                let _ = kill(nix_pid, nix::sys::signal::Signal::SIGKILL);
            }
        }
    }
}

#[fuchsia::test]
async fn test_connect() {
    let (env, _temp_dir) = create_test_env("test_connect");
    let _guard = DaemonCleanupGuard::new(&env, "/target-connect");

    let sub_cmd = UartSubCommand::Connect(ConnectCommand {
        no_retry: false,
        baud: NonZeroU32::new(115200),
        socket: None,
        reconnect: false,
        protocol: None,
    });

    let (stdout, _) = run_tool(&env, Some("/target-connect"), sub_cmd).await.unwrap();
    assert_eq!("Connect called for /target-connect\n", stdout);

    // Verify metadata was written
    let metadata = read_metadata(&env.context, "/target-connect").unwrap().unwrap();
    assert_eq!(metadata.target, canonicalize_target("/target-connect"));
    assert!(metadata.pid > 0);
}

#[fuchsia::test]
async fn test_disconnect() {
    let (env, _temp_dir) = create_test_env("test_disconnect");
    let _guard = DaemonCleanupGuard::new(&env, "/target-disconnect");

    // First connect to create metadata
    let conn_cmd = UartSubCommand::Connect(ConnectCommand {
        no_retry: false,
        baud: NonZeroU32::new(115200),
        socket: None,
        reconnect: false,
        protocol: None,
    });
    run_tool(&env, Some("/target-disconnect"), conn_cmd).await.unwrap();

    let (stdout, _) = run_tool(
        &env,
        Some("/target-disconnect"),
        UartSubCommand::Disconnect(DisconnectCommand {}),
    )
    .await
    .unwrap();
    assert_eq!("Disconnect called for /target-disconnect\n", stdout);

    // Verify metadata was deleted
    assert!(read_metadata(&env.context, "/target-disconnect").unwrap().is_none());
}

#[fuchsia::test]
fn test_parse_connect_success() {
    let args = ["connect", "--baud", "9600"];
    let cmd = UartCommand::from_args(&["uart"], &args).unwrap();
    assert_eq!(
        cmd.sub_cmd,
        UartSubCommand::Connect(ConnectCommand {
            no_retry: false,
            baud: NonZeroU32::new(9600),
            socket: None,
            reconnect: false,
            protocol: None,
        })
    );
}

#[fuchsia::test]
fn test_parse_connect_default_baud() {
    let args = ["connect"];
    let cmd = UartCommand::from_args(&["uart"], &args).unwrap();
    assert_eq!(
        cmd.sub_cmd,
        UartSubCommand::Connect(ConnectCommand {
            no_retry: false,
            baud: None,
            socket: None,
            reconnect: false,
            protocol: None,
        })
    );
}

#[fuchsia::test]
fn test_parse_connect_invalid_baud() {
    let args = ["connect", "--baud", "abc"];
    let res = UartCommand::from_args(&["uart"], &args);
    assert!(res.is_err());
    let err_msg = res.unwrap_err().output;
    assert!(err_msg.contains("invalid digit found in string"), "Error: {}", err_msg);
}

#[fuchsia::test]
fn test_parse_connect_negative_baud() {
    let args = ["connect", "--baud", "-115200"];
    let res = UartCommand::from_args(&["uart"], &args);
    assert!(res.is_err());
    let err_msg = res.unwrap_err().output;
    assert!(err_msg.contains("invalid digit found in string"), "Error: {}", err_msg);
}

#[fuchsia::test]
fn test_parse_connect_overflow_baud() {
    let args = ["connect", "--baud", "999999999999999999999999999999"];
    let res = UartCommand::from_args(&["uart"], &args);
    assert!(res.is_err());
    let err_msg = res.unwrap_err().output;
    assert!(err_msg.contains("number too large to fit in target type"), "Error: {}", err_msg);
}

#[fuchsia::test]
fn test_parse_connect_zero_baud() {
    let args = ["connect", "--baud", "0"];
    let res = UartCommand::from_args(&["uart"], &args);
    assert!(res.is_err());
    let err_msg = res.unwrap_err().output;
    assert!(err_msg.contains("number would be zero for non-zero type"), "Error: {}", err_msg);
}

#[fuchsia::test]
fn test_parse_disconnect_extra_arg() {
    let args = ["disconnect", "/dev/ttyUSB0"];
    let res = UartCommand::from_args(&["uart"], &args);
    assert!(res.is_err());
    let err_msg = res.unwrap_err().output;
    assert!(err_msg.contains("Unrecognized argument"), "Error: {}", err_msg);
}

#[fuchsia::test]
fn test_parse_list_extra_arg() {
    let args = ["list", "/dev/ttyUSB0"];
    let res = UartCommand::from_args(&["uart"], &args);
    assert!(res.is_err());
    let err_msg = res.unwrap_err().output;
    assert!(err_msg.contains("Unrecognized argument"), "Error: {}", err_msg);
}

#[fuchsia::test]
fn test_target_id_hashing() {
    let target = "my-test-target-hashing";
    let canonical = canonicalize_target(target);
    let mut hasher = sha2::Sha256::new();
    hasher.update(canonical.as_bytes());
    let result = hasher.finalize();
    let expected_hash = hex::encode(&result[..8]);
    assert_eq!(get_target_id(target), expected_hash);
}

#[fuchsia::test]
async fn test_metadata_serialization_deserialization() {
    let (env, _temp_dir) = create_test_env("test_metadata_serialization_deserialization");
    let target = "/test-serial-deserial";
    let _guard = DaemonCleanupGuard::new(&env, target);
    let pid = 12345;
    let socket_path = get_socket_path(&env.context, target).unwrap();

    write_metadata(&env.context, &socket_path, target, pid, NonZeroU32::new(115200)).unwrap();

    let meta = read_metadata(&env.context, target).unwrap().unwrap();
    assert_eq!(meta.pid, pid);
    assert_eq!(meta.target, target);
    assert_eq!(meta.baud, NonZeroU32::new(115200));

    delete_metadata(&env.context, target).unwrap();
    assert!(read_metadata(&env.context, target).unwrap().is_none());
}

#[fuchsia::test]
async fn test_liveness_check() {
    let current_pid = std::process::id();
    assert!(is_running(current_pid));

    assert!(!is_running(0));
    assert!(!is_running(u32::MAX));
}

#[fuchsia::test]
async fn test_corrupt_metadata_recovery() {
    let (env, _temp_dir) = create_test_env("test_corrupt_metadata_recovery");
    let target = "/test-corrupt-target";
    let _guard = DaemonCleanupGuard::new(&env, target);
    let path = get_metadata_path(&env.context, target).unwrap();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    // Write corrupt JSON content
    fs::write(&path, b"invalid json content").unwrap();

    let meta = read_metadata(&env.context, target).unwrap().unwrap();
    assert_eq!(meta.pid, 0);
    assert_eq!(meta.target, target);
    assert!(!is_running(meta.pid));
}

#[fuchsia::test]
async fn test_connect_handles_corrupt_metadata() {
    let (env, _temp_dir) = create_test_env("test_connect_handles_corrupt_metadata");
    let target = "/test-corrupt-connect-target";
    let _guard = DaemonCleanupGuard::new(&env, target);
    let path = get_metadata_path(&env.context, target).unwrap();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    // Write corrupt JSON content
    fs::write(&path, b"invalid json content").unwrap();

    let sub_cmd = make_connect_cmd(115200, false);

    // This should succeed because the corrupt metadata is treated as not running (pid = 0)
    let (stdout, _) = run_tool(&env, Some(target), sub_cmd).await.unwrap();
    assert_eq!(format!("Connect called for {}\n", target), stdout);

    // Verify metadata was successfully written with valid JSON and our process ID
    let metadata = read_metadata(&env.context, target).unwrap().unwrap();
    assert_eq!(metadata.target, canonicalize_target(target));
    assert!(metadata.pid > 0);
}

#[fuchsia::test]
async fn test_connect_state_transitions() {
    let (env, _temp_dir) = create_test_env("test_connect_state_transitions");
    let target = "/test-transition-target";
    let _guard = DaemonCleanupGuard::new(&env, target);

    // 1. Initial connect -> success
    run_tool(&env, Some(target), make_connect_cmd(115200, false)).await.unwrap();

    // 2. Connect again without reconnect flag -> fail (Already connected)
    let err = run_tool(&env, Some(target), make_connect_cmd(115200, false)).await.unwrap_err();
    assert!(err.to_string().contains("Already connected to target"));

    // 3. Connect again with reconnect flag -> success
    run_tool(&env, Some(target), make_connect_cmd(115200, true)).await.unwrap();

    // 4. Disconnect -> success
    let (stdout, _) =
        run_tool(&env, Some(target), UartSubCommand::Disconnect(DisconnectCommand {}))
            .await
            .unwrap();
    assert_eq!("Disconnect called for /test-transition-target\n", stdout);

    // 5. Disconnect again -> fail (Not connected)
    let err_disc2 = run_tool(&env, Some(target), UartSubCommand::Disconnect(DisconnectCommand {}))
        .await
        .unwrap_err();
    assert!(err_disc2.to_string().contains("Not connected to target"));

    // 6. Connect again -> success
    run_tool(&env, Some(target), make_connect_cmd(115200, false)).await.unwrap();
}

#[fuchsia::test]
async fn test_reconnect_argument_merging() {
    let (env1, _temp_dir) =
        create_test_env_with_log_levels("reconnect_argument_merging", Some("debug"), Some("info"));

    let target = "/test-reconnect-merge-target";
    let _guard = DaemonCleanupGuard::new(&env1, target);

    // Initial connect with baud=115200
    run_tool(&env1, Some(target), make_connect_cmd(115200, false)).await.unwrap();

    let meta = read_metadata(&env1.context, target).unwrap().unwrap();
    assert_eq!(meta.baud, NonZeroU32::new(115200));

    // Reconnect without specifying baud inherits previous baud (115200)
    let recon_cmd = UartSubCommand::Connect(ConnectCommand {
        no_retry: false,
        baud: None,
        socket: None,
        reconnect: true,
        protocol: None,
    });
    run_tool(&env1, Some(target), recon_cmd).await.unwrap();

    let meta2 = read_metadata(&env1.context, target).unwrap().unwrap();
    assert_eq!(meta2.baud, NonZeroU32::new(115200));
}

#[fuchsia::test]
async fn test_disconnect_cleans_up_stale_files() {
    let (env, _temp_dir) = create_test_env("test_disconnect_stale");
    let target = "/test-disconnect-stale-target";
    let _guard = DaemonCleanupGuard::new(&env, target);

    let conn_cmd = make_connect_cmd(115200, false);
    run_tool(&env, Some(target), conn_cmd).await.unwrap();

    let meta_path = get_metadata_path(&env.context, target).unwrap();
    let socket_path = get_socket_path(&env.context, target).unwrap();
    let control_path = uart_driver_api::get_control_socket_path(&socket_path);
    assert!(meta_path.exists());
    assert!(socket_path.exists());
    assert!(control_path.exists());

    run_tool(&env, Some(target), UartSubCommand::Disconnect(DisconnectCommand {})).await.unwrap();

    assert!(!meta_path.exists());
    assert!(!socket_path.exists());
    assert!(!control_path.exists());
}

#[fuchsia::test]
async fn test_disconnect_by_target_id() {
    let (env, _temp_dir) = create_test_env("test_disconnect_target_id");
    let target = "/dev/ttyUSB99";
    let _guard = DaemonCleanupGuard::new(&env, target);

    let conn_cmd = make_connect_cmd(115200, false);
    run_tool(&env, Some(target), conn_cmd).await.unwrap();

    let canonical = canonicalize_target(target);
    let target_id = get_target_id(&canonical);

    // Disconnect using the 16-hex target ID
    let (stdout, _) =
        run_tool(&env, Some(&target_id), UartSubCommand::Disconnect(DisconnectCommand {}))
            .await
            .unwrap();
    assert!(stdout.contains(&format!("Disconnect called for {}", target)));

    assert!(read_metadata(&env.context, target).unwrap().is_none());
}

#[fuchsia::test]
async fn test_disconnect_pattern_matching_tty_usb0() {
    // Verify that disconnecting by device filename (e.g. "ttyUSB0") correctly matches "/dev/ttyUSB0".
    let (env, _temp_dir) = create_test_env("test_disconnect_pattern_matching");
    let target = "/dev/ttyUSB0";
    let _guard = DaemonCleanupGuard::new(&env, target);

    let conn_cmd = make_connect_cmd(1_000_000, false);
    run_tool(&env, Some(target), conn_cmd).await.unwrap();

    // Disconnect using "ttyUSB0" without "/dev/" prefix
    let (stdout, _) =
        run_tool(&env, Some("ttyUSB0"), UartSubCommand::Disconnect(DisconnectCommand {}))
            .await
            .unwrap();
    assert_eq!("Disconnect called for /dev/ttyUSB0\n", stdout);

    assert!(read_metadata(&env.context, target).unwrap().is_none());
}

#[fuchsia::test]
async fn test_terminate_process_kills_child() {
    // Verify that terminate_process cleanly sends SIGTERM/SIGKILL to terminate a child process.
    let mut child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
    let pid = child.id();
    let res = crate::driver::terminate_process(pid).await;
    assert!(res.is_ok());

    let start = std::time::Instant::now();
    let mut exited = false;
    while start.elapsed() < std::time::Duration::from_secs(2) {
        if let Ok(Some(_)) = child.try_wait() {
            exited = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    if !exited {
        let _ = child.kill();
    }
    assert!(exited, "Child process should have exited after terminate_process");
}

#[fuchsia::test]
async fn test_socket_path_length_limit() {
    let (env, _temp_dir) = create_test_env("test_socket_path_len");
    let _guard = DaemonCleanupGuard::new(&env, "placeholder");

    // Construct a socket path exceeding the OS limit (> 108 bytes on Linux, > 104 on macOS)
    let long_socket = format!("/tmp/{}", "a".repeat(120));
    let conn_cmd = UartSubCommand::Connect(ConnectCommand {
        no_retry: false,
        baud: NonZeroU32::new(115200),
        socket: Some(long_socket),
        reconnect: false,
        protocol: None,
    });

    let res = run_tool(&env, Some("/dev/test_long"), conn_cmd).await;
    assert!(res.is_err());
    let err = res.unwrap_err();
    assert!(err.to_string().contains("UNIX socket path exceeds limit"));
}

#[fuchsia::test]
async fn test_list() {
    let (env, _temp_dir) = create_test_env("test_list");
    let _guard = DaemonCleanupGuard::new(&env, "/target-list");
    let tool = ListTool { cmd: ListCommand {}, context: env.context.clone() };
    let buffers = TestBuffers::default();
    let writer = ffx_writer::MachineWriter::new_test(None, &buffers);
    tool.main(writer).await.unwrap();
    assert_eq!("No active UART connections.\n", buffers.stdout.into_string());
}

#[fuchsia::test]
async fn test_list_robustness_missing_metadata() {
    let (env, _temp_dir) = create_test_env("test_list_missing_meta");
    let target = "/target-list-missing-meta";
    let _guard = DaemonCleanupGuard::new(&env, target);

    // First connect to create metadata and socket
    run_tool(&env, Some(target), make_connect_cmd(115200, false)).await.unwrap();

    // Get expected daemon PID from metadata
    let metadata = read_metadata(&env.context, target).unwrap().unwrap();
    let expected_pid = metadata.pid;

    // Delete the metadata JSON file manually to simulate missing metadata
    let metadata_path = get_metadata_path(&env.context, target).unwrap();
    fs::remove_file(&metadata_path).unwrap();

    // Run UartTool::list
    let (list_output, _) =
        run_tool(&env, None, UartSubCommand::List(ListCommand {})).await.unwrap();

    // Verify list output includes the live PID retrieved via peer credentials
    assert!(list_output.contains(&expected_pid.to_string()), "Output: {}", list_output);

    // Manually clean up the daemon process since metadata is deleted and guard won't find it
    let nix_pid = Pid::from_raw(expected_pid as i32);
    let _ = kill(nix_pid, nix::sys::signal::Signal::SIGKILL);
}

fn create_dummy_socket_file(env: &ffx_config::TestEnv, target: &str) -> PathBuf {
    let socket_path = get_socket_path(&env.context, target).unwrap();
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&socket_path, b"").unwrap();
    socket_path
}

fn create_stale_metadata(
    env: &ffx_config::TestEnv,
    target: &str,
    pid: u32,
    baud: Option<NonZeroU32>,
) {
    let socket_path = get_socket_path(&env.context, target).unwrap();
    write_metadata(&env.context, &socket_path, target, pid, baud).unwrap();
}
async fn setup_mixed_state_daemons(
    env: &ffx_config::TestEnv,
    prefix: &str,
) -> (ConnectionMetadata, ConnectionMetadata, String, String) {
    let (t1, t2, t3, t4) = (
        format!("{prefix}-1"),
        format!("{prefix}-2"),
        format!("{prefix}-3"),
        format!("{prefix}-4"),
    );
    run_tool(env, Some(&t1), make_connect_cmd(115200, false)).await.unwrap();
    let meta1 = read_metadata(&env.context, &t1).unwrap().unwrap();

    run_tool(env, Some(&t2), make_connect_cmd(115200, false)).await.unwrap();
    let meta2 = read_metadata(&env.context, &t2).unwrap().unwrap();
    fs::remove_file(&get_metadata_path(&env.context, &t2).unwrap()).unwrap();

    create_dummy_socket_file(env, &t3);
    create_stale_metadata(env, &t4, 999999, NonZeroU32::new(115200));
    (meta1, meta2, t3, t4)
}

#[fuchsia::test]
async fn test_list_mixed_states() {
    let (env, _temp_dir) = create_test_env("test_list_mixed");
    let _main_guard = DaemonCleanupGuard::new(&env, "/target-list-mixed-1");
    let (meta1, meta2, t3, t4) = setup_mixed_state_daemons(&env, "/target-list-mixed").await;

    let (output, _) = run_tool(&env, None, UartSubCommand::List(ListCommand {})).await.unwrap();
    assert!(output.contains(&meta1.pid.to_string()), "Daemon 1 missing in list: {}", output);
    assert!(output.contains(&meta2.pid.to_string()), "Daemon 2 missing in list: {}", output);
    assert!(!output.contains(&t3), "Stale socket 3 should not be listed: {}", output);
    assert!(!output.contains(&t4), "Stale metadata 4 should not be listed: {}", output);

    let _ = kill(Pid::from_raw(meta1.pid as i32), nix::sys::signal::Signal::SIGKILL);
    let _ = kill(Pid::from_raw(meta2.pid as i32), nix::sys::signal::Signal::SIGKILL);
}

fn setup_mock_active_target(
    env: &ffx_config::TestEnv,
    target: &str,
    pid: u32,
) -> (PathBuf, PathBuf, std::os::unix::net::UnixListener) {
    let socket_path = create_dummy_socket_file(env, target);
    let meta_path = get_metadata_path(&env.context, target).unwrap();
    let control_socket_path = socket_path.with_extension(uart_driver_api::CONTROL_SOCKET_EXTENSION);
    let listener = std::os::unix::net::UnixListener::bind(&control_socket_path).unwrap();

    let meta = ConnectionMetadata {
        pid,
        target: target.to_string(),
        status: ConnectionStatus::Connected,
        id: Some(get_target_id(&canonicalize_target(target))),
        baud: NonZeroU32::new(115200),
        protocol: UartProtocol::ResendSP,
        log_level: None,
        nodename: None,
        serial: None,
    };
    fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();
    (socket_path, meta_path, listener)
}

#[fuchsia::test]
async fn test_list_formatting_with_metadata() {
    let (env, _temp_dir) = create_test_env("test_list_formatting");
    let target = "/target-list-formatting";
    let _guard = DaemonCleanupGuard::new(&env, target);

    let mut child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
    let pid = child.id();
    let (_sock, _meta, _listener) = setup_mock_active_target(&env, target, pid);

    let (output, _) = run_tool(&env, None, UartSubCommand::List(ListCommand {})).await.unwrap();
    for expected in ["TARGET", "BAUD", "PROTOCOL", &pid.to_string(), "115200", "ResendSP"] {
        assert!(output.contains(expected), "Output did not contain {expected}: {output}");
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn make_mock_metrics() -> DaemonMetrics {
    DaemonMetrics {
        checksum_errors: 12,
        retransmissions: 34,
        active_protocol: UartProtocol::ResendSP,
        estimated_rtt_ms: 56,
        connection_drops: 7,
        handshake_failures: 8,
        last_read_timestamp_ms: 0,
        last_write_timestamp_ms: 0,
        outgoing_queue_len: 90,
        last_target_error: None,
    }
}

fn spawn_mock_control_server(
    listener: std::os::unix::net::UnixListener,
    metrics_json: String,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.write_all(metrics_json.as_bytes());
        }
    })
}

fn assert_status_output(output: &str, target_id: &str, socket_path: &Path) {
    assert!(output.contains("Connection Status for target"), "Output: {}", output);
    assert!(output.contains(&format!("Target ID:           {}", target_id)), "Output: {}", output);
    assert!(output.contains("Daemon PID:          12345"), "Output: {}", output);
    assert!(output.contains("Node Name:           fuchsia-test-node"), "Output: {}", output);
    assert!(output.contains("Serial Number:       SN98765"), "Output: {}", output);
    assert!(output.contains("Status:              Connected"), "Output: {}", output);
    assert!(output.contains("Active Protocol:     ResendSP"), "Output: {}", output);
    assert!(output.contains("Estimated RTT:       56 ms"), "Output: {}", output);
    assert!(output.contains("Retransmissions:     34"), "Output: {}", output);
    assert!(output.contains("Checksum Errors:     12"), "Output: {}", output);
    assert!(output.contains("Log File:"), "Output: {}", output);

    let log_id = uart_driver_api::get_log_id_from_socket_path(socket_path);
    let expected_log_file = uart_driver_api::get_driver_log_file_path(Path::new(""), log_id, 0);
    let expected_log_name = expected_log_file.to_string_lossy();
    assert!(
        output.contains(expected_log_name.as_ref()),
        "Expected log name {} in output: {}",
        expected_log_name,
        output
    );
}

#[fuchsia::test]
async fn test_status_output() {
    let (env, _temp_dir) = create_test_env("test_status_output");
    let target = "/target-status-output";
    let _guard = DaemonCleanupGuard::new(&env, target);

    let socket_path = create_dummy_socket_file(&env, target);
    let meta_path = get_metadata_path(&env.context, target).unwrap();

    let target_id = get_target_id(&canonicalize_target(target));

    let meta = ConnectionMetadata {
        pid: 12345,
        target: target.to_string(),
        status: ConnectionStatus::Connected,
        id: Some(target_id.clone()),
        baud: NonZeroU32::new(1000000),
        protocol: UartProtocol::ResendSP,
        log_level: None,
        nodename: Some("fuchsia-test-node".to_string()),
        serial: Some("SN98765".to_string()),
    };
    fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

    let control_socket_path = socket_path.with_extension("control");
    let control_listener = std::os::unix::net::UnixListener::bind(&control_socket_path).unwrap();

    let mock_metrics = make_mock_metrics();
    let metrics_json = serde_json::to_string(&mock_metrics).unwrap();
    let _mock_server = spawn_mock_control_server(control_listener, metrics_json);

    let (output, _) =
        run_tool(&env, Some(target), UartSubCommand::Status(StatusCommand {})).await.unwrap();

    assert_status_output(&output, &target_id, &socket_path);
}

#[fuchsia::test]
async fn test_status_machine_output() {
    let (env, _temp_dir) = create_test_env("test_status_machine_output");
    let target = "/target-status-machine";
    let _guard = DaemonCleanupGuard::new(&env, target);

    let socket_path = create_dummy_socket_file(&env, target);
    let meta_path = get_metadata_path(&env.context, target).unwrap();

    let target_id = get_target_id(&canonicalize_target(target));

    let meta = ConnectionMetadata {
        pid: 12345,
        target: target.to_string(),
        status: ConnectionStatus::Connected,
        id: Some(target_id.clone()),
        baud: NonZeroU32::new(1000000),
        protocol: UartProtocol::ResendSP,
        log_level: None,
        nodename: Some("fuchsia-test-node".to_string()),
        serial: Some("SN98765".to_string()),
    };
    fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

    let control_socket_path = socket_path.with_extension("control");
    let control_listener = std::os::unix::net::UnixListener::bind(&control_socket_path).unwrap();

    let mock_metrics = make_mock_metrics();
    let metrics_json = serde_json::to_string(&mock_metrics).unwrap();

    let _mock_server = spawn_mock_control_server(control_listener, metrics_json);

    let (output, _) = run_tool_with_format(
        &env,
        Some(target),
        UartSubCommand::Status(StatusCommand {}),
        Some(ffx_writer::Format::Json),
    )
    .await
    .unwrap();

    let status_info: ConnectionStatusInfo =
        serde_json::from_str(&output).expect("Failed to parse JSON status output");
    assert_eq!(status_info.target, format!("uart:{}", target));
    assert_eq!(status_info.target_id, target_id);
    assert_eq!(status_info.pid, 12345);
    assert_eq!(status_info.status, "Connected");
    assert_eq!(status_info.nodename.as_deref(), Some("fuchsia-test-node"));
    assert_eq!(status_info.serial.as_deref(), Some("SN98765"));
    assert_eq!(status_info.baud, Some(1000000));
    assert_eq!(status_info.active_protocol, "ResendSP");
    assert_eq!(status_info.estimated_rtt_ms, 56);
    assert_eq!(status_info.retransmissions, 34);
    assert_eq!(status_info.checksum_errors, 12);
    assert_eq!(status_info.connection_drops, 7);
    assert_eq!(status_info.handshake_failures, 8);
    assert_eq!(status_info.outgoing_queue_len, 90);
    assert!(!status_info.is_stalled);
}

#[fuchsia::test]
async fn test_implicit_resolution_single_active() {
    let (env, _temp_dir) = create_test_env("test_implicit_single");
    let target = "/target-implicit-single";
    let _guard = DaemonCleanupGuard::new(&env, target);

    // Connect to target to make it active
    let connect_cmd = make_connect_cmd(115200, false);
    run_tool(&env, Some(target), connect_cmd).await.unwrap();

    // Run disconnect without target. It should implicitly resolve to /target-implicit-single.
    let (stdout, _stderr) =
        run_tool(&env, None, UartSubCommand::Disconnect(DisconnectCommand {})).await.unwrap();

    assert!(stdout.contains("No target specified. Using active connection"), "Stdout: {}", stdout);
    assert!(stdout.contains("Disconnect called for /target-implicit-single"), "Stdout: {}", stdout);
}

#[fuchsia::test]
async fn test_implicit_resolution_multiple_active() {
    let (env, _temp_dir) = create_test_env("test_implicit_multiple");
    let target1 = "/target-implicit-1";
    let target2 = "/target-implicit-2";
    let _guard1 = DaemonCleanupGuard::new(&env, target1);
    let _guard2 = DaemonCleanupGuard::new(&env, target2);

    // Connect to both targets
    let connect_cmd1 = make_connect_cmd(115200, false);
    run_tool(&env, Some(target1), connect_cmd1).await.unwrap();

    let connect_cmd2 = make_connect_cmd(115200, false);
    run_tool(&env, Some(target2), connect_cmd2).await.unwrap();

    // Run disconnect without target. It should fail because of ambiguity.
    let err =
        run_tool(&env, None, UartSubCommand::Disconnect(DisconnectCommand {})).await.unwrap_err();
    let err_msg = err.to_string();
    assert!(err_msg.contains("multiple active UART connections found"), "Error: {}", err_msg);
    assert!(err_msg.contains(target1), "Error: {}", err_msg);
    assert!(err_msg.contains(target2), "Error: {}", err_msg);
}

#[fuchsia::test]
async fn test_implicit_resolution_no_active() {
    let (env, _temp_dir) = create_test_env("test_implicit_none");
    let _guard = DaemonCleanupGuard::new(&env, "/placeholder");

    // Run disconnect without target. It should fail because no active.
    let err =
        run_tool(&env, None, UartSubCommand::Disconnect(DisconnectCommand {})).await.unwrap_err();
    assert!(
        err.to_string().contains("no active UART connections found"),
        "Error: {}",
        err.to_string()
    );
}

#[fuchsia::test]
async fn test_implicit_resolution_connect_fails() {
    let (env, _temp_dir) = create_test_env("test_implicit_connect_fails");
    let target = "/target-implicit-connect";
    let _guard = DaemonCleanupGuard::new(&env, target);

    // Connect to target to make it active
    let connect_cmd = make_connect_cmd(115200, false);
    run_tool(&env, Some(target), connect_cmd).await.unwrap();

    // Now try to run another connect command WITHOUT target.
    // It should fail saying "No target specified" even though there is 1 active.
    let connect_no_target = make_connect_cmd(115200, false);
    let err = run_tool(&env, None, connect_no_target).await.unwrap_err();
    assert!(
        err.to_string().contains(
            "No target specified. Please specify a target using 'ffx -t uart:<target> uart connect'"
        ),
        "Error: {}",
        err.to_string()
    );
}

#[fuchsia::test]
async fn test_concurrency_race_leak() {
    let (env, _temp_dir) = create_test_env("test_race");
    let target = "/target-race";
    let _guard = DaemonCleanupGuard::new(&env, target);

    // 1. Write metadata for Driver A (PID 12345) to simulate it is running
    let socket_path = get_socket_path(&env.context, target).unwrap();
    write_metadata(&env.context, &socket_path, target, 12345, NonZeroU32::new(115200)).unwrap();

    // 2. Simulate Task B calling reconnect, deleting metadata:
    delete_metadata(&env.context, target).unwrap();

    // Simulate socket path existing/locked
    let socket_path = get_socket_path(&env.context, target).unwrap();
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&socket_path, b"").unwrap();

    // 3. User runs ListTool::list
    let list_tool = ListTool { cmd: ListCommand {}, context: env.context.clone() };
    let list_buffers = TestBuffers::default();
    let list_writer = ffx_writer::MachineWriter::new_test(None, &list_buffers);
    list_tool.main(list_writer).await.unwrap();

    // Since the metadata file was deleted, List should NOT find the daemon!
    let list_output = list_buffers.stdout.into_string();
    assert!(!list_output.contains(target), "Daemon should not be listed (orphan state!)");

    let _ = fs::remove_file(&socket_path);
}

#[fuchsia::test]
async fn test_probe_active_connection() {
    let (env, _temp_dir) = create_test_env("test_probe_active");
    let target = "/target-probe-active";
    let _guard = DaemonCleanupGuard::new(&env, target);

    run_tool(&env, Some(target), make_connect_cmd(115200, false)).await.unwrap();

    let (stdout, _) = run_tool(
        &env,
        Some(target),
        UartSubCommand::Probe(ProbeCommand { baud: NonZeroU32::new(115200).unwrap() }),
    )
    .await
    .unwrap();
    assert!(
        stdout
            .contains("Target UART service is ALIVE (active connection via background driver PID"),
        "Unexpected stdout: {}",
        stdout
    );

    // Verify machine JSON output
    let (json_stdout, _) = run_tool_with_format(
        &env,
        Some(target),
        UartSubCommand::Probe(ProbeCommand { baud: NonZeroU32::new(115200).unwrap() }),
        Some(ffx_writer::Format::Json),
    )
    .await
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_stdout).unwrap();
    ffx_writer::VerifiedMachineWriter::<ProbeResult>::verify_schema(&parsed).unwrap();
    assert_eq!(parsed["alive"], true);
    assert_eq!(parsed["method"], "background_driver");
    assert_eq!(parsed["protocol"], "ResendSP");
    assert_eq!(parsed["baud"], 115200);
    assert!(parsed["driver_pid"].as_u64().is_some());
}

#[fuchsia::test]
async fn test_parse_probe_zero_baud() {
    let args = ["probe", "--baud", "0"];
    let res = UartCommand::from_args(&["uart"], &args);
    assert!(res.is_err());
    let err_msg = res.unwrap_err().output;
    assert!(err_msg.contains("number would be zero for non-zero type"), "Error: {}", err_msg);
}

#[fuchsia::test]
async fn test_probe_connecting_daemon_reports_error() {
    let (env, _temp_dir) = create_test_env("test_probe_connecting");
    let target = "/target-probe-connecting";
    let _guard = DaemonCleanupGuard::new(&env, target);

    run_tool(&env, Some(target), make_connect_cmd(115200, false)).await.unwrap();
    update_metadata_status(&env.context, target, ConnectionStatus::Connecting).unwrap();

    let res = run_tool(
        &env,
        Some(target),
        UartSubCommand::Probe(ProbeCommand { baud: NonZeroU32::new(115200).unwrap() }),
    )
    .await;
    assert!(res.is_err());
    assert!(res.unwrap_err().to_string().contains("currently connecting to target"));
}

#[fuchsia::test]
async fn test_connect_uart_stream_file_exclusive_lock() {
    let (_env, temp_dir) = create_test_env("test_lock");
    let temp_file = temp_dir.path().join("mock_device");
    std::fs::write(&temp_file, b"").unwrap();

    // Acquire lock on the file
    let file = std::fs::File::open(&temp_file).unwrap();
    let _lock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, err)| err)
        .unwrap();

    // Attempt to connect via connect_uart_stream. It should fail at flock.
    let res =
        stream::connect_uart_stream(temp_file.to_str().unwrap(), NonZeroU32::new(9600).unwrap())
            .await;
    assert!(res.is_err());
    let err = res.err().unwrap();
    assert!(
        matches!(err, ConnectionError::TtyConfigureFailed { .. }),
        "Expected TtyConfigureFailed, got {:?}",
        err
    );
    if let ConnectionError::TtyConfigureFailed { error } = err {
        assert!(
            error.contains("Port is already in use"),
            "Expected port in use error, got: {}",
            error
        );
    }
}

#[fuchsia::test]
async fn test_canonicalize_target_does_not_resolve_symlinks() {
    let (_env, temp_dir) = create_test_env("test_symlink");
    let target_file = temp_dir.path().join("my_device");
    std::fs::write(&target_file, b"").unwrap();

    let symlink_file = temp_dir.path().join("my_device_link");
    std::os::unix::fs::symlink(&target_file, &symlink_file).unwrap();

    // Canonicalize both
    let path_str = target_file.to_str().unwrap();
    let link_str = symlink_file.to_str().unwrap();

    let id1 = get_target_id(path_str);
    let id2 = get_target_id(link_str);

    // They must resolve to different target IDs because we do not resolve symlinks!
    assert_ne!(id1, id2);
}

#[fuchsia::test]
async fn test_is_pty_target_virtual_socket_detection() {
    let (_env, temp_dir) = create_test_env("test_virtual_socket_detection");

    // 1. Regular file / character device mock (not PTY, not socket) -> false
    let regular_file = temp_dir.path().join("regular_serial");
    fs::write(&regular_file, b"").unwrap();
    assert!(!is_pty_target(&regular_file));

    // 2. Generic UNIX domain socket (e.g. virtual serial bridge or test proxy) -> true
    let generic_socket = temp_dir.path().join("serial-0403-6001.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&generic_socket).unwrap();
    assert!(is_pty_target(&generic_socket));
}

fn setup_mock_connection_fixture(
    shared_data_dir: &Path,
    target: &str,
) -> (std::process::Child, PathBuf) {
    fs::create_dir_all(shared_data_dir).unwrap();
    let target_id = get_target_id(&canonicalize_target(target));
    let child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
    let meta = ConnectionMetadata {
        pid: child.id(),
        target: target.to_string(),
        status: ConnectionStatus::Connected,
        id: Some(target_id.clone()),
        baud: NonZeroU32::new(115200),
        protocol: UartProtocol::ResendSP,
        log_level: None,
        nodename: None,
        serial: None,
    };
    let meta_path = shared_data_dir.join(format!("ffx_uart_{}.json", target_id));
    fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

    let socket_path = shared_data_dir.join(format!("ffx_uart_{}.sock", target_id));
    bind_unlistened_socket(&socket_path);

    (child, meta_path)
}

#[fuchsia::test]
async fn test_doctor_no_issues() {
    let (env, temp_path) = create_test_env("test_doctor_no_issues");
    let target = "/emu:test-target-healthy";
    let _guard = DaemonCleanupGuard::new(&env, target);
    let (mut child, _meta_path) =
        setup_mock_connection_fixture(&temp_path.path().join("ffx_uart"), target);

    let tool = DoctorTool { cmd: DoctorCommand { fix: false }, context: env.context.clone() };
    let buffers = TestBuffers::default();
    let writer = ffx_writer::SimpleWriter::new_test(&buffers);

    tool.main(writer).await.unwrap();
    let output = buffers.stdout.into_string();
    assert!(output.contains("Pass"), "Expected pass, output: {}", output);
    assert!(!output.contains("WARNING"), "Expected no warnings, output: {}", output);
    assert!(output.contains("Status: 0 issues found"), "Expected 0 issues, output: {}", output);

    let _ = child.kill();
    let _ = child.wait();
}

/// Kills and reaps a spawned child process on drop so that a failing assertion does not leak it.
struct ChildProcessGuard(std::process::Child);

impl ChildProcessGuard {
    /// Spawns a long-running `sleep` process to stand in for a live driver PID.
    fn spawn_sleeper() -> Self {
        Self(std::process::Command::new("sleep").arg("60").spawn().unwrap())
    }

    fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for ChildProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Writes a `ConnectionMetadata` record for `target` to `ffx_uart_<file_target_id>.json` in
/// `shared_data_dir` and returns its path.
fn write_doctor_test_metadata(
    shared_data_dir: &Path,
    file_target_id: &str,
    target: &str,
    pid: u32,
) -> PathBuf {
    fs::create_dir_all(shared_data_dir).unwrap();
    let meta = ConnectionMetadata {
        pid,
        target: target.to_string(),
        status: ConnectionStatus::Connected,
        id: Some(file_target_id.to_string()),
        baud: NonZeroU32::new(115200),
        protocol: UartProtocol::ResendSP,
        log_level: None,
        nodename: None,
        serial: None,
    };
    let meta_path = shared_data_dir.join(format!("ffx_uart_{file_target_id}.json"));
    fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();
    meta_path
}

/// Creates a UNIX domain socket file at `path` with no listener behind it.
///
/// The socket is bound but never put into the listening state, so connecting to it always fails
/// with `ECONNREFUSED`. Binding a `UnixListener` and dropping it is not equivalent: a concurrent
/// test that spawns a process can briefly hold a duplicate of the listening descriptor, letting a
/// connection succeed and making the socket appear live.
fn bind_unlistened_socket(path: &Path) {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, bind, socket};
    use std::os::fd::AsRawFd;
    let fd = socket(AddressFamily::Unix, SockType::Stream, SockFlag::SOCK_CLOEXEC, None).unwrap();
    bind(fd.as_raw_fd(), &UnixAddr::new(path).unwrap()).unwrap();
}

/// Creates a UNIX socket file with no listener for `target_id` in `shared_data_dir`.
fn create_orphaned_socket(shared_data_dir: &Path, target_id: &str) -> PathBuf {
    fs::create_dir_all(shared_data_dir).unwrap();
    let sock_path = shared_data_dir.join(format!("ffx_uart_{target_id}.sock"));
    bind_unlistened_socket(&sock_path);
    assert!(
        std::os::unix::fs::FileTypeExt::is_socket(&fs::metadata(&sock_path).unwrap().file_type()),
        "Orphan socket fixture must be an actual UNIX domain socket"
    );
    sock_path
}

/// Runs `ffx uart doctor` (with `--fix` if `fix` is set) and returns its result and stdout.
async fn run_doctor(context: &EnvironmentContext, fix: bool) -> (Result<()>, String) {
    let tool = DoctorTool { cmd: DoctorCommand { fix }, context: context.clone() };
    let buffers = TestBuffers::default();
    let writer = ffx_writer::SimpleWriter::new_test(&buffers);
    let result = tool.main(writer).await;
    (result, buffers.stdout.into_string())
}

#[fuchsia::test]
async fn test_doctor_detects_and_fixes_stale_metadata() {
    let (env, temp_path) = create_test_env("test_doctor_stale_metadata");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    let target = "/emu:test-target";
    let target_id = get_target_id(&canonicalize_target(target));
    let meta_path = write_doctor_test_metadata(&shared_data_dir, &target_id, target, 999999);

    let (result, output) = run_doctor(&env.context, false).await;
    assert!(result.is_err(), "Doctor should fail when finding issues");
    assert!(output.contains("Stale metadata found"), "Expected stale metadata report: {output}");
    assert!(meta_path.exists(), "Metadata file should not be deleted without --fix");

    let (result, output) = run_doctor(&env.context, true).await;
    assert!(result.is_ok(), "Repair should succeed: {output}");
    assert!(output.contains("Attempting repair... OK"), "Repair should succeed: {output}");
    assert!(!meta_path.exists(), "Metadata file should be deleted after fix");
}

#[fuchsia::test]
async fn test_doctor_detects_and_fixes_corrupt_metadata() {
    let (env, temp_path) = create_test_env("test_doctor_corrupt_metadata");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    fs::create_dir_all(&shared_data_dir).unwrap();
    let corrupt_target_id = get_target_id(&canonicalize_target("/emu:test-target-corrupt"));
    let corrupt_path = shared_data_dir.join(format!("ffx_uart_{corrupt_target_id}.json"));
    fs::write(&corrupt_path, b"not valid json").unwrap();
    let unrelated_path = shared_data_dir.join("unrelated.json");
    fs::write(&unrelated_path, b"not valid json").unwrap();

    let (result, output) = run_doctor(&env.context, false).await;
    assert!(result.is_err(), "Doctor should fail when finding issues");
    assert!(
        output.contains("Corrupted metadata JSON"),
        "Expected corrupted metadata report: {output}"
    );
    assert!(corrupt_path.exists(), "Corrupt metadata file should not be deleted without --fix");

    let (result, output) = run_doctor(&env.context, true).await;
    assert!(result.is_ok(), "Repair should succeed: {output}");
    assert!(!corrupt_path.exists(), "Corrupt metadata file should be deleted after fix");
    assert!(unrelated_path.exists(), "Unrelated JSON file should not be deleted");
}

#[fuchsia::test]
async fn test_doctor_detects_and_fixes_orphaned_socket() {
    let (env, temp_path) = create_test_env("test_doctor_orphaned_socket");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    let orphan_target_id = get_target_id(&canonicalize_target("/emu:test-target-orphan"));
    let orphan_sock_path = create_orphaned_socket(&shared_data_dir, &orphan_target_id);

    let (result, output) = run_doctor(&env.context, false).await;
    assert!(result.is_err(), "Doctor should fail when finding issues");
    assert!(output.contains("Orphaned UART file"), "Expected orphaned file report: {output}");
    assert!(orphan_sock_path.exists(), "Orphan socket file should not be deleted without --fix");

    let (result, output) = run_doctor(&env.context, true).await;
    assert!(result.is_ok(), "Repair should succeed: {output}");
    assert!(!orphan_sock_path.exists(), "Orphan socket file should be deleted after fix");
}

#[fuchsia::test]
async fn test_doctor_fix_preserves_socket_with_live_listener() {
    let (env, temp_path) = create_test_env("test_doctor_live_listener_socket");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    fs::create_dir_all(&shared_data_dir).unwrap();
    let target_id = get_target_id(&canonicalize_target("/emu:test-target-starting"));
    let sock_path = shared_data_dir.join(format!("ffx_uart_{target_id}.sock"));
    // Keep the listener open to simulate a driver that has bound its socket but not yet
    // written its metadata.
    let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();

    let (result, output) = run_doctor(&env.context, true).await;
    assert!(result.is_ok(), "A starting driver is not an unresolved issue: {output}");
    assert!(output.contains("has an active listener"), "Expected live listener warning: {output}");
    assert!(!output.contains("Orphaned UART file"), "Live socket must not be orphaned: {output}");
    assert!(sock_path.exists(), "Socket with a live listener must not be deleted by --fix");
}

#[fuchsia::test]
async fn test_doctor_fix_preserves_dead_pid_metadata_with_live_listener() {
    let (env, temp_path) = create_test_env("test_doctor_dead_pid_live_listener");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    let target = "/emu:test-target-restarting";
    let target_id = get_target_id(&canonicalize_target(target));
    let meta_path = write_doctor_test_metadata(&shared_data_dir, &target_id, target, 999999);
    let sock_path = shared_data_dir.join(format!("ffx_uart_{target_id}.sock"));
    let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();

    let (result, output) = run_doctor(&env.context, true).await;
    assert!(result.is_ok(), "A restarting driver is not an unresolved issue: {output}");
    assert!(output.contains("names dead PID"), "Expected restarting driver warning: {output}");
    assert!(!output.contains("Stale metadata found"), "Metadata must not be stale: {output}");
    assert!(meta_path.exists(), "Metadata must not be deleted while a listener is active");
    assert!(sock_path.exists(), "Socket with a live listener must not be deleted by --fix");
}

#[fuchsia::test]
async fn test_doctor_fix_preserves_stale_control_socket_of_starting_driver() {
    let (env, temp_path) = create_test_env("test_doctor_stale_control_starting_driver");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    let target_id = get_target_id(&canonicalize_target("/emu:test-target-stale-control"));
    // A driver that has bound its data socket but has not yet written its metadata or replaced
    // the control socket left behind by a previous driver.
    let control_path = shared_data_dir.join(format!("ffx_uart_{target_id}.control"));
    fs::create_dir_all(&shared_data_dir).unwrap();
    bind_unlistened_socket(&control_path);
    let sock_path = shared_data_dir.join(format!("ffx_uart_{target_id}.sock"));
    let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();

    let (result, output) = run_doctor(&env.context, true).await;
    assert!(result.is_ok(), "A starting driver is not an unresolved issue: {output}");
    assert!(!output.contains("Orphaned UART file"), "Stale control must not be orphaned: {output}");
    assert!(!output.contains("Attempting repair"), "No repair should be attempted: {output}");
    assert!(sock_path.exists(), "Socket with a live listener must not be deleted by --fix");
    assert!(control_path.exists(), "A starting driver's files must not be deleted by --fix");
}

/// Binds and returns a listener on the UNIX socket for `target_id` with extension `ext`.
fn bind_doctor_test_socket(
    shared_data_dir: &Path,
    target_id: &str,
    ext: &str,
) -> std::os::unix::net::UnixListener {
    fs::create_dir_all(shared_data_dir).unwrap();
    let path = shared_data_dir.join(format!("ffx_uart_{target_id}.{ext}"));
    std::os::unix::net::UnixListener::bind(path).unwrap()
}

#[fuchsia::test]
async fn test_has_live_connection() {
    use crate::doctor::has_live_connection;

    let temp_dir = tempfile::tempdir().unwrap();
    let shared = temp_dir.path();
    let live_child = ChildProcessGuard::spawn_sleeper();
    let live_pid = live_child.id();
    let dead_pid = 999999;

    let target_for = |name: &str| {
        let target = format!("/emu:{name}");
        let id = get_target_id(&canonicalize_target(&target));
        (target, id)
    };

    // No files at all.
    let (_, id) = target_for("no-files");
    assert!(!has_live_connection(shared, &id).await, "No files is not live");

    // Metadata naming a running driver.
    let (target, id) = target_for("live-pid");
    write_doctor_test_metadata(shared, &id, &target, live_pid);
    assert!(has_live_connection(shared, &id).await, "Running driver is live");

    // Metadata naming a dead driver, with no listener.
    let (target, id) = target_for("dead-pid");
    write_doctor_test_metadata(shared, &id, &target, dead_pid);
    assert!(!has_live_connection(shared, &id).await, "Dead driver without listener is not live");

    // Metadata naming a dead driver, with a listener (a driver is replacing it).
    let (target, id) = target_for("dead-pid-listener");
    write_doctor_test_metadata(shared, &id, &target, dead_pid);
    let _l1 = bind_doctor_test_socket(shared, &id, uart_driver_api::UNIX_SOCKET_EXTENSION);
    assert!(has_live_connection(shared, &id).await, "Dead driver with listener is live");

    // No metadata, with a listener on the control socket only (a driver is starting).
    let (_, id) = target_for("no-meta-listener");
    let _l2 = bind_doctor_test_socket(shared, &id, uart_driver_api::CONTROL_SOCKET_EXTENSION);
    assert!(has_live_connection(shared, &id).await, "Starting driver is live");

    // No metadata and a socket with no listener.
    let (_, id) = target_for("no-meta-orphan");
    create_orphaned_socket(shared, &id);
    assert!(!has_live_connection(shared, &id).await, "Orphaned socket is not live");

    // Corrupted metadata is never live, even with a listener.
    let (_, id) = target_for("corrupt-listener");
    fs::write(shared.join(format!("ffx_uart_{id}.json")), b"not valid json").unwrap();
    let _l3 = bind_doctor_test_socket(shared, &id, uart_driver_api::UNIX_SOCKET_EXTENSION);
    assert!(!has_live_connection(shared, &id).await, "Corrupted metadata is not live");

    // Mismatched metadata is never live, even when it names a running driver with a listener.
    let (_, id) = target_for("mismatch-listener");
    write_doctor_test_metadata(shared, &id, "/emu:some-other-target", live_pid);
    let _l4 = bind_doctor_test_socket(shared, &id, uart_driver_api::UNIX_SOCKET_EXTENSION);
    assert!(!has_live_connection(shared, &id).await, "Mismatched metadata is not live");
}

#[fuchsia::test]
async fn test_doctor_apply_fix_skips_targets_with_live_connection() {
    use crate::doctor::FixType;
    use crate::subtools::doctor::FixOutcome;

    let (env, temp_path) = create_test_env("test_doctor_apply_fix_skips_live");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    let live_id = get_target_id(&canonicalize_target("/emu:test-target-now-live"));
    let live_sock = shared_data_dir.join(format!("ffx_uart_{live_id}.sock"));
    let _listener =
        bind_doctor_test_socket(&shared_data_dir, &live_id, uart_driver_api::UNIX_SOCKET_EXTENSION);
    let orphan_id = get_target_id(&canonicalize_target("/emu:test-target-still-orphaned"));
    let orphan_sock = create_orphaned_socket(&shared_data_dir, &orphan_id);

    let tool = DoctorTool { cmd: DoctorCommand { fix: true }, context: env.context.clone() };
    let buffers = TestBuffers::default();
    let mut writer = ffx_writer::SimpleWriter::new_test(&buffers);

    // Every target has gained a live connection since diagnosis, so nothing is repaired.
    let fix = FixType::DeleteMetadataFiles { target_ids: vec![live_id.clone()] };
    let outcome = tool.apply_fix(fix, &mut writer).await.unwrap();
    assert_eq!(outcome, FixOutcome::Skipped);
    assert!(live_sock.exists(), "Socket with a live listener must not be deleted");
    let output = buffers.stdout.into_string();
    assert!(output.contains(&format!("skipping target {live_id}")), "Expected skip: {output}");

    // Skipping one target does not prevent repairing another.
    let buffers = TestBuffers::default();
    let mut writer = ffx_writer::SimpleWriter::new_test(&buffers);
    let fix = FixType::DeleteMetadataFiles { target_ids: vec![live_id.clone(), orphan_id] };
    let outcome = tool.apply_fix(fix, &mut writer).await.unwrap();
    assert_eq!(outcome, FixOutcome::Repaired);
    assert!(live_sock.exists(), "Socket with a live listener must not be deleted");
    assert!(!orphan_sock.exists(), "Orphaned socket should be deleted");
}

#[fuchsia::test]
async fn test_doctor_connection_check_matches_metadata_id() {
    let (mut env, temp_path) = create_test_env("test_doctor_connection_check_meta_id");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    let file_target_id = get_target_id(&canonicalize_target("/emu:test-target-file"));
    let requested_target = "/emu:test-target-requested";
    let requested_target_id = get_target_id(&canonicalize_target(requested_target));
    // The file is named after one target but its metadata records the requested target's ID.
    let meta_path =
        write_doctor_test_metadata(&shared_data_dir, &file_target_id, requested_target, 999999);
    let content = fs::read_to_string(&meta_path).unwrap();
    let mut meta: ConnectionMetadata = serde_json::from_str(&content).unwrap();
    meta.id = Some(requested_target_id.clone());
    fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

    env.context.override_target_specifier(&Some(format!("uart:{requested_target_id}")));
    let (result, output) = run_doctor(&env.context, false).await;
    assert!(result.is_err(), "Mismatched metadata should be reported: {output}");
    assert!(
        !output.contains("No active or stale UART connections"),
        "Connection check should match the metadata ID like MetadataCheck does: {output}"
    );
    assert!(output.contains("Mismatched metadata target ID"), "Expected mismatch: {output}");
    assert!(meta_path.exists(), "Metadata should not be deleted without --fix");
}

#[fuchsia::test]
async fn test_doctor_fix_terminates_mismatched_daemon() {
    let (env, temp_path) = create_test_env("test_doctor_mismatched_metadata");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    let mismatch_child = ChildProcessGuard::spawn_sleeper();
    let mismatch_pid = mismatch_child.id();
    let mismatch_target_id = get_target_id(&canonicalize_target("/emu:test-target-mismatch"));
    let different_target = "/emu:different-target";
    let different_target_id = get_target_id(&canonicalize_target(different_target));
    let mismatch_path = write_doctor_test_metadata(
        &shared_data_dir,
        &mismatch_target_id,
        different_target,
        mismatch_pid,
    );

    let (result, output) = run_doctor(&env.context, false).await;
    assert!(result.is_err(), "Doctor should fail when finding issues");
    let expected_mismatch_msg =
        format!("(expected '{mismatch_target_id}', found '{different_target_id}')");
    assert!(
        output.contains("Mismatched metadata target ID") && output.contains(&expected_mismatch_msg),
        "Output did not contain distinct mismatched metadata IDs ({expected_mismatch_msg}): {output}",
    );
    assert!(mismatch_path.exists(), "Mismatched metadata file should not be deleted without --fix");
    assert!(
        crate::sys::is_running(mismatch_pid),
        "Mismatched daemon should remain running without --fix"
    );

    let (result, output) = run_doctor(&env.context, true).await;
    assert!(result.is_ok(), "Repair should succeed: {output}");
    assert!(
        !crate::sys::is_running(mismatch_pid),
        "Mismatched daemon process should be terminated by --fix before deleting files"
    );
    assert!(!mismatch_path.exists(), "Mismatched metadata file should be deleted after fix");
}

#[fuchsia::test]
async fn test_doctor_scoped_fix_leaves_other_targets_alone() {
    let (mut env, temp_path) = create_test_env("test_doctor_scoped_fix");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    fs::create_dir_all(&shared_data_dir).unwrap();
    let scoped_target_id = get_target_id(&canonicalize_target("/dev/ttyUSB80"));
    let scoped_path = shared_data_dir.join(format!("ffx_uart_{scoped_target_id}.json"));
    fs::write(&scoped_path, b"not valid json").unwrap();
    let other_target_id = get_target_id(&canonicalize_target("/dev/ttyUSB81"));
    let other_sock_path = create_orphaned_socket(&shared_data_dir, &other_target_id);

    env.context.override_target_specifier(&Some(format!("uart:{scoped_target_id}")));
    let (result, output) = run_doctor(&env.context, true).await;
    assert!(result.is_ok(), "Scoped repair should succeed: {output}");
    assert!(!scoped_path.exists(), "Scoped target's corrupt metadata should be deleted");
    assert!(other_sock_path.exists(), "Other target's orphaned socket must not be deleted");
    assert!(
        !output.contains(&other_target_id),
        "Scoped run should not report other targets: {output}"
    );
}

#[fuchsia::test]
async fn test_doctor_reports_unreadable_metadata() {
    use std::os::unix::fs::PermissionsExt;
    let (env, temp_path) = create_test_env("test_doctor_unreadable_metadata");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    fs::create_dir_all(&shared_data_dir).unwrap();
    let target_id = get_target_id(&canonicalize_target("/dev/ttyUSB82"));
    let meta_path = shared_data_dir.join(format!("ffx_uart_{target_id}.json"));
    fs::write(&meta_path, b"{}").unwrap();
    fs::set_permissions(&meta_path, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&meta_path).is_ok() {
        // Permission bits are not enforced (e.g. running as root), so the file stays readable.
        return;
    }

    let (result, output) = run_doctor(&env.context, false).await;
    assert!(result.is_err(), "Doctor should fail when finding issues");
    assert!(
        output.contains("Unreadable metadata file"),
        "Expected unreadable metadata report: {output}"
    );
}

#[fuchsia::test]
async fn test_doctor_reports_failed_repair() {
    use std::os::unix::fs::PermissionsExt;
    let (env, temp_path) = create_test_env("test_doctor_failed_repair");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    fs::create_dir_all(&shared_data_dir).unwrap();
    let target_id = get_target_id(&canonicalize_target("/dev/ttyUSB83"));
    let corrupt_path = shared_data_dir.join(format!("ffx_uart_{target_id}.json"));
    fs::write(&corrupt_path, b"not valid json").unwrap();
    fs::set_permissions(&shared_data_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let probe_path = shared_data_dir.join("write_probe");
    if fs::write(&probe_path, b"").is_ok() {
        // Permission bits are not enforced (e.g. running as root), so removal cannot fail.
        let _ = fs::remove_file(&probe_path);
        fs::set_permissions(&shared_data_dir, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let (result, output) = run_doctor(&env.context, true).await;
    fs::set_permissions(&shared_data_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.is_err(), "Doctor should fail when a repair fails: {output}");
    assert!(
        output.contains("Attempting repair... FAILED"),
        "Expected failed repair report: {output}"
    );
    assert!(output.contains("1 issues found (0 repaired)"), "Expected 0 repaired: {output}");
    assert!(corrupt_path.exists(), "Corrupt metadata should remain after a failed repair");
}

#[test]
fn test_resolve_target_filter_id() {
    let hex_id = "0123456789abcdef";
    assert_eq!(crate::doctor::resolve_target_filter_id(hex_id), hex_id);
    assert_eq!(crate::doctor::resolve_target_filter_id(&format!("uart:{hex_id}")), hex_id);
    assert_eq!(crate::doctor::resolve_target_filter_id("0123456789ABCDEF"), hex_id);
    assert_eq!(
        crate::doctor::resolve_target_filter_id("0123456789abcde"),
        get_target_id(&canonicalize_target("0123456789abcde")),
        "IDs with the wrong length should be hashed"
    );
    assert_eq!(
        crate::doctor::resolve_target_filter_id("uart:/dev/ttyUSB0"),
        get_target_id(&canonicalize_target("/dev/ttyUSB0")),
        "Non-hex targets should be hashed after stripping the `uart:` prefix"
    );
}

#[fuchsia::test]
async fn test_terminate_process_refuses_current_process() {
    crate::driver::terminate_process(std::process::id()).await.unwrap();
    assert!(crate::sys::is_running(std::process::id()), "Current process must not be terminated");
}

#[fuchsia::test]
async fn test_doctor_target_specifier_without_healthy_metadata() {
    let (mut env, temp_path) = create_test_env("test_doctor_target_filter");
    let shared_data_dir = temp_path.path().join("ffx_uart");
    fs::create_dir_all(&shared_data_dir).unwrap();

    // 1. Disconnected nodename specifier should warn cleanly rather than failing in resolve_target.
    env.context.override_target_specifier(&Some("fuchsia-disconnected-node".to_string()));
    let tool = DoctorTool { cmd: DoctorCommand { fix: false }, context: env.context.clone() };
    let buffers = TestBuffers::default();
    let writer = ffx_writer::SimpleWriter::new_test(&buffers);
    tool.main(writer).await.unwrap();
    let output = buffers.stdout.into_string();
    assert!(
        output.contains(
            "No active or stale UART connections detected for target 'fuchsia-disconnected-node'"
        ),
        "Expected target-scoped warning, output: {output}"
    );

    // 2. Orphaned UNIX socket targeted by 16-hex ID (without .json) should be detected and fixed.
    let orphan_id = get_target_id(&canonicalize_target("/dev/ttyUSB77"));
    let orphan_sock = shared_data_dir.join(format!("ffx_uart_{orphan_id}.sock"));
    bind_unlistened_socket(&orphan_sock);

    env.context.override_target_specifier(&Some(format!("uart:{orphan_id}")));
    let tool_detect =
        DoctorTool { cmd: DoctorCommand { fix: false }, context: env.context.clone() };
    let buffers_detect = TestBuffers::default();
    let writer_detect = ffx_writer::SimpleWriter::new_test(&buffers_detect);
    assert!(
        tool_detect.main(writer_detect).await.is_err(),
        "Doctor without --fix should fail when an orphaned socket is present"
    );
    let detect_output = buffers_detect.stdout.into_string();
    assert!(
        detect_output.contains("Orphaned UART file"),
        "Expected orphaned socket detection, output: {detect_output}"
    );

    let tool_fix = DoctorTool { cmd: DoctorCommand { fix: true }, context: env.context.clone() };
    let buffers_fix = TestBuffers::default();
    let writer_fix = ffx_writer::SimpleWriter::new_test(&buffers_fix);
    tool_fix.main(writer_fix).await.unwrap();
    assert!(!orphan_sock.exists(), "Orphaned socket should be removed by --fix");
}

#[fuchsia::test]
async fn test_doctor_no_connections_warning() {
    let (env, _temp_path) = create_test_env("test_doctor_no_connections_warning");

    let tool = DoctorTool { cmd: DoctorCommand { fix: false }, context: env.context.clone() };
    let buffers = TestBuffers::default();
    let writer = ffx_writer::SimpleWriter::new_test(&buffers);

    tool.main(writer).await.unwrap();
    let output = buffers.stdout.into_string();
    assert!(
        output.contains("No active or stale UART connections detected"),
        "Should report warning when no connections exist, output: {}",
        output
    );
    assert!(
        output.contains("0 issues found (0 repaired)"),
        "Should report 0 issues, output: {}",
        output
    );
}
