// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[cfg(console_enabled)]
pub mod console;
#[cfg(not(console_enabled))]
use {debug as _, kprint as _, pretty as _, zerocopy as _, zx_status as _};

use core::ffi::c_void;
use core::ptr::NonNull;

/// Hardware RNG interface.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct HwRngOps {
    /// Callback to draw entropy from the hardware RNG.
    pub hw_rng_get_entropy: unsafe extern "C" fn(buf: Option<NonNull<c_void>>, len: usize) -> usize,
}

zr::static_assert_size_and_align!(HwRngOps, 8, 8);

static mut HW_RNG_OPS: Option<&'static HwRngOps> = None;

/// Draw entropy from hardware RNG.
///
/// The caller is responsible to check that the return value equals `buf.len()`.
/// Otherwise it means the operation failed.
#[must_use]
pub fn hw_rng_get_entropy(buf: &mut [u8]) -> usize {
    // SAFETY: Mutation only occurs during early boot registration before multi-threading and
    // remains immutable thereafter.
    if let Some(ops) = unsafe { HW_RNG_OPS } {
        // SAFETY: When `buf.len() > 0`, `buf.as_mut_ptr()` points to `buf.len()` valid writable
        // bytes. When `buf.len() == 0`, `buf.as_mut_ptr()` is guaranteed non-null and aligned by
        // Rust slice invariants (e.g., dangling pointer `0x1`), writing 0 bytes requires 0
        // allocated bytes, and hardware RNG drivers perform no writes when `len == 0`. In both
        // cases, `NonNull::new(buf.as_mut_ptr().cast())` yields a valid pointer for writes of
        // `buf.len()` bytes, and `hw_rng_register` requires `ops.hw_rng_get_entropy` to be a valid
        // callback.
        unsafe { (ops.hw_rng_get_entropy)(NonNull::new(buf.as_mut_ptr().cast()), buf.len()) }
    } else {
        0
    }
}

/// Register the ops of hardware RNG with HW RNG driver.
///
/// # Safety
///
/// - Must only be called during early boot single-threaded initialization before secondary
///   CPUs and multi-threading are started, and must not be called concurrently.
/// - `new_ops.hw_rng_get_entropy` must be a valid function pointer to a hardware RNG callback.
pub unsafe fn hw_rng_register(new_ops: &'static HwRngOps) {
    // SAFETY: Called during early boot registration before multi-threading.
    unsafe {
        HW_RNG_OPS = Some(new_ops);
    }
}

/// Return whether there is a functioning hardware RNG.
#[must_use]
pub fn hw_rng_is_registered() -> bool {
    // SAFETY: Mutation only occurs during early boot registration before multi-threading and
    // remains immutable thereafter.
    unsafe { HW_RNG_OPS }.is_some()
}

/// Draw entropy from hardware RNG.
///
/// The caller is responsible to check that the return value equals `len`.
/// Otherwise it means the operation failed.
///
/// # Safety
///
/// If `len > 0`, `buf` must be non-null, point to writable memory of at least `len` bytes,
/// and not be accessed concurrently by any other thread for the duration of the call.
#[unsafe(no_mangle)]
#[must_use]
pub unsafe extern "C" fn rust_hw_rng_get_entropy(buf: *mut c_void, len: usize) -> usize {
    assert!(len == 0 || !buf.is_null(), "buf must not be null when len > 0");

    // SAFETY: When `len > 0`, `buf` was verified non-null and the caller guarantees it points
    // to writable memory of at least `len` bytes with no concurrent access. When `len == 0`,
    // `zr::slice_from_raw_parts_mut` safely handles any pointer (including null) by returning
    // `&mut []`.
    let slice = unsafe { zr::slice_from_raw_parts_mut(buf.cast::<u8>(), len) };
    hw_rng_get_entropy(slice)
}

/// Register the ops of hardware RNG with HW RNG driver.
///
/// # Safety
///
/// - Must only be called during early boot single-threaded initialization before secondary
///   CPUs and multi-threading are started, and must not be called concurrently.
/// - `new_ops` must not be null and must point to a valid, initialized `HwRngOps` that
///   remains allocated and unmodified for the kernel lifetime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_hw_rng_register(new_ops: Option<&'static HwRngOps>) {
    let new_ops = new_ops.expect("new_ops must not be null");
    // SAFETY: Caller guarantees `new_ops` points to a valid `HwRngOps` and that registration
    // occurs during early boot before multi-threading.
    unsafe { hw_rng_register(new_ops) };
}

/// Return whether there is a functioning hardware RNG.
#[unsafe(no_mangle)]
#[must_use]
pub extern "C" fn rust_hw_rng_is_registered() -> bool {
    hw_rng_is_registered()
}
