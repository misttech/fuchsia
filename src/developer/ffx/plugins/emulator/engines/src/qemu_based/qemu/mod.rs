// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The qemu module encapsulates the interactions with the emulator instance
//! started via the QEMU emulator.
//! Some of the functions related to QEMU are pub(crate) to allow reuse by
//! femu module since femu is a wrapper around an older version of QEMU.

use super::get_host_tool;
use crate::qemu_based::QemuBasedEngine;
use crate::qemu_based::comms::QemuSocket;
use emulator_instance::{
    CpuArchitecture, DiskImage, EmulatorConfiguration, EmulatorInstanceData, EmulatorInstanceInfo,
    EmulatorInstances, EngineState, EngineType, NetworkingMode, PointingDevice, write_to_disk,
};
use ffx_config::EnvironmentContext;
use ffx_emulator_common::config::QEMU_TOOL;
use ffx_emulator_common::{find_unused_vsock_cid, validate_snapshot_name};
use ffx_emulator_config::{BoxFuture, EmulatorEngine, EngineConsoleType, ShowDetail, SnapshotInfo};
use fho::{Result, bug, return_bug, return_user_error};
use monitor::Monitor;
use serde_json::{Deserializer, Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod image_conversion;
mod monitor;

/// The line which `qemu-img snapshot -l` prints above the table of the snapshots in an image.
const IMAGE_SNAPSHOTS_HEADING: &str = "Snapshot list:";

/// Timeout for opening the QMP socket, and for each QMP command.
const QMP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct QemuEngine {
    data: EmulatorInstanceData,
    emu_instances: EmulatorInstances,
    context: EnvironmentContext,
}

impl QemuEngine {
    pub(crate) fn new(
        context: &EnvironmentContext,
        data: EmulatorInstanceData,
        emu_instances: EmulatorInstances,
    ) -> Self {
        Self { data, emu_instances, context: context.clone() }
    }

    fn validate_configuration(&self) -> Result<()> {
        if self.data.get_emulator_configuration().device.pointing_device == PointingDevice::Touch
            && !self.data.get_emulator_configuration().runtime.headless
        {
            let message = format!(
                "{}\n{}",
                "Touchscreen as a pointing device is not available on Qemu.",
                "If you encounter errors, try changing the pointing device to 'mouse' in the \
                Virtual Device specification."
            );
            log::info!("{message}");
            eprintln!("{message}");
        }
        self.validate_network_flags(&self.data.get_emulator_configuration())
            .and_then(|()| self.check_required_files(&self.data.get_emulator_configuration().guest))
            .and_then(|()| self.validate_snapshot_support())
    }

    fn validate_staging(&self) -> Result<()> {
        self.check_required_files(&self.data.get_emulator_configuration().guest)
    }

    /// Checks that snapshots, if they are enabled, are supported for the way the guest is booted.
    fn validate_snapshot_support(&self) -> Result<()> {
        let emu_config = self.data.get_emulator_configuration();
        if !emu_config.runtime.snapshots_enabled {
            return Ok(());
        }
        // Booting through UEFI uses additional writable firmware and disk images, which are not
        // set up to hold snapshots. That is how --uefi boots the guest, and also how the guests
        // of some products are booted without it.
        if emu_config.guest.is_gpt {
            return_user_error!(
                "Snapshots are not supported for emulators which boot using UEFI. \
                Start the emulator without --uefi to use snapshots."
            );
        }
        if emu_config.guest.is_efi() {
            return_user_error!(
                "Snapshots are not supported for emulators which boot using UEFI, and the guest \
                of this product bundle is booted using UEFI."
            );
        }
        match emu_config.guest.disk_image {
            Some(DiskImage::Fvm(_)) | Some(DiskImage::Fxfs(_)) => Ok(()),
            _ => return_user_error!(
                "Snapshots are saved in the emulator's disk image, but the product bundle does \
                not provide a disk image which supports them."
            ),
        }
    }

    /// Connects to the monitor of the running emulator, to manage its snapshots.
    fn connect_to_snapshot_monitor(&self) -> Result<Monitor> {
        let name = self.data.get_name();
        if self.get_engine_state() != EngineState::Running {
            return_user_error!(
                "Emulator '{name}' is not running (current state: {}). Snapshots can only be \
                saved and loaded while the emulator is running. To resume a stopped emulator \
                from a snapshot, use `ffx emu start --from-snapshot <snapshot>`.",
                self.get_engine_state()
            );
        }
        self.check_snapshots_enabled()?;
        let emu_config = self.data.get_emulator_configuration();
        let socket_path = self.get_path_for_console_type(
            &emu_config.runtime.instance_directory,
            EngineConsoleType::Command,
        );
        let memory = emu_config.device.memory.as_bytes().unwrap_or(u64::MAX);
        Monitor::connect(&socket_path, monitor::snapshot_timeout(memory))
    }

    /// Runs `qemu-img snapshot` with the given arguments on the disk image of the stopped
    /// emulator, which holds its snapshots, and returns what the tool prints. If the tool fails,
    /// the error is described as "Error <action> <image>". While the emulator is running it holds
    /// a lock on the image, and its snapshots are managed through its monitor instead.
    fn run_image_snapshot_command(&self, args: &[&str], action: &str) -> Result<String> {
        self.check_snapshots_enabled()?;
        // The snapshots are stored in the qcow2 image which the instance was staged with.
        let image_path = match &self.data.get_emulator_configuration().guest.disk_image {
            Some(DiskImage::Fvm(path)) | Some(DiskImage::Fxfs(path)) => path,
            _ => return_bug!(
                "Emulator '{}' has no disk image which can hold snapshots.",
                self.data.get_name()
            ),
        };
        let qemu_img = super::get_qemu_img_tool(&self.context)?;
        let mut command = Command::new(&qemu_img);
        command.arg("snapshot").args(args).args(["-f", "qcow2"]).arg(image_path);
        log::debug!("Running command on the snapshots of the disk image: {:?}", command);

        let result =
            command.output().map_err(|e| bug!("Error running {}: {e}", qemu_img.display()))?;
        if !result.status.success() {
            return_bug!(
                "Error {action} {}: {}",
                image_path.display(),
                String::from_utf8_lossy(&result.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&result.stdout).into_owned())
    }

    /// Returns the snapshots which are stored in the disk image of the stopped emulator.
    fn stored_snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        let output = self.run_image_snapshot_command(&["-l"], "reading the snapshots of")?;
        monitor::parse_snapshot_table(&output, IMAGE_SNAPSHOTS_HEADING)
    }

    /// Deletes the snapshot with the given name from the disk image of the stopped emulator.
    fn delete_stored_snapshot(&self, name: &str) -> Result<()> {
        // qemu-img only says that a snapshot was not found if there is none with the name. Look
        // for it first, so that the snapshots which do exist are listed instead, as they are for
        // a running emulator.
        monitor::check_snapshot_exists(name, &self.stored_snapshots()?)?;
        self.run_image_snapshot_command(
            &["-d", name],
            &format!("deleting snapshot '{name}' from"),
        )?;
        Ok(())
    }

    /// Checks that the instance was staged with support for snapshots.
    fn check_snapshots_enabled(&self) -> Result<()> {
        if !self.data.get_emulator_configuration().runtime.snapshots_enabled {
            return_user_error!(
                "Emulator '{}' does not support snapshots. Snapshot support has to be \
                enabled when the emulator instance is created, by starting it with \
                `ffx emu start --enable-snapshots`.",
                self.data.get_name()
            );
        }
        Ok(())
    }
}

impl EmulatorEngine for QemuEngine {
    fn get_instance_data(&self) -> &EmulatorInstanceData {
        &self.data
    }

    fn stage(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if let Some(vsock) = &mut self.emu_config_mut().device.vsock {
                // The default CID value of 0 indicates that selection should be
                // left to ffx.
                if vsock.enabled && vsock.cid == 0 {
                    vsock.cid = find_unused_vsock_cid()?;
                }
            }
            let ctx_clone = self.context.clone();
            let result = <Self as QemuBasedEngine>::stage(self, &ctx_clone)
                .await
                .and_then(|()| self.validate_staging());
            match result {
                Ok(()) => {
                    self.data.set_engine_state(EngineState::Staged);
                    self.save_to_disk().await
                }
                Err(e) => {
                    self.data.set_engine_state(EngineState::Error);
                    self.save_to_disk().await.and(Err(e))
                }
            }
        })
    }

    fn start<'a>(
        &'a mut self,
        context: &'a EnvironmentContext,
        emulator_cmd: Command,
    ) -> BoxFuture<'a, Result<i32>> {
        Box::pin(async move { self.run(context, emulator_cmd).await })
    }

    fn show(&self, details: Vec<ShowDetail>) -> Vec<ShowDetail> {
        <Self as QemuBasedEngine>::show(self, details)
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.stop_emulator().await })
    }

    fn configure(&mut self) -> Result<()> {
        let result = if self.emu_config().runtime.config_override {
            let message = "Custom configuration provided; bypassing validation.";
            eprintln!("{message}");
            log::info!("{message}");
            Ok(())
        } else {
            self.validate_configuration()
        };
        // QEMU 9+ does not support auto-assignment of host ports
        // for the guest ports which are forwarded with user-mode
        // networking, so the host ports which are still unassigned
        // are picked here, and the ones which were asked for
        // explicitly are checked to be free. This is done in
        // configure() rather than stage() because an instance
        // which is reused is not staged again, but is configured
        // again with the port map of the new invocation. That map
        // replaces the saved one, so the host ports of the previous
        // run are not kept: unassigned ports are picked afresh, and
        // a port which another process has taken since is either
        // avoided or reported as in use.
        let result = result.and_then(|()| {
            if self.emu_config().host.networking == NetworkingMode::User {
                crate::finalize_port_mapping(self.emu_config_mut())
            } else {
                Ok(())
            }
        });
        if result.is_ok() {
            self.data.set_engine_state(EngineState::Configured);
        } else {
            self.data.set_engine_state(EngineState::Error);
        }
        result
    }

    fn engine_state(&self) -> EngineState {
        self.get_engine_state()
    }

    fn engine_type(&self) -> EngineType {
        self.data.get_engine_type()
    }

    fn is_running(&mut self) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            let running = self.data.is_running();
            if self.engine_state() == EngineState::Running && running == false {
                self.set_engine_state(EngineState::Staged);
                if self.save_to_disk().await.is_err() {
                    log::warn!("Problem saving serialized emulator to disk during state update.");
                }
            }
            running
        })
    }

    fn attach(&self, console: EngineConsoleType) -> Result<()> {
        self.attach_to(&self.data.get_emulator_configuration().runtime.instance_directory, console)
    }

    fn save_snapshot<'a>(&'a mut self, name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            validate_snapshot_name(name)?;
            self.connect_to_snapshot_monitor()?.save_snapshot(name)
        })
    }

    fn load_snapshot<'a>(&'a mut self, name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            validate_snapshot_name(name)?;
            self.connect_to_snapshot_monitor()?.load_snapshot(name)
        })
    }

    fn list_snapshots(&mut self) -> BoxFuture<'_, Result<Vec<SnapshotInfo>>> {
        Box::pin(async move {
            if self.get_engine_state() == EngineState::Running {
                self.connect_to_snapshot_monitor()?.list_snapshots()
            } else {
                self.stored_snapshots()
            }
        })
    }

    fn delete_snapshot<'a>(&'a mut self, name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            validate_snapshot_name(name)?;
            if self.get_engine_state() == EngineState::Running {
                self.connect_to_snapshot_monitor()?.delete_snapshot(name)
            } else {
                self.delete_stored_snapshot(name)
            }
        })
    }

    /// Build the Command to launch Qemu emulator running Fuchsia.
    fn build_emulator_cmd(&self) -> Command {
        let mut cmd = Command::new(&self.data.get_emulator_binary());
        let emulator_configuration = self.data.get_emulator_configuration();
        cmd.args(&emulator_configuration.flags.args);

        // Can't have kernel args if there is no kernel, but if there is a custom configuration template,
        // add them anyway since the configuration and the custom template could be out of sync.
        if emulator_configuration.guest.kernel_image.is_some()
            || emulator_configuration.runtime.config_override
        {
            let extra_args = emulator_configuration
                .flags
                .kernel_args
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            if extra_args.len() > 0 {
                cmd.args(["-append", &extra_args]);
            }
        }
        if self.data.get_emulator_configuration().flags.envs.len() > 0 {
            // Add environment variables if not already present.
            // This does not overwrite any existing values.
            let unset_envs = emulator_configuration
                .flags
                .envs
                .iter()
                .filter(|(k, _)| self.context.env_var(k).is_err());
            if unset_envs.clone().count() > 0 {
                cmd.envs(unset_envs);
            }
        }
        cmd
    }

    /// Loads the path to the qemu binary to execute. This is based on the guest OS architecture.
    ///
    /// Currently this is done by getting the default CLI which is for x64 images, and then
    /// replacing it if the guest OS is arm64.
    /// TODO(http://fxdev.bug/98862): Improve the SDK metadata to have multiple binaries per tool.
    fn load_emulator_binary(&mut self) -> Result<()> {
        let cli_name = match self.data.get_emulator_configuration().device.cpu.architecture {
            CpuArchitecture::Arm64 => Some("qemu-system-aarch64"),
            CpuArchitecture::Riscv64 => Some("qemu-system-riscv64"),
            _ => None,
        };

        let qemu_x64_path = match get_host_tool(&self.context, QEMU_TOOL) {
            Ok(qemu_path) => qemu_path.canonicalize().map_err(|e| {
                bug!("Failed to canonicalize the path to the emulator binary: {qemu_path:?}: {e}")
            })?,
            Err(e) => return_bug!("Cannot find {QEMU_TOOL} in the SDK: {e}"),
        };

        // If we need to, replace the executable name.
        let emulator_binary = if let Some(exe_name) = cli_name {
            // Realistically, the file is always in a directory, so the empty path is a reasonable
            // fallback since it will "never" happen
            let mut p = PathBuf::from(qemu_x64_path.parent().unwrap_or_else(|| Path::new("")));
            p.push(exe_name);
            p
        } else {
            qemu_x64_path
        };

        if !emulator_binary.exists() || !emulator_binary.is_file() {
            return_bug!("Giving up finding emulator binary. Tried {:?}", emulator_binary)
        }
        self.data.set_emulator_binary(emulator_binary);

        Ok(())
    }

    fn emu_config(&self) -> &EmulatorConfiguration {
        self.data.get_emulator_configuration()
    }

    fn emu_config_mut(&mut self) -> &mut EmulatorConfiguration {
        self.data.get_emulator_configuration_mut()
    }

    fn screenshot<'a>(&'a mut self, output_path: &'a Path) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let absolute_path = crate::qemu_based::make_absolute_path(output_path)?;
            crate::qemu_based::check_screenshot_preconditions(self, &absolute_path);
            self.capture_screenshot_qmp(&absolute_path).await
        })
    }

    fn save_to_disk(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            write_to_disk(
                &self.data,
                &self
                    .emu_instances
                    .get_instance_dir(self.data.get_name(), true)
                    .unwrap_or_else(|_| panic!("instance directory for {}", self.data.get_name())),
            )
            .map_err(|e| bug!("Error saving instance to disk: {e}"))
        })
    }
}

