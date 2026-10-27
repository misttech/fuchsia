// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI bindings for `InstrumentationData` VMO publication.

use core::mem::{MaybeUninit, offset_of};

use crate::object::HandleOwner;

/// C ABI projection of `InstrumentationDataVmo`.  Matches `InstrumentationDataVmoFfi` in
/// `private.h`.
#[repr(C)]
pub(super) struct InstrumentationDataVmoFfi {
    pub announce: *const u8,
    pub announce_len: usize,
    pub sink_name: *const u8,
    pub sink_name_len: usize,
    pub units: *const u8,
    pub units_len: usize,
    pub scale: usize,
    pub handle: Option<HandleOwner>,
}

zr::static_assert_size_and_align!(InstrumentationDataVmoFfi, 64, 8);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, announce) == 0);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, announce_len) == 8);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, sink_name) == 16);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, sink_name_len) == 24);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, units) == 32);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, units_len) == 40);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, scale) == 48);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, handle) == 56);

unsafe extern "C" {
    /// Populates `out` with the LLVM profdata instrumentation VMO descriptor (`LlvmProfdataVmo`).
    ///
    /// # Safety
    ///
    /// Must be called at most once, after the VM system is initialized, because `LlvmProfdataVmo`
    /// initializes `gProfdataLiveData` and switches live coverage data to the mapped VMO.
    pub(super) fn cpp_llvm_profdata_vmo(out: &mut MaybeUninit<InstrumentationDataVmoFfi>);

    /// Populates `out` with the SanitizerCoverage PC table VMO descriptor (`SancovGetPcVmo`).
    ///
    /// # Safety
    ///
    /// Must only be called after `InitSancov` (`LK_INIT_LEVEL_KERNEL`) has initialized
    /// `gSancovPcVmo`.
    pub(super) fn cpp_sancov_get_pc_vmo(out: &mut MaybeUninit<InstrumentationDataVmoFfi>);

    /// Populates `out` with the SanitizerCoverage counter table VMO descriptor
    /// (`SancovGetCountsVmo`).
    ///
    /// # Safety
    ///
    /// Must only be called after `InitSancov` (`LK_INIT_LEVEL_KERNEL`) has initialized
    /// `gSancovCountsVmo`.
    pub(super) fn cpp_sancov_get_counts_vmo(out: &mut MaybeUninit<InstrumentationDataVmoFfi>);
}
