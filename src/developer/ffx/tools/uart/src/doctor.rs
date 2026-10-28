// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Diagnostic checks and automatic repair for `ffx uart`.

use crate::driver::SOCKET_PROBE_TIMEOUT;
use crate::metadata::{
    CONTROL_SOCKET_EXTENSION, ConnectionMetadata, METADATA_FILE_EXTENSION, UART_FILE_PREFIX,
    UNIX_SOCKET_EXTENSION, canonicalize_target, get_shared_uart_path, get_target_id, is_target_id,
};
use crate::sys::is_driver_running;
use ffx_config::EnvironmentContext;
use fho::Result;
use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use tokio::net::UnixStream;

/// Outcome of a single diagnostic check executed by `ffx uart doctor`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum CheckStatus {
    /// The check passed with no issues detected.
    Success,
    /// A non-fatal condition was observed that does not require repair.
    Warning(String),
    /// A failure condition was detected, optionally accompanied by an automated repair action.
    Failure {
        /// Human-readable description of the detected failure.
        message: String,
        /// The automated repair action to execute when `--fix` is enabled.
        fix_type: Option<FixType>,
    },
}

/// Automated repair actions that `ffx uart doctor --fix` can perform.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum FixType {
    /// Remove stale or corrupted metadata and socket files for one or more target IDs.
    DeleteMetadataFiles {
        /// The 16-character hex target IDs whose files should be removed.
        target_ids: Vec<String>,
    },
    /// Terminate a process holding an exclusive lock on `port_path` and clean up stale metadata.
    KillDriverLockingPort {
        /// Process ID holding the exclusive port lock.
        pid: u32,
        /// Filesystem path of the locked serial port.
        port_path: PathBuf,
    },
}

/// Trait implemented by individual `ffx uart doctor` diagnostic checks.
#[allow(async_fn_in_trait)]
pub trait DiagnosticCheck {
    /// Returns the human-readable name of this diagnostic check.
    fn name(&self) -> &'static str;

    /// Runs the diagnostic check against `context`, optionally scoped to `target`.
    async fn diagnose(
        &self,
        context: &EnvironmentContext,
        target: Option<&str>,
    ) -> Result<CheckStatus>;
}

/// Returns `true` if `path` refers to a regular file or a UNIX domain socket.
fn is_file_or_socket(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() || m.file_type().is_socket())
}

/// Returns the path of the UART file with extension `ext` for `target_id` under `shared_path`.
pub(crate) fn uart_file_path(shared_path: &Path, target_id: &str, ext: &str) -> PathBuf {
    shared_path.join(format!("{UART_FILE_PREFIX}{target_id}.{ext}"))
}

/// Returns `true` if a process is accepting connections on the UNIX socket at `path`.
async fn socket_has_listener(path: &Path) -> bool {
    matches!(tokio::time::timeout(SOCKET_PROBE_TIMEOUT, UnixStream::connect(path)).await, Ok(Ok(_)))
}

/// Returns `true` if any socket for `target_id` under `shared_path` has an active listener.
async fn any_socket_has_listener(shared_path: &Path, target_id: &str) -> bool {
    for ext in [CONTROL_SOCKET_EXTENSION, UNIX_SOCKET_EXTENSION] {
        if socket_has_listener(&uart_file_path(shared_path, target_id, ext)).await {
            return true;
        }
    }
    false
}

/// Returns the conflicting ID if `meta` does not describe the connection whose files are named
/// after `file_target_id`, checking both the recorded ID and the ID derived from the target path.
fn mismatched_metadata_id(meta: &ConnectionMetadata, file_target_id: &str) -> Option<String> {
    let computed_id = get_target_id(&canonicalize_target(&meta.target));
    let meta_id = meta.id.as_deref().unwrap_or(&computed_id);
    if meta_id != file_target_id {
        Some(meta_id.to_string())
    } else if computed_id != file_target_id {
        Some(computed_id)
    } else {
        None
    }
}

/// Returns `true` if `target_id` currently has a live connection that repairs must not disturb.
///
/// A connection is live if its metadata is consistent and names a running driver, or if its
/// metadata is absent or names a dead driver while one of its sockets has an active listener.
/// The latter covers a driver that has bound its sockets but not yet written its metadata.
/// Corrupted, unreadable, or mismatched metadata never counts as live, so those are always
/// repaired.
pub(crate) async fn has_live_connection(shared_path: &Path, target_id: &str) -> bool {
    let json_path = uart_file_path(shared_path, target_id, METADATA_FILE_EXTENSION);
    match fs::read_to_string(&json_path) {
        Ok(content) => {
            let Ok(meta) = serde_json::from_str::<ConnectionMetadata>(&content) else {
                return false;
            };
            if mismatched_metadata_id(&meta, target_id).is_some() {
                return false;
            }
            if is_driver_running(meta.pid) {
                return true;
            }
        }
        Err(e) if e.kind() != ErrorKind::NotFound => return false,
        Err(_) => {}
    }
    any_socket_has_listener(shared_path, target_id).await
}

