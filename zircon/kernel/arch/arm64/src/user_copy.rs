// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Copies between user and kernel memory that catch faults on the user side.

use core::arch::asm;
use core::ffi::c_void;
use core::mem::{align_of, offset_of, size_of};

use arm64_thread_bindings::{
    ARM64_DFR_RUN_ACCESS_FAULT_HANDLER_BIT, ARM64_DFR_RUN_FAULT_HANDLER_BIT,
};
use zx_status::Status;

use super::feature::arm64_mmu_features;
use super::thread::thread_arch;
use super::vm::is_user_accessible_range;
use crate::arch_rs::ops::num_spinlocks_held;
use crate::arch_rs::{CopyContext, FaultInfo, UserCopyCaptureFaultsError, blocking_disallowed};
use crate::kernel::thread::current_get;
use crate::user_copy::internal::validate_user_accessible_range;

/// The return value of `_arm64_user_copy` (`struct Arm64UserCopyRet`).
type Arm64UserCopyRet = arm64_user_copy_bindings::Arm64UserCopyRet;

/// A user copy's result as the capture-faults exports return it to C++
/// (`struct Arm64UserCopyCaptureFaultsRet`).
type Arm64UserCopyCaptureFaultsRet = arm64_user_copy_bindings::Arm64UserCopyCaptureFaultsRet;

// `_arm64_user_copy` returns the status in w0, the fault flags in the upper half of x0 and the
// fault address in x1.
zr::static_assert!(size_of::<Arm64UserCopyRet>() == 16);
zr::static_assert!(align_of::<Arm64UserCopyRet>() == 8);
zr::static_assert!(offset_of!(Arm64UserCopyRet, status) == 0);
zr::static_assert!(offset_of!(Arm64UserCopyRet, pf_flags) == 4);
zr::static_assert!(offset_of!(Arm64UserCopyRet, pf_va) == 8);
zr::static_assert!(size_of::<Arm64UserCopyCaptureFaultsRet>() == 24);
zr::static_assert!(align_of::<Arm64UserCopyCaptureFaultsRet>() == 8);

// The data fault handler reads these bits of `data_fault_resume`.
zr::static_assert!(ARM64_DFR_RUN_FAULT_HANDLER_BIT == 63);
zr::static_assert!(ARM64_DFR_RUN_ACCESS_FAULT_HANDLER_BIT == 62);

const ARM64_USER_COPY_CAPTURE_ALL_FAULTS: u64 =
    !((1u64 << ARM64_DFR_RUN_ACCESS_FAULT_HANDLER_BIT) | (1u64 << ARM64_DFR_RUN_FAULT_HANDLER_BIT));
const ARM64_USER_COPY_CAPTURE_PAGE_FAULTS: u64 = !(1u64 << ARM64_DFR_RUN_FAULT_HANDLER_BIT);
const ARM64_USER_COPY_DO_FAULTS: u64 = !0u64;

unsafe extern "C" {
    /// The fault-handling memcpy in `user-copy.S`; see `user-copy.h`.
    fn _arm64_user_copy(
        dst: *mut c_void,
        src: *const c_void,
        len: usize,
        fault_return: *mut u64,
        fault_return_mask: u64,
    ) -> Arm64UserCopyRet;
}

// Shortcuts for setting and clearing PSTATE.PAN. `.arch_extension pan` lets the assembler accept
// the instructions without giving these functions a target feature, which would keep them from
// inlining into their callers.

/// Sets PSTATE.PAN.
#[inline(always)]
fn arm64_enable_pan() {
    // SAFETY: The instruction writes only PSTATE.PAN, which decides whether EL1 accesses to
    // EL0-accessible memory fault. It is UNDEFINED without FEAT_PAN; the only caller checks
    // `arm64_mmu_features.pan` first.
    unsafe { asm!(".arch_extension pan", "msr pan, #1", options(nostack, preserves_flags)) }
}

/// Clears PSTATE.PAN.
#[inline(always)]
fn arm64_disable_pan() {
    // SAFETY: The instruction writes only PSTATE.PAN, which decides whether EL1 accesses to
    // EL0-accessible memory fault. It is UNDEFINED without FEAT_PAN; the only caller checks
    // `arm64_mmu_features.pan` first.
    unsafe { asm!(".arch_extension pan", "msr pan, #0", options(nostack, preserves_flags)) }
}

