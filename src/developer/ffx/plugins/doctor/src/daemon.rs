// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::doctor_ledger::{DoctorLedger, LedgerMode, LedgerOutcome};
use crate::types::{get_abi_revision, get_api_level};
use anyhow::Result;
use doctor_utils::DaemonManager;
use ffx_build_version::VersionInfo;
use fidl_fuchsia_developer_ffx::DaemonProxy;
use std::collections::HashSet;
use std::io::Write;
use std::time::Duration;
use timeout::timeout;

pub async fn get_daemon_pid<W: Write>(
    daemon_manager: &impl DaemonManager,
    ledger: &mut DoctorLedger<W>,
) -> Option<Vec<usize>> {
    match daemon_manager.get_pid().await {
        Ok(vec) => return Some(vec),
        Err(e) => {
            let node = ledger
                .add_node(&format!("Error getting daemon pid: {}", e), LedgerMode::Automatic)
                .ok()?;
            ledger.set_outcome(node, LedgerOutcome::SoftWarning).ok()?;
            return None;
        }
    }
}

// Return the elements of `a` that are not in `b`.
// Note: this function preserves order for simpler testing.
pub fn difference(a: &[usize], b: &[usize]) -> Vec<usize> {
    let sb: HashSet<usize> = b.iter().copied().collect();
    a.iter().filter(|&e| !sb.contains(e)).copied().collect()
}

#[derive(Default)]
pub struct DaemonPidState {
    pub has_error: bool,
    pub current_pids: Vec<usize>,
    pub added_pids: Vec<usize>,
    pub dropped_pids: Vec<usize>,
}

impl DaemonPidState {
    pub fn new() -> Self {
        Self::default()
    }

    // Update the current, the added, and the dropped daemon pids.
    // Display if there are any errors while fetching the pids.
    // Note: we display pid fetching error only one time. has_error is set once there is an error.
    pub async fn update<W: Write>(
        &mut self,
        daemon_manager: &impl DaemonManager,
        ledger: &mut DoctorLedger<W>,
    ) {
        self.added_pids.clear();
        self.dropped_pids.clear();

        if self.has_error {
            self.current_pids.clear();
            return;
        }

        // Get pid vector
        let new_pids = match get_daemon_pid(daemon_manager, ledger).await {
            Some(v) => v,
            None => {
                self.current_pids.clear();
                self.has_error = true;
                return;
            }
        };

        // Update
        self.added_pids.extend(difference(&new_pids, &self.current_pids));
        self.dropped_pids.extend(difference(&self.current_pids, &new_pids));
        self.current_pids.clear();
        self.current_pids.extend(new_pids);
    }
}

