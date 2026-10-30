// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Outer cross-test FFI boundary for running the host test suites across all four backend
//! combinations (`C++ -> C++`, `C++ -> Rust`, `Rust -> Rust`, `Rust -> C++`, plus `LockstepPair`)
//! during Checkpoints 1–3 (`SMMUv2_rust_port_plan.md` §3).
//!
//! This module provides:
//! 1. `extern "C"` declarations for the C++ cross-test shims in `cross_test_ffi.cc`,
//! 2. `#[unsafe(no_mangle)]` `rust_*` FFI entrypoints called by C++ cross-test harnesses,
//! 3. Safe RAII wrappers (`CppSmmuHandle`, `CppBtiWrapper`, `CppPmtWrapper`) around C++ objects,
//! 4. [`CppSmmuWrapper`], which implements [`SmmuUnderTest`] over the C++ driver (`Rust -> C++`),
//!    and
//! 5. `rust_model_test_*` entrypoints that execute every shared sequence in
//!    `common::test_model::sequences` across `RustSmmuHarness`, `CppSmmuWrapper`, and
//!    `LockstepPair<RustSmmuHarness, CppSmmuWrapper>`.

use core::ffi::{c_char, c_void};
use core::mem::{align_of, size_of};
use core::ptr::NonNull;

use fbl::RefPtr;
use region_alloc::{Region, RegionSpan, TestRegionSet};
use zx_status::Status;
use zx_status::sys::{ZX_OK, zx_status_t};

use crate::arm_smmu::common::device_aspace::TlbInvalCallback;
use crate::arm_smmu::common::{
    Allocation, ArmSmmuMode, CppPinnedVmObjectRef, DeviceAspace, PageCache, QueryAddressResult,
    RequireContiguousMapping, SmmuEnv, SmmuPage, SmmuPinnedVmo, TlbInvalOp, ZX_MAX_NAME_LEN,
    get_smmu_mode_cstr, validate_smmu_mode_cstr,
};
use crate::arm_smmu::v2::{
    Smmu, SmmuBti, SmmuPmt, ZbiDcfgArmSmmuDriver, ZbiDcfgArmSmmuIrq, ZxIommuDescArmSmmu,
};

zr::static_assert!(size_of::<ZbiDcfgArmSmmuIrq>() == 8);
zr::static_assert!(align_of::<ZbiDcfgArmSmmuIrq>() == 4);
zr::static_assert!(size_of::<ZbiDcfgArmSmmuDriver>() == 1120);
zr::static_assert!(align_of::<ZbiDcfgArmSmmuDriver>() == 8);
zr::static_assert!(size_of::<ZxIommuDescArmSmmu>() == 8);
zr::static_assert!(align_of::<ZxIommuDescArmSmmu>() == 8);

/// Opaque FFI wrapper around a Rust [`DeviceAspace`] tracking whether translation tables have
/// been freed so [`rust_device_aspace_root_tt_paddr`] can return `0` afterward without tripping
/// [`DeviceAspace::root_tt_paddr`]'s debug assertion.
pub struct RustDeviceAspace {
    inner: kalloc::Box<DeviceAspace>,
    tables_freed: bool,
}

/// Raw region pointer type matching `struct RustDeviceAspaceAlloc` in `cross_test_ffi.h`.
pub type RustDeviceAspaceAlloc = Region;

/// Opaque handle to a C++ `iommu::Iommu` instance for cross-testing.
#[repr(C)]
pub struct CppSmmu {
    _private: [u8; 0],
}

/// Opaque handle to a C++ `iommu::Bti` instance for cross-testing.
#[repr(C)]
pub struct CppSmmuBti {
    _private: [u8; 0],
}

/// Opaque handle to a C++ `iommu::Pmt` instance for cross-testing.
#[repr(C)]
pub struct CppSmmuPmt {
    _private: [u8; 0],
}

