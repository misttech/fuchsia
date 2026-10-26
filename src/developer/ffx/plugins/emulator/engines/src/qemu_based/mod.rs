// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The qemu_base module encapsulates traits and functions specific
//! for engines using QEMU as the emulator platform.

use crate::arg_templates::process_flag_template_with_context;
use crate::qemu_based::comms::{QemuSocket, spawn_pipe_thread};
use crate::show_output;
use emulator_instance::{
    AccelerationMode, ConsoleType, DiskImage, EmulatorConfiguration, EngineState, GuestConfig,
    NetworkingMode, Ramdisk, RamdiskKind, SerialMode,
};
use errors::ffx_bail;
use ffx_config::EnvironmentContext;
use ffx_emulator_common::config::EMU_START_TIMEOUT;
use ffx_emulator_common::tuntap::{TAP_INTERFACE_NAME, tap_ready};
use ffx_emulator_common::{config, dump_log_to_out, host_is_mac, process};
use ffx_emulator_config::{EmulatorEngine, EngineConsoleType, ShowDetail};
use ffx_target::{KnockError, TargetInfoQuery};
use fho::{FfxContext, Result, bug, return_bug, return_user_error, user_error};
use fuchsia_async::Timer;
use serde_json::{Deserializer, Value, json};
use shared_child::SharedChild;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};
use std::{env, str};
use tempfile::NamedTempFile;

pub(crate) fn make_absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map_err(|e| bug!("Failed to get current directory: {e}"))
            .map(|cwd| cwd.join(path))
    }
}

pub(crate) fn check_screenshot_preconditions(engine: &dyn EmulatorEngine, absolute_path: &Path) {
    assert!(
        engine.engine_state() == EngineState::Running,
        "Engine must be running (Current state: {:?})",
        engine.engine_state()
    );
    let config = engine.emu_config();
    assert!(
        config.device.screen.width > 0 && config.device.screen.height > 0,
        "The emulator virtual display has an invalid resolution ({}x{}).",
        config.device.screen.width,
        config.device.screen.height
    );
    assert!(absolute_path.is_absolute(), "Path must be absolute ({:?})", absolute_path);
    assert!(!absolute_path.is_dir(), "Output path must not be a directory ({:?})", absolute_path);
    if let Some(parent) = absolute_path.parent() {
        assert!(parent.is_dir(), "Parent path must be a directory ({:?})", parent);
    }
}

/// Reads the next QMP message from `responses`, waiting until `deadline` at most.
///
/// `responses` must be a JSON stream deserializer that reads from `stream` or from a clone of
/// it. `waiting_for` describes the expected message in error messages.
fn qmp_read<I>(
    stream: &UnixStream,
    responses: &mut I,
    deadline: Instant,
    waiting_for: &str,
) -> Result<Value>
where
    I: Iterator<Item = serde_json::Result<Value>>,
{
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return_user_error!("Timed out waiting for {waiting_for}.");
    }
    // The read timeout is a socket option, so it also applies to clones of `stream`.
    stream.set_read_timeout(Some(remaining)).map_err(|e| bug!("{e}"))?;
    let error = match responses.next() {
        Some(Ok(data)) => return Ok(data),
        None => return_user_error!("QMP connection closed while waiting for {waiting_for}."),
        Some(Err(error)) => error,
    };
    let kind = error.io_error_kind();
    if Instant::now() >= deadline
        || matches!(kind, Some(ErrorKind::WouldBlock | ErrorKind::TimedOut))
    {
        return_user_error!("Timed out waiting for {waiting_for}.");
    }
    if matches!(
        kind,
        Some(ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::UnexpectedEof)
    ) {
        return_user_error!("QMP connection closed while waiting for {waiting_for}: {error}");
    }
    return_user_error!("Error reading {waiting_for}: {error}")
}

/// Executes one QMP command on an initialized QMP connection.
///
/// Returns the `return` value of the reply. Skips messages that are not the reply, such as
/// asynchronous events. Errors if QEMU replies with an `error` object, or if no reply arrives
/// within `timeout`.
///
/// `responses` must be a JSON stream deserializer that reads from `stream` or from a clone of
/// it.
pub(crate) fn qmp_execute<I>(
    stream: &UnixStream,
    responses: &mut I,
    command: &str,
    arguments: Option<Value>,
    timeout: Duration,
) -> Result<Value>
where
    I: Iterator<Item = serde_json::Result<Value>>,
{
    // QEMU sends each reply only to the connection that sent the command. A counter is enough
    // to tell this command's reply apart from replies to earlier commands.
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let command_id = format!("{command}-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));

    let mut request = json!({ "execute": command, "id": command_id });
    if let Some(arguments) = arguments {
        request["arguments"] = arguments;
    }
    let deadline = Instant::now() + timeout;
    let mut writer = stream;
    writer
        .write_all(format!("{request}\n").as_bytes())
        .map_err(|e| user_error!("Error writing QMP command '{command}': {e}"))?;

    let waiting_for = format!("the QMP response to '{command}'");
    loop {
        let data = qmp_read(stream, responses, deadline, &waiting_for)?;
        if data.get("id").and_then(Value::as_str) != Some(command_id.as_str()) {
            log::debug!("Skipping QMP message while waiting for '{command}': {data}");
            continue;
        }
        if let Some(error) = data.get("error") {
            let desc = error
                .get("desc")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_string);
            return_user_error!("QMP command '{command}' failed: {desc}");
        }
        if let Some(return_value) = data.get("return") {
            return Ok(return_value.clone());
        }
        return_user_error!("Unexpected QMP response to '{command}': {data}");
    }
}

pub(crate) fn get_host_tool(context: &EnvironmentContext, name: &str) -> Result<PathBuf> {
    // Attempts to get a host tool from the SDK manifest. If it fails, falls
    // back to attempting to derive the path to the host tool binary by simply checking
    // for its existence in `ffx`'s directory.
    // TODO(b/289524702): When issues around including aemu in the sdk are resolved, this
    // hack can be removed.
    match ffx_config::get_host_tool(context, name) {
        Ok(path) => Ok(path),
        Err(error) => {
            log::warn!(
                "failed to get host tool {} from manifest. Trying local SDK dir: {}",
                name,
                error
            );
            let mut ffx_path = context
                .rerun_bin()
                .or_else(|_| std::env::current_exe())
                .map_err(|e| bug!("getting current ffx exe path for host tool {name}: {e}"))?;
            ffx_path = std::fs::canonicalize(ffx_path.clone())
                .map_err(|e| bug!("canonicalizing ffx path {ffx_path:?}: {e}"))?;

            let tool_path = ffx_path
                .parent()
                .ok_or_else(|| bug!("ffx path missing parent {ffx_path:?}"))?
                .join(name);

            if tool_path.exists() {
                log::info!("Using {tool_path:?} based on {ffx_path:?} directory for tool {name}");
                Ok(tool_path)
            } else {
                return_bug!(
                    "{error}. Host tool '{name}' not found after checking in `ffx` directory as stopgap."
                )
            }
        }
    }
}

/// The disk image tool which is distributed alongside the QEMU binary.
const QEMU_IMG_TOOL: &str = "qemu-img";

/// Returns the path to the `qemu-img` tool, which is located next to the QEMU binary.
fn get_qemu_img_tool(context: &EnvironmentContext) -> Result<PathBuf> {
    let qemu_path = get_host_tool(context, config::QEMU_TOOL)
        .map_err(|e| bug!("Cannot find {} in the SDK: {e}", config::QEMU_TOOL))?;
    let qemu_path = qemu_path.canonicalize().map_err(|e| {
        bug!("Failed to canonicalize the path to the emulator binary: {qemu_path:?}: {e}")
    })?;
    let tool_path = qemu_path.with_file_name(QEMU_IMG_TOOL);
    if !tool_path.is_file() {
        return_bug!("Cannot find the {QEMU_IMG_TOOL} tool. Tried {tool_path:?}");
    }
    Ok(tool_path)
}

/// Returns the path at which the qcow2 version of the raw disk image at `raw_path` is staged.
fn qcow2_image_path(raw_path: &Path) -> PathBuf {
    let mut file_name = raw_path.file_name().unwrap_or_default().to_os_string();
    file_name.push(".qcow2");
    raw_path.with_file_name(file_name)
}

const KNOCK_TARGET_TIMEOUT: Duration = Duration::from_secs(6);

pub(crate) mod comms;
pub(crate) mod crosvm;
pub(crate) mod femu;
pub(crate) mod gpt;
pub(crate) mod qemu;

const COMMAND_CONSOLE: &str = "./monitor";
const MACHINE_CONSOLE: &str = "./qmp";
const SERIAL_CONSOLE: &str = "./serial";

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct PortPair {
    pub guest: u16,
    pub host: u16,
}
/// How long `QemuBasedEngine::stop_emulator()` waits for the emulator process to exit after it has
/// been killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

/// Waits for the process with the given pid to exit, for at most `timeout`. Returns whether it has
/// exited.
async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let start = Instant::now();
    while process::is_running(pid) {
        if start.elapsed() >= timeout {
            return false;
        }
        Timer::new(Duration::from_millis(100)).await;
    }
    true
}

/// QemuBasedEngine collects the interface for
/// emulator engine implementations that use
/// QEMU as the emulator.
/// This allows the implementation to be shared
/// across multiple engine types.
pub(crate) trait QemuBasedEngine: EmulatorEngine {
    /// Checks that the required files are present
    fn check_required_files(&self, guest: &GuestConfig) -> Result<()> {
        guest.check_required_files().map_err(|e| user_error!(e))
    }

