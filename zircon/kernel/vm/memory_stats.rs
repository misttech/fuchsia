// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::platform_rs::timer::DurationMono;
use crate::stall::StallAggregator;
use crate::vm::compression::VmCompression;
use crate::vm::discardable_vmo_tracker::{DiscardablePageCounts, DiscardableVmoTracker};
use crate::vm::evictor::{Evictor, EvictorStats};
use crate::vm::page_queues::{Counts, PageQueues, ReclaimCounts};
use crate::vm::page_state::VmPageState;
use crate::vm::{heap, page as vm_page, pmm};
use page::{SHIFT as PAGE_SHIFT, SIZE as PAGE_SIZE};
use zx_types::{
    InfoMapsTypeUnionV1, InfoMapsTypeUnionV2, zx_info_kmem_stats_compression_t,
    zx_info_kmem_stats_extended_t, zx_info_kmem_stats_t, zx_info_kmem_stats_v1_t,
    zx_info_maps_mapping_v1_t, zx_info_maps_mapping_v2_t, zx_info_maps_t, zx_info_maps_v1_t,
    zx_info_maps_v2_t, zx_info_memory_stall_t, zx_info_vmo_t, zx_info_vmo_v1_t, zx_info_vmo_v2_t,
    zx_info_vmo_v3_t,
};

mod sealed {
    pub trait Sealed {}
}

/// Trait for converting a [`zx_info_vmo_t`] to a versioned VMO info record.
pub trait VmoInfoVersion: sealed::Sealed {
    /// Converts `vmo` to this versioned VMO info type.
    fn from_vmo_info(vmo: &zx_info_vmo_t) -> Self;
}

impl sealed::Sealed for zx_info_vmo_t {}
impl VmoInfoVersion for zx_info_vmo_t {
    #[inline]
    fn from_vmo_info(vmo: &zx_info_vmo_t) -> Self {
        *vmo
    }
}

impl sealed::Sealed for zx_info_vmo_v1_t {}
impl VmoInfoVersion for zx_info_vmo_v1_t {
    #[inline]
    fn from_vmo_info(vmo: &zx_info_vmo_t) -> Self {
        zx_info_vmo_v1_t {
            koid: vmo.koid,
            name: vmo.name,
            size_bytes: vmo.size_bytes,
            parent_koid: vmo.parent_koid,
            num_children: vmo.num_children,
            num_mappings: vmo.num_mappings,
            share_count: vmo.share_count,
            flags: vmo.flags,
            committed_bytes: vmo.committed_bytes,
            handle_rights: vmo.handle_rights,
            cache_policy: vmo.cache_policy,
            ..Default::default()
        }
    }
}

impl sealed::Sealed for zx_info_vmo_v2_t {}
impl VmoInfoVersion for zx_info_vmo_v2_t {
    #[inline]
    fn from_vmo_info(vmo: &zx_info_vmo_t) -> Self {
        zx_info_vmo_v2_t {
            koid: vmo.koid,
            name: vmo.name,
            size_bytes: vmo.size_bytes,
            parent_koid: vmo.parent_koid,
            num_children: vmo.num_children,
            num_mappings: vmo.num_mappings,
            share_count: vmo.share_count,
            flags: vmo.flags,
            committed_bytes: vmo.committed_bytes,
            handle_rights: vmo.handle_rights,
            cache_policy: vmo.cache_policy,
            metadata_bytes: vmo.metadata_bytes,
            committed_change_events: vmo.committed_change_events,
            ..Default::default()
        }
    }
}

impl sealed::Sealed for zx_info_vmo_v3_t {}
impl VmoInfoVersion for zx_info_vmo_v3_t {
    #[inline]
    fn from_vmo_info(vmo: &zx_info_vmo_t) -> Self {
        zx_info_vmo_v3_t {
            koid: vmo.koid,
            name: vmo.name,
            size_bytes: vmo.size_bytes,
            parent_koid: vmo.parent_koid,
            num_children: vmo.num_children,
            num_mappings: vmo.num_mappings,
            share_count: vmo.share_count,
            flags: vmo.flags,
            committed_bytes: vmo.committed_bytes,
            handle_rights: vmo.handle_rights,
            cache_policy: vmo.cache_policy,
            metadata_bytes: vmo.metadata_bytes,
            committed_change_events: vmo.committed_change_events,
            populated_bytes: vmo.populated_bytes,
            ..Default::default()
        }
    }
}

