// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::doctor_ledger::{DoctorLedger, LedgerMode, LedgerOutcome};
use anyhow::Result;
use ffx_config::EnvironmentContext;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FindUsbDriverError {
    #[error("ffx-usb-driver is not running.")]
    DriverIsNotRunning,
    #[error("Could not get information for ffx-usb-driver process PID{0}")]
    UnableToFindSocket(u32),
    #[error("ffx-usb-driver process found, but could not determine socket path.")]
    UnableToDetermineSocket,
    #[error("Io Error")]
    IoError(#[from] std::io::Error),
    #[error("Invalid PID")]
    InvalidPid(#[from] std::num::ParseIntError),
    #[error("`ss` command failed. stderr: {0}")]
    SsCommandFailed(String),
    #[error("Missing required command{}: {}", if .0.len() > 1 { "s"} else {""}, .0.join(","))]
    CommandsMissing(Vec<String>),
}

#[derive(Debug)]
pub struct UsbDriverStatus {
    pub pid: u32,
    pub socket_path: String,
}

#[mockall::automock]
pub trait UsbDriverFinder {
    async fn find(&self) -> Result<Vec<UsbDriverStatus>, FindUsbDriverError>;
}

pub struct CommandUsbDriverFinder {}

impl CommandUsbDriverFinder {
    pub fn command_exists_in_path<P>(exe_name: P) -> bool
    where
        P: AsRef<std::path::Path>,
    {
        use std::env;
        env::var_os("PATH")
            .and_then(|paths| {
                env::split_paths(&paths)
                    .filter_map(|dir| {
                        let full_path = dir.join(&exe_name);
                        if full_path.is_file() { Some(full_path) } else { None }
                    })
                    .next()
            })
            .is_some()
    }
}

impl UsbDriverFinder for CommandUsbDriverFinder {
    async fn find(&self) -> Result<Vec<UsbDriverStatus>, FindUsbDriverError> {
        let pgrep = "pgrep";
        let ss = "ss";
        let mut non_nonexistent_commands = vec![];
        if !Self::command_exists_in_path(pgrep) {
            non_nonexistent_commands.push(pgrep.to_string());
        }
        if !Self::command_exists_in_path(ss) {
            non_nonexistent_commands.push(ss.to_string());
        }
        if non_nonexistent_commands.len() > 0 {
            return Err(FindUsbDriverError::CommandsMissing(non_nonexistent_commands));
        }

        log::trace!("Executing pgrep");
        let pgrep_output = Command::new(pgrep).arg("-f").arg("ffx-usb-driver").output()?;

        if !pgrep_output.status.success() || pgrep_output.stdout.is_empty() {
            return Err(FindUsbDriverError::DriverIsNotRunning);
        }

        let pid_str = String::from_utf8_lossy(&pgrep_output.stdout);
        let mut statuses = Vec::new();

        let ss_output = Command::new(ss).arg("-lxp").output()?;
        if !ss_output.status.success() {
            let stderr = String::from_utf8_lossy(&ss_output.stderr);
            return Err(FindUsbDriverError::SsCommandFailed(stderr.to_string()));
        }
        let ss_stdout = String::from_utf8_lossy(&ss_output.stdout);

        for line in pid_str.lines() {
            let pid: u32 = line.parse()?;

            // The output of `ss -lxp` has the socket path in the 5th column (index 4)
            // for listening unix sockets.
            // e.g. u_str LISTEN 0 128 /tmp/ffx-usb-123.sock 12345 * 0 users:(("...",...))
            let socket_path = ss_stdout
                .lines()
                .find(|ss_line| ss_line.contains(&format!("pid={}", pid)))
                .and_then(|ss_line| ss_line.split_whitespace().nth(4));

            if let Some(socket_path) = socket_path {
                statuses.push(UsbDriverStatus { pid, socket_path: socket_path.to_string() });
            } else {
                // If we can't find the socket for one of the pids, we should probably
                // return an error for that specific one.
                return Err(FindUsbDriverError::UnableToFindSocket(pid));
            }
        }

        if statuses.is_empty() {
            return Err(FindUsbDriverError::UnableToDetermineSocket);
        }

        Ok(statuses)
    }
}

pub async fn check_usb_driver<W: Write, D: UsbDriverFinder>(
    finder: &D,
    ledger: &mut DoctorLedger<W>,
    env_context: &EnvironmentContext,
) -> Result<()> {
    if !env_context.get(ffx_config::keys::USB_ENABLED).unwrap_or(false) {
        return Ok(());
    }
    let usb_driver_node = ledger.add_node("FFX USB Driver", LedgerMode::Automatic)?;

    let usb_driver_statuses = match finder.find().await {
        Ok(statuses) => statuses,
        Err(FindUsbDriverError::DriverIsNotRunning) => {
            let info_node = ledger.add_node(
                "The ffx-usb-driver is not running. It should be started automatically when \
                needed. If this error persists and there are ongoing issues communicating with the \
                target, this may be a bug.",
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(info_node, LedgerOutcome::Warning)?;
            ledger.close(usb_driver_node)?;
            return Ok(());
        }
        Err(e) => {
            let node = ledger.add_node(&format!("{}", e), LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(usb_driver_node)?;
            return Ok(());
        }
    };
    let pid_socket_ledger_mode = if usb_driver_statuses.len() > 1 {
        let warning_node =
            ledger.add_node("Multiple ffx-usb-driver processes running.", LedgerMode::Automatic)?;
        ledger.set_outcome(warning_node, LedgerOutcome::Warning)?;
        LedgerMode::Automatic
    } else {
        LedgerMode::Verbose
    };

    let expected_socket_path: PathBuf =
        match env_context.get(usb_driver_api::CONFIG_USB_SOCKET_PATH) {
            Ok(pb) => pb,
            Err(e) => {
                let warning_node = ledger.add_node(
                    format!(
                        "Could not find USB Driver Socket path with config {}. Error: {}",
                        usb_driver_api::CONFIG_USB_SOCKET_PATH,
                        e,
                    )
                    .as_str(),
                    LedgerMode::Automatic,
                )?;
                ledger.set_outcome(warning_node, LedgerOutcome::Warning)?;
                ledger.close(usb_driver_node)?;
                return Ok(());
            }
        };

    for usb_driver_status in usb_driver_statuses {
        let UsbDriverStatus { pid, socket_path } = usb_driver_status;

        let running_node = ledger.add_node("ffx-usb-driver is running.", LedgerMode::Automatic)?;
        let pid_node = ledger.add_node(&format!("PID: {}", pid), pid_socket_ledger_mode)?;
        ledger.set_outcome(pid_node, LedgerOutcome::Success)?;
        let socket_node =
            ledger.add_node(&format!("Socket: {}", socket_path), pid_socket_ledger_mode)?;
        ledger.set_outcome(socket_node, LedgerOutcome::Success)?;

        if expected_socket_path.as_path() != std::path::Path::new(&socket_path) {
            let warning_node = ledger.add_node(
                &format!(
                    "ffx-usb-driver is listening on a different socket than what is configured: {}. Expected: {}",
                    socket_path,
                    expected_socket_path.display()
                ),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(warning_node, LedgerOutcome::Warning)?;
            ledger.close(warning_node)?;
        }
        ledger.close(running_node)?;
    }
    ledger.close(usb_driver_node)?;
    Ok(())
}
