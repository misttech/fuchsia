// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! arm64 thread state.

use core::mem::offset_of;

use arm64_thread_bindings::{CURRENT_MDSCR_OFFSET, CURRENT_PERCPU_PTR_OFFSET};

use crate::kernel::thread::{Thread, get_arch};

/// Architecture-specific thread state (`struct arch_thread`).
type ArchThread = arm64_thread_bindings::arch_thread;

// `arch_thread` has `shadow_call_sp` only when the shadow call stack is on, as it is for kernel
// Rust. CURRENT_MDSCR_OFFSET is 56 only with that field, so bindgen's libclang saw it on too.
zr::static_assert!(cfg!(sanitize = "shadow-call-stack"));
zr::static_assert!(CURRENT_MDSCR_OFFSET == 56);

// The assembly offsets are relative to `thread_pointer_location`, where TPIDR_EL1 points.
zr::static_assert!(
    offset_of!(ArchThread, current_percpu_ptr) as isize
        - offset_of!(ArchThread, __bindgen_anon_1.thread_pointer_location) as isize
        == CURRENT_PERCPU_PTR_OFFSET as isize
);
zr::static_assert!(
    offset_of!(ArchThread, mdscr_el1) as isize
        - offset_of!(ArchThread, __bindgen_anon_1.thread_pointer_location) as isize
        == CURRENT_MDSCR_OFFSET as isize
);

/// Returns a pointer to the arm64 architectural state embedded in `thread`.
///
/// The pointer is only offset from `thread`, never read through, so this works on a thread that
/// is not yet fully constructed.
///
/// # Safety
///
/// `thread` points to a C++ `Thread`.
#[inline(always)]
pub(super) unsafe fn thread_arch(thread: *mut Thread) -> *mut ArchThread {
    // SAFETY: the caller guarantees `thread`, and the facade only offsets the pointer.
    unsafe { get_arch(thread).cast() }
}

/// [`thread_arch`] for a `*const Thread`, whose arch state is only read.
///
/// # Safety
///
/// As for [`thread_arch`].
#[inline(always)]
pub(super) unsafe fn thread_arch_const(thread: *const Thread) -> *const ArchThread {
    // SAFETY: As for `thread_arch`, which only offsets the pointer: nothing is written through it.
    unsafe { thread_arch(thread.cast_mut()) }.cast_const()
}