// External C++ cross-test FFI shims defined in
// `zircon/kernel/dev/iommu/arm_smmu/tests/cross_test_ffi.cc`.
unsafe extern "C" {
    /// Creates a C++ `arm_smmu::Smmu` from borrowed `config` and writes a leaked `fbl::RefPtr`
    /// pointer into `*out_smmu` on `ZX_OK` (owned by caller until `cpp_smmu_destroy` or
    /// `cpp_smmu_release`).
    fn cpp_smmu_create(
        config: *const ZbiDcfgArmSmmuDriver,
        out_smmu: *mut *mut CppSmmu,
    ) -> zx_status_t;
    /// Looks up and binds a C++ `arm_smmu::Smmu` matching borrowed `desc[..desc_len]`, writing a
    /// leaked `fbl::RefPtr` pointer (or null when disabled) to `*out_smmu` on `ZX_OK`.
    fn cpp_smmu_fetch(desc: *const u8, desc_len: usize, out_smmu: *mut *mut CppSmmu)
    -> zx_status_t;
    /// Consumes ownership of `smmu` and runs `SmmuTestAccess::DestroySmmu`.
    fn cpp_smmu_destroy(smmu: *mut CppSmmu);
    /// Consumes and releases a leaked `fbl::RefPtr<iommu::Iommu>` pointer (`smmu`).
    fn cpp_smmu_release(smmu: *mut CppSmmu);

    /// Creates a C++ `SmmuBti` on borrowed `smmu` for `bti_id` and writes a leaked `fbl::RefPtr`
    /// pointer to `*out_bti` on `ZX_OK` (owned by caller until `cpp_smmu_bti_release`).
    fn cpp_smmu_create_bti(
        smmu: *mut CppSmmu,
        bti_id: u64,
        out_bti: *mut *mut CppSmmuBti,
    ) -> zx_status_t;
    /// Notifies borrowed `bti` that its user-mode dispatcher reached zero handles.
    fn cpp_smmu_bti_on_zero_handles(bti: *mut CppSmmuBti);
    /// Consumes and releases a leaked `fbl::RefPtr<iommu::Bti>` pointer (`bti`).
    fn cpp_smmu_bti_release(bti: *mut CppSmmuBti);
    /// Releases all quarantined PMTs on borrowed `bti`.
    fn cpp_smmu_bti_release_quarantine(bti: *mut CppSmmuBti);
    /// Returns `true` if borrowed `bti` is currently in the fault state.
    fn cpp_smmu_bti_in_fault_state(bti: *const CppSmmuBti) -> bool;
    /// Returns the number of quarantined PMTs held by borrowed `bti`.
    fn cpp_smmu_bti_quarantine_count(bti: *const CppSmmuBti) -> usize;
    /// Returns the total active + quarantined PMO count for borrowed `bti`.
    fn cpp_smmu_bti_pmo_count(bti: *const CppSmmuBti) -> u64;
    /// Returns the minimum contiguity in bytes guaranteed by borrowed `bti`.
    fn cpp_smmu_bti_minimum_contiguity(bti: *const CppSmmuBti) -> u64;
    /// Returns the total address space size in bytes for borrowed `bti`.
    fn cpp_smmu_bti_aspace_size(bti: *const CppSmmuBti) -> u64;
    /// Returns the 64-bit BTI identifier for borrowed `bti`.
    fn cpp_smmu_bti_id(bti: *const CppSmmuBti) -> u64;
    /// Writes the NUL-terminated debug name of borrowed `bti` into `out_name` (`ZX_MAX_NAME_LEN`
    /// bytes).
    fn cpp_smmu_bti_get_name(bti: *const CppSmmuBti, out_name: *mut c_char);
    /// Sets the debug name of borrowed `bti` from borrowed `name[..name_len]`.
    fn cpp_smmu_bti_set_name(bti: *mut CppSmmuBti, name: *const c_char, name_len: usize);
    /// Moves from the C++ `PinnedVmObject` at `pinned_vmo`, maps it into borrowed `bti`, and
    /// writes a leaked `fbl::RefPtr<iommu::Pmt>` to `*out_pmt` on `ZX_OK` (owned by caller until
    /// `cpp_smmu_pmt_release`).
    fn cpp_smmu_bti_map(
        bti: *mut CppSmmuBti,
        pinned_vmo: *mut c_void,
        perms: u32,
        require_contiguous: bool,
        out_pmt: *mut *mut CppSmmuPmt,
    ) -> zx_status_t;

    /// Queries the mapped IOVA and contiguous byte count on borrowed `pmt` into
    /// `*out_device_vaddr` and `*out_size`.
    fn cpp_smmu_pmt_query_address(
        pmt: *mut CppSmmuPmt,
        offset: u64,
        size: usize,
        out_device_vaddr: *mut u64,
        out_size: *mut usize,
    ) -> zx_status_t;
    /// Unpins and unmaps the memory represented by borrowed `pmt`.
    fn cpp_smmu_pmt_release_pinned_memory(pmt: *mut CppSmmuPmt);
    /// Notifies borrowed `pmt` that its user-mode dispatcher reached zero handles.
    fn cpp_smmu_pmt_on_zero_handles(pmt: *mut CppSmmuPmt);
    /// Consumes and releases a leaked `fbl::RefPtr<iommu::Pmt>` pointer (`pmt`).
    fn cpp_smmu_pmt_release(pmt: *mut CppSmmuPmt);

    /// Validates a borrowed NUL-terminated SMMU mode string using the C++ parser.
    fn cpp_smmu_validate_mode_string(mode_str: *const c_char) -> bool;
    /// Parses a borrowed NUL-terminated SMMU mode string for an optional `base_addr`, writing the
    /// discriminant to `*out_mode` when returning `true`.
    fn cpp_smmu_get_mode(
        mode_str: *const c_char,
        has_base_addr: bool,
        base_addr: u64,
        out_mode: *mut u32,
    ) -> bool;
}

// Exported `rust_*` FFI shims called by C++ cross-test harnesses (`C++ -> Rust`).

/// Creates a Rust `Smmu` instance from `config` and writes its raw `RefPtr` pointer to `out_smmu`.
///
/// # Safety
///
/// `config` must point to a valid initialized `ZbiDcfgArmSmmuDriver` for the duration of the call,
/// and `out_smmu` must be a valid writable non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_create(
    config: *const ZbiDcfgArmSmmuDriver,
    out_smmu: *mut *const Smmu,
) -> zx_status_t {
    if out_smmu.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: `out_smmu` was verified non-null and caller guarantees it is writable.
    unsafe {
        out_smmu.write(core::ptr::null());
    }
    if config.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: Caller guarantees `config` and `out_smmu` are valid non-null pointers.
    unsafe {
        match Smmu::create(&*config) {
            Ok(smmu) => {
                out_smmu.write(RefPtr::into_raw(smmu));
                ZX_OK
            }
            Err(status) => status.into_raw(),
        }
    }
}

/// Fetches a Rust `Smmu` instance matching `desc[..desc_len]` into `out_smmu` (writing null when
/// disabled).
///
/// # Safety
///
/// `desc` must be valid for `desc_len` bytes (`(null, 0)` is permitted), and `out_smmu` must be a
/// valid writable non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_fetch(
    desc: *const u8,
    desc_len: usize,
    out_smmu: *mut *const Smmu,
) -> zx_status_t {
    if out_smmu.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: `out_smmu` was verified non-null above, and caller guarantees it is writable.
    unsafe {
        out_smmu.write(core::ptr::null());
    }
    if desc.is_null() && desc_len != 0 {
        if SmmuEnv::boot_option_smmu_mode(None) == ArmSmmuMode::Disabled {
            return ZX_OK;
        }
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: Caller guarantees `desc` is valid for `desc_len` bytes (or `(null, 0)`) and
    // `out_smmu` is writable.
    unsafe {
        let slice = zr::slice_from_raw_parts(desc, desc_len);
        match Smmu::fetch(slice) {
            Ok(Some(smmu)) => {
                out_smmu.write(RefPtr::into_raw(smmu));
                ZX_OK
            }
            Ok(None) => ZX_OK,
            Err(status) => status.into_raw(),
        }
    }
}

/// Consumes a raw `RefPtr<Smmu>` pointer and tears down the `Smmu` instance for testing.
///
/// # Safety
///
/// If non-null, `smmu` must be an unconsumed pointer produced by `RefPtr::into_raw` (consumed at
/// most once across `rust_smmu_destroy` and `rust_smmu_release`), and all active PMTs on BTIs
/// belonging to `smmu` must have been unpinned or closed beforehand.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_destroy(smmu: *const Smmu) {
    if smmu.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `smmu` is an unconsumed pointer produced by `RefPtr::into_raw`.
    let smmu_ref = unsafe { RefPtr::from_raw(smmu) };
    Smmu::destroy_for_test(smmu_ref);
}

