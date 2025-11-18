// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use crate::vfs::FsNodeOps;
use crate::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use crate::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use crate::vfs::pseudo::stub_empty_file::StubEmptyFile;
use anyhow::Error;
use fidl_fuchsia_power_cpu as fcpu;
use fuchsia_component::client::connect_to_protocol_sync;
use itertools::Itertools;
use starnix_logging::{bug_ref, log_warn};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use std::collections::HashMap;

pub fn build_cpu_class_directory(dir: &SimpleDirectoryMutator) {
    let cpu_count = match get_cpu_domains() {
        Ok(domains) => {
            let domain_map: HashMap<u64, &fcpu::DomainInfo> = domains
                .iter()
                .flat_map(|domain| {
                    domain
                        .core_ids
                        .as_ref()
                        .expect("core_ids not available.")
                        .iter()
                        .map(move |id| (*id, domain))
                })
                .collect();

            for (core_id, domain) in domain_map.iter() {
                let name = format!("cpu{}", core_id);
                dir.subdir(&name, 0o755, |dir| build_cpu_directory(dir, domain));
            }

            domain_map.len()
        }
        Err(e) => {
            log_warn!(
                "Could not retrieve CPU domains from fuchsia.power.cpu.DomainController, using kernel CPU count instead: {e:?}"
            );
            zx::system_get_num_cpus() as usize
        }
    };

    dir.entry(
        "online",
        BytesFile::new_node(format!("0-{}\n", cpu_count - 1).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "possible",
        BytesFile::new_node(format!("0-{}\n", cpu_count - 1).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.subdir("vulnerabilities", 0o755, |dir| {
        for (name, contents) in VULNERABILITIES {
            let contents = contents.to_string();
            dir.entry(name, BytesFile::new_node(contents.into_bytes()), mode!(IFREG, 0o444));
        }
    });
    dir.subdir("cpufreq", 0o755, |dir| {
        dir.subdir("policy0", 0o755, |dir| {
            dir.subdir("stats", 0o755, |dir| {
                dir.entry("reset", CpuFreqStatsResetFile::new_node(), mode!(IFREG, 0o200));
            });

            let related_cpus = (0..cpu_count).map(|i| i.to_string()).join(" ") + "\n";
            dir.entry(
                "related_cpus",
                BytesFile::new_node(related_cpus.into_bytes()),
                mode!(IFREG, 0o444),
            );
            dir.entry(
                "scaling_cur_freq",
                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                mode!(IFREG, 0o444),
            );
            dir.entry(
                "scaling_min_freq",
                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                mode!(IFREG, 0o444),
            );
            dir.entry(
                "scaling_max_freq",
                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                mode!(IFREG, 0o444),
            );
            dir.entry(
                "scaling_available_frequencies",
                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                mode!(IFREG, 0o444),
            );
            dir.entry(
                "scaling_available_governors",
                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                mode!(IFREG, 0o444),
            );
            dir.entry(
                "scaling_governor",
                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                mode!(IFREG, 0o444),
            );
        });
    });
    dir.subdir("soc", 0o755, |dir| {
        dir.subdir("0", 0o755, |dir| {
            dir.entry(
                "machine",
                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                mode!(IFREG, 0o444),
            );
        });
    });
}

fn get_cpu_domains() -> Result<Vec<fcpu::DomainInfo>, Error> {
    let domain_controller: fcpu::DomainControllerSynchronousProxy =
        connect_to_protocol_sync::<fcpu::DomainControllerMarker>().map_err(|e| {
            anyhow::anyhow!("Failed to connect to fuchsia.power.cpu.DomainController: {e:?}")
        })?;
    domain_controller
        .list_domains(zx::MonotonicInstant::INFINITE)
        .map_err(|e| anyhow::anyhow!("Failed to get power domains: {e:?}"))
}

fn hz_to_khz(hz: u64) -> u64 {
    return hz / 1000;
}

fn build_cpu_directory(dir: &SimpleDirectoryMutator, domain: &fcpu::DomainInfo) {
    let scaling_available_frequencies =
        domain.available_frequencies_hz.as_ref().expect("available_frequencies_hz not available");
    let cpuinfo_max_freq_khz = hz_to_khz(scaling_available_frequencies[0]);
    let cluster_id = domain.id.as_ref().expect("id not available");

    dir.entry(
        "cpu_capacity",
        StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
        mode!(IFREG, 0o444),
    );
    dir.subdir("cpufreq", 0o755, |dir| {
        dir.entry(
            "cpuinfo_max_freq",
            BytesFile::new_node(format!("{cpuinfo_max_freq_khz}\n").into_bytes()),
            mode!(IFREG, 0o444),
        );

        let frequencies_str =
            scaling_available_frequencies.iter().map(|f| hz_to_khz(*f)).sorted().join(" ");
        dir.entry(
            "scaling_available_frequencies",
            BytesFile::new_node(format!("{frequencies_str}\n").into_bytes()),
            mode!(IFREG, 0o444),
        );
        dir.entry(
            "scaling_boost_frequencies",
            StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
            mode!(IFREG, 0o644),
        );
        dir.subdir("stats", 0o755, |dir| {
            dir.entry(
                "time_in_state",
                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                mode!(IFREG, 0o444),
            );
        });
    });
    dir.subdir("topology", 0o755, |dir| {
        dir.entry(
            "cluster_id",
            BytesFile::new_node(format!("{cluster_id}\n").into_bytes()),
            mode!(IFREG, 0o444),
        );
        dir.entry(
            "physical_package_id",
            BytesFile::new_node(format!("{cluster_id}\n").into_bytes()),
            mode!(IFREG, 0o444),
        );
    });
}

const VULNERABILITIES: &[(&str, &str)] = &[
    ("gather_data_sampling", "Not affected\n"),
    ("itlb_multihit", "Not affected\n"),
    ("l1tf", "Not affected\n"),
    ("mds", "Not affected\n"),
    ("meltdown", "Not affected\n"),
    ("mmio_stale_data", "Not affected\n"),
    ("retbleed", "Not affected\n"),
    ("spec_rstack_overflow", "Not affected\n"),
    ("spec_store_bypass", "Not affected\n"),
    ("spectre_v1", "Not affected\n"),
    ("spectre_v2", "Not affected\n"),
    ("srbds", "Not affected\n"),
    ("tsx_async_abort", "Not affected\n"),
];

struct CpuFreqStatsResetFile {}

impl CpuFreqStatsResetFile {
    pub fn new_node() -> impl FsNodeOps {
        BytesFile::new_node(Self {})
    }
}

impl BytesFileOps for CpuFreqStatsResetFile {
    // Currently a no-op. The value written to this node does not matter.
    fn write(&self, _current_task: &CurrentTask, _data: Vec<u8>) -> Result<(), Errno> {
        Ok(())
    }
}