/// Extracts the non-empty target ID from a UART file path of the form
/// `ffx_uart_<target_id>.<ext>`, or returns `None` if the filename does not follow that pattern.
fn uart_file_target_id(path: &Path) -> Option<&str> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.strip_prefix(UART_FILE_PREFIX))
        .filter(|id| !id.is_empty())
}

/// Derives the 16-character hexadecimal target ID for a target filter string.
///
/// Accepts an explicit 16-character hexadecimal target ID directly so orphaned sockets or
/// corrupted metadata files can be targeted by ID, and otherwise hashes the canonicalized
/// target path.
pub(crate) fn resolve_target_filter_id(target: &str) -> String {
    let raw_target = target.strip_prefix("uart:").unwrap_or(target);
    if is_target_id(raw_target) {
        raw_target.to_ascii_lowercase()
    } else {
        get_target_id(&canonicalize_target(raw_target))
    }
}

/// Checks whether `file_target_id` or `meta` matches the optional `target_filter_id`.
///
/// Returns `true` when `target_filter_id` is `None`. Matching either the filename target ID or
/// the metadata target ID ensures that a target-scoped `doctor` run still inspects and flags
/// files where only one of the filename or internal metadata matches the requested target.
fn matches_target_filter(
    target_filter_id: Option<&str>,
    file_target_id: &str,
    meta: Option<&ConnectionMetadata>,
) -> bool {
    let Some(expected_id) = target_filter_id else {
        return true;
    };
    if file_target_id == expected_id {
        return true;
    }
    match meta {
        Some(m) => match m.id.as_deref() {
            Some(meta_id) => meta_id == expected_id,
            None => get_target_id(&canonicalize_target(&m.target)) == expected_id,
        },
        None => false,
    }
}

/// Returns `(json_path, target_id, ConnectionMetadata)` for all valid metadata files matching `target`.
fn matching_metadata_entries(
    context: &EnvironmentContext,
    target: Option<&str>,
) -> Vec<(PathBuf, String, ConnectionMetadata)> {
    let shared_path = get_shared_uart_path(context).unwrap_or_default();
    let Ok(entries) = fs::read_dir(&shared_path) else {
        return Vec::new();
    };
    let target_filter_id = target.map(resolve_target_filter_id);
    let mut results = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().map_or(false, |ext| ext == "json") {
            let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or_default();
            let file_target_id = filename
                .strip_prefix("ffx_uart_")
                .and_then(|f| f.strip_suffix(".json"))
                .unwrap_or_default()
                .to_string();

            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(meta) = serde_json::from_str::<ConnectionMetadata>(&content) {
                    let target_id = if !file_target_id.is_empty() {
                        file_target_id
                    } else {
                        meta.id
                            .clone()
                            .unwrap_or_else(|| get_target_id(&canonicalize_target(&meta.target)))
                    };
                    if matches_target_filter(target_filter_id.as_deref(), &target_id, Some(&meta)) {
                        results.push((path, target_id, meta));
                    }
                }
            }
        }
    }
    results
}

/// Verifies metadata JSON integrity and detects stale metadata for dead daemon PIDs.
#[derive(Debug)]
pub struct MetadataCheck;

