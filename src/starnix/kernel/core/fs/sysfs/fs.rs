// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::sysfs::{build_kernel_directory, build_power_directory};
use crate::task::{CurrentTask, Kernel};
use crate::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use crate::vfs::pseudo::simple_file::BytesFile;
use crate::vfs::pseudo::stub_empty_file::StubEmptyFile;
use crate::vfs::{
    CacheMode, DirEntry, FileSystem, FileSystemHandle, FileSystemOps, FileSystemOptions, FsStr,
};
use starnix_logging::{Level, bug_ref, track_stub_log};
use starnix_types::vfs::default_statfs;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::{SYSFS_MAGIC, errno, statfs};

struct SysFs;
impl FileSystemOps for SysFs {
    fn statfs(&self, _fs: &FileSystem, _current_task: &CurrentTask) -> Result<statfs, Errno> {
        Ok(default_statfs(SYSFS_MAGIC))
    }
    fn name(&self) -> &'static FsStr {
        "sysfs".into()
    }
}

/// Reports lookups of missing `/sys` entries via `track_stub_log!`, to surface sysfs nodes that
/// userspace expects but which Starnix does not (yet) provide.
///
/// Each distinct missing path is logged once, and counted in the kernel's Inspect `stubs` node.
fn sysfs_not_found_handler(entry: &DirEntry, name: &FsStr) -> Errno {
    let message = format!("Looking for {name} in {entry:?}");
    track_stub_log!(Level::Warn, TODO("https://fxbug.dev/493488790"), &message);
    errno!(ENOENT, message)
}