/// Converts a [`zx_info_vmo_t`] to the requested versioned VMO info record `T`.
#[inline]
pub fn vmo_info_to_version<T: VmoInfoVersion>(vmo: &zx_info_vmo_t) -> T {
    T::from_vmo_info(vmo)
}

/// Trait for converting a [`zx_info_maps_t`] to a versioned maps info record.
pub trait MapsInfoVersion: sealed::Sealed {
    /// Converts `maps` to this versioned maps info type.
    fn from_maps_info(maps: &zx_info_maps_t) -> Self;
}

impl sealed::Sealed for zx_info_maps_t {}
impl MapsInfoVersion for zx_info_maps_t {
    #[inline]
    fn from_maps_info(maps: &zx_info_maps_t) -> Self {
        *maps
    }
}

impl sealed::Sealed for zx_info_maps_v1_t {}
impl MapsInfoVersion for zx_info_maps_v1_t {
    #[inline]
    fn from_maps_info(maps: &zx_info_maps_t) -> Self {
        // SAFETY: InfoMapsTypeUnion has a single FromBytes variant (zx_info_maps_mapping_t) and is
        // always initialized.
        let mapping = unsafe { maps.u.mapping };
        zx_info_maps_v1_t {
            name: maps.name,
            base: maps.base,
            size: maps.size,
            depth: maps.depth,
            r#type: maps.r#type,
            padding1: Default::default(),
            u: InfoMapsTypeUnionV1 {
                mapping: zx_info_maps_mapping_v1_t {
                    mmu_flags: mapping.mmu_flags,
                    padding1: Default::default(),
                    vmo_koid: mapping.vmo_koid,
                    vmo_offset: mapping.vmo_offset,
                    committed_pages: mapping.committed_bytes >> PAGE_SHIFT,
                },
            },
        }
    }
}

impl sealed::Sealed for zx_info_maps_v2_t {}
impl MapsInfoVersion for zx_info_maps_v2_t {
    #[inline]
    fn from_maps_info(maps: &zx_info_maps_t) -> Self {
        // SAFETY: InfoMapsTypeUnion has a single FromBytes variant (zx_info_maps_mapping_t) and is
        // always initialized.
        let mapping = unsafe { maps.u.mapping };
        zx_info_maps_v2_t {
            name: maps.name,
            base: maps.base,
            size: maps.size,
            depth: maps.depth,
            r#type: maps.r#type,
            padding1: Default::default(),
            u: InfoMapsTypeUnionV2 {
                mapping: zx_info_maps_mapping_v2_t {
                    mmu_flags: mapping.mmu_flags,
                    padding1: Default::default(),
                    vmo_koid: mapping.vmo_koid,
                    vmo_offset: mapping.vmo_offset,
                    committed_pages: mapping.committed_bytes >> PAGE_SHIFT,
                    populated_pages: mapping.populated_bytes >> PAGE_SHIFT,
                },
            },
        }
    }
}

/// Converts a [`zx_info_maps_t`] to the requested versioned maps info record `T`.
#[inline]
pub fn maps_info_to_version<T: MapsInfoVersion>(maps: &zx_info_maps_t) -> T {
    T::from_maps_info(maps)
}

/// Trait for converting a [`zx_info_kmem_stats_t`] to a versioned kernel memory stats record.
pub trait KernelStatsInfoVersion: sealed::Sealed {
    /// Converts `stats` to this versioned kernel memory stats type.
    fn from_kmem_stats_info(stats: &zx_info_kmem_stats_t) -> Self;
}

impl sealed::Sealed for zx_info_kmem_stats_t {}
impl KernelStatsInfoVersion for zx_info_kmem_stats_t {
    #[inline]
    fn from_kmem_stats_info(stats: &zx_info_kmem_stats_t) -> Self {
        *stats
    }
}

