// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Diagnostic checks and automatic repair for `ffx uart`.

use crate::metadata::{
    CONTROL_SOCKET_EXTENSION, ConnectionMetadata, METADATA_FILE_EXTENSION, UART_FILE_PREFIX,
    UNIX_SOCKET_EXTENSION, canonicalize_target, get_shared_uart_path, get_target_id, is_target_id,
};
use crate::sys::is_driver_running;
use ffx_config::EnvironmentContext;
use fho::Result;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

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
                if !path.with_extension(METADATA_FILE_EXTENSION).exists()
                    && matches_target_filter(target_filter_id.as_deref(), &target_id, None)
                {
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

            let computed_id = get_target_id(&canonicalize_target(&meta.target));
            let meta_id = meta.id.as_deref().unwrap_or(&computed_id);
            if meta_id != target_id || computed_id != target_id {
                let found_id = if meta_id != target_id { meta_id } else { &computed_id };
                failures.push(format!(
                    "Mismatched metadata target ID in {:?} (expected '{}', found '{}')",
                    path, target_id, found_id
                ));
                target_ids.insert(target_id);
                continue;
            }

            // If PID is not running, metadata is stale.
            if !is_driver_running(meta.pid) {
                failures.push(format!(
                    "Stale metadata found for target '{}' (PID {} is dead)",
                    meta.target, meta.pid
                ));
                target_ids.insert(target_id);
            }
        }

        if !failures.is_empty() {
            return Ok(CheckStatus::Failure {
                message: failures.join("\n    "),
                fix_type: Some(FixType::DeleteMetadataFiles {
                    target_ids: target_ids.into_iter().collect(),
                }),
            });
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
                target_filter.as_deref().is_none_or(|tid| file_id == tid)
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
