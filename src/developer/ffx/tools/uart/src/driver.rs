// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helper routines for locating, spawning, and supervising the `ffx-uart-driver` executable.
//!
//! Manages driver binary resolution in host tool directories, background daemon process
//! spawning with redirected stdio, startup polling, log directory resolution, and graceful
//! or forceful process termination.

use crate::metadata::{
    ConnectionMetadata, ConnectionStatus, UartProtocol, delete_metadata, get_socket_path,
    read_metadata,
};
use crate::sys::{
    DaemonLiveness, check_daemon_liveness, get_peer_pid, is_driver_running,
    read_metrics_from_control_socket,
};
use fho::{Result, bug, user_error};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DAEMON_START_POLL_TIMEOUT: Duration = Duration::from_secs(5);
pub const DAEMON_START_POLL_INTERVAL: Duration = Duration::from_millis(50);
pub const DAEMON_SPAWN_ETXTBSY_MAX_RETRIES: u32 = 40;
pub const DAEMON_SPAWN_ETXTBSY_RETRY_DELAY: Duration = Duration::from_millis(5);
pub const SIGTERM_TIMEOUT: Duration = Duration::from_secs(1);
pub const SIGKILL_TIMEOUT: Duration = Duration::from_millis(200);
pub const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(100);
pub const MACOS_MAX_UNIX_PATH_LEN: usize = 104;
pub const LINUX_MAX_UNIX_PATH_LEN: usize = 108;

pub fn locate_driver(context: &ffx_config::EnvironmentContext) -> PathBuf {
    if let Ok(path) = ffx_config::get_host_tool(context, "ffx-uart-driver") {
        if path.exists() {
            return path;
        }
    }
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            let fallback = dir.join("ffx-uart-driver");
            if fallback.exists() {
                return fallback;
            }
        }
    }
    PathBuf::from("ffx-uart-driver")
}

pub async fn wait_for_daemon_started(
    context: &ffx_config::EnvironmentContext,
    target: &str,
    log_path: &Path,
) -> Result<u32> {
    let start = Instant::now();
    while start.elapsed() < DAEMON_START_POLL_TIMEOUT {
        if let Some(pid) = check_startup_status(context, target, log_path)? {
            return Ok(pid);
        }
        fuchsia_async::Timer::new(DAEMON_START_POLL_INTERVAL).await;
    }
    handle_startup_timeout(context, target, log_path).await
}

pub async fn terminate_process(pid: u32) -> Result<()> {
    if pid <= 1 || pid > i32::MAX as u32 {
        log::warn!("Refusing to terminate invalid process ID {}.", pid);
        return Ok(());
    }
    let nix_pid = Pid::from_raw(pid as i32);
    if let Err(nix::errno::Errno::EPERM) = kill(nix_pid, None) {
        return Err(user_error!("Permission denied to terminate process {}", pid));
    }
    let _ = kill(nix_pid, Signal::SIGTERM);
    if wait_for_process_exit(pid, SIGTERM_TIMEOUT).await {
        return Ok(());
    }

    let _ = kill(nix_pid, Signal::SIGKILL);
    if wait_for_process_exit(pid, SIGKILL_TIMEOUT).await {
        return Ok(());
    }

    if is_driver_running(pid) { Err(user_error!("Failed to kill process {}", pid)) } else { Ok(()) }
}

pub async fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !is_driver_running(pid) {
            return true;
        }
        fuchsia_async::Timer::new(DAEMON_START_POLL_INTERVAL).await;
    }
    !is_driver_running(pid)
}

pub fn check_startup_status(
    context: &ffx_config::EnvironmentContext,
    target: &str,
    log_path: &Path,
) -> Result<Option<u32>> {
    match read_metadata(context, target) {
        Ok(Some(meta)) => {
            if meta.pid == 0 {
                // Metadata file is transiently empty or partially written during startup;
                // continue polling until timeout.
                return Ok(None);
            }
            if !is_driver_running(meta.pid) {
                return match meta.status {
                    ConnectionStatus::Error(reason) => Err(user_error!(
                        "Daemon failed to start: {}. Check driver logs at: {}",
                        reason,
                        log_path.display()
                    )),
                    _ => Err(user_error!(
                        "ffx-uart-driver started but exited immediately. Check driver logs at: {}",
                        log_path.display()
                    )),
                };
            }
            match meta.status {
                ConnectionStatus::Connected => Ok(Some(meta.pid)),
                ConnectionStatus::Error(reason) => {
                    Err(user_error!("Connection failed: {}", reason))
                }
                ConnectionStatus::Connecting => Ok(None),
            }
        }
        Ok(None) | Err(_) => Ok(None),
    }
}