/// Consumes and drops a raw `RefPtr<Smmu>` reference without unregistering the instance.
///
/// # Safety
///
/// If non-null, `smmu` must be an unconsumed pointer produced by `RefPtr::into_raw` (consumed at
/// most once across `rust_smmu_destroy` and `rust_smmu_release`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_release(smmu: *const Smmu) {
    if smmu.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `smmu` is an unconsumed pointer produced by `RefPtr::into_raw`.
    unsafe {
        drop(RefPtr::from_raw(smmu));
    }
}

/// Creates a Rust `SmmuBti` for `bti_id` on `smmu` and writes its raw pointer to `out_bti`.
///
/// # Safety
///
/// `smmu` must point to a valid live `Smmu`, and `out_bti` must be a valid writable non-null
/// pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_create_bti(
    smmu: *const Smmu,
    bti_id: u64,
    out_bti: *mut *const SmmuBti,
) -> zx_status_t {
    if out_bti.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: `out_bti` was verified non-null and caller guarantees it is writable.
    unsafe {
        out_bti.write(core::ptr::null());
    }
    if smmu.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: Caller guarantees `smmu` and `out_bti` are valid non-null pointers.
    unsafe {
        match (*smmu).create_bti(bti_id) {
            Ok(bti) => {
                out_bti.write(RefPtr::into_raw(bti));
                ZX_OK
            }
            Err(status) => status.into_raw(),
        }
    }
}

/// Notifies `bti` that its dispatcher reached zero handles.
///
/// # Safety
///
/// If non-null, `bti` must point to a valid live `SmmuBti` whose dispatcher has not yet reached
/// zero handles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_bti_on_zero_handles(bti: *const SmmuBti) {
    if bti.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `bti` points to a valid `SmmuBti`.
    unsafe {
        (*bti).on_dispatcher_zero_handles();
    }
}

/// Consumes and drops a raw `RefPtr<SmmuBti>` reference.
///
/// # Safety
///
/// If non-null, `bti` must be an unconsumed pointer produced by `RefPtr::into_raw` (consumed at
/// most once).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_bti_release(bti: *const SmmuBti) {
    if bti.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `bti` was produced by `RefPtr::into_raw`.
    unsafe {
        drop(RefPtr::from_raw(bti));
    }
}

/// Releases all quarantined PMTs on `bti`.
///
/// # Safety
///
/// `bti` must point to a valid `SmmuBti`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_bti_release_quarantine(bti: *const SmmuBti) {
    if bti.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `bti` points to a valid `SmmuBti`.
    unsafe {
        (*bti).release_quarantine();
    }
}

/// Returns `true` if `bti` is currently in the fault state.
///
/// # Safety
///
/// `bti` must point to a valid `SmmuBti`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_bti_in_fault_state(bti: *const SmmuBti) -> bool {
    if bti.is_null() {
        return false;
    }
    // SAFETY: Caller guarantees `bti` points to a valid `SmmuBti`.
    unsafe { (*bti).in_fault_state() }
}

/// Returns the number of quarantined PMTs held by `bti`.
///
/// # Safety
///
/// `bti` must point to a valid `SmmuBti`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_bti_quarantine_count(bti: *const SmmuBti) -> usize {
    if bti.is_null() {
        return 0;
    }
    // SAFETY: Caller guarantees `bti` points to a valid `SmmuBti`.
    unsafe { (*bti).quarantine_count() }
}

/// Returns the total active + quarantined PMO count for `bti`.
///
/// # Safety
///
/// `bti` must point to a valid `SmmuBti`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_bti_pmo_count(bti: *const SmmuBti) -> u64 {
    if bti.is_null() {
        return 0;
    }
    // SAFETY: Caller guarantees `bti` points to a valid `SmmuBti`.
    unsafe { (*bti).pmo_count() }
}

/// Returns the minimum contiguity in bytes guaranteed by `bti`.
///
/// # Safety
///
/// `bti` must point to a valid `SmmuBti`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_bti_minimum_contiguity(bti: *const SmmuBti) -> u64 {
    if bti.is_null() {
        return 0;
    }
    // SAFETY: Caller guarantees `bti` points to a valid `SmmuBti`.
    unsafe { (*bti).minimum_contiguity() }
}

/// Returns the total address space size in bytes for `bti`.
///
/// # Safety
///
/// `bti` must point to a valid `SmmuBti`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_bti_aspace_size(bti: *const SmmuBti) -> u64 {
    if bti.is_null() {
        return 0;
    }
    // SAFETY: Caller guarantees `bti` points to a valid `SmmuBti`.
    unsafe { (*bti).aspace_size() }
}

/// Returns the 64-bit BTI identifier for `bti`.
///
/// # Safety
///
/// `bti` must point to a valid `SmmuBti`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_bti_id(bti: *const SmmuBti) -> u64 {
    if bti.is_null() {
        return 0;
    }
    // SAFETY: Caller guarantees `bti` points to a valid `SmmuBti`.
    unsafe { (*bti).bti_id() }
}

/// Writes the NUL-terminated debug name of `bti` into `out_name` (`ZX_MAX_NAME_LEN` bytes).
///
/// # Safety
///
/// `bti` must point to a valid `SmmuBti` and `out_name` must be valid for `ZX_MAX_NAME_LEN` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_bti_get_name(bti: *const SmmuBti, out_name: *mut c_char) {
    if bti.is_null() || out_name.is_null() {
        return;
    }
    let mut buf = [0u8; ZX_MAX_NAME_LEN];
    // SAFETY: Caller guarantees `bti` is valid and `out_name` has `ZX_MAX_NAME_LEN` writable bytes.
    unsafe {
        (*bti).get_name(&mut buf);
        core::ptr::copy_nonoverlapping(buf.as_ptr(), out_name.cast::<u8>(), ZX_MAX_NAME_LEN);
    }
}

