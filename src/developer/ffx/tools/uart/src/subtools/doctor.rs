// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::args::DoctorCommand;
use crate::doctor::{CheckStatus, ConnectionExistsCheck, DiagnosticCheck, FixType, MetadataCheck};
use crate::metadata::{
    CONTROL_SOCKET_EXTENSION, ConnectionMetadata, METADATA_FILE_EXTENSION, UART_FILE_PREFIX,
    UNIX_SOCKET_EXTENSION, find_active_connection, get_shared_uart_path, is_target_id,
};
use crate::sys::{get_peer_pid, is_driver_running};
use ffx_config::EnvironmentContext;
use ffx_writer::{SimpleWriter, ToolIO};
use fho::{FfxMain, FfxTool, Result, user_error};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tokio::net::UnixStream;

const UART_FILE_EXTENSIONS: [&str; 3] =
    [METADATA_FILE_EXTENSION, UNIX_SOCKET_EXTENSION, CONTROL_SOCKET_EXTENSION];

/// Returns the path of the UART file with extension `ext` for `target_id` under `shared_path`.
fn uart_file_path(shared_path: &Path, target_id: &str, ext: &str) -> PathBuf {
    shared_path.join(format!("{UART_FILE_PREFIX}{target_id}.{ext}"))
}

#[derive(Default)]
struct DoctorCounts {
    issues_found: usize,
    issues_repaired: usize,
}

/// Subtool for diagnosing and optionally repairing UART connections (`ffx uart doctor`).
#[derive(Debug, FfxTool)]
#[target(None)]
pub struct DoctorTool {
    #[command]
    pub(crate) cmd: DoctorCommand,
    pub(crate) context: EnvironmentContext,
}

impl DoctorTool {
    fn has_any_file_for_target_id(&self, target_id: &str) -> bool {
        let Ok(shared_path) = get_shared_uart_path(&self.context) else {
            return false;
        };
        UART_FILE_EXTENSIONS
            .into_iter()
            .any(|ext| fs::symlink_metadata(uart_file_path(&shared_path, target_id, ext)).is_ok())
    }

    /// Locates a running `ffx-uart-driver` PID for `target_id` via metadata or active sockets.
    async fn find_running_daemon_pid(shared_path: &Path, target_id: &str) -> Option<u32> {
        let json_path = uart_file_path(shared_path, target_id, METADATA_FILE_EXTENSION);
        if let Ok(content) = fs::read_to_string(&json_path)
            && let Ok(meta) = serde_json::from_str::<ConnectionMetadata>(&content)
            && is_driver_running(meta.pid)
        {
            return Some(meta.pid);
        }

        for ext in [CONTROL_SOCKET_EXTENSION, UNIX_SOCKET_EXTENSION] {
            let socket_path = uart_file_path(shared_path, target_id, ext);
            if fs::symlink_metadata(&socket_path).is_err() {
                continue;
            }
            if let Ok(Ok(stream)) = tokio::time::timeout(
                crate::driver::SOCKET_PROBE_TIMEOUT,
                UnixStream::connect(&socket_path),
            )
            .await
                && let Ok(pid) = get_peer_pid(&stream)
                && is_driver_running(pid)
            {
                return Some(pid);
            }
        }
        None
    }

    /// Terminates any running daemon process for `target_id` so it releases its serial port
    /// lock, then removes the associated metadata and socket files.
    async fn terminate_and_delete_files_for_target_id(&self, target_id: &str) -> Result<()> {
        let shared_path = get_shared_uart_path(&self.context)?;
        if let Some(pid) = Self::find_running_daemon_pid(&shared_path, target_id).await {
            crate::driver::terminate_process(pid).await?;
        }
        for ext in UART_FILE_EXTENSIONS {
            let path = uart_file_path(&shared_path, target_id, ext);
            if let Err(e) = fs::remove_file(&path)
                && e.kind() != ErrorKind::NotFound
            {
                return Err(user_error!(
                    "Failed to remove {} file at {}: {}",
                    ext,
                    path.display(),
                    e
                ));
            }
        }
        Ok(())
    }