/// Wrapper function to make sure we disable and enable pan around each copy operation
///
/// # Safety
///
/// The kernel side of the copy is valid for `len` bytes, the user side lies in the user half of
/// the address space, and `fault_return` is the current thread's `data_fault_resume`.
#[inline(always)]
unsafe fn arm64_user_copy_pan_wrapped(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
    fault_return: *mut u64,
    fault_return_mask: u64,
) -> Arm64UserCopyRet {
    // SAFETY: CPU 0's `arm64_feature_init` is the only writer of `arm64_mmu_features`, and it runs
    // before any other CPU starts and before any user copy.
    if unsafe { arm64_mmu_features.pan } {
        arm64_disable_pan();
    }
    // SAFETY: The caller guarantees the buffers and `fault_return`. A fault on the user side is
    // resolved by the fault handler or resumes at `fault_return`.
    let ret = unsafe { _arm64_user_copy(dst, src, len, fault_return, fault_return_mask) };
    // SAFETY: As above.
    if unsafe { arm64_mmu_features.pan } {
        arm64_enable_pan();
    }
    ret
}

/// Copies `len` bytes from user memory at `src` into kernel memory at `dst`.
///
/// Returns `Status::INVALID_ARGS` if `src` is not a user range or the copy faults. Changes to the
/// return value are observable by user-space.
///
/// # Safety
///
/// `dst` is valid for writes of `len` bytes. The caller must be able to block, and must hold no
/// spinlock and no lock that lockdep tracks: a page fault on `src` may block.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_copy_from_user(
    dst: *mut c_void,
    mut src: *const c_void,
    mut len: usize,
) -> Result<(), Status> {
    debug_assert!(!blocking_disallowed());
    debug_assert!(num_spinlocks_held() == 0);
    // The copy might trigger a page fault that needs to block on a user pager request, in which
    // case it is not permitted to be holding locks. To ensure this doesn't accidentally work if a
    // copy does not happen to trigger a pager, we insist that all copies do not hold locks. This
    // method is an empty inline function if lockdep is not enabled.
    lockdep::assert_no_locks_held();

    // The assembly code just does memcpy with fault handling.  This is
    // the security check that an address from the user is actually a
    // valid userspace address so users can't access kernel memory.
    if !is_user_accessible_range(src.addr(), len) {
        return Err(Status::INVALID_ARGS);
    }
    // Spectre V1: Confine {src, len} to user addresses to prevent the kernel from speculatively
    // reading user-controlled addresses.
    let mut src_addr = src.addr();
    validate_user_accessible_range(&mut src_addr, &mut len);
    src = src.with_addr(src_addr);

    // SAFETY: `current_get()` is the running thread, a live `Thread`, and taking the field's
    // address reads nothing.
    let data_fault_resume = unsafe { &raw mut (*thread_arch(current_get())).data_fault_resume };
    // SAFETY: The caller guarantees `dst`, `{src, len}` is confined to the user half, and
    // `data_fault_resume` is the current thread's.
    let ret = unsafe {
        arm64_user_copy_pan_wrapped(dst, src, len, data_fault_resume, ARM64_USER_COPY_DO_FAULTS)
    }
    .status;

    Status::ok(ret)
}

/// Copies `len` bytes from kernel memory at `src` into user memory at `dst`.
///
/// Returns `Status::INVALID_ARGS` if `dst` is not a user range or the copy faults. Changes to the
/// return value are observable by user-space.
///
/// # Safety
///
/// `src` is valid for reads of `len` initialized bytes. The caller must be able to block, and must
/// hold no spinlock and no lock that lockdep tracks: a page fault on `dst` may block.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_copy_to_user(
    mut dst: *mut c_void,
    src: *const c_void,
    mut len: usize,
) -> Result<(), Status> {
    debug_assert!(!blocking_disallowed());
    debug_assert!(num_spinlocks_held() == 0);
    // See comment above.
    lockdep::assert_no_locks_held();

    if !is_user_accessible_range(dst.addr(), len) {
        return Err(Status::INVALID_ARGS);
    }

    // Spectre V1.1: Confine {dst, len} to user addresses to prevent the kernel from speculatively
    // storing to user-controlled addresses.
    let mut dst_addr = dst.addr();
    validate_user_accessible_range(&mut dst_addr, &mut len);
    dst = dst.with_addr(dst_addr);

    // SAFETY: `current_get()` is the running thread, a live `Thread`, and taking the field's
    // address reads nothing.
    let data_fault_resume = unsafe { &raw mut (*thread_arch(current_get())).data_fault_resume };
    // SAFETY: The caller guarantees `src`, `{dst, len}` is confined to the user half, and
    // `data_fault_resume` is the current thread's.
    let ret = unsafe {
        arm64_user_copy_pan_wrapped(dst, src, len, data_fault_resume, ARM64_USER_COPY_DO_FAULTS)
    }
    .status;

    Status::ok(ret)
}