    /// Stages the source image files in an instance specific directory.
    /// Also resizes the fvms to the desired size and adds the authorized
    /// keys to the zbi.
    /// Returns an updated GuestConfig instance with the file paths set to
    /// the instance paths.
    async fn stage_image_files(
        env: &EnvironmentContext,
        instance_name: &str,
        emu_config: &EmulatorConfiguration,
        reuse: bool,
    ) -> Result<GuestConfig> {
        let mut updated_guest = emu_config.guest.clone();

        // Create the data directory if needed.
        let mut instance_root: PathBuf = env
            .query(ffx_config::keys::EMU_INSTANCE_ROOT_DIR)
            .build()
            .get_file(env)
            .map_err(|e| bug!("Error reading config for instance root: {e}"))?;
        // This should really just be part of the conversion, but the structure of the config file
        // makes it awkward to do. The `file_check` function doesn't allow for returning an error,
        // for example, and the `get_file` function has to support returning a `Vec` in addition to
        // a `PathBuf`.
        if !instance_root.has_root() {
            instance_root = std::fs::canonicalize(instance_root).bug()?;
        }
        instance_root.push(instance_name);
        fs::create_dir_all(&instance_root)
            .map_err(|e| bug!("Error creating {instance_root:?}: {e}"))?;

        if let Some(kernel_image) = &emu_config.guest.kernel_image {
            let kernel_name = kernel_image.file_name().ok_or_else(|| {
                bug!("cannot read kernel file name '{:?}'", emu_config.guest.kernel_image)
            })?;
            let kernel_path = instance_root.join(kernel_name);
            if kernel_path.exists() && reuse {
                log::debug!("Using existing file for {:?}", kernel_path.file_name().unwrap());
            } else {
                fs::copy(&kernel_image, &kernel_path)
                    .map_err(|e| bug!("cannot stage kernel file: {e}"))?;
            }
            updated_guest.kernel_image = Some(kernel_path);
        }

        // If the kernel is an efi image, or has no zbi, skip the zbi processing.
        let (ramdisk, vbmeta_path) = if let Some(ramdisk) = &emu_config.guest.ramdisk {
            let input_path = &ramdisk.path;
            let output_path = instance_root
                .join(input_path.file_name().ok_or_else(|| bug!("cannot read ramdisk file name"))?);
            let mut vbmeta_path = None;
            match ramdisk.kind {
                RamdiskKind::Test => {
                    // Test ramdisks remain unmodified.
                    log::debug!("Staging test ramdisk {input_path:?} into {output_path:?}");
                    fs::copy(input_path, &output_path).unwrap();
                }
                RamdiskKind::Zbi => {
                    if output_path.exists() && reuse {
                        log::debug!(
                            "Using existing file for {:?}",
                            output_path.file_name().unwrap()
                        );
                        // Note: If reusing the ZBI without modifications, there is the potential
                        // that the ssh keys have changed, or the ip address of the host interface
                        // has changed, which will cause the connection to the emulator instance to
                        // fail.
                    } else {
                        // Add the authorized public keys to the zbi image to enable SSH access to
                        // the guest. Also, in the GPT case, bake in the kernel command line parameters.
                        let kernel_cmdline = if emu_config.guest.is_gpt {
                            let c = emu_config.flags.kernel_args.join("\n");
                            log::debug!("Using kernel parameters in the ZBI: {}", c);
                            Some(c)
                        } else {
                            None
                        };
                        let serial_number = match &emu_config.runtime.serial_number {
                            SerialMode::Enabled(serial) => Some(serial.as_str()),
                            _ => None,
                        };
                        ffx_uefi_disk::embed_boot_data(
                            &env,
                            input_path,
                            &output_path,
                            kernel_cmdline.as_deref(),
                            serial_number,
                        )
                        .map_err(|e| bug!("cannot embed boot data: {e}"))?;
                        log::debug!(
                            "Staging {input_path:?} into {output_path:?} and embedding SSH keys",
                        );
                        match (
                            &emu_config.guest.vbmeta_key_file,
                            &emu_config.guest.vbmeta_key_metadata_file,
                            &emu_config.guest.disk_image,
                        ) {
                            (
                                Some(zbi_key_file),
                                Some(zbi_key_metadata_file),
                                Some(DiskImage::Gpt(_)),
                            ) => {
                                let vbmeta_out_path = instance_root.join("zircon.vbmeta");
                                ffx_uefi_disk::generate_vbmeta(
                                    zbi_key_file,
                                    zbi_key_metadata_file,
                                    &output_path,
                                    &vbmeta_out_path,
                                )?;
                                vbmeta_path = Some(vbmeta_out_path);
                            }
                            (zbi_key_file, zbi_key_metadata_file, Some(DiskImage::Gpt(_))) => {
                                return_user_error!(
                                    "Both PEM key (provided: {:?}) and the corresponding metadata file (provided: {:?}) are required.",
                                    zbi_key_file,
                                    zbi_key_metadata_file
                                );
                            }
                            (_, _, _) => {
                                log::debug!(
                                    "Generation of new vbmeta for the modified ZBI not required."
                                );
                            }
                        };
                    }
                }
            }
            (Some(Ramdisk { path: output_path, kind: ramdisk.kind }), vbmeta_path)
        } else {
            log::debug!("Skipping zbi staging; no zbi file in product bundle.");
            (None, None)
        };

        if let Some(disk_image) = &emu_config.guest.disk_image {
            let src_path = disk_image.as_ref();
            let dest_path = instance_root.join(
                src_path.file_name().ok_or_else(|| bug!("cannot read disk image file name"))?,
            );
            // Snapshots can only be stored in a qcow2 image, so when they are enabled the raw
            // image staged at `dest_path` is converted, and the emulator runs from the qcow2
            // image instead.
            let staged_path = if emu_config.runtime.snapshots_enabled {
                qcow2_image_path(&dest_path)
            } else {
                dest_path.clone()
            };

            if staged_path.exists() && reuse {
                log::debug!("Using existing file for {:?}", staged_path.file_name().unwrap());
            } else {
                let original_size: u64 = src_path.metadata().map_err(|e| bug!("{e}"))?.len();

                log::debug!("Disk image original size: {}", original_size);
                log::debug!(
                    "Disk image target size from product bundle {:?}",
                    emu_config.device.storage
                );

                let mut target_size =
                    emu_config.device.storage.as_bytes().expect("get device storage size");

                // The disk image needs to be expanded in size in order to make room
                // for the creation of the data volume. If the original
                // size is larger than the target size, update the target size
                // to 1.1 times the size of the original file.
                if target_size < original_size {
                    let new_target_size: u64 = original_size + (original_size / 10);
                    log::warn!("Disk image original size is larger than target size.");
                    log::warn!("Forcing target size to {new_target_size}");
                    target_size = new_target_size;
                }

                // The method of resizing is different, depending on the type of the disk image.
                match disk_image {
                    DiskImage::Fvm(_) => {
                        fs::copy(src_path, &dest_path)
                            .map_err(|e| bug!("cannot stage disk image file: {e}"))?;
                        Self::fvm_extend(env, &dest_path, target_size)?;
                    }
                    DiskImage::Fxfs(_) => {
                        let mut tmp =
                            NamedTempFile::new_in(&instance_root).map_err(|e| bug!("{e}"))?;

                        {
                            let mut reader = std::fs::File::open(src_path)
                                .map_err(|e| bug!("open failed: {e}"))?;

                            // The image could be either a sparse or a full image.  If sparse
                            // then inflate it to the destination.  If not sparse, then just
                            // copy it.
                            if sparse::is_sparse_image(&mut reader) {
                                sparse::unsparse(&mut reader, tmp.as_file_mut())
                                    .map_err(|e| bug!("cannot stage Fxfs image: {e}"))?;
                            } else {
                                // re-open the file because the check for sparseness can fail and
                                // result in a reader that hasn't seek'd back to the start of the
                                // file.
                                let mut reader = std::fs::File::open(src_path)
                                    .map_err(|e| bug!("re-open failed: {e}"))?;

                                std::io::copy(&mut reader, &mut tmp)
                                    .map_err(|e| bug!("cannot stage Fxfs image: {e}"))?;
                            }
                        }
                        if original_size < target_size {
                            // Resize the image if needed.
                            tmp.as_file().set_len(target_size).map_err(|e| {
                                bug!("Failed to temp file to {target_size} bytes: {e}")
                            })?;
                        }
                        tmp.persist(&dest_path).map_err(|e| {
                            bug!("Failed to persist temp Fxfs image to {dest_path:?}: {e}")
                        })?;
                    }
                    // FAT does not need to be resized.
                    // GPT images are resized during staging when calling into make_fuchsia_vol.
                    DiskImage::Fat(_) | DiskImage::Gpt(_) => (),
                };

                if emu_config.runtime.snapshots_enabled {
                    match disk_image {
                        DiskImage::Fvm(_) | DiskImage::Fxfs(_) => {
                            Self::convert_to_qcow2(env, &dest_path, &staged_path)?;
                        }
                        DiskImage::Fat(_) | DiskImage::Gpt(_) => {
                            return_user_error!(
                                "Snapshots are not supported for emulators which boot from a \
                                FAT or GPT disk image."
                            );
                        }
                    }
                } else {
                    // If the instance had snapshots enabled before it was staged again, the qcow2
                    // image it ran from is still there, along with its snapshots. Nothing uses it
                    // any more, so it is removed rather than left behind.
                    let unused_path = qcow2_image_path(&dest_path);
                    if unused_path.exists() {
                        fs::remove_file(&unused_path).map_err(|e| {
                            bug!(
                                "Error removing the unused disk image {}: {e}",
                                unused_path.display()
                            )
                        })?;
                    }
                }
            }
            // Update the guest config to reference the staged disk image.
            updated_guest.disk_image = match disk_image {
                DiskImage::Fat(_) => Some(disk_image.clone()),
                DiskImage::Fvm(_) => Some(DiskImage::Fvm(staged_path)),
                DiskImage::Fxfs(_) => Some(DiskImage::Fxfs(staged_path)),
                DiskImage::Gpt(_) => Some(DiskImage::Gpt(instance_root.join("gpt_disk.img"))),
            };
        } else {
            updated_guest.disk_image = None;
        }

        updated_guest.ramdisk = ramdisk;
        if emu_config.guest.is_efi() || emu_config.guest.is_gpt {
            let dest = instance_root.join("OVMF_VARS.fd");
            if !dest.exists() {
                fs::copy(&emu_config.guest.ovmf_vars, &dest).map_err(|e| {
                    bug!(
                        "cannot copy ovmf vars file  from {:?} to {dest:?}: {e}",
                        &emu_config.guest.ovmf_vars
                    )
                })?;
            }
            updated_guest.ovmf_vars = dest;
        }

        if emu_config.guest.is_gpt {
            let zedboot_cmdline_path = instance_root.join("zedboot_cmdline");
            if zedboot_cmdline_path.exists() && reuse {
                log::debug!(
                    "Using existing file for {:?}",
                    zedboot_cmdline_path.file_name().unwrap()
                );
            } else {
                let c = emu_config.flags.kernel_args.join("\n");
                gpt::write_zedboot_cmdline(&zedboot_cmdline_path, Some(&c))?;
            }
            let gpt_image_path = if let Some(ref p) = updated_guest.disk_image {
                p.to_path_buf()
            } else {
                return_bug!("No path for the GPT disk image found.");
            };
            if gpt_image_path.exists() && reuse {
                log::debug!("Using existing file for {:?}", gpt_image_path.file_name().unwrap());
            } else {
                let product_path = if let Some(ref path) = emu_config.guest.product_bundle_path {
                    path
                } else {
                    &PathBuf::new()
                };
                let mut image = gpt::FuchsiaFullDiskImageBuilder::new();
                image = image
                    .arch(emu_config.device.cpu.architecture)
                    .cmdline(&zedboot_cmdline_path)
                    .output_path(&gpt_image_path)
                    .product_bundle(product_path)
                    .resize(gpt::DEFAULT_EMU_DISK_SIZE)
                    .use_fxfs(true)
                    .vbmeta(vbmeta_path)
                    .zbi(updated_guest.ramdisk.map(|ramdisk| ramdisk.path));
                log::debug!("Building image with {image:#?}");
                image.build(&env)?;
            }
            // Since the one multi-partition GPT image is passed to qemu, no kernel and zbi images
            // are required to boot the emulator.
            updated_guest.kernel_image = None;
            updated_guest.ramdisk = None;
            updated_guest.is_gpt = true;
        }

        Ok(updated_guest)
    }

    fn fvm_extend(env: &EnvironmentContext, dest_path: &Path, target_size: u64) -> Result<()> {
        let fvm_tool = get_host_tool(env, config::FVM_HOST_TOOL)
            .map_err(|e| bug!("cannot locate fvm tool: {e}"))?;
        let mut resize_command = Command::new(fvm_tool);

        resize_command.arg(&dest_path).arg("extend").arg("--length").arg(target_size.to_string());
        log::debug!("FVM Running command to resize: {:?}", resize_command);

        let resize_result = resize_command.output().map_err(|e| bug!("{e}"))?;

        log::debug!("FVM command result: {resize_result:?}");

        if !resize_result.status.success() {
            bug!(
                "Error resizing fvm: {}",
                str::from_utf8(&resize_result.stderr).map_err(|e| bug!("{e}"))?
            );
        }
        Ok(())
    }

    /// Converts the raw disk image at `raw_path` into a qcow2 image at `qcow2_path`, and removes
    /// the raw image. The conversion is done by the `qemu-img` tool which is distributed
    /// alongside the QEMU binary.
    fn convert_to_qcow2(
        env: &EnvironmentContext,
        raw_path: &Path,
        qcow2_path: &Path,
    ) -> Result<()> {
        let qemu_img = get_qemu_img_tool(env)?;
        let mut convert_command = Command::new(&qemu_img);
        convert_command.args(["convert", "-f", "raw", "-O", "qcow2"]).arg(raw_path).arg(qcow2_path);
        log::debug!("Running command to convert the disk image: {:?}", convert_command);

        let convert_result = convert_command
            .output()
            .map_err(|e| bug!("Error running {}: {e}", qemu_img.display()))?;
        if !convert_result.status.success() {
            return_bug!(
                "Error converting {} to qcow2: {}",
                raw_path.display(),
                String::from_utf8_lossy(&convert_result.stderr)
            );
        }
        fs::remove_file(raw_path)
            .map_err(|e| bug!("Error removing {} after conversion: {e}", raw_path.display()))
    }

    fn validate_network_flags(&self, emu_config: &EmulatorConfiguration) -> Result<()> {
        match emu_config.host.networking {
            NetworkingMode::None => {
                // Check for console/monitor.
                if emu_config.runtime.console == ConsoleType::None {
                    return_user_error!(
                        "Running without networking enabled and no interactive console;\n\
                        there will be no way to communicate with this emulator.\n\
                        Restart with --console/--monitor or with networking enabled to proceed."
                    );
                }
            }
            NetworkingMode::Auto => {
                // Shouldn't be possible to land here.
                return_bug!("Networking mode is unresolved after configuration.");
            }
            NetworkingMode::Tap => {
                // Officially, MacOS tun/tap is unsupported. tap_ready() uses the "ip" command to
                // retrieve details about the target interface, but "ip" is not installed on macs
                // by default. That means, if tap_ready() is called on a MacOS host, it returns a
                // Result::Error, which would cancel emulation. However, if an end-user sets up
                // tun/tap on a MacOS host we don't want to block that, so we check the OS here
                // and make it a warning to run on MacOS instead.
                if host_is_mac() {
                    return_user_error!(
                        "Tun/Tap networking mode is not currently supported on MacOS. \
                        You may experience errors with your current configuration."
                    );
                } else {
                    // tap_ready() has some good error reporting, so just return the Result.
                    return tap_ready();
                }
            }
            NetworkingMode::User => (),
        }
        Ok(())
    }

    async fn stage(&mut self, context: &EnvironmentContext) -> Result<()> {
        let emu_config = self.emu_config_mut();
        let reuse = emu_config.runtime.reuse;
        let name = &emu_config.runtime.name;

        emu_config.guest = Self::stage_image_files(context, &name, emu_config, reuse).await?;

        // This is done to avoid running emu in the same directory as the kernel or other files
        // that are used by qemu. If the multiboot.bin file is in the current directory, it does
        // not start correctly. This probably could be temporary until we know the images loaded
        // do not have files directly in $sdk_root.
        env::set_current_dir(emu_config.runtime.instance_directory.parent().unwrap())
            .map_err(|e| bug!("problem changing directory to instance dir: {e}"))?;

        emu_config.flags = process_flag_template_with_context(emu_config, context)
            .map_err(|e| bug!("Emulator engine failed to process the flags template file: {e}."))?;

        Ok(())
    }

