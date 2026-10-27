// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ContainerStartInfo;
use anyhow::{Error, anyhow};
use starnix_container_structured_config::Config as ContainerStructuredConfig;
use starnix_core::fs::tmpfs::register_selinux_test_suite_ramfs;
use starnix_core::mm::MlockPinFlavor;
use starnix_core::task::{Kernel, KernelFeatures, OomWatermarksMb, SystemLimits};
use starnix_features::{Feature, FeatureAndArgs};
use starnix_modules::{Framebuffer, Modules, validate_module};
use starnix_uapi::error;
use starnix_uapi::errors::Errno;
use std::sync::Arc;

/// A collection of parsed features, and their arguments.
#[derive(Default, Debug)]
pub struct Features {
    /// Features that must be available to the kernel after initialization.
    pub kernel: KernelFeatures,

    /// SELinux configuration.
    pub selinux: SELinuxFeature,

    /// Limits value passed to the kernel during the initialization.
    pub system_limits: SystemLimits,

    /// This controls whether or not the default framebuffer background is black or colorful, to
    /// aid debugging.
    pub enable_visual_debugging: bool,

    /// The value of the initial view ID annotation.
    pub initial_view_id_annotation: String,

    /// Include the /container directory in the root file system.
    pub container: bool,

    /// Include the /test_data directory in the root file system.
    pub test_data: bool,

    /// Include the /custom_artifacts directory in the root file system.
    pub custom_artifacts: bool,

    /// Whether to provide android with a serial number.
    pub android_serialno: bool,

    /// Whether to allow the root filesystem to be read/write.
    pub rootfs_rw: bool,

    pub enable_utc_time_adjustment: bool,

    /// Whether to add android bootreason to kernel cmdline.
    pub android_bootreason: bool,

    pub additional_mounts: Option<Vec<String>>,

    /// Whether to initialize unified tracing with a Fuchsia trace observer.
    pub unified_tracing: bool,
}

#[derive(Default, Debug, PartialEq)]
pub struct SELinuxFeature {
    /// True if SELinux should be enabled in the container.
    pub enabled: bool,

    /// Optional set of options to pass to the SELinux module.
    pub options: String,

    /// Optional set of access-check exceptions to pass to the SELinux module.
    pub exceptions: Vec<String>,
}