impl QemuEngine {
    /// Captures a screenshot and saves it as a PNG file using QEMU's QMP screendump command.
    async fn capture_screenshot_qmp(&mut self, absolute_path: &Path) -> Result<()> {
        let instance_dir =
            self.data.get_emulator_configuration().runtime.instance_directory.clone();
        let console_path =
            self.get_path_for_console_type(&instance_dir, EngineConsoleType::Machine);

        let mut socket = QemuSocket::new(&console_path);
        let stream = self.open_socket(&mut socket, &QMP_TIMEOUT).await?;

        let mut responses = Deserializer::from_reader(&stream).into_iter::<Value>();

        let filename = absolute_path.to_str().ok_or_else(|| bug!("Invalid target path"))?;

        // We take a two-step approach to capture the screenshot:
        // 1. Capture the screenshot in PPM format from QEMU to the temporary destination.
        // 2. Read the PPM file into memory and overwrite it with its PNG-encoded version.
        // This avoids creating extra temporary files on the host filesystem.
        crate::qemu_based::qmp_execute(
            &stream,
            &mut responses,
            "screendump",
            Some(json!({ "filename": filename, "format": "ppm" })),
            QMP_TIMEOUT,
        )?;

        image_conversion::convert_ppm_to_png(
            /* ppm_source= */ absolute_path,
            /* png_destination= */ absolute_path,
        )
    }
}