    async fn run(
        &mut self,
        context: &EnvironmentContext,
        mut emulator_cmd: Command,
    ) -> Result<i32> {
        if self.emu_config().runtime.console == ConsoleType::None {
            let stdout = File::create(&self.emu_config().host.log).map_err(|e| {
                bug!("Couldn't open log file {:?}: {e}", &self.emu_config().host.log)
            })?;
            let stderr = stdout.try_clone().map_err(|e| {
                bug!("Failed trying to clone stdout for the emulator process: {e}.")
            })?;
            emulator_cmd.stdout(stdout).stderr(stderr);
            eprintln!("Logging to {:?}", self.emu_config().host.log);
        }

        // If using TAP, check for an upscript to run.
        if let Some(script) = match &self.emu_config().host.networking {
            NetworkingMode::Tap => &self.emu_config().runtime.upscript,
            _ => &None,
        } {
            let status = Command::new(script)
                .arg(TAP_INTERFACE_NAME)
                .status()
                .map_err(|e| bug!("Problem running upscript '{}': {e}", &script.display()))?;
            if !status.success() {
                return_user_error!(
                    "Upscript {} returned non-zero exit code {}",
                    script.display(),
                    status.code().map_or_else(|| "None".to_string(), |v| format!("{}", v))
                );
            }
        }

        // This is a critical section because if the tool is terminated before `save_to_disk`
        // completes, the emulator will run in its own session (due to `setsid`) but `ffx`
        // will not have recorded its PID. This would leave an orphaned process that cannot
        // be managed by `ffx emu stop`.
        //
        // Block SIGHUP and SIGINT before spawning to prevent race conditions where the
        // terminal closes or the user interrupts before we can save the PID.
        let mut mask = nix::sys::signal::SigSet::empty();
        mask.add(nix::sys::signal::Signal::SIGHUP);
        mask.add(nix::sys::signal::Signal::SIGINT);
        let mut old_mask = nix::sys::signal::SigSet::empty();

        nix::sys::signal::sigprocmask(
            nix::sys::signal::SigmaskHow::SIG_BLOCK,
            Some(&mask),
            Some(&mut old_mask),
        )
        .map_err(|e| bug!("Failed to block signals: {e}"))?;

        log::debug!("Spawning emulator with {emulator_cmd:?}");

        // Configure `pre_exec` to call `setsid` in the child.
        // SAFETY: `pre_exec` is unsafe because it runs in the child process after `fork`
        // but before `exec`. According to its documentation, the closure must only use
        // async-signal-safe operations. `setsid` is async-signal-safe (see
        // https://man7.org/linux/man-pages/man7/signal-safety.7.html). We do not
        // allocate memory or acquire any locks in this closure, which are considered
        // unsafe operations in `pre_exec` documentation due to the risk of deadlocks
        // if another thread held a lock at the time of forking.
        unsafe {
            emulator_cmd.pre_exec(move || {
                nix::unistd::setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            });
        }

        let shared_process = SharedChild::spawn(&mut emulator_cmd)
            .map_err(|e| bug!("Cannot spawn emulator: {e}"))?;
        let child_arc = Arc::new(shared_process);

        self.set_pid(child_arc.id());
        self.set_engine_state(EngineState::Running);

        if self.emu_config().host.networking == NetworkingMode::User {
            // Capture the port mappings for user mode networking.
            let now = fuchsia_async::MonotonicInstant::now();
            match self.read_port_mappings().await {
                Ok(_) => {
                    log::debug!("Writing updated mappings");
                    self.save_to_disk().await?;
                }
                Err(e) => {
                    if self.is_running().await {
                        return Err(e);
                    } else {
                        let log_contents = fs::read_to_string(&self.emu_config().host.log)
                            .map_err(|e| bug!("{e}"))?;
                        return_user_error!("{e}: {log_contents}");
                    }
                }
            };
            let elapsed_ms = now.elapsed().as_millis();
            log::debug!("reading port mappings took {elapsed_ms}ms");
        } else {
            self.save_to_disk().await?;
        }

        // Restore the original signal mask after the critical section.
        nix::sys::signal::sigprocmask(
            nix::sys::signal::SigmaskHow::SIG_SETMASK,
            Some(&old_mask),
            None,
        )
        .map_err(|e| bug!("Failed to restore signal mask: {e}"))?;

        if self.emu_config().runtime.debugger {
            eprintln!("The emulator will wait for a debugger to attach before starting up.");
            eprintln!("Attach to process {} to continue launching the emulator.", self.get_pid());
        }

        if self.emu_config().runtime.console == ConsoleType::Console {
            // When running with '--console' mode, the user is directly interacting with the guest
            // console. Therefore wait until the execution of QEMU or AEMU terminates.
            match fuchsia_async::unblock(move || process::monitored_child_process(&child_arc)).await
            {
                Ok(_) => {
                    return Ok(0);
                }
                Err(e) => {
                    if let Some(stop_error) = self.stop_emulator().await.err() {
                        log::debug!(
                            "Error encountered in stop when handling failed launch: {:?}",
                            stop_error
                        );
                    }
                    ffx_bail!("Emulator launcher did not terminate properly, error: {}", e)
                }
            }
        } else if !self.emu_config().runtime.startup_timeout.is_zero() {
            // Wait until the emulator is considered "active" before returning to the user.
            let startup_timeout = self.emu_config().runtime.startup_timeout.as_secs();
            eprint!("Waiting for Fuchsia to start (up to {} seconds).", startup_timeout);
            log::debug!("Waiting for Fuchsia to start (up to {} seconds)...", startup_timeout);
            let name = self.emu_config().runtime.name.clone();
            let start = Instant::now();
            let mut connection_errors = Vec::new();
            while start.elapsed().as_secs() <= startup_timeout {
                let compat_res = ffx_target::knock_target(
                    &TargetInfoQuery::try_from(name.clone())
                        .map_err(|e| user_error!("Invalid target specifier: {}", e))?,
                    &context,
                    Some(KNOCK_TARGET_TIMEOUT),
                )
                .await;
                if let Ok(()) = compat_res {
                    eprintln!("\nEmulator is ready.");
                    log::debug!("Emulator is ready after {} seconds.", start.elapsed().as_secs());
                    return Ok(0);
                } else {
                    match compat_res.unwrap_err() {
                        KnockError::NonCritical(e) => {
                            connection_errors.push(e);
                            log::debug!(
                                "Unable to connect to emulator: {:?}",
                                connection_errors.last().unwrap()
                            );
                        }
                        KnockError::Critical(e) => {
                            eprintln!("Failed to connect to emulator: {e:?}");
                            if format!("{e:?}").contains("timed out during banner exchange") {
                                eprintln!(
                                    "There was a timeout during SSH banner exchange,\n client was \
                                    unable to establish a connection to the SSH server\n within the timeout \
                                    period. Please check target logs or restart the emulator\n \
                                    by running `ffx emu stop --all && ffx emu start`."
                                );
                            }
                            return Ok(1);
                        }
                    }
                }

                // Perform a check to make sure the process is still alive, otherwise report
                // failure to launch.
                if !self.is_running().await {
                    let log_contents = match fs::read_to_string(&self.emu_config().host.log) {
                        Ok(s) => s,
                        Err(e) => format!("could not read log: {e}"),
                    };
                    let message = format!("Emulator process failed to launch.\n{log_contents}");
                    log::error!("{message}");
                    eprintln!("\n{message}");
                    self.set_engine_state(EngineState::Staged);
                    self.save_to_disk().await?;

                    return Ok(1);
                }

                // Output a little status indicator to show we haven't gotten stuck.
                // Note that we discard the result on the flush call; it's not important enough
                // that we flushed the output stream to derail the launch.
                eprint!(".");
                std::io::stderr().flush().ok();

                // Sleep for a bit to allow the instance to make progress
                Timer::new(Duration::from_secs(1)).await;
            }

            // If we're here, it means the emulator did not start within the timeout.

            eprintln!();
            eprintln!(
                "After {} seconds, the emulator has not responded to network queries.",
                self.emu_config().runtime.startup_timeout.as_secs()
            );
            eprintln!("Here are the following errors encountered while connecting:");
            for (i, e) in connection_errors.iter().enumerate() {
                eprintln!("\t{}: {e:?}", i + 1);
            }
            if connection_errors
                .iter()
                .any(|e| format!("{e:?}").contains("timed out during banner exchange"))
            {
                eprintln!(
                    "There was a timeout during SSH banner exchange,\n client was \
                    unable to establish a connection to the SSH server\n within the timeout \
                    period. Please check target logs or restart the emulator\n \
                    by running `ffx emu stop --all && ffx emu start`."
                );
            }
            if self.is_running().await {
                eprintln!("The emulator process is still running (pid {}).", self.get_pid());
                eprintln!(
                    "The emulator is configured to {} network access.",
                    match self.emu_config().host.networking {
                        NetworkingMode::Tap => "use tun/tap-based",
                        NetworkingMode::User => "use user-mode/port-mapped",
                        NetworkingMode::None => "disable all",
                        NetworkingMode::Auto => return_bug!(
                            "Auto Networking mode should not be possible after staging \
                            is complete. Configuration is corrupt; bailing out."
                        ),
                    }
                );
                eprintln!(
                    "Hardware acceleration is {}.",
                    if self.emu_config().host.acceleration == AccelerationMode::Hyper {
                        "enabled"
                    } else {
                        "disabled, which significantly slows down the emulator"
                    }
                );
                eprintln!(
                    "You can execute `ffx target list` to keep monitoring the device, \
                    or `ffx emu stop` to terminate it."
                );
                eprintln!(
                    "You can also change the timeout if you keep encountering this \
                    message by executing `ffx config set {} <seconds>`.",
                    EMU_START_TIMEOUT
                );
            } else {
                eprintln!();
                eprintln!(
                    "Emulator process failed to launch, but we don't know the cause. \
                    Printing the contents of the emulator log...\n"
                );
                match dump_log_to_out(&self.emu_config().host.log, &mut std::io::stderr()) {
                    Ok(_) => (),
                    Err(e) => eprintln!("Couldn't print the log: {:?}", e),
                };
            }

            log::warn!("Emulator did not respond to a health check before timing out.");
            return Ok(1);
        }
        Ok(0)
    }

    fn show(&self, details: Vec<ShowDetail>) -> Vec<ShowDetail> {
        let mut results: Vec<ShowDetail> = vec![];
        for segment in details {
            match segment {
                ShowDetail::Cmd { .. } => {
                    results.push(show_output::command(&self.build_emulator_cmd()))
                }
                ShowDetail::Config { .. } => results.push(show_output::config(self.emu_config())),
                ShowDetail::Device { .. } => results.push(show_output::device(self.emu_config())),
                ShowDetail::Net { .. } => results.push(show_output::net(self.emu_config())),
                ShowDetail::Instance { .. } => {
                    results.push(show_output::instance(self.get_instance_data()))
                }
                _ => {}
            };
        }
        results
    }

    async fn stop_emulator(&mut self) -> Result<()> {
        if self.is_running().await {
            let pid = self.get_pid();
            log::info!("Terminating running instance {pid}");
            if let Some(terminate_error) = process::terminate(pid).err() {
                log::warn!("Error encountered terminating process: {:?}", terminate_error);
            }
            // The process doesn't go away the moment it is killed: the kernel still has to release
            // the guest's memory and the files which the emulator had open, which takes seconds
            // for a large guest. Wait for that, so that the instance can be started again as soon
            // as stop returns, as `ffx emu stop --persist && ffx emu start --reuse` does.
            if !wait_for_exit(pid, STOP_TIMEOUT).await {
                return_user_error!(
                    "The emulator process (pid {pid}) was killed, but is still shutting down {} \
                    seconds later. Wait for it to exit before starting the instance again.",
                    STOP_TIMEOUT.as_secs()
                );
            }
        }
        self.set_engine_state(EngineState::Staged);
        self.save_to_disk().await
    }

    /// Access to the engine's pid field.
    fn set_pid(&mut self, pid: u32);
    fn get_pid(&self) -> u32;

    /// Access to the engine's engine_state field.
    fn set_engine_state(&mut self, state: EngineState);
    fn get_engine_state(&self) -> EngineState;

    /// Attach to emulator's console socket.
    fn attach_to(&self, path: &Path, console: EngineConsoleType) -> Result<()> {
        let console_path = self.get_path_for_console_type(path, console);
        let mut socket = QemuSocket::new(&console_path);
        socket.connect().map_err(|e| bug!("Error connecting to console: {e}"))?;
        let stream = socket.stream().ok_or_else(|| bug!("No socket connected."))?;
        let (tx, rx) = channel();

        let _t1 = spawn_pipe_thread(
            std::io::stdin(),
            stream.try_clone().map_err(|e| bug!("{e}"))?,
            tx.clone(),
        );
        let _t2 =
            spawn_pipe_thread(stream.try_clone().map_err(|e| bug!("{e}"))?, std::io::stdout(), tx);

        // Now that the threads are reading and writing, we wait for one to send back an error.
        let error = rx.recv().map_err(|e| bug!("recv error: {e}"));
        log::debug!("{error:?}");
        eprintln!("{error:?}");
        stream.shutdown(Shutdown::Both).map_err(|e| bug!("Error shutting down stream: {e}"))?;
        Ok(())
    }

    fn get_path_for_console_type(&self, path: &Path, console: EngineConsoleType) -> PathBuf {
        path.join(match console {
            EngineConsoleType::Command => COMMAND_CONSOLE,
            EngineConsoleType::Machine => MACHINE_CONSOLE,
            EngineConsoleType::Serial => SERIAL_CONSOLE,
            EngineConsoleType::None => panic!("No path exists for EngineConsoleType::None"),
        })
    }