impl Features {
    pub fn record_inspect(&self, parent_node: &fuchsia_inspect::Node) {
        parent_node.record_child("features", |inspect_node| match self {
            Features {
                kernel:
                    KernelFeatures {
                        bpf_v2,
                        devtmpfs,
                        enable_suid,
                        io_uring,
                        error_on_failed_reboot,
                        default_uid,
                        default_seclabel,
                        selinux_test_suite,
                        default_ns_mount_options,
                        mlock_always_onfault,
                        mlock_pin_flavor,
                        crash_report_throttling,
                        wifi,
                        cached_zx_map_info_bytes,
                        dirent_cache_size,
                        fake_ion,
                        remotevol_in_process,
                        log_sysfs_lookup_misses,
                        oom_watermarks_mb,
                        stop_last_on_shutdown,
                    },
                system_limits,
                selinux,
                enable_visual_debugging,
                container,
                test_data,
                custom_artifacts,
                android_serialno,
                rootfs_rw,
                enable_utc_time_adjustment,
                android_bootreason,
                additional_mounts,
                unified_tracing,
                initial_view_id_annotation,
            } => {
                inspect_node.record_bool("selinux", selinux.enabled);
                inspect_node.record_bool("unified_tracing", *unified_tracing);
                inspect_node.record_string(
                    "initial_view_id_annotation",
                    initial_view_id_annotation.as_str(),
                );
                inspect_node.record_bool("container", *container);
                inspect_node.record_bool("test_data", *test_data);
                inspect_node.record_bool("custom_artifacts", *custom_artifacts);
                inspect_node.record_bool("android_serialno", *android_serialno);
                inspect_node.record_bool("rootfs_rw", *rootfs_rw);
                inspect_node.record_bool("android_bootreason", *android_bootreason);
                inspect_node.record_string("ping_group_range", {
                    let range = system_limits.socket.icmp_ping_gids.lock();
                    std::format!("{},{}", range.start, range.end - 1)
                });

                inspect_node.record_child("kernel", |kernel_node| {
                    kernel_node.record_bool("bpf_v2", *bpf_v2);
                    kernel_node.record_bool("devtmpfs", *devtmpfs);
                    kernel_node.record_bool("enable_suid", *enable_suid);
                    kernel_node.record_bool("io_uring", *io_uring);
                    kernel_node.record_bool("error_on_failed_reboot", *error_on_failed_reboot);
                    kernel_node.record_bool("enable_visual_debugging", *enable_visual_debugging);
                    kernel_node.record_int("default_uid", (*default_uid).into());
                    kernel_node.record_string(
                        "default_seclabel",
                        default_seclabel.as_deref().unwrap_or_default(),
                    );
                    kernel_node.record_bool("selinux_test_suite", *selinux_test_suite);
                    kernel_node.record_bool("crash_report_throttling", *crash_report_throttling);
                    kernel_node
                        .record_uint("cached_zx_map_info_bytes", *cached_zx_map_info_bytes as u64);
                    kernel_node.record_bool("log_sysfs_lookup_misses", *log_sysfs_lookup_misses);
                    if let Some(OomWatermarksMb { out_of_memory_mb, critical_mb, warning_mb }) =
                        oom_watermarks_mb
                    {
                        kernel_node.record_uint("out_of_memory_mb", *out_of_memory_mb as u64);
                        kernel_node.record_uint("critical_mb", *critical_mb as u64);
                        kernel_node.record_uint("warning_mb", *warning_mb as u64);
                    }
                    inspect_node.record_string(
                        "default_ns_mount_options",
                        format!("{:?}", default_ns_mount_options),
                    );
                    inspect_node
                        .record_bool("enable_utc_time_adjustment", *enable_utc_time_adjustment);
                    inspect_node.record_bool("mlock_always_onfault", *mlock_always_onfault);
                    inspect_node
                        .record_string("mlock_pin_flavor", format!("{:?}", mlock_pin_flavor));
                    inspect_node.record_bool("wifi", *wifi);
                    inspect_node
                        .record_string("additional_mounts", format!("{:?}", additional_mounts));
                    inspect_node.record_uint("dirent_cache_size", *dirent_cache_size as u64);
                    inspect_node.record_bool("fake_ion", *fake_ion);
                    inspect_node.record_bool("remotevol_in_process", *remotevol_in_process);
                    kernel_node.record_string(
                        "stop_last_on_shutdown",
                        format!("{:?}", stop_last_on_shutdown),
                    );
                });
            }
        });
    }
}

