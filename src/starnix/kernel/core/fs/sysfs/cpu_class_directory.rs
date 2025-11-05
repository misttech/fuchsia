// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use crate::vfs::FsNodeOps;
use crate::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use crate::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use crate::vfs::pseudo::stub_empty_file::StubEmptyFile;
use itertools::Itertools;
use starnix_logging::bug_ref;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;

pub fn build_cpu_class_directory(dir: &SimpleDirectoryMutator) {
    let cpu_count = zx::system_get_num_cpus();

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
    for i in 0..cpu_count {
        let name = format!("cpu{}", i);
        dir.subdir(&name, 0o755, |dir| {
            dir.entry(
                "cpu_capacity",
                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                mode!(IFREG, 0o444),
            );
            dir.subdir("cpufreq", 0o755, |dir| {
                dir.entry(
                    "cpuinfo_max_freq",
                    StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                    mode!(IFREG, 0o444),
                );
                dir.entry(
                    "scaling_available_frequencies",
                    StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
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
        });
    }
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