    /// Connect to the qmp socket for the emulator instance and read the port mappings.
    /// User mode networking needs to map guest TCP ports to host ports. This can be done by
    /// specifying the guest port and either a preassigned port from the command line, or
    /// leaving the host port unassigned, and a port is assigned by the emulator at startup.
    ///
    /// This method waits for the QMP socket to be open, then reads the user mode networking status
    /// to retrieve the port mappings.
    ///
    /// The method returns if all the port mappings are made, or if there is an error communicating
    /// with QEMU. If emu_config().runtime.startup_timeout is positive, an error is returned if
    /// the mappings are not available within this time.
    async fn read_port_mappings(&mut self) -> Result<()> {
        // Check if there are any ports not already mapped.
        if !self.emu_config().host.port_map.values().any(|m| m.host.is_none()) {
            log::debug!("No unmapped ports found.");
            return Ok(());
        }

        let max_elapsed = if self.emu_config().runtime.startup_timeout.is_zero() {
            // if there is no timeout, we should technically return immediately, but it does
            // not make sense with unmapped ports, so give it a few seconds to try.
            Duration::from_secs(10)
        } else {
            self.emu_config().runtime.startup_timeout
        };

        // Open machine socket
        let instance_dir = &self.emu_config().runtime.instance_directory;
        let console_path = self.get_path_for_console_type(instance_dir, EngineConsoleType::Machine);
        let mut socket = QemuSocket::new(&console_path);

        // Start the timeout tracking here so it includes opening the socket,
        // which may have to wait for qemu to create the socket.
        let start = Instant::now();
        let mut qmp_stream = self.open_socket(&mut socket, &max_elapsed).await?;
        let mut response_iter =
            Deserializer::from_reader(qmp_stream.try_clone().map_err(|e| bug!("{e}"))?)
                .into_iter::<Value>();

        // Loop reading the responses on the socket, and send the request to get the
        // user network information.
        loop {
            if start.elapsed() > max_elapsed {
                return_bug!("Reading port mappings timed out");
            }

            match response_iter.next() {
                Some(Ok(data)) => {
                    if let Some(return_string) = data.get("return") {
                        let port_pairs =
                            Self::parse_return_string(return_string.as_str().unwrap_or(""))?;
                        let mut modified = false;
                        // Iterate over the parsed port pairs, then find the matching entry in
                        // the port map.
                        // There are a small number of ports that need to be mapped, so the
                        // nested loop should not be a performance concern.
                        for pair in port_pairs {
                            for v in self.emu_config_mut().host.port_map.values_mut() {
                                if v.guest == pair.guest {
                                    if v.host != Some(pair.host) {
                                        v.host = Some(pair.host);
                                        modified = true;
                                        log::debug!("port mapped {pair:?}");
                                    }
                                }
                            }
                        }

                        // If the mapping was updated and there are no more unmapped ports,
                        // save and return.
                        if modified
                            && !self.emu_config().host.port_map.values().any(|m| m.host.is_none())
                        {
                            return Ok(());
                        }
                    } else {
                        log::debug!("Ignoring non return object {:?}", data);
                    }
                }
                Some(Err(e)) => {
                    log::debug!("Error reading qmp stream {e:?}")
                }
                None => {
                    log::debug!("None returned from qmp iterator");
                    // Pause a moment to allow qemu to make progress.
                    Timer::new(Duration::from_millis(100)).await;
                    continue;
                }
            };

            // Pause a moment to allow qemu to make progress.
            Timer::new(Duration::from_millis(100)).await;
            // Send { "execute": "human-monitor-command", "arguments": { "command-line": "info usernet" } }
            log::debug!("writing info usernet command");
            qmp_stream
                .write_fmt(format_args!(
                    "{}\n",
                    json!({
                        "execute": "human-monitor-command",
                        "arguments": { "command-line": "info usernet"}
                    })
                ))
                .map_err(|e| bug!("Error writing info usernet: {e}"))?;
        }
    }

    /// Parse the user network return string.
    /// The user network info is only available as text, so we need to parse the lines.
    /// This has been tested with AEMU and QEMU up to 7.0, but it is possible
    /// the format may change.
    fn parse_return_string(input: &str) -> Result<Vec<PortPair>> {
        let mut pairs: Vec<PortPair> = vec![];
        log::debug!("parsing_return_string return {input}");
        let mut saw_heading = false;
        for l in input.lines() {
            let parts: Vec<&str> = l.split_whitespace().map(|ele| ele.trim()).collect();

            // The heading has columns with multiple words, so the field count is more than the
            // data row.
            //Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ
            //TCP[ESTABLISHED]   63       10.0.2.15 56727  74.125.199.113   443     0     0
            match parts[..] {
                [
                    "Protocol[State]",
                    "FD",
                    "Source",
                    "Address",
                    "Port",
                    "Dest.",
                    "Address",
                    "Port",
                    "RecvQ",
                    "SendQ",
                ] => {
                    saw_heading = true;
                }
                [protocol_state, _, _, host_port, _, guest_port, _, _] => {
                    if protocol_state == "TCP[HOST_FORWARD]" {
                        let guest: u16 =
                            guest_port.parse().map_err(|e| bug!("error parsing: {e}"))?;
                        let host: u16 =
                            host_port.parse().map_err(|e| bug!("error parsing: {e}"))?;
                        pairs.push(PortPair { guest, host });
                    } else {
                        log::debug!("Skipping non host-forward row: {l}");
                    }
                }
                [] => log::debug!("Skipping empty line"),
                _ => log::debug!("Skipping unknown part collecton {parts:?}"),
            }
        }
        // Check that the heading column names have not changed. This could be a name change or schema change,
        // it could also be that the command did not return the header because the network objects are not available
        // yet, so log an error, but don't return an error.
        if !saw_heading {
            log::error!("Did not see expected header in {input}");
        }
        return Ok(pairs);
    }

