// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::doctor_ledger::{DoctorLedger, LedgerMode, LedgerOutcome};
use crate::types::{get_abi_revision, get_api_level};
use anyhow::Result;
use emulator_instance::{EmulatorInstanceInfo, EmulatorInstances};
use ffx_build_version::VersionInfo;
use ffx_config::{EnvironmentContext, print_config};
use ffx_ssh::{SshKeyErrorKind, SshKeyFiles};
use fuchsia_lockfile::{LockfileCreateError, LockfileCreateErrorKind};
use std::io::{BufWriter, Write};
use std::time::Duration;

const RECORD_CONFIG_SETTING: &str = "doctor.record_config";

pub async fn get_config_permission<W: Write>(
    context: &EnvironmentContext,
    mut writer: W,
) -> Result<bool> {
    match context.get(RECORD_CONFIG_SETTING) {
        Ok(true) => {
            writeln!(
                &mut writer,
                "Config recording is enabled - config data will be recorded. You can change this \
                     with `ffx config set doctor.record_config false"
            )?;
            return Ok(true);
        }
        Ok(false) => {
            writeln!(
                &mut writer,
                "Config recording is disabled - config data will not be recorded. You can change \
                     this with `ffx config set doctor.record_config true"
            )?;
            return Ok(false);
        }
        _ => (),
    }

    let permission: bool;
    loop {
        let mut input = String::new();
        writeln!(&mut writer, "Do you want to include your config data `ffx config get`? [y/n]")?;
        std::io::stdin().read_line(&mut input)?;
        permission = match input.to_lowercase().trim() {
            "yes" | "y" => true,
            "no" | "n" => false,
            _ => continue,
        };
        break;
    }

    writeln!(
        &mut writer,
        "You can permanently enable or disable including config data in doctor records with:"
    )?;
    writeln!(&mut writer, "`ffx config set {} [true|false]`", RECORD_CONFIG_SETTING)?;
    fuchsia_async::Timer::new(Duration::from_millis(1000)).await;

    Ok(permission)
}

pub fn get_user_config(ctx: &EnvironmentContext) -> Result<String> {
    let mut writer = BufWriter::new(Vec::new());
    print_config(ctx, &mut writer)?;
    let config_str = String::from_utf8(writer.into_inner()?)?;
    Ok(config_str)
}

pub async fn check_ffx_info<W: Write>(
    ledger: &mut DoctorLedger<W>,
    version_info: &VersionInfo,
) -> Result<usize> {
    let ffx_node = ledger.add_node("FFX doctor", LedgerMode::Automatic)?;
    let frontend_version =
        version_info.build_version.clone().unwrap_or_else(|| "UNKNOWN".to_string());
    let version_node =
        ledger.add_node(&format!("Frontend version: {}", frontend_version), LedgerMode::Verbose)?;
    ledger.set_outcome(version_node, LedgerOutcome::Success)?;

    let abi_revision_node = ledger.add_node(
        &format!("abi-revision: {}", get_abi_revision(version_info.abi_revision)),
        LedgerMode::Verbose,
    )?;
    ledger.set_outcome(abi_revision_node, LedgerOutcome::Success)?;

    let api_level_node = ledger.add_node(
        &format!("api-level: {}", get_api_level(version_info.api_level)),
        LedgerMode::Verbose,
    )?;
    ledger.set_outcome(api_level_node, LedgerOutcome::Success)?;

    let ffx_path = match std::env::current_exe() {
        Ok(path) => format!("{}", path.display()),
        _ => "not found".to_string(),
    };
    let ffx_path_node =
        ledger.add_node(&format!("Path to ffx: {}", ffx_path), LedgerMode::Normal)?;
    ledger.set_outcome(ffx_path_node, LedgerOutcome::Info)?;

    ledger.close(ffx_node)?;

    Ok(ffx_node)
}

pub async fn check_emulators<W: Write>(
    ledger: &mut DoctorLedger<W>,
    env_context: &EnvironmentContext,
) -> Result<(), anyhow::Error> {
    let emu_instance_root = env_context.get(ffx_config::keys::EMU_INSTANCE_ROOT_DIR)?;
    let emu_instances = EmulatorInstances::new(emu_instance_root);
    let instances = emu_instances.get_all_instances()?;
    let emu_node = ledger.add_node("FFX Emulator Instances", LedgerMode::Normal)?;
    for instance in &instances {
        let instance_node = ledger.add_node("Instance", LedgerMode::Normal)?;
        let instance_name_node =
            ledger.add_node(&format!("Name: {}", instance.get_name()), LedgerMode::Normal)?;
        ledger.set_outcome(instance_name_node, LedgerOutcome::Info)?;
        let instance_running_node = ledger
            .add_node(&format!("Is Running: {}", instance.is_running()), LedgerMode::Normal)?;
        ledger.set_outcome(instance_running_node, LedgerOutcome::Info)?;
        let instance_state_node = ledger.add_node(
            &format!("Engine State: {}", instance.get_engine_state()),
            LedgerMode::Normal,
        )?;
        ledger.set_outcome(instance_state_node, LedgerOutcome::Info)?;
        ledger.close(instance_node)?;
    }
    if instances.is_empty() {
        let empty_node = ledger.add_node("No Emulator instances", LedgerMode::Normal)?;
        ledger.set_outcome(empty_node, LedgerOutcome::Info)?;
        ledger.close(empty_node)?;
    }
    ledger.close(emu_node)?;
    Ok(())
}

