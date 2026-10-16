// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Increase recursion limit because LTO causes overflow.
#![recursion_limit = "256"]

use anyhow::{Error, anyhow, bail};
pub use starnix_consent_sync::init as consent_sync_init;
use starnix_core::bpf::fs::BpfFs;
use starnix_core::device::kobject::DeviceMetadata;
use starnix_core::device::mem::{DevRandom, mem_device_init};
use starnix_core::device::{DeviceMode, simple_device_ops};
use starnix_core::fs::devtmpfs::dev_tmp_fs;
use starnix_core::fs::fuchsia::{RemoteBundle, new_remote_fs, new_remote_vol};
use starnix_core::fs::sysfs::sys_fs;
use starnix_core::fs::tmpfs::tmp_fs;
use starnix_core::task::Kernel;
use starnix_core::vfs::fs_registry::FsRegistry;
use starnix_core::vfs::pipe::register_pipe_fs;
pub use starnix_modules_android_usb::usb_device_init;
pub use starnix_modules_ashmem::ashmem_device_init;
use starnix_modules_binderfs::BinderFs;
pub use starnix_modules_boot::booted_device_init;
use starnix_modules_cgroupfs::{CgroupV1Fs, cgroup2_fs};
use starnix_modules_debugfs::debug_fs;
use starnix_modules_device_mapper::{create_device_mapper, device_mapper_init};
pub use starnix_modules_devpts::create_main_and_replica;
use starnix_modules_devpts::{dev_pts_fs, tty_device_init};
use starnix_modules_ext4::ExtFilesystem;
pub use starnix_modules_fastrpc::fastrpc_device_init;
pub use starnix_modules_framebuffer::{AspectRatio, Framebuffer};
use starnix_modules_functionfs::FunctionFs;
use starnix_modules_fuse::{new_fuse_fs, new_fusectl_fs, open_fuse_device};
pub use starnix_modules_gpu::gpu_device_init;
pub use starnix_modules_gralloc::gralloc_device_init;
pub use starnix_modules_hvdcp_opti::hvdcp_opti_init;
use starnix_modules_inotify::inotify::inotify_init;
pub use starnix_modules_input::uinput::register_uinput_device;
pub use starnix_modules_input::{EventProxyMode, StartRelaysArgs, new_input_relay};
pub use starnix_modules_kgsl::kgsl_device_init;
pub use starnix_modules_layeredfs::{LayeredFsBuilder, LayeredFsMounts};
use starnix_modules_loop::{create_loop_control_device, loop_device_init};
pub use starnix_modules_magma::{get_magma_params, magma_device_init};
pub use starnix_modules_nanohub::nanohub_device_init;
pub use starnix_modules_nmfs::nmfs_init;
pub use starnix_modules_odpm::odpm_device_init;
pub use starnix_modules_overlayfs::OverlayStack;
use starnix_modules_overlayfs::new_overlay_fs;
pub use starnix_modules_perfetto_consumer::start_perfetto_consumer_thread;
use starnix_modules_power_residency::init_power_residency_device;
use starnix_modules_procfs::proc_fs;
use starnix_modules_pstore::pstore_fs;
use starnix_modules_rtc::rtc_device_init;
use starnix_modules_selinuxfs::selinux_fs;
pub use starnix_modules_thermal::{cooling_device_init, thermal_device_init};
pub use starnix_modules_touch_power_policy::TouchPowerPolicyDevice;
use starnix_modules_tracefs::trace_fs;
use starnix_modules_tun::DevTun;
pub use starnix_modules_wakeup_test::register_wakeup_test_device;
use starnix_modules_zram::zram_device_init;
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errors::Errno;
use starnix_uapi::fs_type::FileSystemTypeFlags;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Modules that are enabled by default for all containers unless explicitly disabled via
/// `"-<module>"` in `program.modules`.
const DEFAULT_MODULES: &[&str] =
    &["device_mapper", "hw_random", "loop", "power_residency", "rtc", "tun", "zram"];

/// Validates that `name` is a known container module and that `args` (if `Some`) are valid for
/// that module.
pub fn validate_module(name: &str, args: Option<&[&str]>) -> Result<(), Error> {
    match name {
        "device_mapper" | "hw_random" | "loop" | "power_residency" | "rtc" | "tun" | "zram" => {
            if let Some(args) = args
                && !args.is_empty()
            {
                bail!("module '{name}' does not accept arguments: {args:?}");
            }
        }
        _ => bail!("unsupported module: {name}"),
    }
    Ok(())
}