    /// Opens the given socket waiting up to max_elapsed for the socket to be created and opened.
    async fn open_socket(
        &mut self,
        socket: &mut QemuSocket,
        max_elapsed: &Duration,
    ) -> Result<UnixStream> {
        let start = Instant::now();
        loop {
            if start.elapsed() > *max_elapsed {
                return_user_error!("Timed out opening the emulator's QMP socket.");
            }
            if !self.is_running().await {
                return_user_error!("Emulator instance is not running.");
            }
            // Wait for being able to connect to the socket.
            match socket.connect() {
                Ok(()) => {
                    match socket.stream() {
                        Some(mut qmp_stream) => {
                            // Send the qmp_capabilities command to initialize the conversation.
                            qmp_stream
                                .write_all(b"{ \"execute\": \"qmp_capabilities\" }\n")
                                .map_err(|e| bug!("Error writing qmp_capabilities: {e}"))?;
                            return Ok(qmp_stream);
                        }
                        None => {
                            log::debug!("Could not open machine socket");
                        }
                    };
                }
                Err(e) => {
                    log::debug!("Could not open machine socket: {e:?}");
                }
            };

            Timer::new(Duration::from_millis(100)).await;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use async_trait::async_trait;
    use emulator_instance::{
        DataAmount, DataUnits, EmulatorInstanceData, EmulatorInstanceInfo, EmulatorInstances,
        EngineType, PortMapping,
    };
    use ffx_config::environment::{EnvironmentKind, TestEnvBuilder};

    use serde::{Deserialize, Serialize};
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::os::unix::prelude::PermissionsExt as _;
    use tempfile::{TempDir, tempdir};

    #[derive(Default, Serialize)]
    struct TestEngine {
        pid: u32,
        state: EngineState,
    }
    impl QemuBasedEngine for TestEngine {
        fn set_pid(&mut self, pid: u32) {
            self.pid = pid;
        }
        fn get_pid(&self) -> u32 {
            self.pid
        }
        fn set_engine_state(&mut self, state: EngineState) {
            self.state = state;
        }
        fn get_engine_state(&self) -> EngineState {
            self.state
        }
    }
    #[async_trait(?Send)]
    impl EmulatorEngine for TestEngine {
        fn engine_state(&self) -> EngineState {
            self.state
        }
        fn engine_type(&self) -> EngineType {
            EngineType::default()
        }
        async fn is_running(&mut self) -> bool {
            process::is_running(self.pid)
        }
        async fn save_to_disk(&self) -> Result<()> {
            Ok(())
        }
    }
    const ORIGINAL: &str = "THIS_STRING";
    const ORIGINAL_PADDED: &str = "THIS_STRING\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
    const UPDATED: &str = "THAT_VALUE*";
    const UPDATED_PADDED: &str = "THAT_VALUE*\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
    const VBMETA_TEST_KEY: &str = include_str!("../../test_data/testkey_atx_psk.pem");
    const VBMETA_TEST_KEY_METADATA: &[u8] = include_bytes!("../../test_data/atx_metadata.bin");

    #[derive(Copy, Clone, PartialEq)]
    enum DiskImageFormat {
        Fvm,
        Fxfs,
        Gpt,
    }

    impl DiskImageFormat {
        pub fn as_disk_image(&self, path: impl AsRef<Path>) -> DiskImage {
            match self {
                Self::Fvm => DiskImage::Fvm(path.as_ref().to_path_buf()),
                Self::Fxfs => DiskImage::Fxfs(path.as_ref().to_path_buf()),
                Self::Gpt => DiskImage::Gpt(path.as_ref().to_path_buf()),
            }
        }
    }

    pub(crate) async fn make_fake_sdk(mut builder: TestEnvBuilder) -> TestEnvBuilder {
        let isolate_root = builder.isolate_root();
        let manifest_path = isolate_root.join("meta/manifest.json");
        fs::create_dir_all(manifest_path.parent().unwrap()).expect("temp sdk dir");
        fs::write(
            &manifest_path,
            r#"{ "arch": {  "host": "x86_64-linux-gnu",  "target": ["x64" ] },
            "id": "9999",
            "parts": [
                {
      "meta": "qemu_uefi_internal_x64-meta.json",
      "type": "companion_host_tool"
    }],  "root": "..",
  "schema_version": "1"}"#,
        )
        .expect("sdk manifest");

        const ECHO_SCRIPT_CONTENTS: &str = "#!/bin/bash\necho \"$@\"";

        let fake_qemu = isolate_root.join("fake_qemu");
        fs::write(&fake_qemu, ECHO_SCRIPT_CONTENTS).expect("fake qemu");
        fs::set_permissions(&fake_qemu, fs::Permissions::from_mode(0o770))
            .expect("setting permissions");

        // The qemu-img tool is looked up next to the QEMU binary. Its last argument is the image
        // to work on. This fake version answers `snapshot -l` with the contents of the image, so
        // that a test decides which snapshots are listed for it, and otherwise records the
        // arguments it is called with in the image, which it is being asked to create.
        const FAKE_QEMU_IMG_CONTENTS: &str = "#!/bin/bash\n\
            if [[ \"$1 $2\" == \"snapshot -l\" ]]; then cat \"${@: -1}\"; \
            else echo \"$@\" > \"${@: -1}\"; fi";
        let fake_qemu_img = isolate_root.join(QEMU_IMG_TOOL);
        fs::write(&fake_qemu_img, FAKE_QEMU_IMG_CONTENTS).expect("fake qemu-img");
        fs::set_permissions(&fake_qemu_img, fs::Permissions::from_mode(0o770))
            .expect("setting permissions");

        let fake_aemu = isolate_root.join("fake_aemu");
        fs::write(&fake_aemu, ECHO_SCRIPT_CONTENTS).expect("fake aemu");
        fs::set_permissions(&fake_aemu, fs::Permissions::from_mode(0o770))
            .expect("setting permissions");

        let fake_fvm = isolate_root.join("fake_fvm");
        fs::write(&fake_fvm, ECHO_SCRIPT_CONTENTS).expect("fake fvm");
        fs::set_permissions(&fake_fvm, fs::Permissions::from_mode(0o770))
            .expect("setting permissions");

        let fake_zbi = isolate_root.join("fake_zbi");
        fs::write(&fake_zbi, ECHO_SCRIPT_CONTENTS).expect("fake zbi");
        fs::set_permissions(&fake_zbi, fs::Permissions::from_mode(0o770))
            .expect("setting permissions");

        builder
            .user_config("sdk.root", isolate_root.to_string_lossy())
            .user_config("sdk.overrides.qemu_internal", fake_qemu.to_string_lossy())
            .user_config("sdk.overrides.crosvm_internal", fake_qemu.to_string_lossy())
            .user_config("sdk.overrides.aemu_internal", fake_qemu.to_string_lossy())
            .user_config("sdk.overrides.fvm", fake_fvm.to_string_lossy())
            .user_config("sdk.overrides.zbi", fake_zbi.to_string_lossy())
    }
    // Note that the caller MUST initialize the ffx_config environment before calling this function
    // since we override config values as part of the test. This looks like:
    //     let env = ffx_config::test_init()?;
    // The returned structure must remain in scope for the duration of the test to function
    // properly.
    async fn setup_files(
        guest: &mut GuestConfig,
        temp: &TempDir,
        disk_image_format: DiskImageFormat,
    ) -> Result<PathBuf> {
        let root = temp.path();

        let kernel_path = root.join("kernel");
        let zbi_path = root.join("zbi");
        let ovmf_path = root.join("OVMF_VARS.fd");
        let disk_image_path = disk_image_format.as_disk_image(root.join("disk"));

        let _ = fs::File::options()
            .write(true)
            .create(true)
            .open(&kernel_path)
            .map_err(|e| bug!("Cannot create test kernel file: {e}"))?;
        let _ = fs::File::options()
            .write(true)
            .create(true)
            .open(&zbi_path)
            .map_err(|e| bug!("cannot create test zbi file: {e}"))?;
        let _ = fs::File::options()
            .write(true)
            .create(true)
            .open(&ovmf_path)
            .map_err(|e| bug!("cannot create test ovmf_vars file: {e}"))?;
        let _ = fs::File::options()
            .write(true)
            .create(true)
            .open(&*disk_image_path)
            .map_err(|e| bug!("cannot create test disk image file: {e}"))?;

        guest.kernel_image = Some(kernel_path);
        guest.ramdisk = Some(Ramdisk { path: zbi_path, kind: RamdiskKind::Zbi });
        guest.ovmf_vars = ovmf_path;
        guest.disk_image = Some(disk_image_path);

        Ok(PathBuf::from(root))
    }

    fn write_to(path: &PathBuf, value: &str) -> Result<()> {
        eprintln!("Writing {} to {}", value, path.display());
        let mut file = File::options()
            .write(true)
            .open(path)
            .map_err(|e| bug!("cannot open existing file for write: {}: {e}", path.display()))?;
        File::write(&mut file, value.as_bytes())
            .map_err(|e| bug!("cannot write buffer to file: {}: {e}", path.display()))?;

        Ok(())
    }

    async fn test_staging_no_reuse_common(disk_image_format: DiskImageFormat) -> Result<()> {
        let temp = tempdir().map_err(|e| bug!("cannot get tempdir: {e}"))?;
        let root = temp.path();

        let mut builder = ffx_config::test_env();
        builder = make_fake_sdk(builder).await;
        builder = builder
            .user_config(config::EMU_INSTANCE_ROOT_DIR, root.display().to_string())
            .user_config("ssh.pub", json!([root.join("test_authorized_keys")]))
            .user_config("ssh.priv", json!([root.join("test_ed25519_key")]));

        let env = builder.build().expect("test env");

        let instance_name = "test-instance";
        let mut emu_config = EmulatorConfiguration::default();
        emu_config.device.storage = DataAmount { quantity: 32, units: DataUnits::Bytes };

        setup_files(&mut emu_config.guest, &temp, disk_image_format).await?;
        fs::create_dir_all(&root.join(instance_name)).expect("create test-instance dir");

        emu_config.guest.vbmeta_key_file = Some(root.join("atx_psk.pem"));
        emu_config.guest.vbmeta_key_metadata_file = Some(root.join("avb_atx_metadata.bin"));
        fs::write(emu_config.guest.vbmeta_key_file.as_ref().unwrap(), VBMETA_TEST_KEY)
            .map_err(|e| bug!("cannot write test key file: {e}"))?;
        fs::write(
            emu_config.guest.vbmeta_key_metadata_file.as_ref().unwrap(),
            VBMETA_TEST_KEY_METADATA,
        )
        .map_err(|e| bug!("cannot write test key metadata file: {e}"))?;

        write_to(&emu_config.guest.kernel_image.clone().expect("test kernel filename"), ORIGINAL)
            .map_err(|e| bug!("cannot write original value to kernel file: {e}"))?;
        write_to(emu_config.guest.disk_image.as_ref().unwrap(), ORIGINAL)
            .map_err(|e| bug!("cannot write original value to disk image file: {e}"))?;
        // Need to place a fake zbi file in the `test-instance` because fake_zbi does not actually
        // stage the file into this dir when it is called, but vbmeta signing requires the file
        // to exist.
        fs::write(&root.join(instance_name).join("zbi"), ORIGINAL)
            .map_err(|e| bug!("cannot write fake zbi file: {e}"))?;

        let updated = <TestEngine as QemuBasedEngine>::stage_image_files(
            &env.context,
            instance_name,
            &emu_config,
            false,
        )
        .await;

        // Staging of GPT images is only expected work when mkfs-msdosfs is guaranteed to be found.
        // When make-fuchsia-vol is guaranteed to create the required FAT EFI partition
        // successfully in out-of-tree configurations, the following block can be removed.
        if disk_image_format == DiskImageFormat::Gpt {
            if let EnvironmentKind::InTree { .. } = env.context.env_kind() {
                log::debug!("Test running in-tree, DiskImageFormat::Gpt is expected to work.");
            } else {
                log::debug!(
                    "Skipping test for DiskImageFormat::Gpt, mkfs-msdosfs is not guaranteed to be available"
                );
                return Ok(());
            }
        }

        assert!(updated.is_ok(), "expected OK got {:?}", updated.unwrap_err());

        let actual = updated.map_err(|e| bug!("cannot get updated guest config: {e}"))?;
        let expected = if disk_image_format == DiskImageFormat::Gpt {
            GuestConfig {
                disk_image: Some(
                    disk_image_format.as_disk_image(root.join(instance_name).join("gpt_disk.img")),
                ),
                is_gpt: true,
                ovmf_vars: root.join(instance_name).join("OVMF_VARS.fd"),
                vbmeta_key_file: Some(root.join("atx_psk.pem")),
                vbmeta_key_metadata_file: Some(root.join("avb_atx_metadata.bin")),
                ..Default::default()
            }
        } else {
            GuestConfig {
                kernel_image: Some(root.join(instance_name).join("kernel")),
                ramdisk: Some(Ramdisk {
                    path: root.join(instance_name).join("zbi"),
                    kind: RamdiskKind::Zbi,
                }),
                disk_image: Some(
                    disk_image_format.as_disk_image(root.join(instance_name).join("disk")),
                ),
                ovmf_vars: root.join("OVMF_VARS.fd"),
                vbmeta_key_file: Some(root.join("atx_psk.pem")),
                vbmeta_key_metadata_file: Some(root.join("avb_atx_metadata.bin")),
                ..Default::default()
            }
        };
        assert_eq!(actual, expected);

        // Test no reuse when old files exist. The original files should be overwritten.
        write_to(&emu_config.guest.kernel_image.clone().expect("kernel image name"), UPDATED)
            .map_err(|e| bug!("cannot write updated value to kernel file: {e}"))?;
        write_to(emu_config.guest.disk_image.as_ref().unwrap(), UPDATED)
            .map_err(|e| bug!("cannot write updated value to disk image file: {e}"))?;

        let updated = <TestEngine as QemuBasedEngine>::stage_image_files(
            &env.context,
            instance_name,
            &emu_config,
            false,
        )
        .await;

        assert!(updated.is_ok(), "expected OK got {:?}", updated.unwrap_err());

        let actual = updated.map_err(|e| bug!("cannot get updated guest config, reuse: {e}"))?;
        let expected = if disk_image_format == DiskImageFormat::Gpt {
            GuestConfig {
                disk_image: Some(
                    disk_image_format.as_disk_image(root.join(instance_name).join("gpt_disk.img")),
                ),
                is_gpt: true,
                ovmf_vars: root.join(instance_name).join("OVMF_VARS.fd"),
                vbmeta_key_file: Some(root.join("atx_psk.pem")),
                vbmeta_key_metadata_file: Some(root.join("avb_atx_metadata.bin")),
                ..Default::default()
            }
        } else {
            GuestConfig {
                kernel_image: Some(root.join(instance_name).join("kernel")),
                ramdisk: Some(Ramdisk {
                    path: root.join(instance_name).join("zbi"),
                    kind: RamdiskKind::Zbi,
                }),
                disk_image: Some(
                    disk_image_format.as_disk_image(root.join(instance_name).join("disk")),
                ),
                ovmf_vars: root.join("OVMF_VARS.fd"),
                vbmeta_key_file: Some(root.join("atx_psk.pem")),
                vbmeta_key_metadata_file: Some(root.join("avb_atx_metadata.bin")),
                ..Default::default()
            }
        };
        assert_eq!(actual, expected);

        // The following parts are not applicable for GPT images as those are constructed via
        // make-fuchsia-vol and do not start the emulator with a kernel argument.
        if disk_image_format != DiskImageFormat::Gpt {
            eprintln!(
                "Reading contents from {}",
                actual.kernel_image.clone().expect("kernel file path").display()
            );
            eprintln!("Reading contents from {}", actual.disk_image.as_ref().unwrap().display());
            let mut kernel = File::open(&actual.kernel_image.clone().expect("kernel file path"))
                .map_err(|e| bug!("cannot open overwritten kernel file for read: {e}"))?;
            let mut disk_image = File::open(&*actual.disk_image.unwrap())
                .map_err(|e| bug!("cannot open overwritten disk image file for read: {e}"))?;

            let mut kernel_contents = String::new();
            let mut fvm_contents = String::new();

            kernel
                .read_to_string(&mut kernel_contents)
                .map_err(|e| bug!("cannot read contents of reused kernel file: {e}"))?;
            disk_image
                .read_to_string(&mut fvm_contents)
                .map_err(|e| bug!("cannot read contents of reused disk image file: {e}"))?;

            assert_eq!(kernel_contents, UPDATED);

            // Fxfs will have ORIGINAL padded with nulls for be 32 bytes.
            //(set in emu_config at the top of this method).
            match disk_image_format {
                DiskImageFormat::Fvm | DiskImageFormat::Gpt => assert_eq!(fvm_contents, UPDATED),
                DiskImageFormat::Fxfs => assert_eq!(fvm_contents, UPDATED_PADDED),
            };
        }
        Ok(())
    }

    #[fuchsia::test]
    async fn test_staging_no_reuse_fvm() -> Result<()> {
        test_staging_no_reuse_common(DiskImageFormat::Fvm).await
    }

    #[fuchsia::test]
    async fn test_staging_no_reuse_fxfs() -> Result<()> {
        test_staging_no_reuse_common(DiskImageFormat::Fxfs).await
    }

    #[fuchsia::test]
    async fn test_staging_no_reuse_gpt() -> Result<()> {
        test_staging_no_reuse_common(DiskImageFormat::Gpt).await
    }

    async fn test_staging_with_reuse_common(disk_image_format: DiskImageFormat) -> Result<()> {
        let temp = tempdir().expect("cannot get tempdir");
        let root = temp.path();

        let mut builder = ffx_config::test_env();
        builder = make_fake_sdk(builder).await;
        builder = builder
            .user_config(config::EMU_INSTANCE_ROOT_DIR, root.display().to_string())
            .user_config("ssh.pub", json!([root.join("test_authorized_keys")]))
            .user_config("ssh.priv", json!([root.join("test_ed25519_key")]));

        let env = builder.build().expect("test env");

        let instance_name = "test-instance";
        let mut emu_config = EmulatorConfiguration::default();
        emu_config.device.storage = DataAmount { quantity: 32, units: DataUnits::Bytes };

        setup_files(&mut emu_config.guest, &temp, disk_image_format).await?;
        fs::create_dir_all(&root.join(instance_name)).expect("create test-instance dir");

        emu_config.guest.vbmeta_key_file = Some(root.join("atx_psk.pem"));
        emu_config.guest.vbmeta_key_metadata_file = Some(root.join("avb_atx_metadata.bin"));
        fs::write(emu_config.guest.vbmeta_key_file.as_ref().unwrap(), VBMETA_TEST_KEY)
            .map_err(|e| bug!("cannot write test key file: {e}"))?;
        fs::write(
            emu_config.guest.vbmeta_key_metadata_file.as_ref().unwrap(),
            VBMETA_TEST_KEY_METADATA,
        )
        .map_err(|e| bug!("cannot write test key metadata file: {e}"))?;

        write_to(&emu_config.guest.kernel_image.clone().expect("kernel file path"), ORIGINAL)
            .expect("cannot write original value to kernel file");
        write_to(emu_config.guest.disk_image.as_ref().unwrap(), ORIGINAL)
            .expect("cannot write original value to disk image file");
        // Need to place a fake zbi file in the `test-instance` because fake_zbi does not actually
        // stage the file into this dir when it is called, but vbmeta signing requires the file
        // to exist.
        fs::write(&root.join(instance_name).join("zbi"), ORIGINAL)
            .map_err(|e| bug!("cannot write fake zbi file: {e}"))?;

        let updated: Result<GuestConfig> = <TestEngine as QemuBasedEngine>::stage_image_files(
            &env.context,
            instance_name,
            &emu_config,
            true,
        )
        .await;

        // Staging of GPT images is only expected work when mkfs-msdosfs is guaranteed to be found.
        // When make-fuchsia-vol is guaranteed to create the required FAT EFI partition
        // successfully in out-of-tree configurations, the following block can be removed.
        if disk_image_format == DiskImageFormat::Gpt {
            if let EnvironmentKind::InTree { .. } = env.context.env_kind() {
                log::debug!("Test running in-tree, DiskImageFormat::Gpt is expected to work.");
            } else {
                log::debug!(
                    "Skipping test for DiskImageFormat::Gpt, mkfs-msdosfs is not guaranteed to be available"
                );
                return Ok(());
            }
        }

        assert!(updated.is_ok(), "expected OK got {:?}", updated.unwrap_err());

        let actual = updated.expect("cannot get updated guest config");
        let expected = if disk_image_format == DiskImageFormat::Gpt {
            GuestConfig {
                disk_image: Some(
                    disk_image_format.as_disk_image(root.join(instance_name).join("gpt_disk.img")),
                ),
                is_gpt: true,
                ovmf_vars: root.join(instance_name).join("OVMF_VARS.fd"),
                vbmeta_key_file: Some(root.join("atx_psk.pem")),
                vbmeta_key_metadata_file: Some(root.join("avb_atx_metadata.bin")),
                ..Default::default()
            }
        } else {
            GuestConfig {
                kernel_image: Some(root.join(instance_name).join("kernel")),
                ramdisk: Some(Ramdisk {
                    path: root.join(instance_name).join("zbi"),
                    kind: RamdiskKind::Zbi,
                }),
                disk_image: Some(
                    disk_image_format.as_disk_image(root.join(instance_name).join("disk")),
                ),
                ovmf_vars: root.join("OVMF_VARS.fd"),
                vbmeta_key_file: Some(root.join("atx_psk.pem")),
                vbmeta_key_metadata_file: Some(root.join("avb_atx_metadata.bin")),
                ..Default::default()
            }
        };
        assert_eq!(actual, expected);

        // Test reuse. Note that the ZBI file isn't actually copied in the test, since we replace
        // the ZBI tool with an "echo" command.
        write_to(&emu_config.guest.kernel_image.clone().expect("kernel file path"), UPDATED)
            .expect("cannot write updated value to kernel file");
        write_to(emu_config.guest.disk_image.as_ref().unwrap(), UPDATED)
            .expect("cannot write updated value to disk image file");

        let updated = <TestEngine as QemuBasedEngine>::stage_image_files(
            &env.context,
            instance_name,
            &emu_config,
            true,
        )
        .await;

        assert!(updated.is_ok(), "expected OK got {:?}", updated.unwrap_err());

        let actual = updated.expect("cannot get updated guest config, reuse");
        let expected = if disk_image_format == DiskImageFormat::Gpt {
            GuestConfig {
                disk_image: Some(
                    disk_image_format.as_disk_image(root.join(instance_name).join("gpt_disk.img")),
                ),
                is_gpt: true,
                ovmf_vars: root.join(instance_name).join("OVMF_VARS.fd"),
                vbmeta_key_file: Some(root.join("atx_psk.pem")),
                vbmeta_key_metadata_file: Some(root.join("avb_atx_metadata.bin")),
                ..Default::default()
            }
        } else {
            GuestConfig {
                kernel_image: Some(root.join(instance_name).join("kernel")),
                ramdisk: Some(Ramdisk {
                    path: root.join(instance_name).join("zbi"),
                    kind: RamdiskKind::Zbi,
                }),
                disk_image: Some(
                    disk_image_format.as_disk_image(root.join(instance_name).join("disk")),
                ),
                ovmf_vars: root.join("OVMF_VARS.fd"),
                vbmeta_key_file: Some(root.join("atx_psk.pem")),
                vbmeta_key_metadata_file: Some(root.join("avb_atx_metadata.bin")),
                ..Default::default()
            }
        };
        assert_eq!(actual, expected);

        // The following parts are not applicable for GPT images as those are constructed via
        // make-fuchsia-vol and do not start the emulator with a kernel argument.
        if disk_image_format != DiskImageFormat::Gpt {
            eprintln!(
                "Reading contents from {}",
                actual.kernel_image.clone().expect("kernel file path").display()
            );
            eprintln!(
                "Reading contents from {}",
                actual.disk_image.clone().expect("disk_image file path").display()
            );
            let mut kernel = File::open(&actual.kernel_image.expect("kernel file path"))
                .expect("cannot open reused kernel file for read");
            let mut fvm = File::open(&*actual.disk_image.unwrap())
                .expect("cannot open reused fvm file for read");

            let mut kernel_contents = String::new();
            let mut fvm_contents = String::new();

            kernel
                .read_to_string(&mut kernel_contents)
                .expect("cannot read contents of reused kernel file");
            fvm.read_to_string(&mut fvm_contents).expect("cannot read contents of reused fvm file");

            assert_eq!(kernel_contents, ORIGINAL);

            // Fxfs will have ORIGINAL padded with nulls for be 32 bytes.
            //(set in emu_config at the top of this method).
            match disk_image_format {
                DiskImageFormat::Fvm | DiskImageFormat::Gpt => assert_eq!(fvm_contents, ORIGINAL),
                DiskImageFormat::Fxfs => assert_eq!(fvm_contents, ORIGINAL_PADDED),
            };
        }

        Ok(())
    }

    #[fuchsia::test]
    async fn test_staging_with_reuse_fvm() -> Result<()> {
        test_staging_with_reuse_common(DiskImageFormat::Fvm).await
    }

    #[fuchsia::test]
    async fn test_staging_with_reuse_fxfs() -> Result<()> {
        test_staging_with_reuse_common(DiskImageFormat::Fxfs).await
    }

    #[fuchsia::test]
    async fn test_staging_with_reuse_gpt() -> Result<()> {
        test_staging_with_reuse_common(DiskImageFormat::Gpt).await
    }

    // There's no equivalent test for FVM for now -- extending FVM images is more complex and
    // depends on an external binary, making testing challenging.
    #[fuchsia::test]
    async fn test_staging_resize_fxfs() -> Result<()> {
        let builder = ffx_config::test_env();
        let builder = make_fake_sdk(builder).await;
        let temp = tempdir().expect("cannot get tempdir");
        let root = temp.path();
        let env = builder
            .user_config(ffx_config::keys::EMU_INSTANCE_ROOT_DIR, root.to_string_lossy())
            .user_config("ssh.pub", json!([root.join("test_authorized_keys")]))
            .user_config("ssh.priv", json!([root.join("test_ed25519_key")]))
            .build()
            .unwrap();
        let instance_name = "test-instance";
        let mut emu_config = EmulatorConfiguration::default();
        let root = setup_files(&mut emu_config.guest, &temp, DiskImageFormat::Fxfs).await?;

        const EXPECTED_DATA: &[u8] = b"hello, world";

        std::fs::write(
            &emu_config.guest.kernel_image.as_ref().expect("kernel file path"),
            "whatever",
        )
        .expect("writing kernel image");
        std::fs::write(emu_config.guest.disk_image.as_ref().unwrap(), EXPECTED_DATA)
            .expect("writing guest image");
        // Make the input file read-only to ensure that the staged version is RW.
        let mut perms = std::fs::metadata(&emu_config.guest.disk_image.as_ref().unwrap())
            .expect("get permissions")
            .permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&emu_config.guest.disk_image.as_ref().unwrap(), perms)
            .expect("set permissions");

        emu_config.device.storage = DataAmount { units: DataUnits::Kilobytes, quantity: 4 };

        let config = <TestEngine as QemuBasedEngine>::stage_image_files(
            &env.context,
            instance_name,
            &emu_config,
            false,
        )
        .await
        .expect("Failed to get guest config");

        let expected = GuestConfig {
            kernel_image: Some(root.join(instance_name).join("kernel")),
            ramdisk: Some(Ramdisk {
                path: root.join(instance_name).join("zbi"),
                kind: RamdiskKind::Zbi,
            }),
            disk_image: Some(DiskImage::Fxfs(root.join(instance_name).join("disk"))),
            ovmf_vars: root.join("OVMF_VARS.fd"),
            ..Default::default()
        };
        assert_eq!(config, expected);

        let mut disk_image = File::open(&*config.disk_image.unwrap()).expect("disk image");
        let mut disk_contents = vec![];
        disk_image
            .read_to_end(&mut disk_contents)
            .expect("cannot read contents of reused disk image file");

        assert_eq!(disk_contents.len(), 4096);
        assert_eq!(&disk_contents[..EXPECTED_DATA.len()], EXPECTED_DATA);
        assert_eq!(&disk_contents[EXPECTED_DATA.len()..], &[0u8; 4096 - EXPECTED_DATA.len()]);
        assert!(!disk_image.metadata().expect("get metadata").permissions().readonly());

        Ok(())
    }