/// Sets the debug name of `bti` from `name[..name_len]`.
///
/// # Safety
///
/// If `bti` is non-null, it must point to a valid `SmmuBti`, and `name` must be valid for
/// `name_len` bytes (`(null, 0)` is permitted).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_bti_set_name(
    bti: *const SmmuBti,
    name: *const c_char,
    name_len: usize,
) {
    if bti.is_null() || (name.is_null() && name_len != 0) {
        return;
    }
    // SAFETY: Caller guarantees `bti` is valid and `name` is valid for `name_len` bytes (or
    // `(null, 0)`).
    unsafe {
        let slice = zr::slice_from_raw_parts(name.cast::<u8>(), name_len);
        (*bti).set_name(slice);
    }
}

/// Pins and maps `pinned_vmo` into `bti`, writing the resulting `SmmuPmt` pointer to `out_pmt`.
///
/// # Safety
///
/// `bti` must point to a valid live `SmmuBti`, `pinned_vmo` must point to a valid mutable C++
/// `PinnedVmObject` whose ownership is moved from in place during the call, and `out_pmt` must be
/// a valid writable non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_bti_map(
    bti: *const SmmuBti,
    pinned_vmo: *mut c_void,
    perms: u32,
    require_contiguous: bool,
    out_pmt: *mut *const SmmuPmt,
) -> zx_status_t {
    if out_pmt.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: `out_pmt` was verified non-null and caller guarantees it is writable.
    unsafe {
        out_pmt.write(core::ptr::null());
    }
    if bti.is_null() || pinned_vmo.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    let req = if require_contiguous {
        RequireContiguousMapping::Yes
    } else {
        RequireContiguousMapping::No
    };
    // SAFETY: Caller guarantees `bti` is valid, `pinned_vmo` points to a valid mutable C++
    // `PinnedVmObject`, and `out_pmt` is writable.
    unsafe {
        let vmo = SmmuPinnedVmo::from_raw_move(pinned_vmo);
        match (*bti).map(vmo, perms, req) {
            Ok(pmt) => {
                out_pmt.write(RefPtr::into_raw(pmt));
                ZX_OK
            }
            Err(status) => status.into_raw(),
        }
    }
}

/// Queries the mapped IOVA and contiguous byte count for `[offset, offset + size)` on `pmt`.
///
/// # Safety
///
/// `pmt` must point to a valid `SmmuPmt`, and `out_device_vaddr` and `out_size` must be valid
/// writable non-null pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_pmt_query_address(
    pmt: *const SmmuPmt,
    offset: u64,
    size: usize,
    out_device_vaddr: *mut u64,
    out_size: *mut usize,
) -> zx_status_t {
    if pmt.is_null() || out_device_vaddr.is_null() || out_size.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: Caller guarantees `pmt`, `out_device_vaddr`, and `out_size` are valid non-null
    // pointers.
    unsafe {
        match (*pmt).query_address(offset, size) {
            Ok(res) => {
                out_device_vaddr.write(res.device_vaddr);
                out_size.write(res.size);
                ZX_OK
            }
            Err(status) => status.into_raw(),
        }
    }
}

/// Unpins and unmaps the memory represented by `pmt`.
///
/// # Safety
///
/// If non-null, `pmt` must point to a valid `SmmuPmt`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_pmt_release_pinned_memory(pmt: *const SmmuPmt) {
    if pmt.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `pmt` points to a valid `SmmuPmt`.
    unsafe {
        (*pmt).release_pinned_memory();
    }
}

/// Notifies `pmt` that its dispatcher reached zero handles.
///
/// # Safety
///
/// If non-null, `pmt` must point to a valid `SmmuPmt` whose dispatcher has not yet reached zero
/// handles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_pmt_on_zero_handles(pmt: *const SmmuPmt) {
    if pmt.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `pmt` points to a valid `SmmuPmt`.
    unsafe {
        (*pmt).on_dispatcher_zero_handles();
    }
}

/// Consumes and drops a raw `RefPtr<SmmuPmt>` reference.
///
/// # Safety
///
/// If non-null, `pmt` must be an unconsumed pointer produced by `RefPtr::into_raw` (consumed at
/// most once).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_pmt_release(pmt: *const SmmuPmt) {
    if pmt.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `pmt` was produced by `RefPtr::into_raw`.
    unsafe {
        drop(RefPtr::from_raw(pmt));
    }
}

/// Validates a NUL-terminated SMMU mode string using the Rust parser.
///
/// # Safety
///
/// If non-null, `mode_str` must point to a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_validate_mode_string(mode_str: *const c_char) -> bool {
    // SAFETY: Caller guarantees `mode_str` is null or a valid C string.
    unsafe { validate_smmu_mode_cstr(mode_str) }
}

/// Parses a NUL-terminated SMMU mode string for an optional `base_addr` into `out_mode`.
///
/// # Safety
///
/// If non-null, `mode_str` must be a valid NUL-terminated C string, and `out_mode` must be a valid
/// writable non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_smmu_get_mode(
    mode_str: *const c_char,
    has_base_addr: bool,
    base_addr: u64,
    out_mode: *mut u32,
) -> bool {
    if out_mode.is_null() {
        return false;
    }
    let addr = if has_base_addr { Some(base_addr) } else { None };
    // SAFETY: Caller guarantees `mode_str` is null or a valid C string and `out_mode` is writable.
    unsafe {
        let Some(mode) = get_smmu_mode_cstr(mode_str, addr) else {
            return false;
        };
        out_mode.write(mode as u32);
        true
    }
}

/// Allocates a new Rust `PageCache` on the heap for cross-testing (returning null on OOM).
///
/// Ownership of a non-null returned pointer belongs to the caller until passed to
/// [`rust_page_cache_destroy`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_cache_create() -> *mut PageCache {
    match kalloc::Box::try_new(PageCache::new()) {
        Ok(boxed) => kalloc::Box::into_raw(boxed),
        Err(_) => core::ptr::null_mut(),
    }
}

