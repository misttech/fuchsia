// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_core::mm::PAGE_SIZE;
use starnix_core::task::{CurrentTask, KernelStats, OomWatermarksMb};
use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::pseudo::dynamic_file::{DynamicFile, DynamicFileBuf, DynamicFileSource};
use starnix_logging::log_error;
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use std::sync::Arc;

const BYTES_PER_MB: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct ZoneInfoFile {
    kernel_stats: Arc<KernelStats>,
    oom_watermarks_mb: Option<OomWatermarksMb>,
}

impl ZoneInfoFile {
    pub fn new_node(
        kernel_stats: &Arc<KernelStats>,
        oom_watermarks_mb: Option<OomWatermarksMb>,
    ) -> impl FsNodeOps {
        DynamicFile::new_node(Self { kernel_stats: kernel_stats.clone(), oom_watermarks_mb })
    }
}

impl DynamicFileSource for ZoneInfoFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let mem_stats = self
            .kernel_stats
            .get()
            .get_memory_stats_extended(zx::MonotonicInstant::INFINITE)
            .map_err(|e| {
                log_error!("FIDL error getting memory stats: {e}");
                errno!(EIO)
            })?;

        let userpager_total = mem_stats.vmo_pager_total_bytes.unwrap_or_default() / *PAGE_SIZE;
        let userpager_active = mem_stats.vmo_pager_newest_bytes.unwrap_or_default() / *PAGE_SIZE;

        let nr_active_file = userpager_active;
        let nr_inactive_file = userpager_total.saturating_sub(userpager_active);
        let free = mem_stats.free_bytes.unwrap_or_default() / *PAGE_SIZE;
        let present = mem_stats.total_bytes.unwrap_or_default() / *PAGE_SIZE;

        // Convert Fuchsia's out-of-memory, critical, and warning free-memory watermarks
        // (in MiB, see `OomWatermarksMb`) into page counts.
        let (pages_min, pages_low, pages_high) = match self.oom_watermarks_mb {
            Some(watermarks) => (
                (u64::from(watermarks.out_of_memory_mb) * BYTES_PER_MB) / *PAGE_SIZE,
                (u64::from(watermarks.critical_mb) * BYTES_PER_MB) / *PAGE_SIZE,
                (u64::from(watermarks.warning_mb) * BYTES_PER_MB) / *PAGE_SIZE,
            ),
            None => {
                log_error!("OOM watermarks not configured for /proc/zoneinfo");
                (0, 0, 0)
            }
        };

        // Only required fields are written. Add more fields as needed.
        writeln!(sink, "Node 0, zone   Normal")?;
        writeln!(sink, "  per-node stats")?;
        writeln!(sink, "      nr_inactive_file {}", nr_inactive_file)?;
        writeln!(sink, "      nr_active_file {}", nr_active_file)?;
        writeln!(sink, "  pages free     {}", free)?;
        writeln!(sink, "        min      {}", pages_min)?;
        writeln!(sink, "        low      {}", pages_low)?;
        writeln!(sink, "        high     {}", pages_high)?;
        writeln!(sink, "        present  {}", present)?;
        writeln!(sink, "  pagesets")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starnix_core::task::KernelFeatures;
    use starnix_core::testing::spawn_kernel_with_features_and_run;
    use starnix_core::vfs::buffers::VecOutputBuffer;
    use starnix_core::vfs::{FileSystemOptions, LookupContext, Namespace, OpenAccessCheck};
    use starnix_uapi::open_flags::OpenFlags;
    use std::collections::HashMap;

    fn read_and_parse_zoneinfo(current_task: &CurrentTask) -> HashMap<String, u64> {
        let proc_fs =
            crate::proc_fs(current_task, FileSystemOptions::default()).expect("create procfs");
        let ns = Namespace::new(proc_fs);
        let zoneinfo_node = ns
            .root()
            .lookup_child(current_task, &mut LookupContext::default(), "zoneinfo".into())
            .expect("lookup zoneinfo");
        let file = zoneinfo_node
            .open(current_task, OpenAccessCheck::skip(OpenFlags::RDONLY))
            .expect("open zoneinfo");

        let mut buffer = VecOutputBuffer::new(*PAGE_SIZE as usize);
        let bytes_read = file.read(current_task, &mut buffer).expect("read zoneinfo");
        let content = std::str::from_utf8(&buffer.data()[..bytes_read]).expect("valid utf8");

        let mut fields = HashMap::new();
        for line in content.lines() {
            let mut parts = line.split_whitespace();
            if let (Some(key), Some(val_str), None) = (parts.next(), parts.next(), parts.next()) {
                if let Ok(val) = val_str.parse::<u64>() {
                    fields.insert(key.to_string(), val);
                }
            }
        }
        fields
    }

    #[fuchsia::test]
    async fn test_zoneinfo_custom_watermarks() {
        let watermarks =
            OomWatermarksMb { out_of_memory_mb: 80, critical_mb: 200, warning_mb: 400 };
        spawn_kernel_with_features_and_run(
            async move |current_task| {
                let fields = read_and_parse_zoneinfo(current_task);
                assert_eq!(fields.get("min").copied(), Some((80 * BYTES_PER_MB) / *PAGE_SIZE));
                assert_eq!(fields.get("low").copied(), Some((200 * BYTES_PER_MB) / *PAGE_SIZE));
                assert_eq!(fields.get("high").copied(), Some((400 * BYTES_PER_MB) / *PAGE_SIZE));
            },
            KernelFeatures { oom_watermarks_mb: Some(watermarks), ..Default::default() },
        )
        .await;
    }
}