    /// Stages a guest with the given disk image format and with snapshots enabled, and checks
    /// that the disk image is converted to qcow2. Then stages it again, to check whether the
    /// converted image is reused or replaced, and finally without snapshots, to check that the
    /// converted image is removed.
    async fn test_staging_snapshots_enabled_common(
        disk_image_format: DiskImageFormat,
    ) -> Result<()> {
        let temp = tempdir().expect("cannot get tempdir");
        let root = temp.path();
        let builder = ffx_config::test_env();
        let builder = make_fake_sdk(builder).await;
        let env = builder
            .user_config(config::EMU_INSTANCE_ROOT_DIR, root.display().to_string())
            .user_config("ssh.pub", json!([root.join("test_authorized_keys")]))
            .user_config("ssh.priv", json!([root.join("test_ed25519_key")]))
            .build()
            .expect("test env");

        let instance_name = "test-instance";
        let mut emu_config = EmulatorConfiguration::default();
        emu_config.runtime.snapshots_enabled = true;
        emu_config.device.storage = DataAmount { quantity: 32, units: DataUnits::Bytes };
        setup_files(&mut emu_config.guest, &temp, disk_image_format).await?;
        write_to(emu_config.guest.disk_image.as_ref().unwrap(), ORIGINAL)?;

        let raw_path = root.join(instance_name).join("disk");
        let qcow2_path = root.join(instance_name).join("disk.qcow2");
        // The fake qemu-img tool writes the arguments it was called with to the converted image.
        let conversion =
            format!("convert -f raw -O qcow2 {} {}\n", raw_path.display(), qcow2_path.display());

        let staged = <TestEngine as QemuBasedEngine>::stage_image_files(
            &env.context,
            instance_name,
            &emu_config,
            false,
        )
        .await?;

        // The emulator is given the converted image, and the raw image it was made from is gone.
        assert_eq!(staged.disk_image, Some(disk_image_format.as_disk_image(&qcow2_path)));
        assert_eq!(fs::read_to_string(&qcow2_path).expect("read converted image"), conversion);
        assert!(!raw_path.exists(), "the raw disk image should be removed after conversion");

        // When the instance is reused the converted image, which holds the snapshots, is kept.
        const SNAPSHOTS: &str = "this image now contains snapshots";
        fs::write(&qcow2_path, SNAPSHOTS).expect("write converted image");
        let staged = <TestEngine as QemuBasedEngine>::stage_image_files(
            &env.context,
            instance_name,
            &emu_config,
            true,
        )
        .await?;
        assert_eq!(staged.disk_image, Some(disk_image_format.as_disk_image(&qcow2_path)));
        assert_eq!(fs::read_to_string(&qcow2_path).expect("read converted image"), SNAPSHOTS);
        assert!(!raw_path.exists());

        // When the instance is not reused, the disk image is staged and converted again.
        let staged = <TestEngine as QemuBasedEngine>::stage_image_files(
            &env.context,
            instance_name,
            &emu_config,
            false,
        )
        .await?;
        assert_eq!(staged.disk_image, Some(disk_image_format.as_disk_image(&qcow2_path)));
        assert_eq!(fs::read_to_string(&qcow2_path).expect("read converted image"), conversion);
        assert!(!raw_path.exists());

        // When the instance is staged again without snapshots, the emulator runs from a raw image
        // again, and the converted image, which nothing uses any more, is removed.
        emu_config.runtime.snapshots_enabled = false;
        let staged = <TestEngine as QemuBasedEngine>::stage_image_files(
            &env.context,
            instance_name,
            &emu_config,
            false,
        )
        .await?;
        assert_eq!(staged.disk_image, Some(disk_image_format.as_disk_image(&raw_path)));
        assert!(raw_path.exists());
        assert!(!qcow2_path.exists(), "the unused qcow2 disk image should be removed");

        Ok(())
    }

    #[fuchsia::test]
    async fn test_staging_snapshots_enabled_fvm() -> Result<()> {
        test_staging_snapshots_enabled_common(DiskImageFormat::Fvm).await
    }

    #[fuchsia::test]
    async fn test_staging_snapshots_enabled_fxfs() -> Result<()> {
        test_staging_snapshots_enabled_common(DiskImageFormat::Fxfs).await
    }

    #[fuchsia::test]
    async fn test_staging_snapshots_enabled_gpt() -> Result<()> {
        let temp = tempdir().expect("cannot get tempdir");
        let root = temp.path();
        let builder = ffx_config::test_env();
        let builder = make_fake_sdk(builder).await;
        let env = builder
            .user_config(config::EMU_INSTANCE_ROOT_DIR, root.display().to_string())
            .user_config("ssh.pub", json!([root.join("test_authorized_keys")]))
            .user_config("ssh.priv", json!([root.join("test_ed25519_key")]))
            .build()
            .expect("test env");

        let instance_name = "test-instance";
        let mut emu_config = EmulatorConfiguration::default();
        emu_config.runtime.snapshots_enabled = true;
        emu_config.device.storage = DataAmount { quantity: 32, units: DataUnits::Bytes };
        setup_files(&mut emu_config.guest, &temp, DiskImageFormat::Gpt).await?;

        // GPT disk images need a vbmeta for the ZBI, which is signed with these keys. The fake
        // zbi tool does not stage the ZBI, so place a file for it to be generated from.
        emu_config.guest.vbmeta_key_file = Some(root.join("atx_psk.pem"));
        emu_config.guest.vbmeta_key_metadata_file = Some(root.join("avb_atx_metadata.bin"));
        fs::write(emu_config.guest.vbmeta_key_file.as_ref().unwrap(), VBMETA_TEST_KEY)
            .expect("write test key file");
        fs::write(
            emu_config.guest.vbmeta_key_metadata_file.as_ref().unwrap(),
            VBMETA_TEST_KEY_METADATA,
        )
        .expect("write test key metadata file");
        fs::create_dir_all(&root.join(instance_name)).expect("create test-instance dir");
        fs::write(&root.join(instance_name).join("zbi"), ORIGINAL).expect("write fake zbi file");

        // Snapshots are not supported for GPT disk images. The engine reports this when it is
        // configured, but staging must not silently carry on with a raw image either.
        let err = <TestEngine as QemuBasedEngine>::stage_image_files(
            &env.context,
            instance_name,
            &emu_config,
            false,
        )
        .await
        .expect_err("staging a GPT image with snapshots enabled");
        assert!(err.to_string().contains("Snapshots are not supported"), "unexpected error: {err}");

        Ok(())
    }

    #[fuchsia::test]
    async fn test_staging_snapshots_enabled_without_qemu_img() -> Result<()> {
        let temp = tempdir().expect("cannot get tempdir");
        let root = temp.path();
        let builder = ffx_config::test_env();
        let mut builder = make_fake_sdk(builder).await;
        // Remove the tool which the fake SDK provides next to the QEMU binary.
        fs::remove_file(builder.isolate_root().join(QEMU_IMG_TOOL)).expect("remove fake qemu-img");
        let env = builder
            .user_config(config::EMU_INSTANCE_ROOT_DIR, root.display().to_string())
            .user_config("ssh.pub", json!([root.join("test_authorized_keys")]))
            .user_config("ssh.priv", json!([root.join("test_ed25519_key")]))
            .build()
            .expect("test env");

        let mut emu_config = EmulatorConfiguration::default();
        emu_config.runtime.snapshots_enabled = true;
        emu_config.device.storage = DataAmount { quantity: 32, units: DataUnits::Bytes };
        setup_files(&mut emu_config.guest, &temp, DiskImageFormat::Fxfs).await?;

        let err = <TestEngine as QemuBasedEngine>::stage_image_files(
            &env.context,
            "test-instance",
            &emu_config,
            false,
        )
        .await
        .expect_err("staging without the qemu-img tool");
        assert!(
            err.to_string().contains("Cannot find the qemu-img tool"),
            "unexpected error: {err}"
        );

        Ok(())
    }

    #[test]
    fn test_qcow2_image_path() {
        assert_eq!(
            qcow2_image_path(Path::new("/instances/fuchsia-emulator/fxfs.sparse.blk")),
            PathBuf::from("/instances/fuchsia-emulator/fxfs.sparse.blk.qcow2")
        );
        assert_eq!(qcow2_image_path(Path::new("fvm.blk")), PathBuf::from("fvm.blk.qcow2"));
    }