/// Destroys a Rust `PageCache` allocated by `rust_page_cache_create`.
///
/// # Safety
///
/// If non-null, `cache` must be an unconsumed pointer returned by `rust_page_cache_create`
/// (consumed at most once) with zero in-flight and zero cached pages remaining.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_cache_destroy(cache: *mut PageCache) {
    if cache.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `cache` is an unconsumed pointer from `rust_page_cache_create`.
    unsafe {
        drop(kalloc::Box::from_raw(cache));
    }
}

/// Allocates a zeroed 4 KiB page from `cache` into `out_page`.
///
/// # Safety
///
/// `cache` must point to a valid `PageCache` with exclusive access for the call, and `out_page`
/// must be a valid writable non-null pointer to a `vm_page_t*` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_cache_get_page(
    cache: *mut PageCache,
    out_page: *mut *mut c_void,
) -> zx_status_t {
    if out_page.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: `out_page` was verified non-null and caller guarantees it is writable.
    unsafe {
        out_page.write(core::ptr::null_mut());
    }
    if cache.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: Caller guarantees `cache` and `out_page` are valid pointers.
    unsafe {
        match (*cache).get_page() {
            Ok(page) => {
                out_page.write(page.as_raw());
                ZX_OK
            }
            Err(status) => status.into_raw(),
        }
    }
}

/// Returns `page` to `cache`.
///
/// # Safety
///
/// `cache` must point to a valid `PageCache` with exclusive access for the call, and `page` must
/// be a non-null `vm_page_t*` previously checked out from `cache` via `rust_page_cache_get_page`,
/// currently unlinked from any list, and with its 4 KiB page contents zeroed and cache-cleaned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_cache_return_page(cache: *mut PageCache, page: *mut c_void) {
    if cache.is_null() || page.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `cache` is valid and `page` is an unlinked, zeroed `vm_page_t*`
    // checked out from `cache`.
    unsafe {
        if let Some(smmu_page) = SmmuPage::from_raw(page) {
            (*cache).return_page(smmu_page);
        }
    }
}

/// Trims `cache` down to at most `max_cached` cached pages.
///
/// # Safety
///
/// `cache` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_cache_trim(cache: *mut PageCache, max_cached: usize) {
    if cache.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `cache` is a valid pointer.
    unsafe {
        (*cache).trim(max_cached.min(u32::MAX as usize) as u32);
    }
}

/// Returns the number of in-flight pages checked out from `cache`.
///
/// # Safety
///
/// `cache` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_cache_in_flight_pages(cache: *const PageCache) -> usize {
    if cache.is_null() {
        return 0;
    }
    // SAFETY: Caller guarantees `cache` is a valid pointer.
    unsafe { (*cache).in_flight_pages() as usize }
}

/// Returns the number of cached pages retained inside `cache`.
///
/// # Safety
///
/// `cache` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_cache_cache_entries(cache: *const PageCache) -> usize {
    if cache.is_null() {
        return 0;
    }
    // SAFETY: Caller guarantees `cache` is a valid pointer.
    unsafe { (*cache).cache_entries() as usize }
}

/// Creates a Rust `DeviceAspace` and writes its heap pointer to `out_aspace`.
///
/// # Safety
///
/// `out_aspace` must be a valid writable non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_device_aspace_create(
    aspace_start: u64,
    aspace_len: u64,
    max_cache_pages: u32,
    out_aspace: *mut *mut RustDeviceAspace,
) -> zx_status_t {
    if out_aspace.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: `out_aspace` was verified non-null and caller guarantees it is writable.
    unsafe {
        out_aspace.write(core::ptr::null_mut());
    }
    let Ok(mut uninit_wrapper) = kalloc::Box::<RustDeviceAspace>::try_new_uninit() else {
        return Status::NO_MEMORY.into_raw();
    };
    match DeviceAspace::create(aspace_start, aspace_len, max_cache_pages) {
        Ok(inner) => {
            uninit_wrapper.write(RustDeviceAspace { inner, tables_freed: false });
            // SAFETY: `uninit_wrapper` was initialized by `MaybeUninit::write` above, and
            // `out_aspace` is a valid writable non-null pointer.
            unsafe {
                let wrapped = uninit_wrapper.assume_init();
                out_aspace.write(kalloc::Box::into_raw(wrapped));
            }
            ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Destroys a Rust `DeviceAspace` created by `rust_device_aspace_create`.
///
/// # Safety
///
/// If non-null, `aspace` must be an unconsumed pointer returned by `rust_device_aspace_create`
/// (consumed at most once). All `RustDeviceAspaceAlloc` handles returned by
/// `rust_device_aspace_map` on `aspace` must have been unmapped or dropped via
/// `rust_device_aspace_drop_allocation` beforehand (`Region::recycle` dereferences the owning
/// `RegionAllocator`), and `rust_device_aspace_free_translation_tables` must have been called.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_device_aspace_destroy(aspace: *mut RustDeviceAspace) {
    if aspace.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `aspace` is an unconsumed pointer from
    // `rust_device_aspace_create` with no live allocations and freed translation tables.
    unsafe {
        drop(kalloc::Box::from_raw(aspace));
    }
}

/// Maps `pinned_vmo` into `aspace`, writing the allocation handle and span to the out-parameters.
///
/// # Safety
///
/// - `aspace` must point to a valid live `RustDeviceAspace` with exclusive access for the duration
///   of the call.
/// - `pinned_vmo` must point to a valid initialized borrowed `const PinnedVmObject` that remains
///   live and unmodified for the duration of the call.
/// - `out_alloc`, `out_base`, and `out_size` must be valid writable non-null pointers.
/// - `tlb_cbk`/`tlb_ctx` must be sound to invoke during the call and must **not** reentrantly
///   access `aspace` (e.g. via `rust_device_aspace_*` functions).
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_device_aspace_map(
    aspace: *mut RustDeviceAspace,
    pinned_vmo: *const c_void,
    perms: u32,
    tlb_cbk: Option<TlbInvalCallback>,
    tlb_ctx: *mut c_void,
    has_location: bool,
    location: u64,
    out_alloc: *mut *mut RustDeviceAspaceAlloc,
    out_base: *mut u64,
    out_size: *mut u64,
) -> zx_status_t {
    if out_alloc.is_null() || out_base.is_null() || out_size.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: `out_alloc`, `out_base`, and `out_size` were verified non-null and caller guarantees
    // they are writable.
    unsafe {
        out_alloc.write(core::ptr::null_mut());
        out_base.write(0);
        out_size.write(0);
    }
    if aspace.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    let loc = if has_location { Some(location) } else { None };
    // SAFETY: Caller guarantees `aspace` is exclusively valid (and `tlb_cbk` does not access it
    // reentrantly), `pinned_vmo` is a valid borrowed `const PinnedVmObject` if non-null, and
    // callback/out-parameter arguments are valid.
    unsafe {
        let Some(vmo_ref) = CppPinnedVmObjectRef::from_raw(pinned_vmo) else {
            return Status::INVALID_ARGS.into_raw();
        };
        let mut inval = TlbInvalOp::new(tlb_cbk, tlb_ctx);
        match (*aspace).inner.map(&vmo_ref, perms, &mut inval, loc) {
            Ok(alloc) => {
                out_base.write(alloc.base());
                out_size.write(alloc.size());
                out_alloc.write(fbl::UniquePtr::into_raw(alloc));
                ZX_OK
            }
            Err(status) => status.into_raw(),
        }
    }
}

/// Unmaps `alloc` from `aspace`, consuming `alloc`.
///
/// # Safety
///
/// `aspace` must point to a valid live `RustDeviceAspace` with exclusive access, `alloc` must be
/// an unconsumed raw `Region` pointer returned by `rust_device_aspace_map` on `aspace` (consumed
/// at most once), and `tlb_cbk`/`tlb_ctx` must be sound to invoke and must **not** reentrantly
/// access `aspace`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_device_aspace_unmap(
    aspace: *mut RustDeviceAspace,
    alloc: *mut RustDeviceAspaceAlloc,
    tlb_cbk: Option<TlbInvalCallback>,
    tlb_ctx: *mut c_void,
) {
    if aspace.is_null() || alloc.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `aspace` is exclusively valid (with non-reentrant `tlb_cbk`) and
    // `alloc` is an unconsumed `UniquePtr<Region>` raw pointer from `aspace`.
    unsafe {
        let alloc = Allocation::from_raw(alloc);
        let mut inval = TlbInvalOp::new(tlb_cbk, tlb_ctx);
        (*aspace).inner.unmap(alloc, &mut inval);
    }
}

/// Frees all translation tables owned by `aspace`.
///
/// # Safety
///
/// `aspace` must point to a valid live `RustDeviceAspace` with exclusive access whose translation
/// tables have not yet been freed, and `tlb_cbk`/`tlb_ctx` must be sound to invoke and must
/// **not** reentrantly access `aspace`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_device_aspace_free_translation_tables(
    aspace: *mut RustDeviceAspace,
    tlb_cbk: Option<TlbInvalCallback>,
    tlb_ctx: *mut c_void,
) {
    if aspace.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `aspace` is exclusively valid and `tlb_cbk` does not access it
    // reentrantly.
    unsafe {
        let mut inval = TlbInvalOp::new(tlb_cbk, tlb_ctx);
        (*aspace).inner.free_translation_tables(&mut inval);
        (*aspace).tables_freed = true;
    }
}

