// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::InstrumentationDataVmo;
#[cfg(kernel_memory_profiler)]
use {
    super::debugdata::debugdata_vmo_name, super::kernel_mapped_vmo::KernelMappedVmo,
    crate::vm::heap::get_heap_profile, crate::vm::vm_object_paged::VmObjectPaged,
    lazy_init::LazyInit,
};

// This holds the pinned mapping of the live-updated data.
#[cfg(kernel_memory_profiler)]
static MEM_PROF_LIVE_DATA: LazyInit<KernelMappedVmo> = LazyInit::uninit();

/// Publishes the live kernel memory profile VMO when `kernel_memory_profiler` is enabled.
#[cfg(kernel_memory_profiler)]
pub(super) fn memory_profile_vmo() -> InstrumentationDataVmo {
    const SINK: &str = "memory-profile";
    const MODULE: &str = "heap";
    const SUFFIX: &str = "bin";
    let (buf_addr, buf_size) = get_heap_profile().expect("heap profile buffer");

    // SAFETY: `buf_addr` points to `buf_size` bytes of wired kernel memory that lives for the
    // lifetime of the kernel, and the returned VMO is retained in `MEM_PROF_LIVE_DATA` for the
    // lifetime of the kernel so its pages are never released back to the PMM.
    let vmo = unsafe { VmObjectPaged::create_from_wired_pages_raw(buf_addr, buf_size, false) }
        .expect("VmObjectPaged::create_from_wired_pages_raw");

    let mut mapped_vmo = KernelMappedVmo::new();
    mapped_vmo
        .init(
            VmObjectPaged::into_vm_object(vmo),
            /*offset=*/ 0,
            buf_size,
            b"memory-profile-live-data",
        )
        .expect("KernelMappedVmo::init");
    // SAFETY: `MEM_PROF_LIVE_DATA` is private to this module and only initialized here, and
    // `memory_profile_vmo` is called exactly once, from `InstrumentationData::GetVmos`.
    unsafe {
        MEM_PROF_LIVE_DATA.init(mapped_vmo);
    }

    let vmo_name = debugdata_vmo_name(SINK, MODULE, SUFFIX, /*is_static=*/ false);
    InstrumentationDataVmo {
        announce: "Memory profile",
        sink_name: SINK,
        handle: MEM_PROF_LIVE_DATA.publish(&vmo_name, buf_size),
        ..InstrumentationDataVmo::default()
    }
}

/// Returns an empty `InstrumentationDataVmo` when `kernel_memory_profiler` is disabled.
#[cfg(not(kernel_memory_profiler))]
pub(super) fn memory_profile_vmo() -> InstrumentationDataVmo {
    InstrumentationDataVmo::default()
}