impl sealed::Sealed for zx_info_kmem_stats_v1_t {}
impl KernelStatsInfoVersion for zx_info_kmem_stats_v1_t {
    #[inline]
    fn from_kmem_stats_info(stats: &zx_info_kmem_stats_t) -> Self {
        zx_info_kmem_stats_v1_t {
            total_bytes: stats.total_bytes,
            free_bytes: stats.free_bytes + stats.free_loaned_bytes,
            wired_bytes: stats.wired_bytes,
            total_heap_bytes: stats.total_heap_bytes,
            free_heap_bytes: stats.free_heap_bytes,
            vmo_bytes: stats.vmo_bytes,
            mmu_overhead_bytes: stats.mmu_overhead_bytes,
            ipc_bytes: stats.ipc_bytes,
            other_bytes: stats.other_bytes,
        }
    }
}

impl sealed::Sealed for zx_info_kmem_stats_extended_t {}
impl KernelStatsInfoVersion for zx_info_kmem_stats_extended_t {
    #[inline]
    fn from_kmem_stats_info(stats: &zx_info_kmem_stats_t) -> Self {
        zx_info_kmem_stats_extended_t {
            total_bytes: stats.total_bytes,
            free_bytes: stats.free_bytes + stats.free_loaned_bytes,
            wired_bytes: stats.wired_bytes,
            total_heap_bytes: stats.total_heap_bytes,
            free_heap_bytes: stats.free_heap_bytes,
            vmo_bytes: stats.vmo_bytes,
            mmu_overhead_bytes: stats.mmu_overhead_bytes,
            ipc_bytes: stats.ipc_bytes,
            other_bytes: stats.other_bytes,
            vmo_pager_total_bytes: stats.vmo_reclaim_total_bytes,
            vmo_pager_newest_bytes: stats.vmo_reclaim_newest_bytes,
            vmo_pager_oldest_bytes: stats.vmo_reclaim_oldest_bytes,
            vmo_discardable_locked_bytes: stats.vmo_discardable_locked_bytes,
            vmo_discardable_unlocked_bytes: stats.vmo_discardable_unlocked_bytes,
            vmo_reclaim_disable_bytes: stats.vmo_reclaim_disabled_bytes,
        }
    }
}

/// Converts a [`zx_info_kmem_stats_t`] to the requested versioned kernel memory stats record `T`.
#[inline]
pub fn kernel_stats_info_to_version<T: KernelStatsInfoVersion>(stats: &zx_info_kmem_stats_t) -> T {
    T::from_kmem_stats_info(stats)
}