/// Parses container features and modules from `start_info` and `kernel_extra_features`.
pub fn parse_features(
    start_info: &ContainerStartInfo,
    kernel_extra_features: &[String],
) -> Result<(Features, Modules), Error> {
    let ContainerStructuredConfig {
        crash_report_throttling,
        enable_utc_time_adjustment,
        extra_features,
        cached_zx_map_info_bytes,
        mlock_always_onfault,
        mlock_pin_flavor,
        selinux_exceptions,
        ui_visual_debugging_level,
        additional_mounts,
        dirent_cache_size,
        initial_view_id_annotation,
    } = &start_info.config;

    let mut features = Features::default();
    let mut modules = Modules::default();

    let mut legacy_boot_notifier_cpu_boost: Option<String> = None;
    let mut legacy_aspect_ratio: Option<String> = None;

    for entry in start_info
        .program
        .features
        .iter()
        .chain(kernel_extra_features.iter())
        .chain(extra_features.iter())
    {
        let FeatureAndArgs { feature, raw_args } = FeatureAndArgs::try_parse(entry)?;
        match (feature, raw_args) {
            (Feature::AndroidSerialno, _) => features.android_serialno = true,
            (Feature::AndroidBootreason, _) => features.android_bootreason = true,
            (Feature::AspectRatio, Some(args)) => {
                legacy_aspect_ratio = Some(args);
            }
            (Feature::AspectRatio, None) => {
                return Err(anyhow!(
                    "Aspect ratio feature must contain the aspect ratio in the format: aspect_ratio:w:h"
                ));
            }
            (Feature::Container, _) => features.container = true,
            (Feature::CustomArtifacts, _) => features.custom_artifacts = true,
            (Feature::Devtmpfs, _) => features.kernel.devtmpfs = true,
            (
                Feature::AndroidUsb
                | Feature::Ashmem
                | Feature::BootNotifier
                | Feature::DataCollectionConsentSync
                | Feature::Fastrpc
                | Feature::Framebuffer
                | Feature::Gfxstream
                | Feature::GoogleOdpm
                | Feature::Gralloc
                | Feature::HvdcpOpti
                | Feature::Kgsl
                | Feature::Magma
                | Feature::MmcblkStub
                | Feature::Nanohub
                | Feature::NetworkManager
                | Feature::Thermal
                | Feature::Usbfs
                | Feature::WakeupTest,
                _,
            ) => modules.enable(&feature.to_string()),
            (Feature::BootNotifierCpuBoost, Some(arg)) => {
                legacy_boot_notifier_cpu_boost = Some(arg);
            }
            (Feature::BootNotifierCpuBoost, None) => {
                return Err(anyhow!(
                    "boot_notifier_cpu_boost feature must have an argument (e.g. \"boot_notifier_cpu_boost:60\")"
                ));
            }
            (Feature::MagmaSupportedVendors, Some(arg)) => {
                modules.add_with_args("magma", vec![format!("supported_vendors={arg}")]);
            }
            (Feature::MagmaSupportedVendors, None) => {
                return Err(anyhow!(
                    "Feature format must be: magma_supported_vendors:0x1234[,0xabcd]"
                ));
            }
            (Feature::UnifiedTracing, _) => features.unified_tracing = true,
            (Feature::Bpf, Some(version)) => features.kernel.bpf_v2 = version == "v2",
            (Feature::Bpf, None) => {
                return Err(anyhow!("bpf feature must have an argument (e.g. \"bpf:v2\")"));
            }
            (Feature::EnableSuid, _) => features.kernel.enable_suid = true,
            (Feature::IoUring, _) => features.kernel.io_uring = true,
            (Feature::ErrorOnFailedReboot, _) => features.kernel.error_on_failed_reboot = true,
            (Feature::Perfetto, Some(socket_path)) => {
                modules.add_with_args("perfetto", vec![format!("socket_path={socket_path}")]);
            }
            (Feature::Perfetto, None) => {
                return Err(anyhow!("Perfetto feature must contain a socket path"));
            }
            (Feature::PingGroupRange, Some(arg)) => {
                let mut args = arg.split(',');
                let (min, max) = (|| {
                    let min = args.next()?.trim_ascii().parse::<u32>().ok()?;
                    let max = args.next()?.trim_ascii().parse::<u32>().ok()?.checked_add(1)?;
                    if args.next().is_some() {
                        return None;
                    }
                    Some((min, max))
                })()
                .ok_or_else(|| anyhow!("Feature format must be: ping_group_range:0,100"))?;
                *features.system_limits.socket.icmp_ping_gids.lock() = min..max;
            }
            (Feature::PingGroupRange, None) => {
                return Err(anyhow!("Feature format must be: ping_group_range:0,100"));
            }
            (Feature::RootfsRw, _) => features.rootfs_rw = true,
            (Feature::Selinux, arg) => {
                features.selinux = SELinuxFeature {
                    enabled: true,
                    options: arg.unwrap_or_default(),
                    exceptions: selinux_exceptions.clone(),
                };
            }
            (Feature::SelinuxTestSuite, _) => features.kernel.selinux_test_suite = true,
            (Feature::TestData, _) => features.test_data = true,
            (Feature::Cooling, Some(arg)) => {
                modules.add_with_args("thermal", vec![format!("cooling={arg}")]);
            }
            (Feature::Cooling, None) => {
                return Err(anyhow!("cooling feature must have an argument"));
            }
            (Feature::Wifi, _) => features.kernel.wifi = true,
            (Feature::AdditionalMounts, _) => {
                features.additional_mounts = Some(additional_mounts.clone())
            }
            (Feature::FakeIon, _) => features.kernel.fake_ion = true,
            (Feature::RemotevolInProcess, _) => features.kernel.remotevol_in_process = true,
            (Feature::LogSysfsLookupMisses, _) => features.kernel.log_sysfs_lookup_misses = true,
        };
    }

    let legacy_boot_notifier = modules.is_enabled("boot_notifier");
    for spec in &start_info.program.modules {
        modules.add_spec(spec)?;
    }

    if let Some(secs) = legacy_boot_notifier_cpu_boost {
        if !legacy_boot_notifier && !modules.is_enabled("boot_notifier") {
            return Err(anyhow!("boot_notifier_cpu_boost feature requires boot_notifier"));
        }
        let arg = format!("cpu_boost_secs={secs}");
        validate_module("boot_notifier", Some(&[arg.as_str()]))?;
        if modules.args("boot_notifier") == Some(&[]) {
            modules.add_with_args("boot_notifier", vec![arg]);
        }
    }

    if let Some(ratio) = legacy_aspect_ratio {
        let arg = format!("aspect_ratio={ratio}");
        validate_module("framebuffer", Some(&[arg.as_str()]))?;
        if modules.args("framebuffer") == Some(&[]) {
            modules.add_with_args("framebuffer", vec![arg]);
        }
    }

    modules.validate()?;

    if *ui_visual_debugging_level > 0 {
        features.enable_visual_debugging = true;
    }
    features.enable_utc_time_adjustment = *enable_utc_time_adjustment;

    features.kernel.default_uid = start_info.program.default_uid.0;
    features.kernel.default_seclabel = start_info.program.default_seclabel.clone();
    // Soft-transition fallback for out-of-tree container manifests that mount `devtmpfs`
    // before adding `"devtmpfs"` to `program.features`.
    if start_info.program.mounts.iter().chain(additional_mounts.iter()).any(|mount| {
        let mut parts = mount.splitn(3, ':');
        let _mount_point = parts.next();
        parts.next() == Some("devtmpfs")
    }) {
        features.kernel.devtmpfs = true;
    }
    features.kernel.stop_last_on_shutdown =
        start_info.program.stop_last_on_shutdown.iter().map(|path| path.as_str().into()).collect();
    features.kernel.default_ns_mount_options =
        if let Some(mount_options) = &start_info.program.default_ns_mount_options {
            let options = mount_options
                .iter()
                .map(|item| {
                    let mut splitter = item.splitn(2, ":");
                    let key = splitter.next().expect("Failed to parse mount options");
                    let value = splitter.next().expect("Failed to parse mount options");
                    (key.to_string(), value.to_string())
                })
                .collect();
            Some(options)
        } else {
            None
        };

    features.kernel.mlock_always_onfault = *mlock_always_onfault;
    features.kernel.mlock_pin_flavor = MlockPinFlavor::parse(mlock_pin_flavor.as_str())?;
    features.kernel.crash_report_throttling = *crash_report_throttling;
    features.kernel.cached_zx_map_info_bytes = *cached_zx_map_info_bytes;
    features.kernel.dirent_cache_size = *dirent_cache_size;
    features.initial_view_id_annotation = initial_view_id_annotation.clone();
    modules.set_framebuffer_config(
        features.enable_visual_debugging,
        features.initial_view_id_annotation.clone(),
    );

    Ok((features, modules))
}