impl SysFs {
    fn new_fs(kernel: &Kernel, options: FileSystemOptions) -> FileSystemHandle {
        let fs =
            FileSystem::new(kernel, CacheMode::Cached(kernel.fs_cache_config()), SysFs, options)
                .expect("sysfs constructed with valid options");

        fn empty_dir(_: &SimpleDirectoryMutator) {}

        let registry = &kernel.device_registry;
        let root = &registry.objects.root;
        fs.create_root(fs.allocate_ino(), root.clone());
        if kernel.features.log_sysfs_lookup_misses {
            root.set_not_found_handler(sysfs_not_found_handler);
        }
        let dir = SimpleDirectoryMutator::new(fs.clone(), root.clone());

        let dir_mode = 0o755;
        dir.subdir("fs", dir_mode, |dir| {
            dir.subdir("selinux", dir_mode, empty_dir);
            dir.subdir("bpf", dir_mode, empty_dir);
            dir.subdir("cgroup", dir_mode, empty_dir);
            dir.subdir("fuse", dir_mode, |dir| {
                dir.subdir("connections", dir_mode, empty_dir);
            });
            dir.subdir("pstore", dir_mode, empty_dir);
        });

        dir.subdir("block", dir_mode, empty_dir);

        dir.subdir("bus", dir_mode, |dir| {
            dir.subdir("platform", dir_mode, |dir| {
                dir.subdir("drivers", dir_mode, |dir| {
                    dir.entry(
                        "trusty",
                        StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                        mode!(IFREG, 0o444),
                    );
                });
            });
        });

        dir.subdir("class", dir_mode, |dir| {
            dir.subdir("net", dir_mode, empty_dir);
        });
        dir.subdir("dev", dir_mode, |dir| {
            dir.subdir("char", dir_mode, empty_dir);
            dir.subdir("block", dir_mode, empty_dir);
        });
        dir.subdir("firmware", dir_mode, |dir| {
            dir.subdir("devicetree", dir_mode, |dir| {
                dir.subdir("base", dir_mode, |dir| {
                    dir.subdir("chosen", dir_mode, |dir| {
                        dir.subdir("plat", dir_mode, |dir| {
                            if let Some(device_tree) = &kernel.device_tree {
                                if let Some(product_bytes) =
                                    device_tree.root_node.find("plat").and_then(|n| {
                                        n.get_property("product").map(|p| p.value.clone())
                                    })
                                {
                                    let product_bytes = if product_bytes.len() >= 4 {
                                        product_bytes.to_vec()
                                    } else {
                                        let mut padded_bytes = vec![0; 4];
                                        let start = 4 - product_bytes.len();
                                        padded_bytes[start..].copy_from_slice(&product_bytes);
                                        padded_bytes
                                    };
                                    dir.entry(
                                        "product",
                                        BytesFile::new_node(product_bytes),
                                        mode!(IFREG, 0o444),
                                    );
                                }
                            }
                        });
                        dir.subdir("config", dir_mode, |dir| {
                            dir.entry(
                                "pcbcfg",
                                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                                mode!(IFREG, 0o444),
                            );
                        });
                    });
                    dir.subdir("firmware", dir_mode, |dir| {
                        dir.subdir("android", 0o755, |dir| {
                            dir.entry(
                                "compatible",
                                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                                mode!(IFREG, 0o444),
                            );
                            dir.subdir("vbmeta", 0o755, |dir| {
                                dir.entry(
                                    "parts",
                                    StubEmptyFile::new_node(bug_ref!(
                                        "https://fxbug.dev/452096300"
                                    )),
                                    mode!(IFREG, 0o444),
                                );
                            });
                        });
                    });
                    dir.subdir("mcu", dir_mode, |dir| {
                        dir.entry(
                            "board_type",
                            BytesFile::new_node(b"starnix".to_vec()),
                            mode!(IFREG, 0o444),
                        );
                    });
                });
            });
        });

        dir.subdir("kernel", dir_mode, |dir| {
            build_kernel_directory(kernel, dir);
        });

        dir.subdir("power", 0o755, |dir| {
            build_power_directory(kernel, dir);
        });

        dir.subdir("leds", dir_mode, |dir| {
            dir.entry(
                "leds",
                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                mode!(IFREG, 0o444),
            );
        });

        dir.subdir("module", dir_mode, |dir| {
            dir.subdir("dm_bufio", dir_mode, |dir| {
                dir.subdir("parameters", dir_mode, |dir| {
                    dir.entry(
                        "max_age_seconds",
                        StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                        mode!(IFREG, 0o644),
                    );
                });
            });
            dir.subdir("dm_verity", dir_mode, |dir| {
                dir.subdir("parameters", dir_mode, |dir| {
                    dir.entry(
                        "prefetch_cluster",
                        StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/322893670")),
                        mode!(IFREG, 0o644),
                    );
                });
            });
        });

        dir.subdir("devices", dir_mode, |dir| {
            dir.subdir("leds", dir_mode, |_dir| {});
            dir.subdir("virtual", dir_mode, |dir| {
                dir.subdir("leds", dir_mode, |_dir| {});
                dir.subdir("power_supply", dir_mode, |dir| {
                    dir.subdir("bms", dir_mode, |dir| {
                        dir.entry(
                            "capacity",
                            StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                            mode!(IFREG, 0o444),
                        );
                        dir.entry(
                            "capacity_level",
                            StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                            mode!(IFREG, 0o444),
                        );
                        dir.entry(
                            "status",
                            StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                            mode!(IFREG, 0o444),
                        );
                    });
                });
            });
        });

        fs
    }
}

struct SysFsHandle(FileSystemHandle);

pub fn sys_fs(
    current_task: &CurrentTask,
    _options: FileSystemOptions,
) -> Result<FileSystemHandle, Errno> {
    Ok(get_sysfs(current_task.kernel()))
}