/// Drops a leaked `UniquePtr<Region>` raw `Region` pointer returned by `rust_device_aspace_map`
/// without unmapping page table entries, recycling the region back to its owning
/// `RegionAllocator`.
///
/// # Safety
///
/// If non-null, `alloc` must be an unconsumed pointer returned by `rust_device_aspace_map`
/// (consumed at most once across `rust_device_aspace_unmap` and
/// `rust_device_aspace_drop_allocation`), and the `RustDeviceAspace` that produced `alloc` must
/// still be alive (`Region::recycle` dereferences `owner: NonNull<RegionAllocator>`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_device_aspace_drop_allocation(alloc: *mut RustDeviceAspaceAlloc) {
    if alloc.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `alloc` is an unconsumed `UniquePtr<Region>` pointer whose owning
    // `RustDeviceAspace` is still alive.
    unsafe {
        drop(Allocation::from_raw(alloc));
    }
}

/// Returns `aspace_start` for `aspace`.
///
/// # Safety
///
/// If non-null, `aspace` must point to a valid `RustDeviceAspace` with shared access.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_device_aspace_aspace_start(aspace: *const RustDeviceAspace) -> u64 {
    if aspace.is_null() {
        return 0;
    }
    // SAFETY: Caller guarantees `aspace` is a valid pointer with shared access.
    unsafe { (*aspace).inner.first_valid_address() }
}

/// Returns `aspace_len` for `aspace`.
///
/// # Safety
///
/// If non-null, `aspace` must point to a valid `RustDeviceAspace` with shared access.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_device_aspace_aspace_len(aspace: *const RustDeviceAspace) -> u64 {
    if aspace.is_null() {
        return 0;
    }
    // SAFETY: Caller guarantees `aspace` is a valid pointer with shared access.
    unsafe { (*aspace).inner.aspace_len() }
}

/// Returns the number of cached translation table pages in `aspace`.
///
/// # Safety
///
/// If non-null, `aspace` must point to a valid `RustDeviceAspace` with shared access (must not be
/// called reentrantly while `&mut RustDeviceAspace` is active in `map`, `unmap`, or
/// `free_translation_tables`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_device_aspace_cache_entries(
    aspace: *const RustDeviceAspace,
) -> usize {
    if aspace.is_null() {
        return 0;
    }
    // SAFETY: Caller guarantees `aspace` is a valid pointer with shared access.
    unsafe { (*aspace).inner.page_cache().cache_entries() as usize }
}

/// Returns the root translation table physical address for `aspace`, or `0` if freed.
///
/// # Safety
///
/// If non-null, `aspace` must point to a valid `RustDeviceAspace` with shared access.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_device_aspace_root_tt_paddr(aspace: *const RustDeviceAspace) -> u64 {
    if aspace.is_null() {
        return 0;
    }
    // SAFETY: Caller guarantees `aspace` is a valid pointer with shared access.
    unsafe { if (*aspace).tables_freed { 0 } else { (*aspace).inner.root_tt_paddr() } }
}