/// Runs container-level kernel features enabled in `features`.
pub fn run_container_features(kernel: &Arc<Kernel>, features: &Features) -> Result<(), Error> {
    if features.unified_tracing {
        kernel.trace_event_manager.init_trace_observer(kernel);
    }
    if features.kernel.selinux_test_suite {
        register_selinux_test_suite_ramfs(kernel);
    }
    Ok(())
}

/// Runs features requested by individual components inside the container.
pub fn run_component_features(
    kernel: &Kernel,
    entries: &Vec<String>,
    mut incoming_dir: Option<fidl_fuchsia_io::DirectoryProxy>,
) -> Result<(), Errno> {
    for entry in entries {
        match entry.as_str() {
            "framebuffer" => {
                Framebuffer::get(kernel)?.start_server(kernel, incoming_dir.take());
            }
            feature => {
                return error!(ENOSYS, format!("Unsupported feature: {}", feature));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_start_info(features: Vec<String>, modules: Vec<String>) -> ContainerStartInfo {
        ContainerStartInfo::new_for_test(
            features,
            modules,
            ContainerStructuredConfig {
                additional_mounts: Vec::new(),
                cached_zx_map_info_bytes: 0,
                crash_report_throttling: false,
                dirent_cache_size: 0,
                enable_utc_time_adjustment: false,
                extra_features: Vec::new(),
                initial_view_id_annotation: String::new(),
                mlock_always_onfault: false,
                mlock_pin_flavor: "noop".to_string(),
                selinux_exceptions: Vec::new(),
                ui_visual_debugging_level: 0,
            },
        )
    }

    #[fuchsia::test]
    fn parse_program_modules() {
        let start_info = test_start_info(
            vec!["container".to_string()],
            vec![
                "ashmem".to_string(),
                "boot_notifier cpu_boost_secs=60".to_string(),
                "framebuffer aspect_ratio=9:16".to_string(),
                "magma supported_vendors=0x13B5,0x5143,0x8086".to_string(),
                "perfetto socket_path=/dev/socket/traced_consumer".to_string(),
                "thermal cooling=cpu,fcc=8".to_string(),
            ],
        );

        let (features, modules) =
            parse_features(&start_info, &[]).expect("parse_features should succeed");
        assert!(features.container);
        assert!(modules.is_enabled("ashmem"));
        assert_eq!(modules.args("ashmem"), Some(&[][..]));
        assert_eq!(modules.args("boot_notifier"), Some(&["cpu_boost_secs=60".to_string()][..]));
        assert_eq!(modules.args("framebuffer"), Some(&["aspect_ratio=9:16".to_string()][..]));
        assert_eq!(
            modules.args("magma"),
            Some(&["supported_vendors=0x13B5,0x5143,0x8086".to_string()][..])
        );
        assert_eq!(
            modules.args("perfetto"),
            Some(&["socket_path=/dev/socket/traced_consumer".to_string()][..])
        );
        assert_eq!(modules.args("thermal"), Some(&["cooling=cpu,fcc=8".to_string()][..]));
    }

    #[fuchsia::test]
    fn parse_legacy_module_features() {
        let start_info = test_start_info(
            vec![
                "ashmem".to_string(),
                "boot_notifier".to_string(),
                "boot_notifier_cpu_boost:45".to_string(),
                "framebuffer".to_string(),
                "aspect_ratio:16:9".to_string(),
                "magma_supported_vendors:0x13B5,0x8086".to_string(),
                "mmcblk_stub".to_string(),
                "perfetto:/dev/socket/traced_consumer".to_string(),
                "thermal".to_string(),
                "cooling:cpu,fcc=8".to_string(),
            ],
            Vec::new(),
        );

        let (_features, modules) =
            parse_features(&start_info, &[]).expect("legacy features should translate");
        assert!(modules.is_enabled("ashmem"));
        assert!(modules.is_enabled("mmcblk_stub"));
        assert_eq!(modules.args("boot_notifier"), Some(&["cpu_boost_secs=45".to_string()][..]));
        assert_eq!(modules.args("framebuffer"), Some(&["aspect_ratio=16:9".to_string()][..]));
        assert_eq!(
            modules.args("magma"),
            Some(&["supported_vendors=0x13B5,0x8086".to_string()][..])
        );
        assert_eq!(
            modules.args("perfetto"),
            Some(&["socket_path=/dev/socket/traced_consumer".to_string()][..])
        );
        assert_eq!(modules.args("thermal"), Some(&["cooling=cpu,fcc=8".to_string()][..]));
    }

    #[fuchsia::test]
    fn reject_boot_notifier_cpu_boost_without_boot_notifier() {
        let start_info =
            test_start_info(vec!["boot_notifier_cpu_boost:60".to_string()], Vec::new());
        assert!(parse_features(&start_info, &[]).is_err());
    }

    #[fuchsia::test]
    fn reject_mutually_exclusive_odpm_and_hvdcp_opti() {
        let start_info =
            test_start_info(Vec::new(), vec!["google_odpm".to_string(), "hvdcp_opti".to_string()]);
        assert!(parse_features(&start_info, &[]).is_err());

        let mixed_info =
            test_start_info(vec!["google_odpm".to_string()], vec!["hvdcp_opti".to_string()]);
        assert!(parse_features(&mixed_info, &[]).is_err());
    }

    #[fuchsia::test]
    fn hvdcp_opti_supersedes_default_battery_manager_and_rejects_explicit_conflict() {
        let default_info = test_start_info(Vec::new(), Vec::new());
        let (_features, default_modules) =
            parse_features(&default_info, &[]).expect("default modules should parse");
        assert!(default_modules.is_enabled("battery_manager"));
        assert!(default_modules.is_enabled("goog_chip_info"));

        let hvdcp_info = test_start_info(Vec::new(), vec!["hvdcp_opti".to_string()]);
        let (_features, hvdcp_modules) =
            parse_features(&hvdcp_info, &[]).expect("hvdcp_opti should supersede default battery");
        assert!(hvdcp_modules.is_enabled("hvdcp_opti"));
        assert!(!hvdcp_modules.is_enabled("battery_manager"));
        assert_eq!(hvdcp_modules.args("battery_manager"), None);

        let explicit_conflict = test_start_info(
            Vec::new(),
            vec!["battery_manager".to_string(), "hvdcp_opti".to_string()],
        );
        assert!(parse_features(&explicit_conflict, &[]).is_err());
    }

    #[fuchsia::test]
    fn bare_module_does_not_overwrite_module_args() {
        let start_info = test_start_info(
            vec!["boot_notifier".to_string()],
            vec![
                "boot_notifier cpu_boost_secs=60".to_string(),
                "boot_notifier".to_string(),
                "framebuffer aspect_ratio=9:16".to_string(),
                "framebuffer".to_string(),
                "magma supported_vendors=0x13B5".to_string(),
                "magma".to_string(),
                "thermal cooling=cpu,fcc=8".to_string(),
                "thermal".to_string(),
            ],
        );

        let (_features, modules) =
            parse_features(&start_info, &[]).expect("parse_features should succeed");
        assert_eq!(modules.args("boot_notifier"), Some(&["cpu_boost_secs=60".to_string()][..]));
        assert_eq!(modules.args("framebuffer"), Some(&["aspect_ratio=9:16".to_string()][..]));
        assert_eq!(modules.args("magma"), Some(&["supported_vendors=0x13B5".to_string()][..]));
        assert_eq!(modules.args("thermal"), Some(&["cooling=cpu,fcc=8".to_string()][..]));
    }

    #[fuchsia::test]
    fn reject_empty_module_specification() {
        let start_info = test_start_info(Vec::new(), vec!["   ".to_string()]);
        assert!(parse_features(&start_info, &[]).is_err());
    }

    #[fuchsia::test]
    fn disable_default_and_shard_modules() {
        let start_info = test_start_info(
            Vec::new(),
            vec![
                "-zram".to_string(),
                "-gfxstream".to_string(),
                "gfxstream".to_string(),
                "ashmem".to_string(),
                "-ashmem".to_string(),
            ],
        );

        let (_features, modules) =
            parse_features(&start_info, &[]).expect("parse_features should succeed");
        assert!(modules.is_enabled("rtc"));
        assert!(modules.is_enabled("loop"));
        assert!(modules.is_enabled("power_residency"));
        assert!(!modules.is_enabled("zram"));
        assert_eq!(modules.args("zram"), None);
        assert!(!modules.is_enabled("gfxstream"));
        assert!(!modules.is_enabled("ashmem"));
    }

    #[fuchsia::test]
    fn reject_invalid_disable_module_specifications() {
        for invalid_spec in ["-", "--foo", "-zram --extra-arg", "-unknown_module"] {
            let start_info = test_start_info(Vec::new(), vec![invalid_spec.to_string()]);
            assert!(
                parse_features(&start_info, &[]).is_err(),
                "expected '{invalid_spec}' to be rejected"
            );
        }
    }

    #[fuchsia::test]
    fn program_modules_override_legacy_feature_arguments() {
        let start_info = test_start_info(
            vec![
                "boot_notifier".to_string(),
                "boot_notifier_cpu_boost:45".to_string(),
                "framebuffer".to_string(),
                "aspect_ratio:16:9".to_string(),
                "magma_supported_vendors:0x13B5".to_string(),
                "perfetto:/dev/socket/old".to_string(),
                "cooling:cpu".to_string(),
            ],
            vec![
                "boot_notifier cpu_boost_secs=120".to_string(),
                "framebuffer aspect_ratio=9:16".to_string(),
                "magma supported_vendors=0x8086".to_string(),
                "perfetto socket_path=/dev/socket/new".to_string(),
                "thermal cooling=fcc=8".to_string(),
            ],
        );

        let (_features, modules) =
            parse_features(&start_info, &[]).expect("parse_features should succeed");
        assert_eq!(modules.args("boot_notifier"), Some(&["cpu_boost_secs=120".to_string()][..]));
        assert_eq!(modules.args("framebuffer"), Some(&["aspect_ratio=9:16".to_string()][..]));
        assert_eq!(modules.args("magma"), Some(&["supported_vendors=0x8086".to_string()][..]));
        assert_eq!(
            modules.args("perfetto"),
            Some(&["socket_path=/dev/socket/new".to_string()][..])
        );
        assert_eq!(modules.args("thermal"), Some(&["cooling=fcc=8".to_string()][..]));

        let disabled_boot_notifier = test_start_info(
            vec!["boot_notifier".to_string(), "boot_notifier_cpu_boost:45".to_string()],
            vec!["-boot_notifier".to_string()],
        );
        let (_features, modules) = parse_features(&disabled_boot_notifier, &[])
            .expect("disabling legacy boot_notifier via program.modules should succeed");
        assert!(!modules.is_enabled("boot_notifier"));
    }

    #[fuchsia::test]
    fn reject_unknown_modules_and_invalid_module_arguments() {
        for invalid_spec in [
            "unknown_module",
            "ashmem unexpected",
            "ashmem --unexpected",
            "boot_notifier cpu_boost_secs=-1",
            "boot_notifier --cpu-boost-secs=60",
            "boot_notifier cpu-boost-secs=60",
            "boot_notifier unknown_option=1",
            "boot_notifier =60",
            "framebuffer aspect_ratio=0:9",
            "framebuffer aspect_ratio=9:0",
            "framebuffer aspect_ratio=invalid",
            "magma supported_vendors=not_hex",
            "perfetto socket_path=",
            "thermal cooling=",
        ] {
            let start_info = test_start_info(Vec::new(), vec![invalid_spec.to_string()]);
            assert!(
                parse_features(&start_info, &[]).is_err(),
                "expected '{invalid_spec}' to be rejected"
            );
        }
    }

    #[fuchsia::test]
    fn parse_devtmpfs_feature_and_mount_fallback() {
        let default_info = test_start_info(Vec::new(), Vec::new());
        let (features, _modules) =
            parse_features(&default_info, &[]).expect("parse_features should succeed");
        assert!(!features.kernel.devtmpfs);

        let feature_info = test_start_info(vec!["devtmpfs".to_string()], Vec::new());
        let (features, _modules) =
            parse_features(&feature_info, &[]).expect("parse_features should succeed");
        assert!(features.kernel.devtmpfs);

        let mut program_mount_info = test_start_info(Vec::new(), Vec::new());
        program_mount_info.program.mounts.push("/dev:devtmpfs::nosuid,relatime".to_string());
        let (features, _modules) =
            parse_features(&program_mount_info, &[]).expect("parse_features should succeed");
        assert!(features.kernel.devtmpfs);

        let mut additional_mount_info = test_start_info(Vec::new(), Vec::new());
        additional_mount_info
            .config
            .additional_mounts
            .push("/dev:devtmpfs::nosuid,relatime".to_string());
        let (features, _modules) =
            parse_features(&additional_mount_info, &[]).expect("parse_features should succeed");
        assert!(features.kernel.devtmpfs);
    }
}