pub async fn handle_startup_timeout(
    context: &ffx_config::EnvironmentContext,
    target: &str,
    log_path: &Path,
) -> Result<u32> {
    match read_metadata(context, target) {
        Ok(Some(meta)) => {
            if is_driver_running(meta.pid) {
                let socket_path = get_socket_path(context, target)?;
                let control_socket_path =
                    socket_path.with_extension(uart_driver_api::CONTROL_SOCKET_EXTENSION);
                let metrics_msg = if let Some(metrics) =
                    read_metrics_from_control_socket(&control_socket_path).await
                {
                    format!(
                        " (stuck in 'Connecting' state with {} handshake failures)",
                        metrics.handshake_failures
                    )
                } else {
                    " (stuck in 'Connecting' state)".to_string()
                };
                Err(user_error!(
                    "Timed out waiting for connection to be established for target {}{}.\n\
                    The target may not be responding. Check driver logs at: {}",
                    target,
                    metrics_msg,
                    log_path.display()
                ))
            } else {
                Err(user_error!(
                    "ffx-uart-driver started but exited. Check driver logs at: {}",
                    log_path.display()
                ))
            }
        }
        _ => {
            Err(user_error!("Timed out waiting for ffx-uart-driver to start for target {}", target))
        }
    }
}

pub fn get_or_create_log_dir(context: &ffx_config::EnvironmentContext) -> Result<PathBuf> {
    let mut log_dir = match context.get::<PathBuf, _>("shared_data") {
        Ok(p) => p,
        Err(_) => context.get_shared_data_path().map_err(|e| bug!("{e:?}"))?,
    };
    log_dir.push("ffx_uart");
    if !log_dir.exists() {
        fs::create_dir_all(&log_dir).map_err(|e| bug!("{e:?}"))?;
    }
    Ok(log_dir)
}

pub fn get_driver_log_path(log_dir: &Path, socket_path: &Path) -> PathBuf {
    let log_id = uart_driver_api::get_log_id_from_socket_path(socket_path);
    uart_driver_api::get_driver_log_file_path(log_dir, log_id, 0)
}

pub fn validate_socket_path_len(path: &Path) -> Result<()> {
    let path_len = path.to_string_lossy().as_bytes().len();
    let max_len =
        if cfg!(target_os = "macos") { MACOS_MAX_UNIX_PATH_LEN } else { LINUX_MAX_UNIX_PATH_LEN };
    if path_len >= max_len {
        return Err(user_error!(
            "UNIX socket path exceeds limit ({} bytes, max {} bytes for OS)",
            path_len,
            max_len
        ));
    }
    Ok(())
}

pub async fn wait_for_parent_exit(mut child: std::process::Child) -> Result<()> {
    let status = tokio::time::timeout(
        DAEMON_START_POLL_TIMEOUT,
        fuchsia_async::unblock(move || child.wait()),
    )
    .await
    .map_err(|_| user_error!("Timed out waiting for driver parent process to exit"))?
    .map_err(|e| user_error!("Failed to wait for driver parent process: {:?}", e))?;

    if !status.success() {
        return Err(user_error!("Driver parent process exited with error: {:?}", status));
    }
    Ok(())
}

pub fn build_driver_daemon_command(
    driver_path: &Path,
    context: &ffx_config::EnvironmentContext,
    log_level: Option<&str>,
    target: &str,
    baud: std::num::NonZeroU32,
    socket_path: &Path,
    log_dir: &Path,
    no_retry: bool,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(driver_path);
    if let Some(isolate_root) = context.env_kind().isolate_root() {
        cmd.env("FFX_ISOLATE_DIR", isolate_root);
    }
    if let Some(level) = log_level {
        cmd.arg("--log-level").arg(level);
    }
    cmd.arg("uart-driver")
        .arg("--target")
        .arg(target)
        .arg("--baud")
        .arg(baud.to_string())
        .arg("--socket")
        .arg(socket_path)
        .arg("--log-dir")
        .arg(log_dir)
        .arg("--background");

    if no_retry {
        cmd.arg("--no-retry");
    }

    cmd
}