/// The body of [`arch_copy_from_user_capture_faults`] and its C++ export.
///
/// # Safety
///
/// As for [`arch_copy_from_user_capture_faults`].
#[inline(always)]
unsafe fn copy_from_user_capture_faults(
    dst: *mut c_void,
    mut src: *const c_void,
    mut len: usize,
    context: CopyContext,
) -> Result<(), UserCopyCaptureFaultsError> {
    // The assembly code just does memcpy with fault handling.  This is
    // the security check that an address from the user is actually a
    // valid userspace address so users can't access kernel memory.
    if !is_user_accessible_range(src.addr(), len) {
        return Err(Status::INVALID_ARGS.into());
    }
    // Spectre V1: Confine {src, len} to user addresses to prevent the kernel from speculatively
    // reading user-controlled addresses.
    let mut src_addr = src.addr();
    validate_user_accessible_range(&mut src_addr, &mut len);
    src = src.with_addr(src_addr);

    // Ensure that the calling thread is either holding no spinlocks, or has correctly specified a
    // non-blocking copy context.
    debug_assert!(num_spinlocks_held() == 0 || context == CopyContext::BlockingNotAllowed);
    let mut fault_return_mask = ARM64_USER_COPY_CAPTURE_PAGE_FAULTS;
    if context == CopyContext::BlockingNotAllowed {
        fault_return_mask = ARM64_USER_COPY_CAPTURE_ALL_FAULTS;
    }

    // SAFETY: `current_get()` is the running thread, a live `Thread`, and taking the field's
    // address reads nothing.
    let data_fault_resume = unsafe { &raw mut (*thread_arch(current_get())).data_fault_resume };
    // SAFETY: The caller guarantees `dst`, `{src, len}` is confined to the user half, and
    // `data_fault_resume` is the current thread's.
    let ret =
        unsafe { arm64_user_copy_pan_wrapped(dst, src, len, data_fault_resume, fault_return_mask) };

    // A return result of ZX_ERR_INVALID_ARGS means both that a fault occurred and there is valid
    // fault data to resolve it. Any other status is either success (ZX_OK), or a fault without
    // data.
    if ret.status == Status::INVALID_ARGS.into_raw() {
        return Err(UserCopyCaptureFaultsError {
            status: Status::INVALID_ARGS,
            fault_info: Some(FaultInfo { pf_va: ret.pf_va, pf_flags: ret.pf_flags }),
        });
    }
    Status::ok(ret.status).map_err(UserCopyCaptureFaultsError::from)
}

/// The body of [`arch_copy_to_user_capture_faults`] and its C++ export.
///
/// # Safety
///
/// As for [`arch_copy_to_user_capture_faults`].
#[inline(always)]
unsafe fn copy_to_user_capture_faults(
    mut dst: *mut c_void,
    src: *const c_void,
    mut len: usize,
    context: CopyContext,
) -> Result<(), UserCopyCaptureFaultsError> {
    if !is_user_accessible_range(dst.addr(), len) {
        return Err(Status::INVALID_ARGS.into());
    }

    // Spectre V1.1: Confine {dst, len} to user addresses to prevent the kernel from speculatively
    // storing to user-controlled addresses.
    let mut dst_addr = dst.addr();
    validate_user_accessible_range(&mut dst_addr, &mut len);
    dst = dst.with_addr(dst_addr);

    // Ensure that the calling thread is either holding no spinlocks, or has correctly specified a
    // non-blocking copy context.
    debug_assert!(num_spinlocks_held() == 0 || context == CopyContext::BlockingNotAllowed);
    let mut fault_return_mask = ARM64_USER_COPY_CAPTURE_PAGE_FAULTS;
    if context == CopyContext::BlockingNotAllowed {
        fault_return_mask = ARM64_USER_COPY_CAPTURE_ALL_FAULTS;
    }

    // SAFETY: `current_get()` is the running thread, a live `Thread`, and taking the field's
    // address reads nothing.
    let data_fault_resume = unsafe { &raw mut (*thread_arch(current_get())).data_fault_resume };
    // SAFETY: The caller guarantees `src`, `{dst, len}` is confined to the user half, and
    // `data_fault_resume` is the current thread's.
    let ret =
        unsafe { arm64_user_copy_pan_wrapped(dst, src, len, data_fault_resume, fault_return_mask) };

    // A return result of ZX_ERR_INVALID_ARGS means both that a fault occurred and there is valid
    // fault data to resolve it. Any other status is either success (ZX_OK), or a fault without
    // data.
    if ret.status == Status::INVALID_ARGS.into_raw() {
        return Err(UserCopyCaptureFaultsError {
            status: Status::INVALID_ARGS,
            fault_info: Some(FaultInfo { pf_va: ret.pf_va, pf_flags: ret.pf_flags }),
        });
    }
    Status::ok(ret.status).map_err(UserCopyCaptureFaultsError::from)
}