impl Modules {
    fn init_module(&self, name: &str, kernel: &Arc<Kernel>, args: &[&str]) -> Result<(), Error> {
        validate_module(name, Some(args))?;
        let registry = &kernel.device_registry;
        match name {
            "device_mapper" => {
                registry.register_device(
                    kernel,
                    "device-mapper".into(),
                    DeviceMetadata::new(
                        "mapper/control".into(),
                        DeviceId::DEVICE_MAPPER,
                        DeviceMode::Char,
                    ),
                    registry.objects.misc_class(),
                    create_device_mapper,
                )?;
                device_mapper_init(kernel)?;
            }
            "hw_random" => {
                registry.register_device(
                    kernel,
                    "hw_random".into(),
                    DeviceMetadata::new("hwrng".into(), DeviceId::HW_RANDOM, DeviceMode::Char),
                    registry.objects.misc_class(),
                    simple_device_ops::<DevRandom>,
                )?;
            }
            "loop" => {
                registry.register_device(
                    kernel,
                    "loop-control".into(),
                    DeviceMetadata::new(
                        "loop-control".into(),
                        DeviceId::LOOP_CONTROL,
                        DeviceMode::Char,
                    ),
                    registry.objects.misc_class(),
                    create_loop_control_device,
                )?;
                loop_device_init(kernel)?;
            }
            "power_residency" => init_power_residency_device(kernel),
            "rtc" => rtc_device_init(kernel)?,
            "tun" => {
                registry.register_device(
                    kernel,
                    "tun".into(),
                    DeviceMetadata::new("tun".into(), DeviceId::TUN, DeviceMode::Char),
                    registry.objects.misc_class(),
                    simple_device_ops::<DevTun>,
                )?;
            }
            "zram" => zram_device_init(kernel)?,
            _ => bail!("unsupported module: {name}"),
        }
        Ok(())
    }
}

/// Configuration and initialization registry for container kernel modules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modules {
    entries: BTreeMap<String, Option<Vec<String>>>,
}

impl Default for Modules {
    fn default() -> Self {
        let entries =
            DEFAULT_MODULES.iter().map(|&module| (module.to_string(), Some(Vec::new()))).collect();
        Self { entries }
    }
}

impl Modules {
    /// Parses and registers a single module specification string (`"<module> [args...]"` or
    /// `"-<module>"`).
    pub fn add_spec(&mut self, spec: &str) -> Result<(), Error> {
        let mut tokens = spec.split_whitespace();
        let name = tokens.next().ok_or_else(|| anyhow!("empty module specification"))?;
        if let Some(disabled_module) = name.strip_prefix('-') {
            if disabled_module.is_empty() || disabled_module.starts_with('-') {
                bail!("invalid module specification: {spec}");
            }
            if tokens.next().is_some() {
                bail!("cannot specify arguments when disabling module '{disabled_module}'");
            }
            self.disable(disabled_module);
            return Ok(());
        }
        let mut args = Vec::new();
        for token in tokens {
            // Normalize `--option=value` into `["--option", "value"]` for `argh::FromArgs`.
            if token.starts_with('-')
                && let Some((flag, value)) = token.split_once('=')
            {
                args.push(flag.to_string());
                args.push(value.to_string());
            } else {
                args.push(token.to_string());
            }
        }
        self.add_with_args(name, args);
        Ok(())
    }

    /// Disables `module`, preventing it from being initialized even if enabled by default or in an
    /// included manifest shard.
    fn disable(&mut self, module: &str) {
        self.entries.insert(module.to_string(), None);
    }

    /// Enables `module` with no arguments, preserving any arguments or explicit disablement already
    /// configured for `module`.
    pub fn enable(&mut self, module: &str) {
        self.add_with_args(module, Vec::new());
    }

    /// Enables `module` with `args` unless `module` has been explicitly disabled.
    ///
    /// If `args` is empty and `module` is already enabled, any existing arguments configured for
    /// `module` are preserved so that bare `<module>` entries (for example from an included
    /// manifest shard) do not overwrite explicit module arguments.
    pub fn add_with_args(&mut self, module: &str, args: Vec<String>) {
        match self.entries.get_mut(module) {
            Some(None) => {}
            Some(Some(existing_args)) => {
                if !args.is_empty() {
                    *existing_args = args;
                }
            }
            None => {
                self.entries.insert(module.to_string(), Some(args));
            }
        }
    }

    /// Returns whether `module` is enabled.
    pub fn is_enabled(&self, module: &str) -> bool {
        matches!(self.entries.get(module), Some(Some(_)))
    }

    /// Returns the normalized arguments configured for `module`, if enabled.
    pub fn args(&self, module: &str) -> Option<&[String]> {
        self.entries.get(module).and_then(Option::as_deref)
    }