impl QemuBasedEngine for QemuEngine {
    fn set_pid(&mut self, pid: u32) {
        self.data.set_pid(pid);
    }

    fn get_pid(&self) -> u32 {
        self.data.get_pid()
    }

    fn set_engine_state(&mut self, state: EngineState) {
        self.data.set_engine_state(state)
    }

    fn get_engine_state(&self) -> EngineState {
        self.data.get_engine_state()
    }
}

#[cfg(test)]
mod tests {
    use super::monitor::tests::{SNAPSHOT_LIST, start_fake_monitor};
    use super::*;
    use crate::EngineBuilder;
    use crate::qemu_based::tests::make_fake_sdk;
    use emulator_instance::{NetworkingMode, PortMapping};
    use std::ffi::OsStr;
    use std::io::Write as _;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::{fs, thread};
    use tempfile::tempdir;

    /// Returns an engine for an emulator instance which appears to be running, and which keeps
    /// its console sockets in `instance_directory`.
    fn running_engine(
        env: &ffx_config::TestEnv,
        instance_directory: &Path,
        snapshots_enabled: bool,
    ) -> QemuEngine {
        let mut cfg = EmulatorConfiguration::default();
        cfg.runtime.name = "test-emu".to_string();
        cfg.runtime.instance_directory = instance_directory.to_path_buf();
        cfg.runtime.snapshots_enabled = snapshots_enabled;
        let mut data = EmulatorInstanceData::new(cfg, EngineType::Qemu, EngineState::Running);
        // Set the emulator PID to the PID of the test, so that the engine state is Running.
        data.set_pid(std::process::id());
        QemuEngine::new(
            &env.context,
            data,
            EmulatorInstances::new(instance_directory.to_path_buf()),
        )
    }