/// Tests whether `[base, base + size)` is contained by the allocated or available region set.
///
/// # Safety
///
/// `aspace` must point to a valid `RustDeviceAspace` with shared access, and `out_contained` must
/// be a valid writable non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_device_aspace_test_region_contained_by(
    aspace: *const RustDeviceAspace,
    base: u64,
    size: u64,
    is_allocated_set: bool,
    out_contained: *mut bool,
) -> zx_status_t {
    if aspace.is_null() || out_contained.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    let which = if is_allocated_set { TestRegionSet::Allocated } else { TestRegionSet::Available };
    // SAFETY: Caller guarantees `aspace` and `out_contained` are valid pointers.
    unsafe {
        match (*aspace)
            .inner
            .avail_regions()
            .test_region_contained_by(RegionSpan { base, size }, which)
        {
            Ok(contained) => {
                out_contained.write(contained);
                ZX_OK
            }
            Err(status) => status.into_raw(),
        }
    }
}

// Safe RAII wrappers around C++ `arm_smmu` objects for `Rust -> C++` cross-testing.

/// Safe Rust handle around a raw C++ `arm_smmu::Smmu` instance pointer.
pub struct CppSmmuHandle {
    raw: NonNull<CppSmmu>,
}

impl CppSmmuHandle {
    /// Creates and initializes a C++ `arm_smmu::Smmu` from `config`.
    pub fn create(config: &ZbiDcfgArmSmmuDriver) -> Result<Self, Status> {
        let mut raw: *mut CppSmmu = core::ptr::null_mut();
        // SAFETY: `config` and `raw` are valid pointers for the duration of the FFI call.
        let status = unsafe { cpp_smmu_create(config, &raw mut raw) };
        Status::ok(status)?;
        NonNull::new(raw).map(|raw| Self { raw }).ok_or(Status::INTERNAL)
    }

    /// Looks up and binds a C++ `arm_smmu::Smmu` matching `desc`.
    pub fn fetch(desc: &[u8]) -> Result<Option<Self>, Status> {
        let mut raw: *mut CppSmmu = core::ptr::null_mut();
        // SAFETY: `desc` and `raw` are valid for the duration of the FFI call.
        let status = unsafe { cpp_smmu_fetch(desc.as_ptr(), desc.len(), &raw mut raw) };
        Status::ok(status)?;
        Ok(NonNull::new(raw).map(|raw| Self { raw }))
    }

    /// Cleanly tears down this C++ `Smmu` instance and removes it from the static instance list.
    ///
    /// # Safety
    ///
    /// All [`CppBtiWrapper`] and [`CppPmtWrapper`] handles created from this SMMU must have been
    /// closed and dropped before calling `destroy_for_test` (`SmmuTestAccess::DestroySmmu` shuts
    /// down remaining BTIs, clearing `bti->smmu_` and destroying `bti->aspace_`).
    pub unsafe fn destroy_for_test(self) {
        let raw = self.raw.as_ptr();
        core::mem::forget(self);
        // SAFETY: `raw` is a valid C++ `iommu::Iommu` pointer owned by `self`, and caller
        // guarantees no child BTI/PMT wrappers remain live.
        unsafe {
            cpp_smmu_destroy(raw);
        }
    }

    /// Creates a C++ `SmmuBti` for `bti_id` on this `Smmu`.
    pub fn create_bti(&self, bti_id: u64) -> Result<CppBtiWrapper, Status> {
        let mut raw: *mut CppSmmuBti = core::ptr::null_mut();
        // SAFETY: `self.raw` and `raw` are valid pointers for the duration of the FFI call.
        let status = unsafe { cpp_smmu_create_bti(self.raw.as_ptr(), bti_id, &raw mut raw) };
        Status::ok(status)?;
        NonNull::new(raw).map(|raw| CppBtiWrapper { raw }).ok_or(Status::INTERNAL)
    }

    /// Validates a NUL-terminated SMMU mode string using the C++ parser.
    ///
    /// # Safety
    ///
    /// If non-null, `mode_str` must be a valid NUL-terminated C string.
    pub unsafe fn validate_mode_cstr(mode_str: *const c_char) -> bool {
        // SAFETY: Caller guarantees `mode_str` is null or a valid C string.
        unsafe { cpp_smmu_validate_mode_string(mode_str) }
    }

    /// Parses a NUL-terminated SMMU mode string using the C++ parser.
    ///
    /// # Safety
    ///
    /// If non-null, `mode_str` must be a valid NUL-terminated C string.
    pub unsafe fn get_mode_cstr(
        mode_str: *const c_char,
        base_addr: Option<u64>,
    ) -> Option<ArmSmmuMode> {
        let mut raw_mode = 0u32;
        // SAFETY: Caller guarantees `mode_str` is null or a valid C string.
        let ok = unsafe {
            cpp_smmu_get_mode(
                mode_str,
                base_addr.is_some(),
                base_addr.unwrap_or(0),
                &raw mut raw_mode,
            )
        };
        if !ok {
            return None;
        }
        match raw_mode {
            0 => Some(ArmSmmuMode::Disabled),
            1 => Some(ArmSmmuMode::Passthru),
            2 => Some(ArmSmmuMode::Enforced),
            _ => None,
        }
    }
}

impl Drop for CppSmmuHandle {
    fn drop(&mut self) {
        // SAFETY: `self.raw` is a valid C++ `iommu::Iommu` reference owned by `self`.
        unsafe {
            cpp_smmu_release(self.raw.as_ptr());
        }
    }
}

/// Safe Rust wrapper around a C++ `arm_smmu::SmmuBti` handle for `Rust -> C++` tests.
pub struct CppBtiWrapper {
    raw: NonNull<CppSmmuBti>,
}

impl CppBtiWrapper {
    /// Notifies the C++ `SmmuBti` that its user-mode dispatcher reached zero handles and consumes
    /// this handle so no `smmu_`-dependent method can be called after closure.
    pub fn on_dispatcher_zero_handles(self) {
        // SAFETY: `self.raw` is a valid C++ `iommu::Bti` reference whose dispatcher has not yet
        // been closed. `self` drops immediately afterward, releasing `self.raw`.
        unsafe {
            cpp_smmu_bti_on_zero_handles(self.raw.as_ptr());
        }
    }