pub async fn daemon_restart<W: Write>(
    daemon_manager: &impl DaemonManager,
    retry_delay: Duration,
    ledger: &mut DoctorLedger<W>,
) -> Result<()> {
    let mut main_node = ledger.add_node("Killing Daemon", LedgerMode::Automatic)?;

    let mut pid_state = DaemonPidState::new();

    pid_state.update(daemon_manager, ledger).await;

    // Kill the daemon if it is running.
    let daemon_killed = if daemon_manager.is_daemon_running().await {
        let node = ledger.add_node("Killing running daemons.", LedgerMode::Automatic)?;
        if let Err(e) = daemon_manager.kill_all().await {
            let _ = ledger.close(main_node);
            return Err(e.into());
        }
        ledger.set_outcome(node, LedgerOutcome::Success)?;
        true
    } else {
        match daemon_manager.kill_all().await {
            Ok(true) => {
                let node = ledger.add_node("Killing zombie daemons.", LedgerMode::Automatic)?;
                ledger.set_outcome(node, LedgerOutcome::Success)?;
                true
            }
            Ok(false) => {
                let node = ledger.add_node("No running daemons found.", LedgerMode::Automatic)?;
                ledger.set_outcome(node, LedgerOutcome::Success)?;
                false
            }
            Err(e) => {
                let _ = ledger.close(main_node);
                return Err(e.into());
            }
        }
    };

    // Display killed daemon PIDs.
    pid_state.update(daemon_manager, ledger).await;
    if daemon_killed && !pid_state.has_error {
        {
            let node = ledger.add_node(
                &format!("Killed daemon PID: {:?}", pid_state.dropped_pids),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
        }

        if !pid_state.current_pids.is_empty() {
            let node = ledger.add_node(
                &format!("Daemon are still running, PID: {:?}", pid_state.current_pids),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Warning)?;
        }
    }

    ledger.close(main_node)?;

    if daemon_killed {
        // HACK: Wait a few seconds before spawning a new daemon. Attempting
        // to spawn one too quickly after killing one will lead to timeouts
        // when attempting to communicate with the spawned daemon.
        // Temporary fix for https://fxbug.dev/42145822. Remove when that bug is resolved.
        fuchsia_async::Timer::new(Duration::from_millis(5000)).await;
    };

    main_node = ledger.add_node("Starting Daemon", LedgerMode::Automatic)?;

    // Spawn daemon.
    match timeout(retry_delay, daemon_manager.spawn()).await {
        Ok(Ok(_)) => {
            let node = ledger.add_node("Daemon spawned", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
        }
        Ok(Err(e)) => {
            let node =
                ledger.add_node(&format!("Error spawning daemon: {}", e), LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
        Err(_) => {
            let node = ledger.add_node("Timeout spawning daemon", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
    }

    pid_state.update(daemon_manager, ledger).await;
    if !pid_state.has_error {
        let node = ledger
            .add_node(&format!("Daemon PID: {:?}", pid_state.added_pids), LedgerMode::Automatic)?;
        ledger.set_outcome(node, LedgerOutcome::Success)?;
    }

    // Check daemon connection.
    let daemon_proxy = match timeout(retry_delay, daemon_manager.find_and_connect()).await {
        Ok(Ok(val)) => {
            let node = ledger.add_node("Connected to daemon", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
            val
        }
        Ok(Err(e)) => {
            let node = ledger.add_node(
                &format!("Error connecting to daemon: {}. Run `ffx doctor --restart-daemon`", e),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
        Err(_) => {
            let node = ledger.add_node(
                "Timeout while connecting to daemon. Run `ffx doctor --restart-daemon`",
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
    };

    match timeout(retry_delay, daemon_proxy.get_version_info()).await {
        Ok(Ok(v)) => {
            let daemon_version = v.build_version.clone().unwrap_or_else(|| "UNKNOWN".to_string());
            let node = ledger
                .add_node(&format!("Daemon version: {}", daemon_version), LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;

            let node = ledger.add_node(
                &format!("abi-revision: {}", get_abi_revision(v.abi_revision)),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;

            let node = ledger.add_node(
                &format!("api-level: {}", get_api_level(v.api_level)),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
        }
        Ok(Err(e)) => {
            let node = ledger
                .add_node(&format!("Error getting daemon version: {}", e), LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
        Err(_) => {
            let node =
                ledger.add_node("Timeout while getting daemon version", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
    }

    ledger.close(main_node)?;
    Ok(())
}

pub async fn doctor_daemon_restart<W: Write>(
    daemon_manager: &impl DaemonManager,
    spawn_delay: Duration,
    ledger: &mut DoctorLedger<W>,
) -> Result<()> {
    match daemon_restart(daemon_manager, spawn_delay, ledger).await {
        Err(err) => {
            let node = ledger.add_node(&format!("Error: {}", err), LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
        }
        _ => (),
    };
    // Deliberately return Ok(()) to allow other diagnostics to proceed even if daemon restart fails.
    Ok(())
}

pub async fn check_daemon_status<W: Write>(
    ledger: &mut DoctorLedger<W>,
    direct_mode: bool,
    daemon_manager: &impl DaemonManager,
    retry_delay: Duration,
    version_info: &VersionInfo,
    target_spec: &Result<Option<String>, String>,
) -> Result<Option<DaemonProxy>> {
    let main_node = ledger.add_node("Checking daemon", LedgerMode::Automatic)?;

    if daemon_manager.is_daemon_running().await {
        let pid_vec = get_daemon_pid(daemon_manager, ledger).await.unwrap_or_default();
        let node =
            ledger.add_node(&format!("Daemon found: {:?}", pid_vec), LedgerMode::Automatic)?;
        ledger.set_outcome(node, LedgerOutcome::Success)?;
    } else {
        if direct_mode {
            let node = ledger.add_node("No running daemons found.", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Info)?;
            if let Err(e) = report_default_target(ledger, target_spec) {
                let _ = ledger.close(main_node);
                return Err(e);
            }
        } else {
            let node = ledger.add_node(
                "No running daemons found. Run `ffx doctor --restart-daemon`",
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
        }
        ledger.close(main_node)?;
        return Ok(None);
    }

    let daemon_proxy = match timeout(retry_delay, daemon_manager.find_and_connect()).await {
        Ok(Ok(val)) => {
            let node = ledger.add_node("Connecting to daemon", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
            val
        }
        Ok(Err(e)) => {
            let node = ledger.add_node(
                &format!("Error connecting to daemon: {}. Run `ffx doctor --restart-daemon`", e),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(None);
        }
        Err(_) => {
            let node = ledger.add_node(
                "Timeout while connecting to daemon. Run `ffx doctor --restart-daemon`",
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(None);
        }
    };

    match timeout(retry_delay, daemon_proxy.get_version_info()).await {
        Ok(Ok(v)) => {
            let daemon_version = v.build_version.clone().unwrap_or_else(|| "UNKNOWN".to_string());
            let node = ledger
                .add_node(&format!("Daemon version: {}", daemon_version), LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;

            let path = std::env::current_exe().map(|x| x.to_string_lossy().to_string()).ok();
            let have_path = path.is_some();
            if let (Some(path), Some(exec_path)) = (path, v.exec_path.clone()) {
                if path != exec_path {
                    let node = ledger.add_node(
                        &format!("Daemon ran from {} but this command is {}. Run `ffx doctor --restart-daemon`", exec_path, path),
                        LedgerMode::Automatic,
                    )?;
                    ledger.set_outcome(node, LedgerOutcome::SoftWarning)?;
                }

                let node = ledger.add_node(&format!("path: {}", exec_path), LedgerMode::Verbose)?;
                ledger.set_outcome(node, LedgerOutcome::Success)?;
            } else if !have_path {
                let node = ledger.add_node(
                    "Could not get current command path to compare with daemon",
                    LedgerMode::Automatic,
                )?;
                ledger.set_outcome(node, LedgerOutcome::SoftWarning)?;
            } else {
                let node = ledger.add_node("Daemon is too old to report its executable path. Run `ffx doctor --restart-daemon`", LedgerMode::Automatic)?;
                ledger.set_outcome(node, LedgerOutcome::SoftWarning)?;
            }

            let node = ledger.add_node(
                &format!("abi-revision: {}", get_abi_revision(v.abi_revision)),
                LedgerMode::Verbose,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;

            let node = ledger.add_node(
                &format!("api-level: {}", get_api_level(v.api_level)),
                LedgerMode::Verbose,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;

            if v.api_level != version_info.api_level {
                let node = ledger.add_node("Daemon and frontend are at different API levels. Run `ffx doctor --restart-daemon`", LedgerMode::Automatic)?;
                ledger.set_outcome(node, LedgerOutcome::SoftWarning)?;
            }
        }
        Ok(Err(e)) => {
            let node = ledger
                .add_node(&format!("Error getting daemon version: {}", e), LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            // Continue, not a critical error.
        }
        Err(_) => {
            let node =
                ledger.add_node("Timeout while getting daemon version", LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            // Continue, not a critical error.
        }
    }

    if let Err(e) = report_default_target(ledger, target_spec) {
        let _ = ledger.close(main_node);
        return Err(e);
    }

    ledger.close(main_node)?;
    Ok(Some(daemon_proxy))
}

pub fn report_default_target<W: Write>(
    ledger: &mut DoctorLedger<W>,
    target_spec: &std::result::Result<Option<String>, String>,
) -> Result<()> {
    Ok(match target_spec {
        Ok(t) => {
            let default_target_display = {
                if t.is_none() || t.as_ref().unwrap().is_empty() {
                    "(none)".to_string()
                } else {
                    t.as_ref().unwrap().clone()
                }
            };
            let node = ledger.add_node(
                &format!("Default target: {}", default_target_display),
                LedgerMode::Verbose,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
        }
        Err(e) => {
            let node =
                ledger.add_node(&format!("config read failed: {:?}", e), LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
        }
    })
}