    #[fuchsia::test]
    async fn test_save_snapshot() {
        let env = ffx_config::test_init().unwrap();
        let (temp, _socket_path, monitor) = start_fake_monitor(vec![("savevm booted", "")]);
        let mut engine = running_engine(&env, temp.path(), true);

        engine.save_snapshot("booted").await.expect("save snapshot");

        assert_eq!(monitor.join().unwrap(), vec!["savevm booted"]);
    }

    #[fuchsia::test]
    async fn test_load_snapshot() {
        let env = ffx_config::test_init().unwrap();
        let (temp, _socket_path, monitor) = start_fake_monitor(vec![
            ("info snapshots", SNAPSHOT_LIST),
            ("loadvm booted", ""),
            ("info status", "VM status: running\r\n"),
        ]);
        let mut engine = running_engine(&env, temp.path(), true);

        engine.load_snapshot("booted").await.expect("load snapshot");

        assert_eq!(monitor.join().unwrap(), vec!["info snapshots", "loadvm booted", "info status"]);
    }

    #[fuchsia::test]
    async fn test_list_snapshots() {
        let env = ffx_config::test_init().unwrap();
        let (temp, _socket_path, monitor) =
            start_fake_monitor(vec![("info snapshots", SNAPSHOT_LIST)]);
        let mut engine = running_engine(&env, temp.path(), true);

        let snapshots = engine.list_snapshots().await.expect("list snapshots");

        assert_eq!(
            snapshots.iter().map(|snapshot| snapshot.name.as_str()).collect::<Vec<_>>(),
            vec!["booted", "second"]
        );
        assert_eq!(monitor.join().unwrap(), vec!["info snapshots"]);
    }

