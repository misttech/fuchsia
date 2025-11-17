// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use fuchsia_inspect::{HistogramProperty, UintLinearHistogramProperty};
use memory_pinning::ShadowProcess;
use page_buf::PageBuf;
use starnix_sync::Mutex;
use starnix_uapi::errors::Errno;
use starnix_uapi::from_status_like_fdio;
use std::sync::Arc;

/// A singleton cache that can be used to share pinned pages across all map info queries. These
/// queries often generate lock contention in Zircon on Starnix's shared address space when the
/// buffer pages are faulted in. This type holds a long-running allocation in pinned memory to
/// get reduced Zircon VmAspace lock contention in exchange for some extra memory usage and
/// potential lock contention for /proc/pid/status and related files. Luckily it seems that
/// in practice files like /proc/pid/status are not regularly accessed concurrently.
pub struct MapInfoCache {
    buf: Mutex<PageBuf<zx::MapInfo>>,

    _node: fuchsia_inspect::Node,
    spilled_allocation_sizes: UintLinearHistogramProperty,
}

const ZIRCON_NAME: zx::Name = zx::Name::new_lossy("starnix_zx_map_info_cache");

impl MapInfoCache {
    pub fn get_or_init(current_task: &CurrentTask) -> Result<Arc<Self>, Errno> {
        // Keep different shadow processes distinct for accounting purposes.
        struct InfoCacheShadowProcess(memory_pinning::ShadowProcess);

        let kernel = current_task.kernel();
        kernel.expando.get_or_try_init(|| {
            let pinned_shadow_process = kernel.expando.get_or_try_init(|| {
                ShadowProcess::new(ZIRCON_NAME)
                    .map(InfoCacheShadowProcess)
                    .map_err(|e| from_status_like_fdio!(e))
            })?;

            let num_cache_elements = kernel.features.cached_zx_map_info_bytes as usize
                / std::mem::size_of::<zx::MapInfo>();
            Self::new(&pinned_shadow_process.0, num_cache_elements, &kernel.inspect_node)
        })
    }

    fn new(
        shadow_process: &ShadowProcess,
        num_cache_elements: usize,
        parent: &fuchsia_inspect::Node,
    ) -> Result<Self, Errno> {
        let buf = PageBuf::new_with_extra_vmar(num_cache_elements, shadow_process.vmar())
            .map_err(|e| from_status_like_fdio!(e))?;
        buf.set_name(&ZIRCON_NAME);

        let _node = parent.create_child("map_info_cache");
        let len_bytes = buf.len_bytes() as u64;
        _node.record_uint("cache_size_bytes", len_bytes);
        let spilled_allocation_sizes = _node.create_uint_linear_histogram(
            "spilled_allocation_sizes_bytes",
            fuchsia_inspect::LinearHistogramParams {
                floor: len_bytes,
                step_size: 100 * 1024, // 100 KiB
                buckets: 8,            // 800 KiB
            },
        );

        Ok(Self { buf: Mutex::new(buf), _node, spilled_allocation_sizes })
    }

    pub fn with_map_infos<R>(
        &self,
        vmar: &zx::Vmar,
        op: impl FnOnce(Result<&[zx::MapInfo], zx::Status>) -> R,
    ) -> R {
        let mut buf = self.buf.lock();
        match vmar.info_maps(buf.as_mut()) {
            Ok((maps, _, avail)) if maps.len() == avail => op(Ok(maps)),
            Ok((_, _, avail)) => {
                // The buffer was not large enough, fall back to a heap allocation.
                let spilled_bytes = avail * std::mem::size_of::<zx::MapInfo>();
                self.spilled_allocation_sizes.insert(spilled_bytes as u64);
                match vmar.info_maps_vec() {
                    Ok(maps) => op(Ok(&maps)),
                    Err(e) => op(Err(e)),
                }
            }
            Err(e) => op(Err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::{AnyProperty, assert_data_tree};

    #[fuchsia::test]
    async fn basic_test() {
        let inspector = fuchsia_inspect::Inspector::default();
        let shadow_process = ShadowProcess::new(zx::Name::new_lossy("testing123")).unwrap();
        let cache = MapInfoCache::new(&shadow_process, 100, inspector.root()).unwrap();
        let maps_base_addr = cache.with_map_infos(&shadow_process.vmar(), |maps| {
            let maps = maps.unwrap();
            assert_ne!(maps, &[]);
            maps.as_ptr() as usize
        });
        let cache_base_addr = cache.buf.lock().as_mut().as_ptr() as usize;
        assert_eq!(maps_base_addr, cache_base_addr, "map infos must have been read into cache");
        assert_data_tree!(inspector, root: {
            map_info_cache: {
                cache_size_bytes: 16384u64,
                spilled_allocation_sizes_bytes: AnyProperty,
            }
        });
    }

    #[fuchsia::test]
    async fn fall_back_to_heap_over_limit() {
        let inspector = fuchsia_inspect::Inspector::default();
        let shadow_process = ShadowProcess::new(zx::Name::new_lossy("testing123")).unwrap();
        let cache = MapInfoCache::new(&shadow_process, 1, inspector.root()).unwrap();

        // Ensure that the test process' root VMAR has more mappings than can fit in the single page
        // of the cache's buffer.
        let number_of_extra_mappings =
            (zx::system_get_page_size() as usize / std::mem::size_of::<zx::MapInfo>()) * 2;
        let vmo_to_map = zx::Vmo::create(4096).unwrap();
        for _ in 0..number_of_extra_mappings {
            fuchsia_runtime::vmar_root_self()
                .map(0, &vmo_to_map, 0, 4096, zx::VmarFlags::PERM_READ)
                .unwrap();
        }

        let maps_base_addr = cache.with_map_infos(&fuchsia_runtime::vmar_root_self(), |maps| {
            let maps = maps.unwrap();
            assert!(!maps.is_empty());
            maps.as_ptr() as usize
        });

        let cache_base_addr = cache.buf.lock().as_mut().as_ptr() as usize;
        assert_ne!(maps_base_addr, cache_base_addr, "map infos must not have been read into cache");
    }
}