impl DiagnosticCheck for MetadataCheck {
    fn name(&self) -> &'static str {
        "Metadata Integrity"
    }

    async fn diagnose(
        &self,
        context: &EnvironmentContext,
        target: Option<&str>,
    ) -> Result<CheckStatus> {
        let shared_path = get_shared_uart_path(context)?;
        if !shared_path.exists() {
            return Ok(CheckStatus::Success);
        }

        let target_filter_id = target.map(resolve_target_filter_id);

        let entries = match fs::read_dir(&shared_path) {
            Ok(e) => e,
            Err(e) => {
                return Ok(CheckStatus::Failure {
                    message: format!("Failed to read metadata directory {:?}: {}", shared_path, e),
                    fix_type: None,
                });
            }
        };

        let mut failures = Vec::new();
        let mut warnings = Vec::new();
        let mut target_ids = BTreeSet::new();

        for entry in entries.flatten() {
            let path = entry.path();
            if !is_file_or_socket(&path) {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            let Some(target_id) = uart_file_target_id(&path).map(str::to_string) else {
                continue;
            };

            if matches!(ext, Some(UNIX_SOCKET_EXTENSION | CONTROL_SOCKET_EXTENSION)) {
                if path.with_extension(METADATA_FILE_EXTENSION).exists()
                    || !matches_target_filter(target_filter_id.as_deref(), &target_id, None)
                {
                    continue;
                }
                // A starting driver binds its data socket before replacing a stale control
                // socket, so a listener on any of the target's sockets means its files are in use.
                // This matches the `has_live_connection` check that `--fix` applies before
                // repairing.
                if any_socket_has_listener(&shared_path, &target_id).await {
                    warnings.push(format!(
                        "UART socket {:?} has no metadata JSON, but target '{target_id}' has an \
                         active listener; its driver may still be starting",
                        path
                    ));
                } else {
                    failures.push(format!("Orphaned UART file {:?} without metadata JSON", path));
                    target_ids.insert(target_id);
                }
                continue;
            }

            if ext != Some(METADATA_FILE_EXTENSION) {
                continue;
            }

            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    if matches_target_filter(target_filter_id.as_deref(), &target_id, None) {
                        failures.push(format!("Unreadable metadata file {:?}: {}", path, e));
                        target_ids.insert(target_id);
                    }
                    continue;
                }
            };

            let meta: ConnectionMetadata = match serde_json::from_str(&content) {
                Ok(m) => m,
                Err(_) => {
                    if matches_target_filter(target_filter_id.as_deref(), &target_id, None) {
                        failures.push(format!("Corrupted metadata JSON in {:?}", path));
                        target_ids.insert(target_id);
                    }
                    continue;
                }
            };

            if !matches_target_filter(target_filter_id.as_deref(), &target_id, Some(&meta)) {
                continue;
            }

            if let Some(found_id) = mismatched_metadata_id(&meta, &target_id) {
                failures.push(format!(
                    "Mismatched metadata target ID in {:?} (expected '{}', found '{}')",
                    path, target_id, found_id
                ));
                target_ids.insert(target_id);
                continue;
            }

            if is_driver_running(meta.pid) {
                continue;
            }
            // A dead PID with a live socket listener means a new driver is replacing the old one
            // and has not yet rewritten the metadata.
            if any_socket_has_listener(&shared_path, &target_id).await {
                warnings.push(format!(
                    "Metadata for target '{}' names dead PID {}, but its socket has an active \
                     listener; a new driver may still be starting",
                    meta.target, meta.pid
                ));
            } else {
                failures.push(format!(
                    "Stale metadata found for target '{}' (PID {} is dead)",
                    meta.target, meta.pid
                ));
                target_ids.insert(target_id);
            }
        }

        if !failures.is_empty() {
            failures.extend(warnings.into_iter().map(|w| format!("Warning: {w}")));
            return Ok(CheckStatus::Failure {
                message: failures.join("\n    "),
                fix_type: Some(FixType::DeleteMetadataFiles {
                    target_ids: target_ids.into_iter().collect(),
                }),
            });
        }
        if !warnings.is_empty() {
            return Ok(CheckStatus::Warning(warnings.join("\n    ")));
        }

        Ok(CheckStatus::Success)
    }
}

/// Detects when a serial character device is exclusively locked by an external or stale process.
#[derive(Debug)]
pub struct PortLockCheck;

impl PortLockCheck {
    /// Finds the PID holding an exclusive `FLOCK` `WRITE` lock on `(dev, ino)` in `/proc/locks`.
    fn find_flock_holder_pid(dev: u64, ino: u64) -> Option<u32> {
        let locks_data = fs::read_to_string("/proc/locks").ok()?;
        let major = nix::sys::stat::major(dev);
        let minor = nix::sys::stat::minor(dev);
        let expected_dev_ino = format!("{:02x}:{:02x}:{}", major, minor, ino);

        for line in locks_data.lines() {
            if line.contains("FLOCK") && line.contains("WRITE") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 6 && parts[5] == expected_dev_ino {
                    if let Ok(pid) = parts[4].parse::<u32>() {
                        return Some(pid);
                    }
                }
            }
        }
        None
    }

    fn check_port_lock(path: PathBuf, active_daemon_pid: Option<u32>) -> Option<CheckStatus> {
        if !path.exists() || path.to_string_lossy().starts_with("tcp:") {
            return None;
        }
        let metadata = fs::metadata(&path).ok()?;
        if !metadata.file_type().is_char_device() {
            return None;
        }
        // `O_NONBLOCK` keeps the probe from waiting on carrier detect, and `O_NOCTTY` keeps the
        // device from becoming this process's controlling terminal.
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOCTTY)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Some(CheckStatus::Failure {
                    message: format!(
                        "Permission denied opening port {:?}. Check dialout/plugdev group permissions.",
                        path
                    ),
                    fix_type: None,
                });
            }
            Err(_) => return None,
        };

        #[allow(deprecated)]
        let res = nix::fcntl::flock(file.as_raw_fd(), nix::fcntl::FlockArg::LockExclusiveNonblock);
        match res {
            Err(nix::errno::Errno::EWOULDBLOCK) => {
                if let Some(pid) = Self::find_flock_holder_pid(metadata.dev(), metadata.ino()) {
                    if active_daemon_pid == Some(pid) {
                        return None;
                    }
                    // Only offer to kill leaked `ffx-uart-driver` processes; terminal programs
                    // such as minicom or picocom must be closed by the user.
                    if is_driver_running(pid) {
                        return Some(CheckStatus::Failure {
                            message: format!(
                                "Port {:?} is locked exclusively by a stale ffx-uart-driver process (PID {}).",
                                path, pid
                            ),
                            fix_type: Some(FixType::KillDriverLockingPort { pid, port_path: path }),
                        });
                    }
                    return Some(CheckStatus::Failure {
                        message: format!(
                            "Port {:?} is locked exclusively by another process (PID {}). Close that process and retry.",
                            path, pid
                        ),
                        fix_type: None,
                    });
                } else if active_daemon_pid.is_none() {
                    return Some(CheckStatus::Failure {
                        message: format!(
                            "Port {:?} is locked exclusively by another process.",
                            path
                        ),
                        fix_type: None,
                    });
                }
            }
            Ok(()) => {
                #[allow(deprecated)]
                let _ = nix::fcntl::flock(file.as_raw_fd(), nix::fcntl::FlockArg::Unlock);
            }
            _ => {}
        }
        None
    }
}