    /// Validates all configured module names, module arguments, and cross-module constraints.
    pub fn validate(&self) -> Result<(), Error> {
        for (module, args) in &self.entries {
            let arg_refs: Option<Vec<&str>> =
                args.as_ref().map(|a| a.iter().map(String::as_str).collect());
            validate_module(module, arg_refs.as_deref())?;
        }
        Ok(())
    }

    /// Records the enabled modules and their arguments under a `"modules"` child Inspect node.
    pub fn record_inspect(&self, parent_node: &fuchsia_inspect::Node) {
        parent_node.record_child("modules", |inspect_node| {
            for (module, args) in &self.entries {
                if let Some(args) = args {
                    inspect_node.record_string(module, args.join(" "));
                }
            }
        });
    }

    /// Initializes all enabled modules in `kernel`.
    pub fn init_modules(&self, kernel: &Arc<Kernel>) -> Result<(), Error> {
        for (module, args) in &self.entries {
            if let Some(args) = args {
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                self.init_module(module, kernel, &arg_refs)?;
            }
        }
        Ok(())
    }
}

/// Initializes common devices in `Kernel`.
///
/// Adding device nodes to devtmpfs requires the current running task. The `Kernel` constructor does
/// not create an initial task, so this function should be triggered after a `CurrentTask` has been
/// initialized.
pub fn init_common_devices(kernel: &Kernel) -> Result<(), Errno> {
    let registry = &kernel.device_registry;
    registry.register_device(
        kernel,
        "fuse".into(),
        DeviceMetadata::new("fuse".into(), DeviceId::FUSE, DeviceMode::Char),
        registry.objects.misc_class(),
        open_fuse_device,
    )?;
    mem_device_init(kernel)?;
    tty_device_init(kernel)?;
    Ok(())
}

pub fn register_common_file_systems(kernel: &Kernel) {
    let registry = kernel.expando.get::<FsRegistry>();
    registry.register(b"binder".into(), FileSystemTypeFlags::empty(), BinderFs::new_fs);
    registry.register(b"bpf".into(), FileSystemTypeFlags::empty(), BpfFs::new_fs);
    registry.register(b"cgroup".into(), FileSystemTypeFlags::empty(), CgroupV1Fs::new_fs);
    registry.register(b"cgroup2".into(), FileSystemTypeFlags::empty(), cgroup2_fs);
    // Cpusets use the generic cgroup (v1) subsystem.
    // From https://docs.kernel.org/admin-guide/cgroup-v1/cpusets.html
    registry.register(b"cpuset".into(), FileSystemTypeFlags::empty(), CgroupV1Fs::new_fs_cpuset);
    registry.register(b"debugfs".into(), FileSystemTypeFlags::empty(), debug_fs);
    registry.register(b"devpts".into(), FileSystemTypeFlags::empty(), dev_pts_fs);
    registry.register(b"devtmpfs".into(), FileSystemTypeFlags::empty(), dev_tmp_fs);
    registry.register(b"erofs".into(), FileSystemTypeFlags::empty(), starnix_modules_erofs::new_fs);
    registry.register(b"ext4".into(), FileSystemTypeFlags::REQUIRES_DEV, ExtFilesystem::new_fs);
    registry.register(b"functionfs".into(), FileSystemTypeFlags::empty(), FunctionFs::new_fs);
    registry.register(b"fuse".into(), FileSystemTypeFlags::empty(), new_fuse_fs);
    registry.register(b"fusectl".into(), FileSystemTypeFlags::empty(), new_fusectl_fs);
    registry.register(b"overlay".into(), FileSystemTypeFlags::empty(), new_overlay_fs);
    register_pipe_fs(registry.as_ref());
    registry.register(b"proc".into(), FileSystemTypeFlags::empty(), proc_fs);
    registry.register(b"pstore".into(), FileSystemTypeFlags::empty(), pstore_fs);
    registry.register(b"remotefs".into(), FileSystemTypeFlags::empty(), new_remote_fs);
    registry.register(b"remotevol".into(), FileSystemTypeFlags::empty(), new_remote_vol);
    registry.register(b"remote_bundle".into(), FileSystemTypeFlags::empty(), RemoteBundle::new_fs);
    registry.register(b"selinuxfs".into(), FileSystemTypeFlags::empty(), selinux_fs);
    registry.register(b"sysfs".into(), FileSystemTypeFlags::empty(), sys_fs);
    registry.register(b"tmpfs".into(), FileSystemTypeFlags::empty(), tmp_fs);
    registry.register(b"tracefs".into(), FileSystemTypeFlags::empty(), trace_fs);
}

pub fn register_common_syscalls(kernel: &Kernel) {
    inotify_init(kernel);
}
