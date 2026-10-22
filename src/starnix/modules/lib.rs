// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Increase recursion limit because LTO causes overflow.
#![recursion_limit = "256"]

use anyhow::{Context, Error, anyhow, bail};
use bstr::BString;
use fidl_fuchsia_ui_composition as fuicomposition;
use fidl_fuchsia_ui_input as fuiinput;
use fidl_fuchsia_ui_input3 as fuiinput3;
use fidl_fuchsia_ui_policy as fuipolicy;
use fidl_fuchsia_ui_views as fuiviews;
use starnix_consent_sync::init as consent_sync_init;
use starnix_core::bpf::fs::BpfFs;
use starnix_core::device::kobject::DeviceMetadata;
use starnix_core::device::mem::{DevRandom, mem_device_init};
use starnix_core::device::{DeviceMode, simple_device_ops};
use starnix_core::fs::devtmpfs::dev_tmp_fs;
use starnix_core::fs::fuchsia::{RemoteBundle, new_remote_fs, new_remote_vol};
use starnix_core::fs::sysfs::sys_fs;
use starnix_core::fs::tmpfs::tmp_fs;
use starnix_core::task::{Kernel, ThreadLockupDetector};
use starnix_core::vfs::fs_registry::FsRegistry;
use starnix_core::vfs::pipe::register_pipe_fs;
use starnix_logging::log_error;
use starnix_modules_android_usb::usb_device_init;
use starnix_modules_ashmem::ashmem_device_init;
use starnix_modules_battery_manager::battery_manager_device_init;
use starnix_modules_binderfs::BinderFs;
use starnix_modules_cgroupfs::{CgroupV1Fs, cgroup2_fs};
use starnix_modules_debugfs::debug_fs;
use starnix_modules_device_mapper::{create_device_mapper, device_mapper_init};
pub use starnix_modules_devpts::create_main_and_replica;
use starnix_modules_devpts::{dev_pts_fs, tty_device_init};
use starnix_modules_ext4::ExtFilesystem;
use starnix_modules_fastrpc::fastrpc_device_init;
pub use starnix_modules_framebuffer::Framebuffer;
use starnix_modules_functionfs::FunctionFs;
use starnix_modules_fuse::{new_fuse_fs, new_fusectl_fs, open_fuse_device};
use starnix_modules_goog_chip_info::goog_chip_info_device_init;
use starnix_modules_gpu::gpu_device_init;
use starnix_modules_gralloc::gralloc_device_init;
use starnix_modules_hvdcp_opti::hvdcp_opti_init;
use starnix_modules_inotify::inotify::inotify_init;
use starnix_modules_input::uinput::register_uinput_device;
use starnix_modules_input::{EventProxyMode, StartRelaysArgs, new_input_relay};
use starnix_modules_kgsl::kgsl_device_init;
pub use starnix_modules_layeredfs::{LayeredFsBuilder, LayeredFsMounts};
use starnix_modules_loop::{create_loop_control_device, loop_device_init};
use starnix_modules_mmcblk_stub::mmcblk_stub_device_init;
use starnix_modules_nanohub::nanohub_device_init;
use starnix_modules_nmfs::nmfs_init;
use starnix_modules_odpm::odpm_device_init;
pub use starnix_modules_overlayfs::OverlayStack;
use starnix_modules_overlayfs::new_overlay_fs;
use starnix_modules_power_residency::init_power_residency_device;
use starnix_modules_procfs::proc_fs;
use starnix_modules_pstore::pstore_fs;
use starnix_modules_rtc::rtc_device_init;
use starnix_modules_selinuxfs::selinux_fs;
use starnix_modules_touch_power_policy::TouchPowerPolicyDevice;
use starnix_modules_tracefs::trace_fs;
use starnix_modules_tun::DevTun;
use starnix_modules_usbfs::usbfs_init;
use starnix_modules_wakeup_test::register_wakeup_test_device;
use starnix_modules_zram::zram_device_init;
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errors::Errno;
use starnix_uapi::fs_type::FileSystemTypeFlags;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Modules that are enabled by default for all containers unless explicitly disabled via
/// `"-<module>"` in `program.modules`.
const DEFAULT_MODULES: &[&str] = &[
    "battery_manager",
    "device_mapper",
    "goog_chip_info",
    "hw_random",
    "loop",
    "power_residency",
    "rtc",
    "tun",
    "zram",
];