impl DiagnosticCheck for PortLockCheck {
    fn name(&self) -> &'static str {
        "Exclusive Port Lock"
    }

    async fn diagnose(
        &self,
        context: &EnvironmentContext,
        target: Option<&str>,
    ) -> Result<CheckStatus> {
        let mut targets_to_check: Vec<(String, Option<u32>)> =
            matching_metadata_entries(context, target)
                .into_iter()
                .map(|(_p, _id, meta)| {
                    let active_pid =
                        if is_driver_running(meta.pid) { Some(meta.pid) } else { None };
                    (meta.target, active_pid)
                })
                .collect();

        if let Some(t) = target {
            if targets_to_check.is_empty() {
                targets_to_check.push((t.to_string(), None));
            }
        }

        for (t, active_daemon_pid) in targets_to_check {
            if let Some(failure) = Self::check_port_lock(PathBuf::from(&t), active_daemon_pid) {
                return Ok(failure);
            }
        }

        Ok(CheckStatus::Success)
    }
}

/// Verifies that at least one active or stale UART connection record exists.
#[derive(Debug)]
pub struct ConnectionExistsCheck;

impl DiagnosticCheck for ConnectionExistsCheck {
    fn name(&self) -> &'static str {
        "Active Connection Presence"
    }

    async fn diagnose(
        &self,
        context: &EnvironmentContext,
        target: Option<&str>,
    ) -> Result<CheckStatus> {
        let shared_path = get_shared_uart_path(context)?;
        let target_filter = target.map(resolve_target_filter_id);

        let has_files = if !shared_path.exists() {
            false
        } else {
            let entries = match fs::read_dir(&shared_path) {
                Ok(e) => e,
                Err(e) => {
                    return Ok(CheckStatus::Failure {
                        message: format!(
                            "Failed to read metadata directory {:?}: {}",
                            shared_path, e
                        ),
                        fix_type: None,
                    });
                }
            };
            entries.flatten().any(|entry| {
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str());
                if !is_file_or_socket(&path)
                    || !matches!(
                        ext,
                        Some(
                            METADATA_FILE_EXTENSION
                                | UNIX_SOCKET_EXTENSION
                                | CONTROL_SOCKET_EXTENSION
                        )
                    )
                {
                    return false;
                }
                let Some(file_id) = uart_file_target_id(&path) else {
                    return false;
                };
                // Parse metadata when present so this check matches targets the same way
                // `MetadataCheck` does.
                let meta = (ext == Some(METADATA_FILE_EXTENSION))
                    .then(|| fs::read_to_string(&path).ok())
                    .flatten()
                    .and_then(|content| serde_json::from_str::<ConnectionMetadata>(&content).ok());
                matches_target_filter(target_filter.as_deref(), file_id, meta.as_ref())
            })
        };

        if !has_files {
            if let Some(target_spec) = target {
                Ok(CheckStatus::Warning(format!(
                    "No active or stale UART connections detected for target '{}'. Run `ffx uart connect` to establish a connection.",
                    target_spec
                )))
            } else {
                Ok(CheckStatus::Warning(
                    "No active or stale UART connections detected. Run `ffx uart connect` to establish a connection."
                        .to_string(),
                ))
            }
        } else {
            Ok(CheckStatus::Success)
        }
    }
}