    #[fuchsia::test]
    async fn test_delete_snapshot() {
        let env = ffx_config::test_init().unwrap();
        let (temp, _socket_path, monitor) =
            start_fake_monitor(vec![("info snapshots", SNAPSHOT_LIST), ("delvm booted", "")]);
        let mut engine = running_engine(&env, temp.path(), true);

        engine.delete_snapshot("booted").await.expect("delete snapshot");

        assert_eq!(monitor.join().unwrap(), vec!["info snapshots", "delvm booted"]);
    }

    #[fuchsia::test]
    async fn test_delete_snapshot_of_stopped_emulator() {
        let builder = ffx_config::test_env();
        let builder = make_fake_sdk(builder).await;
        let env = builder.build().unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        // The fake qemu-img tool lists the snapshots in an image by printing its contents, and
        // records the arguments of other commands in the image.
        let image_path = temp.path().join("fxfs.blk.qcow2");
        fs::write(
            &image_path,
            "Snapshot list:\n\
            ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT\n\
            1       booted            382 MiB 2026-09-30 22:30:24  0000:00:19.615         --\n\
            2       second           1.45 GiB 2026-10-01 08:15:00  0001:02:03.004         --\n",
        )
        .expect("write image");

        let mut cfg = EmulatorConfiguration::default();
        cfg.runtime.name = "test-emu".to_string();
        cfg.runtime.snapshots_enabled = true;
        cfg.guest.disk_image = Some(DiskImage::Fxfs(image_path.clone()));
        let data = EmulatorInstanceData::new(cfg, EngineType::Qemu, EngineState::Staged);
        let mut engine =
            QemuEngine::new(&env.context, data, EmulatorInstances::new(PathBuf::new()));

        // A snapshot which doesn't exist is reported along with the ones which do.
        let err = engine.delete_snapshot("missing").await.expect_err("no such snapshot");
        assert_eq!(
            err.to_string(),
            "Snapshot 'missing' does not exist. The available snapshots are: booted, second."
        );

        // The snapshot is deleted from the disk image, without the emulator having to run.
        engine.delete_snapshot("booted").await.expect("delete snapshot");
        assert_eq!(
            fs::read_to_string(&image_path).expect("read image"),
            format!("snapshot -d booted -f qcow2 {}\n", image_path.display())
        );
    }

    #[fuchsia::test]
    async fn test_snapshots_require_running_emulator() {
        let env = ffx_config::test_init().unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        let mut cfg = EmulatorConfiguration::default();
        cfg.runtime.name = "test-emu".to_string();
        cfg.runtime.instance_directory = temp.path().to_path_buf();
        cfg.runtime.snapshots_enabled = true;
        let data = EmulatorInstanceData::new(cfg, EngineType::Qemu, EngineState::Staged);
        let mut engine =
            QemuEngine::new(&env.context, data, EmulatorInstances::new(PathBuf::new()));

        let expected = "Emulator 'test-emu' is not running";
        let err = engine.save_snapshot("booted").await.expect_err("save fails").to_string();
        assert!(err.contains(expected), "unexpected error: {err}");
        let err = engine.load_snapshot("booted").await.expect_err("load fails").to_string();
        assert!(err.contains(expected), "unexpected error: {err}");
    }

