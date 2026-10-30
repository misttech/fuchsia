// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::platform_rs::crashlog::PlatformCrashlog;
use crate::platform_rs::mapped_crashlog::MappedCrashlog;
use core::ffi::c_void;
use core::pin::Pin;
use lazy_init::{LazyInit, NoCheck};

static MAPPED_CRASHLOG: LazyInit<MappedCrashlog, NoCheck> = LazyInit::uninit();

// TODO(https://fxbug.dev/507423574): Remove once `allocate_persistent_ram` is ported to Rust.
/// Initializes the platform's [`MappedCrashlog`] with `[base, base + size)` and binds it as the
/// [`PlatformCrashlog`] implementation.
///
/// # Safety
///
/// If `size > 0`, `base` must be 8-byte aligned and point to a valid, mutable persistent RAM
/// region of `size` bytes that remains mapped and exclusively owned by [`MappedCrashlog`] for the
/// lifetime of the kernel. Must be called at most once during single-threaded early boot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_mapped_crashlog_bind(base: *mut c_void, size: usize) {
    // SAFETY: The caller upholds the pointer validity, alignment, exclusive ownership,
    // single-threaded early-boot initialization, and `'static` lifetime requirements.
    unsafe {
        let range = zr::slice_from_raw_parts_mut(base.cast::<u8>(), size);
        let _ = Pin::static_ref(&MAPPED_CRASHLOG).init_pin(MappedCrashlog::new(range));
        let _ = PlatformCrashlog::bind(MAPPED_CRASHLOG.get_unchecked());
    }
}