/// Validates that `name` is a known container module and that `args` (if `Some`) are valid for
/// that module.
pub fn validate_module(name: &str, args: Option<&[&str]>) -> Result<(), Error> {
    let validator: Option<fn(&[&str]) -> Result<(), Error>> = match name {
        "boot_notifier" => Some(starnix_modules_boot::validate_args),
        "framebuffer" => Some(starnix_modules_framebuffer::validate_args),
        "magma" => Some(starnix_modules_magma::validate_args),
        "perfetto" => Some(starnix_modules_perfetto_consumer::validate_args),
        "thermal" => Some(starnix_modules_thermal::validate_args),
        "android_usb"
        | "ashmem"
        | "battery_manager"
        | "data_collection_consent_sync"
        | "device_mapper"
        | "fastrpc"
        | "gfxstream"
        | "goog_chip_info"
        | "google_odpm"
        | "gralloc"
        | "hvdcp_opti"
        | "hw_random"
        | "kgsl"
        | "loop"
        | "mmcblk_stub"
        | "nanohub"
        | "network_manager"
        | "power_residency"
        | "rtc"
        | "tun"
        | "usbfs"
        | "wakeup_test"
        | "zram" => None,
        _ => bail!("unsupported module: {name}"),
    };
    if let Some(args) = args {
        match validator {
            Some(validate) => validate(args)?,
            None if !args.is_empty() => {
                bail!("module '{name}' does not accept arguments: {args:?}");
            }
            None => {}
        }
    }
    Ok(())
}