    /// Releases all quarantined PMTs on this BTI.
    pub fn release_quarantine(&self) {
        // SAFETY: `self.raw` is a valid C++ `iommu::Bti` reference.
        unsafe {
            cpp_smmu_bti_release_quarantine(self.raw.as_ptr());
        }
    }

    /// Returns `true` if this BTI is currently in the fault state.
    pub fn in_fault_state(&self) -> bool {
        // SAFETY: `self.raw` is a valid C++ `iommu::Bti` reference.
        unsafe { cpp_smmu_bti_in_fault_state(self.raw.as_ptr()) }
    }

    /// Returns the number of quarantined PMTs held by this BTI.
    pub fn quarantine_count(&self) -> usize {
        // SAFETY: `self.raw` is a valid C++ `iommu::Bti` reference.
        unsafe { cpp_smmu_bti_quarantine_count(self.raw.as_ptr()) }
    }

    /// Returns the total active + quarantined PMO count for this BTI.
    pub fn pmo_count(&self) -> u64 {
        // SAFETY: `self.raw` is a valid C++ `iommu::Bti` reference.
        unsafe { cpp_smmu_bti_pmo_count(self.raw.as_ptr()) }
    }

    /// Returns the minimum contiguity in bytes guaranteed by this BTI.
    pub fn minimum_contiguity(&self) -> u64 {
        // SAFETY: `self.raw` is a valid C++ `iommu::Bti` reference.
        unsafe { cpp_smmu_bti_minimum_contiguity(self.raw.as_ptr()) }
    }

    /// Returns the total address space size in bytes for this BTI.
    pub fn aspace_size(&self) -> u64 {
        // SAFETY: `self.raw` is a valid C++ `iommu::Bti` reference.
        unsafe { cpp_smmu_bti_aspace_size(self.raw.as_ptr()) }
    }

    /// Returns the 64-bit BTI identifier.
    pub fn bti_id(&self) -> u64 {
        // SAFETY: `self.raw` is a valid C++ `iommu::Bti` reference.
        unsafe { cpp_smmu_bti_id(self.raw.as_ptr()) }
    }

    /// Writes the NUL-terminated debug name of this BTI into `out_name`.
    pub fn get_name(&self, out_name: &mut [u8; ZX_MAX_NAME_LEN]) {
        // SAFETY: `self.raw` is valid and `out_name` has `ZX_MAX_NAME_LEN` bytes.
        unsafe {
            cpp_smmu_bti_get_name(self.raw.as_ptr(), out_name.as_mut_ptr().cast());
        }
    }

    /// Sets the debug name of this BTI.
    pub fn set_name(&self, name: &[u8]) {
        // SAFETY: `self.raw` and `name` are valid for the duration of the FFI call.
        unsafe {
            cpp_smmu_bti_set_name(self.raw.as_ptr(), name.as_ptr().cast(), name.len());
        }
    }

    /// Pins and maps a C++ `PinnedVmObject` into this BTI, consuming the `PinnedVmObject`.
    ///
    /// # Safety
    ///
    /// `pinned_vmo` must point to a valid, mutable C++ `PinnedVmObject` that can be moved from.
    pub unsafe fn map_raw(
        &self,
        pinned_vmo: *mut c_void,
        perms: u32,
        require_contiguous: bool,
    ) -> Result<CppPmtWrapper, Status> {
        let mut raw_pmt: *mut CppSmmuPmt = core::ptr::null_mut();
        // SAFETY: Caller guarantees `pinned_vmo` is valid, and `self.raw` and `raw_pmt` are valid.
        let status = unsafe {
            cpp_smmu_bti_map(
                self.raw.as_ptr(),
                pinned_vmo,
                perms,
                require_contiguous,
                &raw mut raw_pmt,
            )
        };
        Status::ok(status)?;
        NonNull::new(raw_pmt).map(|raw| CppPmtWrapper { raw }).ok_or(Status::INTERNAL)
    }
}

impl Drop for CppBtiWrapper {
    fn drop(&mut self) {
        // SAFETY: `self.raw` is a valid C++ `iommu::Bti` reference owned by `self`.
        unsafe {
            cpp_smmu_bti_release(self.raw.as_ptr());
        }
    }
}

/// Safe Rust wrapper around a C++ `arm_smmu::SmmuPmt` handle for `Rust -> C++` tests.
pub struct CppPmtWrapper {
    raw: NonNull<CppSmmuPmt>,
}

impl CppPmtWrapper {
    /// Queries the mapped IOVA and contiguous byte count for `[offset, offset + size)`.
    pub fn query_address(&self, offset: u64, size: usize) -> Result<QueryAddressResult, Status> {
        let mut device_vaddr = 0u64;
        let mut mapped_size = 0usize;
        // SAFETY: `self.raw`, `device_vaddr`, and `mapped_size` are valid pointers for the call.
        let status = unsafe {
            cpp_smmu_pmt_query_address(
                self.raw.as_ptr(),
                offset,
                size,
                &raw mut device_vaddr,
                &raw mut mapped_size,
            )
        };
        Status::ok(status)?;
        Ok(QueryAddressResult { device_vaddr, size: mapped_size })
    }

    /// Unpins and unmaps the memory represented by this PMT (`zx_pmt_unpin`).
    pub fn release_pinned_memory(&self) {
        // SAFETY: `self.raw` is a valid C++ `iommu::Pmt` reference.
        unsafe {
            cpp_smmu_pmt_release_pinned_memory(self.raw.as_ptr());
        }
    }

    /// Notifies this PMT that its user-mode dispatcher reached zero handles and consumes this
    /// handle so `OnDispatcherZeroHandles` cannot be invoked more than once.
    pub fn on_dispatcher_zero_handles(self) {
        // SAFETY: `self.raw` is a valid C++ `iommu::Pmt` reference whose dispatcher has not yet
        // reached zero handles. `self` drops immediately afterward, releasing `self.raw`.
        unsafe {
            cpp_smmu_pmt_on_zero_handles(self.raw.as_ptr());
        }
    }
}

impl Drop for CppPmtWrapper {
    fn drop(&mut self) {
        // SAFETY: `self.raw` is a valid C++ `iommu::Pmt` reference owned by `self`.
        unsafe {
            cpp_smmu_pmt_release(self.raw.as_ptr());
        }
    }
}