// TODO: figure out a better handle to hang this off to and push this copy code into
// that dispatcher.
/// Returns system-wide kernel memory statistics.
pub fn get_memory_stats() -> zx_info_kmem_stats_t {
    // `get_count` returns an estimate so the sum of the counts may not equal the total.
    let mut state_count = [0u64; VmPageState::COUNT];
    for state in VmPageState::ALL {
        state_count[state.index()] = vm_page::get_count(state);
    }

    let (_total_heap_bytes, free_heap_bytes) = heap::get_info();
    let free_heap_bytes = free_heap_bytes as u64;

    // Note that this intentionally uses u64 instead of
    // usize in case we ever have a 32-bit userspace but more
    // than 4GB physical memory.
    let mut stats =
        zx_info_kmem_stats_t { total_bytes: pmm::count_total_bytes(), ..Default::default() };

    // Holds the sum of bytes in the broken out states. This sum could be less than the total
    // because we aren't counting all possible states (e.g. VmPageState::ALLOC). This sum could
    // be greater than the total because per-state counts are approximate.
    let mut sum_bytes: u64 = 0;

    let page_size = PAGE_SIZE as u64;

    stats.free_bytes = state_count[VmPageState::FREE.index()] * page_size;
    sum_bytes += stats.free_bytes;

    stats.free_loaned_bytes = state_count[VmPageState::FREE_LOANED.index()] * page_size;
    sum_bytes += stats.free_loaned_bytes;

    stats.wired_bytes = state_count[VmPageState::WIRED.index()] * page_size;
    sum_bytes += stats.wired_bytes;

    stats.total_heap_bytes = state_count[VmPageState::HEAP.index()] * page_size;
    sum_bytes += stats.total_heap_bytes;
    stats.free_heap_bytes = free_heap_bytes;

    stats.vmo_bytes = state_count[VmPageState::OBJECT.index()] * page_size;
    sum_bytes += stats.vmo_bytes;

    stats.mmu_overhead_bytes = (state_count[VmPageState::MMU.index()]
        + state_count[VmPageState::IOMMU.index()])
        * page_size;
    sum_bytes += stats.mmu_overhead_bytes;

    stats.ipc_bytes = state_count[VmPageState::IPC.index()] * page_size;
    sum_bytes += stats.ipc_bytes;

    stats.cache_bytes = state_count[VmPageState::CACHE.index()] * page_size;
    sum_bytes += state_count[VmPageState::CACHE.index()] * page_size;

    stats.slab_bytes = state_count[VmPageState::SLAB.index()] * page_size;
    sum_bytes += state_count[VmPageState::SLAB.index()] * page_size;

    stats.zram_bytes = state_count[VmPageState::ZRAM.index()] * page_size;
    sum_bytes += state_count[VmPageState::ZRAM.index()] * page_size;

    // Is there unaccounted memory?
    if stats.total_bytes > sum_bytes {
        // Everything else gets counted as "other".
        stats.other_bytes = stats.total_bytes - sum_bytes;
    } else {
        // One or more of our per-state counts may have been off. We'll ignore it.
        stats.other_bytes = 0;
    }

    let reclaim_counts: ReclaimCounts = pmm::page_queues().get_reclaim_queue_counts();
    let queue_counts: Counts = pmm::page_queues().queue_counts();

    stats.vmo_reclaim_total_bytes = reclaim_counts.total as u64 * page_size;
    stats.vmo_reclaim_newest_bytes = reclaim_counts.newest as u64 * page_size;
    stats.vmo_reclaim_oldest_bytes = reclaim_counts.oldest as u64 * page_size;
    stats.vmo_reclaim_disabled_bytes = queue_counts.high_priority as u64 * page_size;

    let discardable_counts: DiscardablePageCounts =
        DiscardableVmoTracker::debug_discardable_page_counts();

    stats.vmo_discardable_locked_bytes = discardable_counts.locked * page_size;
    stats.vmo_discardable_unlocked_bytes = discardable_counts.unlocked * page_size;

    stats
}

/// Returns system-wide page compression statistics.
pub fn get_compression_stats() -> zx_info_kmem_stats_compression_t {
    let Some(compression) = pmm::node().get_page_compression() else {
        return zx_info_kmem_stats_compression_t::default();
    };
    let mut kstats = zx_info_kmem_stats_compression_t::default();
    let stats = compression.get_stats();
    kstats.uncompressed_storage_bytes = stats.memory_usage.uncompressed_content_bytes;
    kstats.compressed_storage_bytes = stats.memory_usage.compressed_storage_bytes;
    kstats.compressed_fragmentation_bytes = stats.memory_usage.compressed_storage_bytes
        - stats.memory_usage.compressed_storage_used_bytes;
    kstats.compression_time = stats.compression_time;
    kstats.decompression_time = stats.decompression_time;
    kstats.total_page_compression_attempts = stats.total_page_compression_attempts;
    kstats.failed_page_compression_attempts = stats.failed_page_compression_attempts;
    kstats.total_page_decompressions = stats.total_page_decompressions;
    kstats.compressed_page_evictions = stats.compressed_page_evictions;
    kstats.eager_page_compressions = PageQueues::get_lru_pages_compressed();
    let evictor_stats: EvictorStats = Evictor::get_global_stats();
    kstats.memory_pressure_page_compressions = evictor_stats.compression_other;
    kstats.critical_memory_page_compressions = evictor_stats.compression_oom;
    kstats.pages_decompressed_unit_ns = DurationMono::from_seconds(1).into_nanos() as u64;
    zr::static_assert!(8 <= VmCompression::NUM_LOG_BUCKETS);
    for i in 0..8 {
        kstats.pages_decompressed_within_log_time[i] =
            stats.pages_decompressed_within_log_seconds[i];
    }
    kstats
}

/// Returns system-wide memory stall statistics.
pub fn get_stall_stats() -> zx_info_memory_stall_t {
    let stats = StallAggregator::get().read_stats();
    zx_info_memory_stall_t {
        stall_time_some: stats.stalled_time_some.into_nanos(),
        stall_time_full: stats.stalled_time_full.into_nanos(),
    }
}