    #[fuchsia::test]
    async fn test_snapshots_require_snapshot_support() {
        let env = ffx_config::test_init().unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        let mut engine = running_engine(&env, temp.path(), false);

        let expected = "Emulator 'test-emu' does not support snapshots";
        let err = engine.save_snapshot("booted").await.expect_err("save fails").to_string();
        assert!(err.contains(expected), "unexpected error: {err}");
        let err = engine.load_snapshot("booted").await.expect_err("load fails").to_string();
        assert!(err.contains(expected), "unexpected error: {err}");
        let err = engine.list_snapshots().await.expect_err("list fails").to_string();
        assert!(err.contains(expected), "unexpected error: {err}");
        let err = engine.delete_snapshot("booted").await.expect_err("delete fails").to_string();
        assert!(err.contains(expected), "unexpected error: {err}");
    }

    #[fuchsia::test]
    async fn test_snapshots_require_valid_name() {
        let env = ffx_config::test_init().unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        // There is no monitor for the engine to connect to, so these would fail differently if
        // the names were not checked before they are sent to the emulator.
        let mut engine = running_engine(&env, temp.path(), true);

        for name in ["", "two words", "booted\nquit", "-booted"] {
            let err = engine.save_snapshot(name).await.expect_err("save fails").to_string();
            assert!(err.contains("Invalid snapshot name"), "unexpected error: {err}");
            let err = engine.load_snapshot(name).await.expect_err("load fails").to_string();
            assert!(err.contains("Invalid snapshot name"), "unexpected error: {err}");
            let err = engine.delete_snapshot(name).await.expect_err("delete fails").to_string();
            assert!(err.contains("Invalid snapshot name"), "unexpected error: {err}");
        }
    }

    #[fuchsia::test]
    async fn test_list_snapshots_of_stopped_emulator() {
        let builder = ffx_config::test_env();
        let builder = make_fake_sdk(builder).await;
        let env = builder.build().unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        // The fake qemu-img tool lists the snapshots in an image by printing its contents.
        let image_path = temp.path().join("fxfs.blk.qcow2");
        fs::write(
            &image_path,
            "Snapshot list:\n\
            ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT\n\
            1       booted            382 MiB 2026-09-30 22:30:24  0000:00:19.615         --\n\
            2       second           1.45 GiB 2026-10-01 08:15:00  0001:02:03.004         --\n",
        )
        .expect("write image");

        let mut cfg = EmulatorConfiguration::default();
        cfg.runtime.name = "test-emu".to_string();
        cfg.runtime.snapshots_enabled = true;
        cfg.guest.disk_image = Some(DiskImage::Fxfs(image_path));
        let stopped_engine = |cfg: &EmulatorConfiguration| {
            let data =
                EmulatorInstanceData::new(cfg.clone(), EngineType::Qemu, EngineState::Staged);
            QemuEngine::new(&env.context, data, EmulatorInstances::new(PathBuf::new()))
        };

        // The snapshots are read from the disk image, without the emulator having to run.
        let snapshots = stopped_engine(&cfg).list_snapshots().await.expect("list snapshots");
        assert_eq!(
            snapshots,
            vec![
                SnapshotInfo {
                    name: "booted".to_string(),
                    vm_size: "382 MiB".to_string(),
                    date: "2026-09-30 22:30:24".to_string(),
                    vm_clock: "0000:00:19.615".to_string(),
                },
                SnapshotInfo {
                    name: "second".to_string(),
                    vm_size: "1.45 GiB".to_string(),
                    date: "2026-10-01 08:15:00".to_string(),
                    vm_clock: "0001:02:03.004".to_string(),
                },
            ]
        );

        // An instance which was staged without support for snapshots can't hold any.
        cfg.runtime.snapshots_enabled = false;
        let err = stopped_engine(&cfg).list_snapshots().await.expect_err("no snapshots");
        assert!(
            err.to_string().contains("Emulator 'test-emu' does not support snapshots"),
            "unexpected error: {err}"
        );

        // The disk image has to be readable.
        cfg.runtime.snapshots_enabled = true;
        cfg.guest.disk_image = Some(DiskImage::Fxfs(temp.path().join("missing.qcow2")));
        let err = stopped_engine(&cfg).list_snapshots().await.expect_err("no disk image");
        assert!(
            err.to_string().contains("Error reading the snapshots of"),
            "unexpected error: {err}"
        );
    }