    #[fuchsia::test]
    async fn test_embed_boot_data() -> Result<()> {
        let temp = tempdir().expect("cannot get tempdir");
        let root = temp.path();
        let builder = ffx_config::test_env();
        let builder = make_fake_sdk(builder).await;
        let env = builder
            .user_config("ssh.pub", json!([root.join("test_authorized_keys")]))
            .user_config("ssh.priv", json!([root.join("test_ed25519_key")]))
            .build()
            .unwrap();
        let mut emu_config = EmulatorConfiguration::default();

        let root_dir = setup_files(&mut emu_config.guest, &temp, DiskImageFormat::Fvm).await?;

        let Some(Ramdisk { path: src, kind: RamdiskKind::Zbi }) = emu_config.guest.ramdisk else {
            panic!("No ZBI path");
        };
        let dest = root_dir.join("dest.zbi");

        ffx_uefi_disk::embed_boot_data(&env.context, &src, &dest, None, None)?;

        Ok(())
    }

    #[fuchsia::test]
    async fn test_embed_boot_data_with_kernel_cmdline() -> Result<()> {
        let temp = tempdir().expect("cannot get tempdir");
        let root = temp.path();
        let builder = ffx_config::test_env();
        let builder = make_fake_sdk(builder).await;
        let env = builder
            .user_config("ssh.pub", json!([root.join("test_authorized_keys")]))
            .user_config("ssh.priv", json!([root.join("test_ed25519_key")]))
            .build()
            .unwrap();
        let mut emu_config = EmulatorConfiguration::default();

        let root_dir = setup_files(&mut emu_config.guest, &temp, DiskImageFormat::Fvm).await?;

        let Some(Ramdisk { path: src, kind: RamdiskKind::Zbi }) = emu_config.guest.ramdisk else {
            panic!("No ZBI path");
        };
        let dest = root_dir.join("dest.zbi");

        ffx_uefi_disk::embed_boot_data(&env.context, &src, &dest, Some("kernel.boot=yes"), None)?;

        Ok(())
    }

    #[fuchsia::test]
    async fn test_embed_boot_data_with_serial_number() -> Result<()> {
        let temp = tempdir().expect("cannot get tempdir");
        let root = temp.path();
        let builder = ffx_config::test_env();
        let builder = make_fake_sdk(builder).await;
        let env = builder
            .user_config("ssh.pub", json!([root.join("test_authorized_keys")]))
            .user_config("ssh.priv", json!([root.join("test_ed25519_key")]))
            .build()
            .unwrap();
        let mut emu_config = EmulatorConfiguration::default();

        let root_dir = setup_files(&mut emu_config.guest, &temp, DiskImageFormat::Fvm).await?;

        let Some(Ramdisk { path: src, kind: RamdiskKind::Zbi }) = emu_config.guest.ramdisk else {
            panic!("No ZBI path");
        };
        let dest = root_dir.join("dest.zbi");

        ffx_uefi_disk::embed_boot_data(&env.context, &src, &dest, None, Some("TESTSERIAL"))?;

        Ok(())
    }

    #[fuchsia::test]
    async fn test_generate_vbmeta() -> Result<()> {
        let builder = ffx_config::test_env();
        let builder = make_fake_sdk(builder).await;
        let _env = builder.build().unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        let mut emu_config = EmulatorConfiguration::default();

        let root = setup_files(&mut emu_config.guest, &temp, DiskImageFormat::Fvm).await?;

        let tempdir = temp.path();
        let key_path = tempdir.join("atx_psk.pem");
        let metadata_path = tempdir.join("avb_atx_metadata.bin");
        std::fs::write(&key_path, VBMETA_TEST_KEY).expect("write test key file");
        std::fs::write(&metadata_path, VBMETA_TEST_KEY_METADATA).expect("write test key metadata");

        let Some(Ramdisk { path: zbi, kind: RamdiskKind::Zbi }) = emu_config.guest.ramdisk else {
            panic!("No ZBI path");
        };
        let dest = root.join("dest.vbmeta");

        ffx_uefi_disk::generate_vbmeta(&key_path, &metadata_path, &zbi, &dest)?;
        assert!(dest.exists());

        Ok(())
    }

    #[fuchsia::test]
    async fn test_authorized_keys_to_boot_loader_file() -> Result<()> {
        let builder = ffx_config::test_env();
        let builder = make_fake_sdk(builder).await;
        let _env = builder.build().unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        let mut emu_config = EmulatorConfiguration::default();

        let _root = setup_files(&mut emu_config.guest, &temp, DiskImageFormat::Fvm).await?;

        let tempdir = temp.path();
        let testkey = "some test key";

        let keyfile = tempdir.join("key");
        fs::write(&keyfile, testkey).expect("write test key file");
        let bootloaderfile = tempdir.join("btfl");

        ffx_uefi_disk::authorized_keys_to_boot_loader_file(&keyfile, &bootloaderfile)?;

        let v = fs::read(&bootloaderfile).unwrap();
        assert_eq!(
            v,
            vec![
                // 19 + "ssh.authorized_keys" + "some test key"
                19, 115, 115, 104, 46, 97, 117, 116, 104, 111, 114, 105, 122, 101, 100, 95, 107,
                101, 121, 115, 115, 111, 109, 101, 32, 116, 101, 115, 116, 32, 107, 101, 121
            ]
        );

        Ok(())
    }

    #[fuchsia::test]
    fn test_validate_net() -> Result<()> {
        // User mode doesn't have specific requirements, so it should return OK.
        let engine = TestEngine::default();
        let mut emu_config = EmulatorConfiguration::default();
        emu_config.host.networking = NetworkingMode::User;
        let result = engine.validate_network_flags(&emu_config);
        assert!(result.is_ok(), "{:?}", result.unwrap_err());

        // No networking returns an error if no console is selected.
        emu_config.host.networking = NetworkingMode::None;
        emu_config.runtime.console = ConsoleType::None;
        let result = engine.validate_network_flags(&emu_config);
        assert!(result.is_err());

        emu_config.runtime.console = ConsoleType::Console;
        let result = engine.validate_network_flags(&emu_config);
        assert!(result.is_ok(), "{:?}", result.unwrap_err());

        // Tap mode errors if host is Linux and there's no interface, but we can't mock the
        // interface, so we can't test this case yet.
        emu_config.host.networking = NetworkingMode::Tap;

        // Validation runs after configuration is merged with values from PBMs and runtime, so Auto
        // values should already be resolved. If not, that's a failure.
        emu_config.host.networking = NetworkingMode::Auto;
        let result = engine.validate_network_flags(&emu_config);
        assert!(result.is_err());

        Ok(())
    }

    #[fuchsia::test]
    async fn test_wait_for_exit() {
        // A process which exits is waited for, and is gone afterwards.
        let child = Command::new("sleep").arg("0.2").spawn().expect("spawn sleep");
        let start = Instant::now();
        assert!(wait_for_exit(child.id(), Duration::from_secs(30)).await);
        assert!(start.elapsed() >= Duration::from_millis(200));
        assert!(!process::is_running(child.id()));

        // A process which keeps running is given up on when the timeout runs out.
        let mut child = Command::new("sleep").arg("60").spawn().expect("spawn sleep");
        let start = Instant::now();
        assert!(!wait_for_exit(child.id(), Duration::from_millis(300)).await);
        assert!(start.elapsed() >= Duration::from_millis(300));
        assert!(process::is_running(child.id()));
        child.kill().expect("kill sleep");
        child.wait().expect("wait for sleep");
    }

    #[fuchsia::test]
    async fn test_stop_emulator_waits_for_exit() {
        let child = Command::new("sleep").arg("60").spawn().expect("spawn sleep");
        let mut engine = TestEngine { pid: child.id(), state: EngineState::Running };
        assert!(engine.is_running().await);

        engine.stop_emulator().await.expect("stop");

        // The process has exited, rather than only having been signalled, when stop returns.
        assert!(!process::is_running(child.id()));
        assert!(!engine.is_running().await);
        assert_eq!(engine.get_engine_state(), EngineState::Staged);
    }

    #[fuchsia::test]
    async fn test_stop_emulator_not_running() {
        let mut engine = TestEngine { pid: 0, state: EngineState::Running };
        engine.stop_emulator().await.expect("stop");
        assert_eq!(engine.get_engine_state(), EngineState::Staged);
    }

    #[derive(Deserialize, Debug)]
    struct Arguments {
        #[serde(alias = "command-line")]
        pub command_line: String,
    }
    #[derive(Deserialize, Debug)]
    struct QMPCommand {
        pub execute: String,
        pub arguments: Option<Arguments>,
    }

