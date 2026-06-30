// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::doctor_ledger::{LedgerMode, LedgerNodeGuard, LedgerOutcome};
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
    ledger: &mut LedgerNodeGuard<'_, W>,
) -> Option<Vec<usize>> {
    match daemon_manager.get_pid().await {
        Ok(vec) => return Some(vec),
        Err(e) => {
            ledger
                .add_node_with_outcome(
                    &format!("Error getting daemon pid: {}", e),
                    LedgerMode::Automatic,
                    LedgerOutcome::SoftWarning,
                )
                .ok()?;
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
        ledger: &mut LedgerNodeGuard<'_, W>,
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
    ledger: &mut LedgerNodeGuard<'_, W>,
) -> Result<()> {
    let mut pid_state = DaemonPidState::new();
    let daemon_killed = {
        let mut main_node = ledger.add_node("Killing Daemon", LedgerMode::Automatic)?;
        pid_state.update(daemon_manager, &mut main_node).await;

        let killed = if daemon_manager.is_daemon_running().await {
            main_node.add_node_with_outcome(
                "Killing running daemons.",
                LedgerMode::Automatic,
                LedgerOutcome::Success,
            )?;
            daemon_manager.kill_all().await?;
            true
        } else {
            match daemon_manager.kill_all().await {
                Ok(true) => {
                    main_node.add_node_with_outcome(
                        "Killing zombie daemons.",
                        LedgerMode::Automatic,
                        LedgerOutcome::Success,
                    )?;
                    true
                }
                Ok(false) => {
                    main_node.add_node_with_outcome(
                        "No running daemons found.",
                        LedgerMode::Automatic,
                        LedgerOutcome::Success,
                    )?;
                    false
                }
                Err(e) => return Err(e.into()),
            }
        };

        pid_state.update(daemon_manager, &mut main_node).await;
        if killed && !pid_state.has_error {
            main_node.add_node_with_outcome(
                &format!("Killed daemon PID: {:?}", pid_state.dropped_pids),
                LedgerMode::Automatic,
                LedgerOutcome::Success,
            )?;

            if !pid_state.current_pids.is_empty() {
                main_node.add_node_with_outcome(
                    &format!("Daemon is still running, PID: {:?}", pid_state.current_pids),
                    LedgerMode::Automatic,
                    LedgerOutcome::Warning,
                )?;
            }
        }
        killed
    };

    if daemon_killed {
        // HACK: Wait a few seconds before spawning a new daemon. Attempting
        // to spawn one too quickly after killing one will lead to timeouts
        // when attempting to communicate with the spawned daemon.
        // Temporary fix for https://fxbug.dev/42145822. Remove when that bug is resolved.
        fuchsia_async::Timer::new(Duration::from_millis(5000)).await;
    };

    {
        let mut main_node = ledger.add_node("Starting Daemon", LedgerMode::Automatic)?;

        // Spawn daemon.
        match timeout(retry_delay, daemon_manager.spawn()).await {
            Ok(Ok(_)) => {
                main_node.add_node_with_outcome(
                    "Daemon spawned",
                    LedgerMode::Automatic,
                    LedgerOutcome::Success,
                )?;
            }
            Ok(Err(e)) => {
                main_node.add_node_with_outcome(
                    &format!("Error spawning daemon: {}", e),
                    LedgerMode::Automatic,
                    LedgerOutcome::Failure,
                )?;
                return Ok(());
            }
            Err(_) => {
                main_node.add_node_with_outcome(
                    "Timeout spawning daemon",
                    LedgerMode::Automatic,
                    LedgerOutcome::Failure,
                )?;
                return Ok(());
            }
        }

        pid_state.update(daemon_manager, &mut main_node).await;
        if !pid_state.has_error {
            main_node.add_node_with_outcome(
                &format!("Daemon PID: {:?}", pid_state.added_pids),
                LedgerMode::Automatic,
                LedgerOutcome::Success,
            )?;
        }

        // Check daemon connection.
        let daemon_proxy = match timeout(retry_delay, daemon_manager.find_and_connect()).await {
            Ok(Ok(val)) => {
                main_node.add_node_with_outcome(
                    "Connected to daemon",
                    LedgerMode::Automatic,
                    LedgerOutcome::Success,
                )?;
                val
            }
            Ok(Err(e)) => {
                main_node.add_node_with_outcome(
                    &format!(
                        "Error connecting to daemon: {}. Run `ffx doctor --restart-daemon`",
                        e
                    ),
                    LedgerMode::Automatic,
                    LedgerOutcome::Failure,
                )?;
                return Ok(());
            }
            Err(_) => {
                main_node.add_node_with_outcome(
                    "Timeout while connecting to daemon. Run `ffx doctor --restart-daemon`",
                    LedgerMode::Automatic,
                    LedgerOutcome::Failure,
                )?;
                return Ok(());
            }
        };

        match timeout(retry_delay, daemon_proxy.get_version_info()).await {
            Ok(Ok(v)) => {
                let daemon_version =
                    v.build_version.clone().unwrap_or_else(|| "UNKNOWN".to_string());
                main_node.add_node_with_outcome(
                    &format!("Daemon version: {}", daemon_version),
                    LedgerMode::Automatic,
                    LedgerOutcome::Success,
                )?;
                main_node.add_node_with_outcome(
                    &format!("abi-revision: {}", get_abi_revision(v.abi_revision)),
                    LedgerMode::Automatic,
                    LedgerOutcome::Success,
                )?;
                main_node.add_node_with_outcome(
                    &format!("api-level: {}", get_api_level(v.api_level)),
                    LedgerMode::Automatic,
                    LedgerOutcome::Success,
                )?;
            }
            Ok(Err(e)) => {
                main_node.add_node_with_outcome(
                    &format!("Error getting daemon version: {}", e),
                    LedgerMode::Automatic,
                    LedgerOutcome::Failure,
                )?;
                return Ok(());
            }
            Err(_) => {
                main_node.add_node_with_outcome(
                    "Timeout while getting daemon version",
                    LedgerMode::Automatic,
                    LedgerOutcome::Failure,
                )?;
                return Ok(());
            }
        }
    }
    Ok(())
}

pub async fn doctor_daemon_restart<W: Write>(
    daemon_manager: &impl DaemonManager,
    spawn_delay: Duration,
    ledger: &mut LedgerNodeGuard<'_, W>,
) -> Result<()> {
    match daemon_restart(daemon_manager, spawn_delay, ledger).await {
        Err(err) => {
            ledger.add_node_with_outcome(
                &format!("Error: {}", err),
                LedgerMode::Automatic,
                LedgerOutcome::Failure,
            )?;
        }
        _ => (),
    };
    // Deliberately return Ok(()) to allow other diagnostics to proceed even if daemon restart fails.
    Ok(())
}

pub async fn check_daemon_status<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
    direct_mode: bool,
    daemon_manager: &impl DaemonManager,
    retry_delay: Duration,
    version_info: &VersionInfo,
    target_spec: &Result<Option<String>, String>,
) -> Result<Option<DaemonProxy>> {
    let mut main_node = ledger.add_node("Checking daemon", LedgerMode::Automatic)?;

    if daemon_manager.is_daemon_running().await {
        let pid_vec = get_daemon_pid(daemon_manager, &mut main_node).await.unwrap_or_default();
        main_node.add_node_with_outcome(
            &format!("Daemon found: {:?}", pid_vec),
            LedgerMode::Automatic,
            LedgerOutcome::Success,
        )?;
    } else {
        if direct_mode {
            main_node.add_node_with_outcome(
                "No running daemons found.",
                LedgerMode::Automatic,
                LedgerOutcome::Info,
            )?;
            report_default_target(&mut main_node, target_spec)?;
        } else {
            main_node.add_node_with_outcome(
                "No running daemons found. Run `ffx doctor --restart-daemon`",
                LedgerMode::Automatic,
                LedgerOutcome::Failure,
            )?;
        }
        return Ok(None);
    }

    let daemon_proxy = match timeout(retry_delay, daemon_manager.find_and_connect()).await {
        Ok(Ok(val)) => {
            main_node.add_node_with_outcome(
                "Connecting to daemon",
                LedgerMode::Automatic,
                LedgerOutcome::Success,
            )?;
            val
        }
        Ok(Err(e)) => {
            main_node.add_node_with_outcome(
                &format!("Error connecting to daemon: {}. Run `ffx doctor --restart-daemon`", e),
                LedgerMode::Automatic,
                LedgerOutcome::Failure,
            )?;
            return Ok(None);
        }
        Err(_) => {
            main_node.add_node_with_outcome(
                "Timeout while connecting to daemon. Run `ffx doctor --restart-daemon`",
                LedgerMode::Automatic,
                LedgerOutcome::Failure,
            )?;
            return Ok(None);
        }
    };

    match timeout(retry_delay, daemon_proxy.get_version_info()).await {
        Ok(Ok(v)) => {
            let daemon_version = v.build_version.clone().unwrap_or_else(|| "UNKNOWN".to_string());
            main_node.add_node_with_outcome(
                &format!("Daemon version: {}", daemon_version),
                LedgerMode::Verbose,
                LedgerOutcome::Success,
            )?;

            let path = std::env::current_exe().map(|x| x.to_string_lossy().to_string()).ok();
            let have_path = path.is_some();
            if let (Some(path), Some(exec_path)) = (path, v.exec_path.clone()) {
                if path != exec_path {
                    main_node.add_node_with_outcome(
                        &format!("Daemon ran from {} but this command is {}. Run `ffx doctor --restart-daemon`", exec_path, path),
                        LedgerMode::Automatic,
                        LedgerOutcome::SoftWarning,
                    )?;
                }

                main_node.add_node_with_outcome(
                    &format!("path: {}", exec_path),
                    LedgerMode::Verbose,
                    LedgerOutcome::Success,
                )?;
            } else if !have_path {
                main_node.add_node_with_outcome(
                    "Could not get current command path to compare with daemon",
                    LedgerMode::Automatic,
                    LedgerOutcome::SoftWarning,
                )?;
            } else {
                main_node.add_node_with_outcome("Daemon is too old to report its executable path. Run `ffx doctor --restart-daemon`", LedgerMode::Automatic, LedgerOutcome::SoftWarning)?;
            }

            main_node.add_node_with_outcome(
                &format!("abi-revision: {}", get_abi_revision(v.abi_revision)),
                LedgerMode::Verbose,
                LedgerOutcome::Success,
            )?;

            main_node.add_node_with_outcome(
                &format!("api-level: {}", get_api_level(v.api_level)),
                LedgerMode::Verbose,
                LedgerOutcome::Success,
            )?;

            if v.api_level != version_info.api_level {
                main_node.add_node_with_outcome("Daemon and frontend are at different API levels. Run `ffx doctor --restart-daemon`", LedgerMode::Automatic, LedgerOutcome::SoftWarning)?;
            }
        }
        Ok(Err(e)) => {
            main_node.add_node_with_outcome(
                &format!("Error getting daemon version: {}", e),
                LedgerMode::Verbose,
                LedgerOutcome::Failure,
            )?;
        }
        Err(_) => {
            main_node.add_node_with_outcome(
                "Timeout while getting daemon version",
                LedgerMode::Verbose,
                LedgerOutcome::Failure,
            )?;
        }
    }

    report_default_target(&mut main_node, target_spec)?;

    Ok(Some(daemon_proxy))
}

pub fn report_default_target<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
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
            ledger.add_node_with_outcome(
                &format!("Default target: {}", default_target_display),
                LedgerMode::Verbose,
                LedgerOutcome::Success,
            )?;
        }
        Err(e) => {
            ledger.add_node_with_outcome(
                &format!("config read failed: {:?}", e),
                LedgerMode::Verbose,
                LedgerOutcome::Failure,
            )?;
        }
    })
}