    #[fuchsia::test]
    fn test_validate_snapshot_support() {
        let env = ffx_config::test_init().unwrap();
        let validate = |cfg: &EmulatorConfiguration| {
            let data = EmulatorInstanceData::new(cfg.clone(), EngineType::Qemu, EngineState::New);
            QemuEngine::new(&env.context, data, EmulatorInstances::new(PathBuf::new()))
                .validate_snapshot_support()
        };

        // Guests which boot from a kernel and a FVM or Fxfs disk image support snapshots.
        let mut cfg = EmulatorConfiguration::default();
        cfg.runtime.snapshots_enabled = true;
        cfg.guest.kernel_image = Some("/path/to/kernel".into());
        cfg.guest.disk_image = Some(DiskImage::Fxfs("/path/to/fxfs.blk".into()));
        assert!(validate(&cfg).is_ok());
        cfg.guest.disk_image = Some(DiskImage::Fvm("/path/to/fvm.blk".into()));
        assert!(validate(&cfg).is_ok());

        // There has to be a disk image for the snapshots to be saved in.
        cfg.guest.disk_image = None;
        let err = validate(&cfg).expect_err("no disk image").to_string();
        assert!(err.contains("does not provide a disk image"), "unexpected error: {err}");

        // Guests which boot using UEFI don't support snapshots. Only --uefi can be left out to
        // boot the guest differently.
        cfg.guest.disk_image = Some(DiskImage::Gpt("/path/to/gpt.img".into()));
        cfg.guest.is_gpt = true;
        let err = validate(&cfg).expect_err("GPT disk image").to_string();
        assert!(err.contains("boot using UEFI"), "unexpected error: {err}");
        assert!(err.contains("without --uefi"), "unexpected error: {err}");

        cfg.guest.disk_image = Some(DiskImage::Fat("/path/to/bootloader.fat".into()));
        cfg.guest.kernel_image = None;
        cfg.guest.is_gpt = false;
        let err = validate(&cfg).expect_err("EFI bootloader").to_string();
        assert!(err.contains("boot using UEFI"), "unexpected error: {err}");
        assert!(!err.contains("--uefi"), "unexpected error: {err}");

        cfg.guest.disk_image = Some(DiskImage::Fxfs("/path/to/fxfs.blk".into()));
        cfg.guest.kernel_image = Some("/path/to/kernel.efi".into());
        let err = validate(&cfg).expect_err("EFI kernel").to_string();
        assert!(err.contains("boot using UEFI"), "unexpected error: {err}");
        assert!(!err.contains("--uefi"), "unexpected error: {err}");

        // None of this matters when snapshots are not enabled.
        cfg.runtime.snapshots_enabled = false;
        assert!(validate(&cfg).is_ok());
    }

    #[fuchsia::test]
    fn test_configure_assigns_host_ports() {
        let env = ffx_config::test_init().unwrap();
        let mut cfg = EmulatorConfiguration::default();
        cfg.host.networking = NetworkingMode::User;
        cfg.host.port_map.insert("ssh".to_string(), PortMapping { guest: 22, host: None });
        // Skip the validation of the image files, which don't exist in this test.
        cfg.runtime.config_override = true;
        let data = EmulatorInstanceData::new(cfg, EngineType::Qemu, EngineState::Staged);
        let mut engine =
            QemuEngine::new(&env.context, data, EmulatorInstances::new(PathBuf::new()));

        // QEMU needs a host port for every port which is forwarded to the guest. An instance
        // which is reused is configured, but not staged, so they have to be assigned here.
        engine.configure().expect("configure engine");

        let ssh = engine.emu_config().host.port_map.get("ssh").expect("ssh port mapping");
        assert_eq!(ssh.guest, 22);
        assert!(ssh.host.is_some_and(|port| port != 0), "no host port assigned: {ssh:?}");
        assert_eq!(engine.engine_state(), EngineState::Configured);
    }

    #[fuchsia::test]
    fn test_build_emulator_cmd() {
        let env = ffx_config::test_env().build().unwrap();
        let program_name = "/test_femu_bin";
        let mut cfg = EmulatorConfiguration::default();
        cfg.host.networking = NetworkingMode::User;
        cfg.flags.envs.insert("FLAG_NAME_THAT_DOES_NOT_EXIST".into(), "1".into());

        let mut emu_data = EmulatorInstanceData::new(cfg, EngineType::Qemu, EngineState::New);
        emu_data.set_emulator_binary(program_name.into());
        let test_engine =
            QemuEngine::new(&env.context, emu_data, EmulatorInstances::new(PathBuf::new()));
        let cmd = test_engine.build_emulator_cmd();
        assert_eq!(cmd.get_program(), program_name);
        assert_eq!(
            cmd.get_envs().collect::<Vec<_>>(),
            [(OsStr::new("FLAG_NAME_THAT_DOES_NOT_EXIST"), Some(OsStr::new("1")))]
        );
    }