pub async fn spawn_driver_daemon(
    driver_path: &Path,
    context: &ffx_config::EnvironmentContext,
    log_level: Option<&str>,
    target: &str,
    baud: std::num::NonZeroU32,
    socket_path: &Path,
    log_dir: &Path,
    no_retry: bool,
) -> Result<()> {
    let mut cmd = build_driver_daemon_command(
        driver_path,
        context,
        log_level,
        target,
        baud,
        socket_path,
        log_dir,
        no_retry,
    );
    log::info!("Spawning UART driver: {:?}", cmd);
    let mut attempts = 0;
    let child = loop {
        match cmd.spawn() {
            Ok(child) => break child,
            Err(e)
                if (e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    || e.raw_os_error() == Some(nix::libc::ETXTBSY))
                    && attempts < DAEMON_SPAWN_ETXTBSY_MAX_RETRIES =>
            {
                attempts += 1;
                fuchsia_async::Timer::new(DAEMON_SPAWN_ETXTBSY_RETRY_DELAY).await;
            }
            Err(e) => return Err(user_error!("Failed to spawn ffx-uart-driver: {:?}", e)),
        }
    };

    wait_for_parent_exit(child).await
}

pub fn cleanup_target_files(context: &ffx_config::EnvironmentContext, target: &str) {
    if let Err(e) = delete_metadata(context, target) {
        log::warn!("Failed to delete metadata for target {target}: {:?}", e);
    }
    if let Ok(socket_path) = get_socket_path(context, target) {
        if socket_path.exists() {
            if let Err(e) = fs::remove_file(&socket_path) {
                log::warn!("Failed to remove socket file at {}: {:?}", socket_path.display(), e);
            }
        }
        let control_path = socket_path.with_extension(uart_driver_api::CONTROL_SOCKET_EXTENSION);
        if control_path.exists() {
            if let Err(e) = fs::remove_file(&control_path) {
                log::warn!(
                    "Failed to remove control socket file at {}: {:?}",
                    control_path.display(),
                    e
                );
            }
        }
    }
}