/// Copies `len` bytes from user memory at `src` into kernel memory at `dst`, returning any fault
/// instead of resolving it.
///
/// With `CopyContext::BlockingAllowed` only page faults are captured, and access faults are still
/// resolved; with `CopyContext::BlockingNotAllowed` every fault is captured. The error carries
/// fault information only if the copy faulted with data to resolve the fault.
///
/// # Safety
///
/// `dst` is valid for writes of `len` bytes, and the caller holds no spinlock unless `context` is
/// `CopyContext::BlockingNotAllowed`.
pub unsafe fn arch_copy_from_user_capture_faults(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
    context: CopyContext,
) -> Result<(), UserCopyCaptureFaultsError> {
    // SAFETY: The caller upholds the copy's requirements.
    unsafe { copy_from_user_capture_faults(dst, src, len, context) }
}

/// Copies `len` bytes from kernel memory at `src` into user memory at `dst`, returning any fault
/// instead of resolving it.
///
/// With `CopyContext::BlockingAllowed` only page faults are captured, and access faults are still
/// resolved; with `CopyContext::BlockingNotAllowed` every fault is captured. The error carries
/// fault information only if the copy faulted with data to resolve the fault.
///
/// # Safety
///
/// `src` is valid for reads of `len` initialized bytes, and the caller holds no spinlock unless
/// `context` is `CopyContext::BlockingNotAllowed`.
pub unsafe fn arch_copy_to_user_capture_faults(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
    context: CopyContext,
) -> Result<(), UserCopyCaptureFaultsError> {
    // SAFETY: The caller upholds the copy's requirements.
    unsafe { copy_to_user_capture_faults(dst, src, len, context) }
}

/// Flattens a capture-faults copy's result for C++, which rebuilds the
/// `UserCopyCaptureFaultsResult` from it.
#[inline(always)]
fn capture_faults_ret(
    result: Result<(), UserCopyCaptureFaultsError>,
) -> Arm64UserCopyCaptureFaultsRet {
    let (status, fault_info) = match result {
        Ok(()) => (Status::result_into_raw(Ok(())), None),
        Err(UserCopyCaptureFaultsError { status, fault_info }) => (status.into_raw(), fault_info),
    };
    let FaultInfo { pf_va, pf_flags } = fault_info.unwrap_or(FaultInfo { pf_va: 0, pf_flags: 0 });
    Arm64UserCopyCaptureFaultsRet {
        copy: Arm64UserCopyRet { status, pf_flags, pf_va },
        has_fault_info: fault_info.is_some(),
    }
}

/// [`arch_copy_from_user_capture_faults`] for the C++ function of the same name.
///
/// # Safety
///
/// As for [`arch_copy_from_user_capture_faults`], and `context` holds one of `CopyContext`'s
/// enumerators.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_copy_from_user_capture_faults(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
    context: CopyContext,
) -> Arm64UserCopyCaptureFaultsRet {
    // SAFETY: The caller upholds the copy's requirements.
    capture_faults_ret(unsafe { copy_from_user_capture_faults(dst, src, len, context) })
}

/// [`arch_copy_to_user_capture_faults`] for the C++ function of the same name.
///
/// # Safety
///
/// As for [`arch_copy_to_user_capture_faults`], and `context` holds one of `CopyContext`'s
/// enumerators.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_copy_to_user_capture_faults(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
    context: CopyContext,
) -> Arm64UserCopyCaptureFaultsRet {
    // SAFETY: The caller upholds the copy's requirements.
    capture_faults_ret(unsafe { copy_to_user_capture_faults(dst, src, len, context) })
}