impl Modules {
    fn init_module(&self, name: &str, kernel: &Arc<Kernel>, args: &[&str]) -> Result<(), Error> {
        validate_module(name, Some(args))?;
        let registry = &kernel.device_registry;
        match name {
            "android_usb" => {
                usb_device_init(kernel).context("Failed to add android usb device nodes")?;
            }
            "ashmem" => ashmem_device_init(kernel),
            "battery_manager" => battery_manager_device_init(kernel),
            "boot_notifier" => starnix_modules_boot::module_init(kernel, args)?,
            "data_collection_consent_sync" => consent_sync_init(kernel),
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
            "fastrpc" => fastrpc_device_init(kernel),
            "framebuffer" => init_framebuffer_module(
                kernel,
                args,
                self.enable_visual_debugging,
                &self.initial_view_id_annotation,
            )?,
            "gfxstream" => gpu_device_init(kernel),
            "goog_chip_info" => goog_chip_info_device_init(kernel),
            // Note: `google_odpm` and `hvdcp_opti` both statically register `iio:device0`;
            // `Modules::validate` rejects enabling both simultaneously.
            "google_odpm" => odpm_device_init(kernel),
            // The virtgralloc0 device allows vulkan_selector to indicate to gralloc whether
            // swiftshader or magma will be used. This is separate from the magma module because
            // the policy choice whether to use magma or swiftshader is in vulkan_selector, and it
            // can potentially choose swiftshader for testing purposes even when magma0 is present.
            // Also, it's nice to indicate swiftshader the same way regardless of whether the magma
            // module is enabled or disabled.
            "gralloc" => gralloc_device_init(kernel),
            "hvdcp_opti" => hvdcp_opti_init(kernel),
            "hw_random" => {
                registry.register_device(
                    kernel,
                    "hw_random".into(),
                    DeviceMetadata::new("hwrng".into(), DeviceId::HW_RANDOM, DeviceMode::Char),
                    registry.objects.misc_class(),
                    simple_device_ops::<DevRandom>,
                )?;
            }
            "kgsl" => kgsl_device_init(kernel),
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
            "magma" => starnix_modules_magma::module_init(kernel, args)?,
            "mmcblk_stub" => {
                mmcblk_stub_device_init(kernel).context("Failed to add stub mmcblk0 device")?;
            }
            "nanohub" => nanohub_device_init(kernel),
            "network_manager" => {
                if let Err(e) = nmfs_init(kernel) {
                    log_error!("Network manager initialization failed: ({e:?})");
                }
            }
            "perfetto" => starnix_modules_perfetto_consumer::module_init(kernel, args)?,
            "power_residency" => init_power_residency_device(kernel),
            "rtc" => rtc_device_init(kernel)?,
            "thermal" => starnix_modules_thermal::module_init(kernel, args)?,
            "tun" => {
                registry.register_device(
                    kernel,
                    "tun".into(),
                    DeviceMetadata::new("tun".into(), DeviceId::TUN, DeviceMode::Char),
                    registry.objects.misc_class(),
                    simple_device_ops::<DevTun>,
                )?;
            }
            "usbfs" => {
                usbfs_init(kernel).context("Failed to initialize usbfs")?;
            }
            "wakeup_test" => register_wakeup_test_device(kernel)?,
            "zram" => zram_device_init(kernel)?,
            _ => bail!("unsupported module: {name}"),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModuleState {
    DefaultEnabled,
    Enabled(Vec<String>),
    Disabled,
}

/// Configuration and initialization registry for container kernel modules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modules {
    entries: BTreeMap<String, ModuleState>,
    enable_visual_debugging: bool,
    initial_view_id_annotation: String,
}

impl Default for Modules {
    fn default() -> Self {
        let entries = DEFAULT_MODULES
            .iter()
            .map(|&module| (module.to_string(), ModuleState::DefaultEnabled))
            .collect();
        Self { entries, enable_visual_debugging: false, initial_view_id_annotation: String::new() }
    }
}

impl Modules {
    /// Sets container-level visual debugging and view annotation options used when the
    /// `framebuffer` module is initialized.
    pub fn set_framebuffer_config(
        &mut self,
        enable_visual_debugging: bool,
        initial_view_id_annotation: String,
    ) {
        self.enable_visual_debugging = enable_visual_debugging;
        self.initial_view_id_annotation = initial_view_id_annotation;
    }

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
        self.entries.insert(module.to_string(), ModuleState::Disabled);
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
            Some(ModuleState::Disabled) => {}
            Some(ModuleState::Enabled(existing_args)) => {
                if !args.is_empty() {
                    *existing_args = args;
                }
            }
            Some(ModuleState::DefaultEnabled) | None => {
                self.entries.insert(module.to_string(), ModuleState::Enabled(args));
            }
        }
    }

    /// Returns whether `module` is enabled.
    pub fn is_enabled(&self, module: &str) -> bool {
        self.args(module).is_some()
    }

    /// Returns the normalized arguments configured for `module`, if enabled.
    pub fn args(&self, module: &str) -> Option<&[String]> {
        match self.entries.get(module)? {
            ModuleState::DefaultEnabled => {
                // `hvdcp_opti` registers its own `power_supply/battery` device, superseding the
                // default `battery_manager` module unless `battery_manager` was explicitly enabled.
                if module == "battery_manager"
                    && matches!(self.entries.get("hvdcp_opti"), Some(ModuleState::Enabled(_)))
                {
                    None
                } else {
                    Some(&[])
                }
            }
            ModuleState::Enabled(args) => Some(args.as_slice()),
            ModuleState::Disabled => None,
        }
    }

    /// Validates all configured module names, module arguments, and cross-module constraints.
    pub fn validate(&self) -> Result<(), Error> {
        for module in self.entries.keys() {
            let arg_refs: Option<Vec<&str>> =
                self.args(module).map(|a| a.iter().map(String::as_str).collect());
            validate_module(module, arg_refs.as_deref())?;
        }
        if self.is_enabled("battery_manager") && self.is_enabled("hvdcp_opti") {
            bail!("modules 'battery_manager' and 'hvdcp_opti' cannot be enabled simultaneously");
        }
        if self.is_enabled("google_odpm") && self.is_enabled("hvdcp_opti") {
            bail!("modules 'google_odpm' and 'hvdcp_opti' cannot be enabled simultaneously");
        }
        Ok(())
    }