pub fn get_sysfs(kernel: &Kernel) -> FileSystemHandle {
    kernel
        .expando
        .get_or_init(|| SysFsHandle(SysFs::new_fs(kernel, FileSystemOptions::default())))
        .0
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::KernelFeatures;
    use crate::testing::{
        anon_test_file, spawn_kernel_and_run, spawn_kernel_with_features_and_run,
    };
    use crate::vfs::{MountInfo, VecInputBuffer, VecOutputBuffer};
    use starnix_uapi::open_flags::OpenFlags;
    use std::sync::Arc;

    fn read_sysfs_file(current_task: &CurrentTask, path: &str) -> Vec<u8> {
        let root = &current_task.kernel().device_registry.objects.root;
        let node = root.lookup(path.into()).unwrap_or_else(|| panic!("{path} should exist"));
        assert_eq!(node.info().mode, mode!(IFREG, 0o444), "{path} mode");
        let file_ops = node.create_file_ops(current_task, OpenFlags::RDONLY).unwrap();
        let file = anon_test_file(current_task, file_ops, OpenFlags::RDONLY);
        let mut buf = VecOutputBuffer::new(16);
        let n = file.read(current_task, &mut buf).unwrap();
        Vec::from(buf)[..n].to_vec()
    }

    #[::fuchsia::test]
    async fn sysfs_registers_platform_and_soc_devices() {
        spawn_kernel_and_run(async |current_task| {
            let objects = &current_task.kernel().device_registry.objects;
            let root = &objects.root;

            let soc_uevent = root
                .lookup("devices/platform/soc/uevent".into())
                .expect("soc uevent node should exist");
            // Calling soc_device() again should return the cached Device without replacing its
            // sysfs entries.
            let _soc = objects.soc_device();
            let soc_uevent_after = root
                .lookup("devices/platform/soc/uevent".into())
                .expect("soc uevent node should still exist");
            assert!(Arc::ptr_eq(&soc_uevent, &soc_uevent_after));

            assert!(root.lookup("devices/platform/uevent".into()).is_some());
            assert!(root.lookup("bus/platform/devices/soc".into()).is_some());
            assert!(root.lookup("bus/platform/devices/1c40000.qcom,spmi".into()).is_some());
            assert!(root.lookup("bus/spmi/devices/spmi-0".into()).is_some());
            assert!(root.lookup("bus/spmi/devices/0-00".into()).is_some());
            assert!(root.lookup("bus/iio/devices/iio:device3".into()).is_some());
            assert!(
                root.lookup(
                    "devices/platform/soc/1c40000.qcom,spmi/spmi-0/0-00/1c40000.qcom,spmi:qcom,pm5100@0:qpnp,qbg@4f00/iio:device3/in_resistance_resistance_id_input"
                        .into(),
                )
                .is_some()
            );
            assert!(root.lookup("class/drm/card0".into()).is_some());
            assert!(root.lookup("class/drm/sde-conn-0-DSI-1".into()).is_some());
            assert!(
                root.lookup(
                    "devices/platform/soc/5e00000.qcom,mdss_mdp/drm/card0/sde-conn-0-DSI-1/display_power_state"
                        .into(),
                )
                .is_some()
            );
            assert!(root.lookup("class/drm/card0-DSI-1".into()).is_some());
            assert_eq!(
                read_sysfs_file(
                    current_task,
                    "devices/platform/soc/5e00000.qcom,mdss_mdp/drm/card0/card0-DSI-1/panel/panel_name",
                ),
                b"Placeholder",
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_log_sysfs_lookup_misses() {
        for (log_sysfs_lookup_misses, name) in
            [(false, "missing_when_disabled"), (true, "missing_when_enabled")]
        {
            spawn_kernel_with_features_and_run(
                async move |current_task| {
                    let sysfs = get_sysfs(current_task.kernel());
                    let res = sysfs.root().component_lookup(
                        current_task,
                        &MountInfo::detached(),
                        name.into(),
                    );
                    assert_eq!(res.unwrap_err(), errno!(ENOENT));
                },
                KernelFeatures { log_sysfs_lookup_misses, ..Default::default() },
            )
            .await;

            let inspector =
                starnix_logging::track_stub_lazy_node_callback().await.expect("lazy node callback");
            let hierarchy = fuchsia_inspect::reader::read(&inspector).await.expect("read inspect");
            let expected =
                format!(r#"Looking for {name} in DirEntry {{ fs: "sysfs", path: "/" }}"#);
            assert_eq!(hierarchy.get_child(&expected).is_some(), log_sysfs_lookup_misses);
        }
    }

    #[::fuchsia::test]
    async fn sysfs_registers_stub_devices() {
        spawn_kernel_and_run(async |current_task| {
            let root = &current_task.kernel().device_registry.objects.root;

            // Backlight, parented by the display controller.
            assert!(root.lookup("class/backlight/panel0-backlight".into()).is_some());
            assert!(
                root.lookup(
                    "devices/platform/soc/5e00000.qcom,mdss_mdp/backlight/panel0-backlight/brightness"
                        .into(),
                )
                .is_some()
            );

            // eMMC host, card and block stub share a single device directory tree.
            assert!(root.lookup("class/mmc_host/mmc0".into()).is_some());
            assert!(root.lookup("bus/mmc/devices/mmc0:0001".into()).is_some());
            assert!(root.lookup("devices/virtual/mmc_host/mmc0/uevent".into()).is_some());
            assert!(
                root.lookup("devices/virtual/mmc_host/mmc0/mmc0:0001/life_time".into()).is_some()
            );
            assert!(
                root.lookup("devices/virtual/mmc_host/mmc0/mmc0:0001/block/mmcblk0/size".into())
                    .is_some()
            );

            // SoC identification device.
            assert!(root.lookup("bus/soc/devices/soc0".into()).is_some());
            assert!(root.lookup("devices/soc0/revision".into()).is_some());
            assert!(root.lookup("devices/soc0/uevent".into()).is_some());

            // Backing device info.
            assert!(root.lookup("class/bdi/0:80".into()).is_some());
            assert!(root.lookup("devices/virtual/bdi/0:80/read_ahead_kb".into()).is_some());

            // Battery power supply.
            assert!(root.lookup("class/power_supply/battery".into()).is_some());
            let battery_capacity = root
                .lookup("devices/virtual/power_supply/battery/capacity".into())
                .expect("battery capacity node should exist");
            assert_eq!(battery_capacity.info().mode, mode!(IFREG, 0o644));
            let file_ops =
                battery_capacity.create_file_ops(current_task, OpenFlags::RDWR).unwrap();
            let file = anon_test_file(current_task, file_ops, OpenFlags::RDWR);
            let mut input = VecInputBuffer::new(b"77\n");
            assert_eq!(file.write(current_task, &mut input).unwrap(), 3);

            // Google SoC chip info.
            assert!(root.lookup("devices/system/goog-chip-info/uevent".into()).is_some());
            for name in ["device_table", "dvfs_table", "ids_table"] {
                let path = format!("devices/system/goog-chip-info/{name}");
                assert_eq!(read_sysfs_file(current_task, &path), b"Placeholder");
            }

            // Device-less classes.
            assert!(root.lookup("class/powercap".into()).is_some());
            assert!(root.lookup("class/udc".into()).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn sysfs_registers_cpu_devices() {
        spawn_kernel_and_run(async |current_task| {
            let root = &current_task.kernel().device_registry.objects.root;

            assert!(root.lookup("devices/system/cpu/uevent".into()).is_some());
            assert!(root.lookup("devices/system/cpu/online".into()).is_some());
            assert!(root.lookup("devices/system/cpu/possible".into()).is_some());
            assert!(root.lookup("devices/system/cpu/cpufreq".into()).is_some());
            assert!(root.lookup("devices/system/cpu/cpu0/uevent".into()).is_some());
            assert!(root.lookup("devices/system/cpu/cpu0/topology".into()).is_some());
            assert!(root.lookup("bus/cpu/devices/cpu0".into()).is_some());
        })
        .await;
    }
}