    #[fuchsia::test]
    async fn test_read_port_mappings() -> Result<()> {
        let env = ffx_config::test_env().build().unwrap();
        let temp = tempdir().expect("cannot get tempdir");
        let mut data: EmulatorInstanceData =
            EmulatorInstanceData::new_with_state("test-instance", EngineState::New);
        let root = setup_files(
            &mut data.get_emulator_configuration_mut().guest,
            &temp,
            DiskImageFormat::Fvm,
        )
        .await?;
        let emu_instances = EmulatorInstances::new(root.clone());

        data.set_instance_directory(&root.join("test-instance").to_string_lossy());
        fs::create_dir_all(&data.get_emulator_configuration().runtime.instance_directory)
            .expect("creating instance dir");

        data.get_emulator_configuration_mut()
            .host
            .port_map
            .insert("ssh".into(), PortMapping { guest: 22, host: None });
        data.get_emulator_configuration_mut()
            .host
            .port_map
            .insert("http".into(), PortMapping { guest: 80, host: None });
        data.get_emulator_configuration_mut()
            .host
            .port_map
            .insert("premapped".into(), PortMapping { guest: 11, host: Some(1111) });

        // use the current pid for the emulator instance

        let mut engine = crate::FemuEngine::new(&env.context, data, emu_instances);
        engine.set_pid(std::process::id());
        engine.set_engine_state(EngineState::Running);

        // Change the working directory to handle long path names to the socket while opening it,
        // then change back.
        let cwd = env::current_dir().expect("getting current dir");
        // Set up a socket that behaves like QMP
        env::set_current_dir(engine.emu_config().runtime.instance_directory.clone())
            .expect("setting current dir");
        let listener = UnixListener::bind(MACHINE_CONSOLE).expect("binding machine console");
        env::set_current_dir(&cwd).expect("setting current dir");

        // Helper function for this test to be the QEMU side of the QMP socket.
        fn do_qmp(mut stream: UnixStream) -> Result<()> {
            let mut request_iter =
                Deserializer::from_reader(stream.try_clone().map_err(|e| bug!("{e}"))?)
                    .into_iter::<Value>();

            // Responses to the `info usernet` request. The last response should end the interaction
            // because if fulfills all the port mappings which are being looked for.
            let responses = vec![
                json!({}),
                json!({"return" :
                 "VLAN -1 (net0):\r\nProtocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ\r\n"
                }),
                json!({"return": r#"VLAN -1 (net0):
                Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ
                TCP[HOST_FORWARD]  24               * 36167       10.0.2.15    22     0     0
                UDP[236 sec]       49               * 33338         0.0.0.0 33337     0     0
                "#}),
                json!({"return": r#"VLAN -1 (net0):
                Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ
                TCP[ESTABLISHED]   45       127.0.0.1 36167       10.0.2.15    22     0     0
                TCP[HOST_FORWARD]  25               * 36975       10.0.2.15    80     0     0
                TCP[HOST_FORWARD]  24               * 36167       10.0.2.15    22     0     0
                UDP[236 sec]       49               * 33338         0.0.0.0 33337     0     0
                "#}),
            ];

            let mut index = 0;
            loop {
                match request_iter.next() {
                    Some(Ok(data)) => {
                        if let Ok(cmd) = serde_json::from_value::<QMPCommand>(data.clone()) {
                            match cmd.execute.as_str() {
                                "human-monitor-command" => {
                                    if let Some(arguments) = cmd.arguments {
                                        assert_eq!(arguments.command_line, "info usernet");
                                    }
                                    stream
                                        .write_fmt(format_args!(
                                            "{}\n",
                                            responses[index].to_string()
                                        ))
                                        .map_err(|e| bug!("Error writing {e}"))?;
                                    index += 1;
                                }
                                "qmp_capabilities" => {
                                    stream.write_all(
                                        json!(
                                        {
                                        "QMP": {
                                            "version": {
                                                "qemu": {
                                                    "micro": 0,
                                                    "minor": 12,
                                                    "major": 2
                                                },
                                                "package": "(gradle_1.3.0-beta4-78860-g2764d93fd1)"
                                                },
                                                "capabilities": []
                                                }
                                            }
                                        )
                                        .to_string()
                                        .as_bytes(),
                                    ).map_err(|e| bug!("Error writing {e}"))?;
                                }
                                _ => return_bug!("unknown request is here! {cmd:?}"),
                            }
                        } else {
                            return_bug!("Unknown message {data:?}");
                        }
                    }
                    Some(Err(e)) => return_bug!("Error reading QMP request: {e:?}"),
                    None => (),
                }
            }
        }

        // Set up a side thread that will accept an incoming connection and then exit.
        let _listener_thread = std::thread::spawn(move || -> Result<()> {
            // accept connections and process them, spawning a new thread for each one
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        /* connection succeeded */
                        std::thread::spawn(|| match do_qmp(stream) {
                            Ok(_) => (),
                            Err(e) => panic!("do_qmp failed: {e:?}"),
                        });
                    }
                    Err(err) => {
                        /* connection failed */
                        return_bug!("Error connecting incoming: {err:?}");
                    }
                }
            }
            Ok(())
        });

        <crate::FemuEngine as QemuBasedEngine>::read_port_mappings(&mut engine).await?;

        for (name, mapping) in &engine.emu_config().host.port_map {
            match name.as_str() {
                "http" => assert_eq!(
                    mapping.host,
                    Some(36975),
                    "mismatch for {:?}",
                    &engine.emu_config().host.port_map
                ),
                "ssh" => assert_eq!(
                    mapping.host,
                    Some(36167),
                    "mismatch for {:?}",
                    &engine.emu_config().host.port_map
                ),
                "premapped" => assert_eq!(
                    mapping.host,
                    Some(1111),
                    "mismatch for {:?}",
                    &engine.emu_config().host.port_map
                ),
                _ => return_bug!("Unexpected port mapping: {name} {mapping:?}"),
            };
        }

        Ok(())
    }

    #[test]
    fn test_parse_return_string() -> Result<()> {
        let normal_expected = r#"VLAN -1 (net0):\r
          Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ\r
          TCP[HOST_FORWARD]  81               * 43265       10.0.2.15  2345     0     0\r
          TCP[HOST_FORWARD]  80               * 38989       10.0.2.15  5353     0     0\r
          TCP[HOST_FORWARD]  79               * 43751       10.0.2.15    22     0     0\r"#;
        let condensed_expected = r#"VLAN -1 (net0):
          Protocol[State] FD Source Address Port  Dest. Address Port RecvQ SendQ
          TCP[HOST_FORWARD] 81    * 43265  10.0.2.15    2345 0 0
          TCP[HOST_FORWARD] 80 * 38989       10.0.2.15  5353     0     0\r
          TCP[HOST_FORWARD] 79   * 43751 10.0.2.15 22     0     0"#;
        let broken_expected = r#"VLAN -1 (net0):\r
          Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ\r
          TCP[HOST_FORWARD]  81               * 43265       10.0.2.15  2345     0     0\r
          TCP[HOST_FORWARD]  80               \r"#;
        let missing_fd_expected = r#"VLAN -1 (net0):\r
          Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ\r
          TCP[HOST_FORWARD]  81               * 43265       10.0.2.15  2345     0     0\r
          TCP[CLOSED]                         * 38989       10.0.2.15  5353     0     0\r
          TCP[SYN_SYNC]      80               * 43751       10.0.2.15    22     0     0\r"#;
        let established_expected = r#"VLAN -1 (net0):\r
          Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ\r
          TCP[ESTABLISHED]  81               * 42265       10.0.2.15  2345     0     0\r
          TCP[HOST_FORWARD]  83               * 43265       10.0.2.15  2345     0     0\r
          TCP[HOST_FORWARD]  80               * 38989       10.0.2.15  5353     0     0\r
          TCP[HOST_FORWARD]  79               * 43751       10.0.2.15    22     0     0\r"#;

        let testdata: Vec<(&str, Result<Vec<PortPair>>)> = vec![
            ("", Ok(vec![])),
            (
                "VLAN -1 (net0):\r\n  Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ\r\n",
                Ok(vec![]),
            ),
            (
                normal_expected,
                Ok(vec![
                    PortPair { guest: 2345, host: 43265 },
                    PortPair { guest: 5353, host: 38989 },
                    PortPair { guest: 22, host: 43751 },
                ]),
            ),
            (
                condensed_expected,
                Ok(vec![
                    PortPair { guest: 2345, host: 43265 },
                    PortPair { guest: 5353, host: 38989 },
                    PortPair { guest: 22, host: 43751 },
                ]),
            ),
            (broken_expected, Ok(vec![PortPair { guest: 2345, host: 43265 }])),
            (missing_fd_expected, Ok(vec![PortPair { guest: 2345, host: 43265 }])),
            (
                established_expected,
                Ok(vec![
                    PortPair { guest: 2345, host: 43265 },
                    PortPair { guest: 5353, host: 38989 },
                    PortPair { guest: 22, host: 43751 },
                ]),
            ),
        ];

        for (input, result) in testdata {
            let actual = <TestEngine as QemuBasedEngine>::parse_return_string(input);
            match actual {
                Ok(port_list) => assert_eq!(port_list, result.ok().unwrap()),
                Err(e) => assert_eq!(e.to_string(), result.err().unwrap().to_string()),
            };
        }
        Ok(())

        // TCP with other state.
    }

    #[fuchsia::test]
    fn test_make_absolute_path_for_absolute_path() {
        const ABSOLUTE_PATH: &str = "/tmp/test.png";
        assert!(!cfg!(windows));
        let path = Path::new(ABSOLUTE_PATH);
        assert_eq!(make_absolute_path(path).unwrap(), PathBuf::from(ABSOLUTE_PATH));
    }

    #[fuchsia::test]
    fn test_make_absolute_path_for_relative_path() {
        const RELATIVE_PATH: &str = "test.png";
        let relative_path = Path::new(RELATIVE_PATH);
        let expected_absolute_path = std::env::current_dir().unwrap().join(relative_path);
        assert_eq!(make_absolute_path(relative_path).unwrap(), expected_absolute_path);
    }

    #[fuchsia::test]
    #[should_panic(expected = "Engine must be running")]
    fn test_check_screenshot_preconditions_not_running() {
        const ABSOLUTE_PATH: &str = "/tmp/test.png";
        let mut config = EmulatorConfiguration::default();
        config.device.screen.width = 100;
        config.device.screen.height = 100;
        let data = EmulatorInstanceData::new(config, EngineType::Qemu, EngineState::Staged);
        let env = ffx_config::test_init().unwrap();
        let engine =
            crate::QemuEngine::new(&env.context, data, EmulatorInstances::new(PathBuf::new()));
        check_screenshot_preconditions(&engine, Path::new(ABSOLUTE_PATH));
    }

    #[fuchsia::test]
    fn test_check_screenshot_preconditions_headless() {
        const ABSOLUTE_PATH: &str = "/tmp/test.png";
        let mut config = EmulatorConfiguration::default();
        config.runtime.headless = true;
        config.device.screen.width = 100;
        config.device.screen.height = 100;
        let mut data = EmulatorInstanceData::new(config, EngineType::Qemu, EngineState::Running);
        // Set the emulator PID to a fake ID so that `get_engine_state()` returns Running.
        let fake_emulator_pid = std::process::id();
        data.set_pid(fake_emulator_pid);
        let env = ffx_config::test_init().unwrap();
        let engine =
            crate::QemuEngine::new(&env.context, data, EmulatorInstances::new(PathBuf::new()));
        check_screenshot_preconditions(&engine, Path::new(ABSOLUTE_PATH));
    }

    #[fuchsia::test]
    #[should_panic(expected = "The emulator virtual display has an invalid resolution")]
    fn test_check_screenshot_preconditions_invalid_resolution() {
        const NON_EXISTENT_PATH: &str = "/tmp/nonexistent.png";
        let mut config = EmulatorConfiguration::default();
        config.device.screen.width = 0;
        config.device.screen.height = 0;
        let mut data = EmulatorInstanceData::new(config, EngineType::Qemu, EngineState::Running);
        // Set the emulator PID to a fake ID so that `get_engine_state()` returns Running.
        let fake_emulator_pid = std::process::id();
        data.set_pid(fake_emulator_pid);
        let env = ffx_config::test_init().unwrap();
        let engine =
            crate::QemuEngine::new(&env.context, data, EmulatorInstances::new(PathBuf::new()));
        check_screenshot_preconditions(&engine, Path::new(NON_EXISTENT_PATH));
    }

    #[fuchsia::test]
    fn test_check_screenshot_preconditions_overwrites_file() {
        const EXISTING_PATH: &str = "existing.png";
        let temp = tempdir().unwrap();
        let file_path = temp.path().join(EXISTING_PATH);
        std::fs::write(&file_path, "data").unwrap();

        let mut config = EmulatorConfiguration::default();
        config.device.screen.width = 100;
        config.device.screen.height = 100;
        let mut data = EmulatorInstanceData::new(config, EngineType::Qemu, EngineState::Running);
        // Set the emulator PID to a fake ID so that `get_engine_state()` returns Running.
        let fake_emulator_pid = std::process::id();
        data.set_pid(fake_emulator_pid);
        let env = ffx_config::test_init().unwrap();
        let engine =
            crate::QemuEngine::new(&env.context, data, EmulatorInstances::new(PathBuf::new()));
        check_screenshot_preconditions(&engine, &file_path);
    }

    #[fuchsia::test]
    #[should_panic(expected = "Output path must not be a directory")]
    fn test_check_screenshot_preconditions_is_dir() {
        let temp = tempdir().unwrap();
        let dir_path = temp.path().to_path_buf();

        let mut config = EmulatorConfiguration::default();
        config.device.screen.width = 100;
        config.device.screen.height = 100;
        let mut data = EmulatorInstanceData::new(config, EngineType::Qemu, EngineState::Running);
        // Set the emulator PID to a fake ID so that `get_engine_state()` returns Running.
        let fake_emulator_pid = std::process::id();
        data.set_pid(fake_emulator_pid);
        let env = ffx_config::test_init().unwrap();
        let engine =
            crate::QemuEngine::new(&env.context, data, EmulatorInstances::new(PathBuf::new()));
        check_screenshot_preconditions(&engine, &dir_path);
    }

    /// Runs `server` as the QEMU side of a QMP connection on a socket pair, and returns the
    /// client side of the connection.
    fn fake_qmp_pair<F>(server: F) -> (UnixStream, std::thread::JoinHandle<()>)
    where
        F: FnOnce(UnixStream, &mut dyn Iterator<Item = serde_json::Result<Value>>) + Send + 'static,
    {
        let (client, server_stream) = UnixStream::pair().expect("socket pair");
        let handle = std::thread::spawn(move || {
            let reader = server_stream.try_clone().expect("clone");
            let mut requests = Deserializer::from_reader(reader).into_iter::<Value>();
            server(server_stream, &mut requests);
        });
        (client, handle)
    }

    fn write_json(mut stream: &UnixStream, value: Value) {
        stream.write_all(format!("{value}\n").as_bytes()).expect("write");
    }

    #[fuchsia::test]
    fn test_qmp_execute_success() {
        let (client, handle) = fake_qmp_pair(|stream, requests| {
            write_json(&stream, json!({"QMP": {"version": {}, "capabilities": []}}));
            let request = requests.next().unwrap().unwrap();
            assert_eq!(request["execute"], "input-send-event");
            assert_eq!(request["arguments"], json!({"events": []}));
            write_json(&stream, json!({"return": {"ok": 1}, "id": request["id"]}));
        });
        let mut responses = Deserializer::from_reader(&client).into_iter::<Value>();
        let result = qmp_execute(
            &client,
            &mut responses,
            "input-send-event",
            Some(json!({"events": []})),
            Duration::from_secs(10),
        )
        .expect("qmp_execute");
        assert_eq!(result, json!({"ok": 1}));
        handle.join().unwrap();
    }

    #[fuchsia::test]
    fn test_qmp_execute_no_arguments() {
        let (client, handle) = fake_qmp_pair(|stream, requests| {
            let request = requests.next().unwrap().unwrap();
            assert_eq!(request["execute"], "query-status");
            assert!(request.get("arguments").is_none(), "{request}");
            write_json(&stream, json!({"return": {}, "id": request["id"]}));
        });
        let mut responses = Deserializer::from_reader(&client).into_iter::<Value>();
        let result =
            qmp_execute(&client, &mut responses, "query-status", None, Duration::from_secs(10))
                .expect("qmp_execute");
        assert_eq!(result, json!({}));
        handle.join().unwrap();
    }

    #[fuchsia::test]
    fn test_qmp_execute_skips_events_and_other_ids() {
        let (client, handle) = fake_qmp_pair(|stream, requests| {
            let request = requests.next().unwrap().unwrap();
            // Reply to the qmp_capabilities command sent by open_socket(), which has no id.
            write_json(&stream, json!({"return": {}}));
            write_json(&stream, json!({"event": "RTC_CHANGE", "data": {"offset": 0}}));
            write_json(&stream, json!({"return": {"wrong": true}, "id": "some-other-id"}));
            write_json(&stream, json!({"return": {"right": true}, "id": request["id"]}));
        });
        let mut responses = Deserializer::from_reader(&client).into_iter::<Value>();
        let result =
            qmp_execute(&client, &mut responses, "cmd", None, Duration::from_secs(10)).unwrap();
        assert_eq!(result, json!({"right": true}));
        handle.join().unwrap();
    }

    #[fuchsia::test]
    fn test_qmp_execute_error() {
        let (client, handle) = fake_qmp_pair(|stream, requests| {
            let request = requests.next().unwrap().unwrap();
            write_json(
                &stream,
                json!({
                    "error": {
                        "class": "GenericError",
                        "desc": "Input handler not found for event type rel"
                    },
                    "id": request["id"]
                }),
            );
        });
        let mut responses = Deserializer::from_reader(&client).into_iter::<Value>();
        let err =
            qmp_execute(&client, &mut responses, "input-send-event", None, Duration::from_secs(10))
                .unwrap_err();
        assert_eq!(
            err.to_string(),
            "QMP command 'input-send-event' failed: Input handler not found for event type rel"
        );
        handle.join().unwrap();
    }

    #[fuchsia::test]
    fn test_qmp_execute_timeout() {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let (client, handle) = fake_qmp_pair(move |_stream, requests| {
            // Read the request but never answer it; keep the socket open until the client is
            // done so that the client sees a timeout rather than a closed connection.
            let _ = requests.next();
            let _ = done_rx.recv();
        });
        let mut responses = Deserializer::from_reader(&client).into_iter::<Value>();
        let start = Instant::now();
        let err = qmp_execute(&client, &mut responses, "cmd", None, Duration::from_millis(200))
            .unwrap_err();
        assert!(start.elapsed() >= Duration::from_millis(200));
        assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
        assert!(err.to_string().contains("Timed out"), "{err}");
        done_tx.send(()).unwrap();
        handle.join().unwrap();
    }

    #[fuchsia::test]
    fn test_qmp_execute_connection_closed() {
        let (client, handle) = fake_qmp_pair(|stream, requests| {
            let _ = requests.next();
            write_json(&stream, json!({"event": "SHUTDOWN"}));
            // Dropping the stream closes the connection.
        });
        let mut responses = Deserializer::from_reader(&client).into_iter::<Value>();
        let err =
            qmp_execute(&client, &mut responses, "cmd", None, Duration::from_secs(10)).unwrap_err();
        assert!(err.to_string().contains("connection closed"), "{err}");
        handle.join().unwrap();
    }
}