pub async fn check_env_context<W: Write>(
    ledger: &mut DoctorLedger<W>,
    env_context: &EnvironmentContext,
) -> Result<(), anyhow::Error> {
    let env_node = ledger.add_node("FFX Environment Context", LedgerMode::Normal)?;
    let environment_kind_node = ledger.add_node(
        &format!("Kind of Environment: {kind}", kind = env_context.env_kind()),
        LedgerMode::Normal,
    )?;
    ledger.set_outcome(environment_kind_node, LedgerOutcome::Success)?;
    let (outcome, description) = match env_context.env_file_path() {
        Ok(env_file) => (
            LedgerOutcome::Success,
            format!("Environment File Location: {env_file}", env_file = env_file.display()),
        ),
        Err(e) => {
            (LedgerOutcome::Failure, format!("Error find or loading the environment file: {e:?}"))
        }
    };
    let env_file_node = ledger.add_node(&description, LedgerMode::Verbose)?;
    ledger.set_outcome(env_file_node, outcome)?;
    let build_dir_node = if let Some(build_dir) = env_context.build_dir() {
        ledger.add_node(
            &format!(
                "Environment-default build directory: {build_dir}",
                build_dir = build_dir.display()
            ),
            LedgerMode::Normal,
        )?
    } else {
        ledger.add_node("No build directory discovered in the environment.", LedgerMode::Verbose)?
    };
    ledger.set_outcome(build_dir_node, LedgerOutcome::Success)?;
    if let Err(e) = check_lock_files(ledger, env_context).await {
        let _ = ledger.close(env_node);
        return Err(e);
    }
    if let Err(e) = check_ssh_keys(env_context, ledger).await {
        let _ = ledger.close(env_node);
        return Err(e);
    }
    ledger.close(env_node)?;
    Ok(())
}

pub async fn check_lock_files<W: Write>(
    ledger: &mut DoctorLedger<W>,
    env_context: &EnvironmentContext,
) -> Result<(), anyhow::Error> {
    let locks = ffx_config::environment::Environment::check_locks(env_context).await?;
    let lock_node = ledger.add_node("Config Lock Files", LedgerMode::Automatic)?;
    for (file, locked) in locks {
        let (outcome, description) = match locked {
            Ok(lockfile) => (
                LedgerOutcome::Success,
                format!(
                    "{path} locked by {lock}",
                    path = file.display(),
                    lock = lockfile.display()
                ),
            ),
            Err(err) => match *err {
                LockfileCreateError {
                    kind: LockfileCreateErrorKind::TimedOut,
                    lock_path,
                    owner,
                    ..
                } => {
                    let mut msg = format!(
                        "Lockfile `{lockfile}` was owned by another process that didn't release it in our timeout.",
                        lockfile = lock_path.display(),
                    );

                    if let Some(owner) = owner {
                        msg = format!("{msg} Check that it's running? Pid {pid}", pid = owner.pid);
                    }

                    (LedgerOutcome::Failure, msg)
                }
                LockfileCreateError {
                    kind: LockfileCreateErrorKind::Io(error), lock_path, ..
                } => (
                    LedgerOutcome::Failure,
                    format!(
                        "Could not open lockfile `{lockfile}` due to error: {error:?}. Check permissions on the directory.",
                        lockfile = lock_path.display(),
                    ),
                ),
            },
        };
        let node = ledger.add_node(&description, LedgerMode::Automatic)?;
        ledger.set_outcome(node, outcome)?;
    }
    ledger.close(lock_node)?;
    Ok(())
}