    #[fuchsia::test]
    fn test_build_emulator_cmd_existing_env() {
        let env = ffx_config::test_env()
            .env_var("FLAG_NAME_THAT_DOES_EXIST", "preset_value")
            .build()
            .unwrap();
        let program_name = "/test_femu_bin";
        let mut cfg = EmulatorConfiguration::default();
        cfg.host.networking = NetworkingMode::User;
        cfg.flags.envs.insert("FLAG_NAME_THAT_DOES_EXIST".into(), "1".into());

        let mut emu_data = EmulatorInstanceData::new(cfg, EngineType::Qemu, EngineState::New);
        emu_data.set_emulator_binary(program_name.into());
        let test_engine: QemuEngine =
            QemuEngine::new(&env.context, emu_data, EmulatorInstances::new(PathBuf::new()));
        let cmd = test_engine.build_emulator_cmd();
        assert_eq!(cmd.get_program(), program_name);
        assert_eq!(cmd.get_envs().collect::<Vec<_>>(), []);
    }

    #[fuchsia::test]
    async fn test_build_cmd_with_custom_template() {
        let builder = ffx_config::test_env();
        let builder = make_fake_sdk(builder).await;
        let env = builder.build().unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        let emu_instances = EmulatorInstances::new(temp.path().to_owned());

        let mut cfg = EmulatorConfiguration::default();
        let template_file = temp.path().join("custom-template.json");
        fs::write(
            &template_file,
            r#"
         {
         "args": [
             "-kernel",
             "boot-shim.bin",
             "-initrd",
             "test.zbi"
         ],
         "envs": {},
         "features": [],
         "kernel_args": ["zircon.nodename=some-emu","TERM=dumb"],
         "options": []
         }"#,
        )
        .expect("custom template contents");
        cfg.runtime.template = Some(template_file);
        cfg.runtime.config_override = true;
        let engine = EngineBuilder::new(&env.context, emu_instances.clone())
            .config(cfg.clone())
            .engine_type(EngineType::Qemu)
            .build()
            .await
            .expect("engine built");

        let cmd = engine.build_emulator_cmd();
        let actual: Vec<_> = cmd.get_args().collect();

        let expected = vec![
            "-kernel",
            "boot-shim.bin",
            "-initrd",
            "test.zbi",
            "-append",
            "TERM=dumb zircon.nodename=some-emu",
        ];

        assert_eq!(actual, expected)
    }

    /// An error that the fake QMP server returns for one command.
    struct FakeQmpError {
        /// Index of the failing command, not counting `qmp_capabilities`.
        command_index: usize,
        desc: &'static str,
    }

    /// Starts a fake QMP server on `<instance_dir>/qmp` for one connection.
    ///
    /// The server answers every command with success, except for the one selected by `error`.
    /// It sends each request, except `qmp_capabilities`, to the returned receiver.
    fn start_fake_qmp(
        instance_dir: &Path,
        error: Option<FakeQmpError>,
    ) -> (mpsc::Receiver<Value>, thread::JoinHandle<()>) {
        let listener = UnixListener::bind(instance_dir.join("qmp")).expect("bind fake qmp socket");
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let greeting = json!({"QMP": {"version": {}, "capabilities": []}});
            stream.write_all(format!("{greeting}\n").as_bytes()).expect("greeting");
            let reader = stream.try_clone().expect("clone");
            let mut command_index = 0;
            for request in Deserializer::from_reader(reader).into_iter::<Value>() {
                let Ok(request) = request else { break };
                let reply = if request["execute"] == "qmp_capabilities" {
                    json!({"return": {}})
                } else {
                    sender.send(request.clone()).expect("send");
                    let reply = match &error {
                        Some(error) if error.command_index == command_index => json!({
                            "error": {"class": "GenericError", "desc": error.desc},
                            "id": request["id"]
                        }),
                        _ => json!({"return": {}, "id": request["id"]}),
                    };
                    command_index += 1;
                    reply
                };
                if stream.write_all(format!("{reply}\n").as_bytes()).is_err() {
                    break;
                }
            }
        });
        (receiver, handle)
    }

    #[fuchsia::test]
    async fn test_screenshot_qmp_error() {
        let env = ffx_config::test_init().unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        let error = FakeQmpError { command_index: 0, desc: "no display" };
        let (receiver, handle) = start_fake_qmp(temp.path(), Some(error));
        let mut engine = running_engine(&env, temp.path(), false);

        let output = temp.path().join("screenshot.png");
        let err = engine.capture_screenshot_qmp(&output).await.unwrap_err().to_string();
        assert_eq!(err, "QMP command 'screendump' failed: no display");
        let request = receiver.try_recv().expect("screendump request");
        assert_eq!(request["execute"], "screendump");
        assert_eq!(request["arguments"]["filename"], output.to_str().unwrap());
        drop(engine);
        handle.join().unwrap();
    }
}