    /// Returns any extra kernel command-line parameters contributed by enabled modules.
    pub fn extra_cmdline_params(&self) -> Result<Option<BString>, Error> {
        if let Some(args) = self.args("magma") {
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            return Ok(Some(starnix_modules_magma::cmdline_params(&arg_refs)?));
        }
        Ok(None)
    }

    /// Records the enabled modules and their arguments under a `"modules"` child Inspect node.
    pub fn record_inspect(&self, parent_node: &fuchsia_inspect::Node) {
        parent_node.record_child("modules", |inspect_node| {
            for module in self.entries.keys() {
                if let Some(args) = self.args(module) {
                    inspect_node.record_string(module, args.join(" "));
                }
            }
        });
    }

    /// Initializes all enabled modules in `kernel`.
    pub fn init_modules(&self, kernel: &Arc<Kernel>) -> Result<(), Error> {
        for module in self.entries.keys() {
            if let Some(args) = self.args(module) {
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                self.init_module(module, kernel, &arg_refs)?;
            }
        }
        Ok(())
    }
}

fn init_framebuffer_module(
    kernel: &Kernel,
    args: &[&str],
    enable_visual_debugging: bool,
    initial_view_id_annotation: &str,
) -> Result<(), Error> {
    let framebuffer = Framebuffer::device_init(
        kernel,
        args,
        enable_visual_debugging,
        initial_view_id_annotation.to_string(),
    )
    .context("initializing framebuffer")?;

    let (touch_source_client, touch_source_server) = fidl::endpoints::create_endpoints();
    let (mouse_source_client, mouse_source_server) = fidl::endpoints::create_endpoints();
    let view_bound_protocols = fuicomposition::ViewBoundProtocols {
        touch_source_v2: Some(touch_source_server),
        mouse_source_v2: Some(mouse_source_server),
        ..Default::default()
    };
    let view_identity = fuiviews::ViewIdentityOnCreation::from(
        fuchsia_scenic::ViewRefPair::new().expect("Failed to create ViewRefPair"),
    );
    let view_ref = fuchsia_scenic::duplicate_view_ref(&view_identity.view_ref)
        .expect("Failed to dup view ref.");
    let keyboard =
        fuchsia_component::client::connect_to_protocol_sync::<fuiinput3::KeyboardMarker>()
            .expect("Failed to connect to keyboard");
    let registry_proxy = fuchsia_component::client::connect_to_protocol_sync::<
        fuipolicy::DeviceListenerRegistryMarker,
    >()
    .context("Failed to connect to policy device listener registry")?;
    let device_listener_registry = fuchsia_component::client::connect_to_protocol_sync::<
        fuiinput::DeviceListenerRegistryMarker,
    >()
    .context("Failed to connect to device listener registry")?;

    // `view_identity` and `view_bound_protocols` must be populated before
    // `Framebuffer::start_server` is called.
    *framebuffer.view_identity.lock() = Some(view_identity);
    *framebuffer.view_bound_protocols.lock() = Some(view_bound_protocols);

    let (display_width, display_height) = {
        let framebuffer_info = framebuffer.info.read();
        (framebuffer_info.xres as i32, framebuffer_info.yres as i32)
    };

    let (input_events_relay, input_events_relay_handle) = new_input_relay();
    input_events_relay.start_relays(
        kernel,
        StartRelaysArgs {
            event_proxy_mode: EventProxyMode::WakeContainer,
            touch_source_client_end: touch_source_client,
            keyboard_proxy: keyboard,
            mouse_source_client_end: mouse_source_client,
            view_ref,
            registry_proxy,
            device_listener_registry: Some(device_listener_registry),
            display_width,
            display_height,
        },
    );

    register_uinput_device(kernel, input_events_relay_handle)?;

    // Channel we use to inform the relay of changes to `touch_standby`
    let (touch_standby_sender, touch_standby_receiver) =
        ThreadLockupDetector::tracked_channel::<bool>();
    let touch_policy_device = TouchPowerPolicyDevice::new(touch_standby_sender);
    touch_policy_device.clone().register(kernel);
    touch_policy_device.start_relay(kernel, touch_standby_receiver);

    framebuffer.start_server(kernel, /* incoming_dir = */ None);
    Ok(())
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