pub async fn check_ssh_keys<W: Write>(
    ctx: &EnvironmentContext,
    ledger: &mut DoctorLedger<W>,
) -> Result<()> {
    let ssh_node: usize;
    match SshKeyFiles::load(ctx) {
        Ok(ssh_files) => {
            let (description, outcome) = match ssh_files.check_keys(false) {
                Ok(_) => (
                    "The public & private Fuchsia keys are consistent".to_string(),
                    LedgerOutcome::Success,
                ),
                Err(e) => match e.kind {
                    SshKeyErrorKind::BadKeyType => (
                        format!("SSH keys type not supported: {}", e.message),
                        LedgerOutcome::Warning,
                    ),
                    SshKeyErrorKind::BadConfiguration => {
                        (format!("SSH keys configuration problem: {e}"), LedgerOutcome::Failure)
                    }
                    SshKeyErrorKind::IOError | SshKeyErrorKind::FileNotFound => (
                        format!(
                            "{}. Check configuration or run `ffx doctor --repair-keys`",
                            e.message
                        ),
                        LedgerOutcome::Failure,
                    ),
                    SshKeyErrorKind::KeyMismatch => (
                        format!(
                            "{}. Check configuration or run `ffx doctor --repair-keys`",
                            e.message
                        ),
                        LedgerOutcome::Failure,
                    ),
                    _ => (
                        format!(
                            "SSH keys problem: {e}. Check configuration or run `ffx doctor --repair-keys`"
                        ),
                        LedgerOutcome::Failure,
                    ),
                },
            };
            ssh_node = ledger.add_node(&description, LedgerMode::Automatic)?;
            ledger.set_outcome(ssh_node, outcome)?;
        }
        Err(e) => {
            ssh_node = ledger
                .add_node(&format!("Could not get SSH key paths {e}"), LedgerMode::Automatic)?;
            ledger.set_outcome(ssh_node, LedgerOutcome::Failure)?;
        }
    };
    ledger.close(ssh_node)?;
    Ok(())
}

#[cfg(all(target_os = "linux", not(test)))]
pub async fn check_inotify_watches<W: Write>(ledger: &mut DoctorLedger<W>) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let watch_node = ledger.add_node("System Inotify Watches", LedgerMode::Automatic)?;
    let mut total_watches = 0;

    let max_watches = match std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches") {
        Ok(content) => match content.trim().parse::<usize>() {
            Ok(v) => v,
            Err(_) => {
                let node =
                    ledger.add_node("Could not parse max_user_watches", LedgerMode::Verbose)?;
                ledger.set_outcome(node, LedgerOutcome::Failure)?;
                ledger.close(watch_node)?;
                return Ok(());
            }
        },
        Err(e) => {
            let node = ledger.add_node(
                &format!("Could not read max_user_watches: {}", e),
                LedgerMode::Verbose,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(watch_node)?;
            return Ok(());
        }
    };

    let uid = match std::fs::metadata("/proc/self") {
        Ok(m) => m.uid(),
        Err(e) => {
            let node =
                ledger.add_node(&format!("Could not get uid: {}", e), LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(watch_node)?;
            return Ok(());
        }
    };

    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let fname = entry.file_name();
            let fname_str = fname.to_string_lossy();
            if !fname_str.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }

            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.uid() != uid {
                    continue;
                }
            } else {
                continue;
            }

            let fd_path = path.join("fd");
            if let Ok(fd_entries) = std::fs::read_dir(fd_path) {
                for fd_entry in fd_entries.flatten() {
                    let fd_path = fd_entry.path();
                    if let Ok(target) = std::fs::read_link(&fd_path) {
                        if target.to_string_lossy() == "anon_inode:inotify" {
                            let fd_num = fd_entry.file_name();
                            let fdinfo_path = path.join("fdinfo").join(fd_num);
                            if let Ok(content) = std::fs::read_to_string(fdinfo_path) {
                                total_watches += content
                                    .lines()
                                    .filter(|l| l.starts_with("inotify wd:"))
                                    .count();
                            }
                        }
                    }
                }
            }
        }
    }

    if max_watches > 0 {
        let percent = (total_watches * 100) / max_watches;
        let remaining = max_watches.saturating_sub(total_watches);
        if percent >= 80 && remaining < 10000 {
            let node = ledger.add_node(
                &format!("User is consuming {} / {} inotify watches", total_watches, max_watches),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Warning)?;
            let suggestion = ledger.add_node(
                &format!(
                    "Consider increasing max_user_watches: `sudo sysctl fs.inotify.max_user_watches={}`",
                    max_watches + 1048576
                ),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(suggestion, LedgerOutcome::Warning)?;
        } else {
            let node = ledger.add_node(
                &format!("User is consuming {} / {} inotify watches", total_watches, max_watches),
                LedgerMode::Verbose,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
        }
    }

    ledger.close(watch_node)?;
    Ok(())
}

#[cfg(any(not(target_os = "linux"), test))]
pub async fn check_inotify_watches<W: Write>(_ledger: &mut DoctorLedger<W>) -> Result<()> {
    Ok(())
}