    /// Normalizes a user-supplied target specifier without requiring an existing healthy
    /// UART metadata record, so diagnostics can inspect disconnected, corrupted, or orphaned files.
    fn resolve_doctor_target(&self, spec: &str) -> Result<String> {
        let raw_target = spec.strip_prefix("uart:").unwrap_or(spec);
        if raw_target.is_empty() {
            return Err(user_error!("Empty target specifier."));
        }
        if raw_target.contains('/') {
            return Ok(raw_target.to_string());
        }
        if is_target_id(raw_target)
            && self.has_any_file_for_target_id(&raw_target.to_ascii_lowercase())
        {
            return Ok(raw_target.to_ascii_lowercase());
        }
        if let Ok(Some((_path, meta))) = find_active_connection(&self.context, raw_target) {
            return Ok(meta.target);
        }
        Ok(raw_target.to_string())
    }

    async fn apply_fix(&self, fix: FixType, _writer: &mut SimpleWriter) -> Result<()> {
        match fix {
            FixType::DeleteMetadataFiles { target_ids } => {
                for target_id in target_ids {
                    self.terminate_and_delete_files_for_target_id(&target_id).await?;
                }
                Ok(())
            }
        }
    }

    async fn run_check<C: DiagnosticCheck>(
        &self,
        check: C,
        target: Option<&str>,
        counts: &mut DoctorCounts,
        writer: &mut SimpleWriter,
    ) -> Result<()> {
        writer.print(format!("Checking {}...\n", check.name()))?;
        match check.diagnose(&self.context, target).await {
            Ok(CheckStatus::Success) => writer.print("  [✓] Pass\n")?,
            Ok(CheckStatus::Warning(msg)) => writer.print(format!("  [!] WARNING: {}\n", msg))?,
            Ok(CheckStatus::Failure { message, fix_type }) => {
                counts.issues_found += 1;
                writer.print(format!("  [✗] FAILED: {}\n", message))?;
                if let Some(fix) = fix_type {
                    if self.cmd.fix {
                        writer.print("    Attempting repair... ")?;
                        match self.apply_fix(fix, writer).await {
                            Ok(()) => {
                                writer.print("OK\n")?;
                                counts.issues_repaired += 1;
                            }
                            Err(e) => writer.print(format!("FAILED: {}\n", e))?,
                        }
                    } else {
                        writer.print("    (Run with --fix to attempt repair)\n")?;
                    }
                }
            }
            Err(e) => {
                counts.issues_found += 1;
                writer.print(format!("  [✗] ERROR running check: {}\n", e))?;
            }
        }
        Ok(())
    }

    /// Executes all diagnostic checks and optionally applies automated fixes.
    pub(crate) async fn doctor(
        &self,
        target: Option<String>,
        writer: &mut SimpleWriter,
    ) -> Result<()> {
        let mut counts = DoctorCounts::default();
        let target_str = target.as_deref();

        self.run_check(ConnectionExistsCheck, target_str, &mut counts, writer).await?;
        self.run_check(MetadataCheck, target_str, &mut counts, writer).await?;

        let DoctorCounts { issues_found, issues_repaired } = counts;
        writer.print(format!(
            "\nStatus: {} issues found ({} repaired).\n",
            issues_found, issues_repaired
        ))?;

        if issues_found > issues_repaired {
            Err(user_error!("Doctor found unresolved issues."))
        } else {
            Ok(())
        }
    }
}

impl FfxMain for DoctorTool {
    type Writer = SimpleWriter;
    type Error = fho::Error;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        let target_specifier = ffx_target::get_target_specifier(&self.context)?;
        let target = match target_specifier {
            Some(spec) => Some(self.resolve_doctor_target(&spec)?),
            None => None,
        };
        self.doctor(target, &mut writer).await
    }
}