pub async fn get_active_connections(
    context: &ffx_config::EnvironmentContext,
) -> Result<Vec<ConnectionMetadata>> {
    let mut list_entries = Vec::new();
    let mut shared_path = match context.get::<PathBuf, _>("shared_data") {
        Ok(p) => p,
        Err(_) => context.get_shared_data_path().map_err(|e| bug!("{e:?}"))?,
    };
    shared_path.push("ffx_uart");

    if shared_path.exists() {
        if let Ok(entries) = fs::read_dir(&shared_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
                        let dot_ext = format!(".{}", uart_driver_api::UNIX_SOCKET_EXTENSION);
                        if let Some(rest) = filename.strip_prefix("ffx_uart_") {
                            if let Some(target_id) = rest.strip_suffix(&dot_ext) {
                                if !target_id.is_empty() {
                                    if let Some(meta) =
                                        inspect_active_socket(context, &path, target_id).await
                                    {
                                        list_entries.push(meta);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(list_entries)
}

pub async fn inspect_active_socket(
    context: &ffx_config::EnvironmentContext,
    path: &Path,
    target_id: &str,
) -> Option<ConnectionMetadata> {
    let meta_path = path.with_extension(uart_driver_api::METADATA_FILE_EXTENSION);
    let control_socket_path = path.with_extension(uart_driver_api::CONTROL_SOCKET_EXTENSION);

    if meta_path.exists() {
        read_and_validate_active_metadata(context, &meta_path, &control_socket_path, target_id)
            .await
    } else {
        probe_unregistered_active_socket(context, path, &control_socket_path, target_id).await
    }
}

pub async fn read_and_validate_active_metadata(
    _context: &ffx_config::EnvironmentContext,
    meta_path: &Path,
    control_socket_path: &Path,
    target_id: &str,
) -> Option<ConnectionMetadata> {
    let content = fs::read_to_string(meta_path).ok()?;
    let mut meta = serde_json::from_str::<ConnectionMetadata>(&content).ok()?;
    meta.id = Some(target_id.to_string());
    if check_daemon_liveness(meta.pid, control_socket_path).await != DaemonLiveness::Alive {
        return None;
    }
    Some(meta)
}

pub async fn probe_unregistered_active_socket(
    context: &ffx_config::EnvironmentContext,
    path: &Path,
    control_socket_path: &Path,
    target_id: &str,
) -> Option<ConnectionMetadata> {
    let probe_target = if control_socket_path.exists() { control_socket_path } else { path };
    let stream =
        tokio::time::timeout(SOCKET_PROBE_TIMEOUT, tokio::net::UnixStream::connect(probe_target))
            .await
            .ok()?
            .ok()?;
    let std_stream = stream.into_std().ok()?;
    let pid = get_peer_pid(&std_stream).ok()?;
    if check_daemon_liveness(pid, control_socket_path).await != DaemonLiveness::Alive {
        return None;
    }
    let target = uart_driver_api::socket_path_to_target_path(path, context);
    Some(ConnectionMetadata {
        pid,
        target,
        status: ConnectionStatus::Connected,
        id: Some(target_id.to_string()),
        baud: None,
        protocol: UartProtocol::Unknown,
        log_level: None,
        nodename: None,
        serial: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_validate_socket_path_len() {
        let short_path = Path::new("/tmp/test.sock");
        assert!(validate_socket_path_len(short_path).is_ok());

        let oversized = PathBuf::from(format!("/tmp/{}", "a".repeat(200)));
        assert!(validate_socket_path_len(&oversized).is_err());
    }

    #[fuchsia::test]
    fn test_get_driver_log_path() {
        let log_dir = Path::new("/tmp/logs");
        let sock_path = Path::new("/tmp/ffx_uart_12345.sock");
        let log_path = get_driver_log_path(log_dir, sock_path);
        assert!(log_path.starts_with(log_dir));
        assert!(log_path.to_string_lossy().ends_with(".0.log"));
    }

    #[fuchsia::test]
    fn test_build_driver_daemon_command() {
        let env = ffx_config::test_init().unwrap();
        let driver_path = Path::new("/bin/ffx-uart-driver");
        let socket_path = Path::new("/tmp/test.sock");
        let log_dir = Path::new("/tmp/logs");

        let cmd = build_driver_daemon_command(
            driver_path,
            &env.context,
            Some("debug"),
            "/dev/ttyUSB0",
            std::num::NonZeroU32::new(115200).unwrap(),
            socket_path,
            log_dir,
            true,
        );

        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.contains(&"--log-level".to_string()));
        assert!(args.contains(&"debug".to_string()));
        assert!(args.contains(&"uart-driver".to_string()));
        assert!(args.contains(&"--target".to_string()));
        assert!(args.contains(&"/dev/ttyUSB0".to_string()));
        assert!(args.contains(&"--baud".to_string()));
        assert!(args.contains(&"115200".to_string()));
        assert!(args.contains(&"--socket".to_string()));
        assert!(args.contains(&"/tmp/test.sock".to_string()));
        assert!(args.contains(&"--log-dir".to_string()));
        assert!(args.contains(&"/tmp/logs".to_string()));
        assert!(args.contains(&"--background".to_string()));
        assert!(args.contains(&"--no-retry".to_string()));
    }

    #[fuchsia::test]
    fn test_check_startup_status_transient_empty_metadata_returns_none() {
        let temp = tempfile::tempdir().unwrap();
        let env = ffx_config::test_env()
            .runtime_config("shared_data", temp.path().to_str().unwrap())
            .build()
            .unwrap();
        let target = "/dev/ttyUSB0";
        let meta_path = crate::metadata::get_metadata_path(&env.context, target).unwrap();
        if let Some(parent) = meta_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&meta_path, b"").unwrap();
        let log_path = temp.path().join("driver.log");
        assert_eq!(check_startup_status(&env.context, target, &log_path).unwrap(), None);
    }
}
