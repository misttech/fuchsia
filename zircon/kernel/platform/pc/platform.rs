// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::platform_rs::crashlog::PlatformCrashlog;
use crate::platform_rs::efi_crashlog::EfiCrashlog;
use crate::platform_rs::mapped_crashlog::MappedCrashlog;
use core::ffi::{c_char, c_void};
use core::pin::Pin;
use core::ptr::slice_from_raw_parts;
use lazy_init::{LazyInit, NoCheck};

static MAPPED_CRASHLOG: LazyInit<MappedCrashlog, NoCheck> = LazyInit::uninit();
static EFI_CRASHLOG: EfiCrashlog = EfiCrashlog::new();

// TODO(https://fxbug.dev/576428709): Remove once `platform_save_bootloader_data` is ported to Rust.
/// Records the location of any previous EFI crashlog handed off by the bootloader.
///
/// # Safety
///
/// If `len > 0`, `ptr` must point to a valid, immutable byte slice of `len` bytes that remains
/// valid for all subsequent calls to `recover`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_efi_crashlog_set_last_crashlog_location(
    ptr: *const c_char,
    len: usize,
) {
    // SAFETY: The caller guarantees that if `len > 0`, `ptr` points to a valid, immutable byte
    // slice of `len` bytes that remains valid for all subsequent calls to `recover`.
    unsafe {
        EFI_CRASHLOG.set_last_crashlog_location(slice_from_raw_parts(ptr.cast::<u8>(), len));
    }
}

// TODO(https://fxbug.dev/576428709): Remove once `platform_save_bootloader_data` is ported to Rust.
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

// TODO(https://fxbug.dev/576428709): Remove once `platform_init_crashlog` is ported to Rust.
/// Binds the [`EfiCrashlog`] implementation as the [`PlatformCrashlog`] implementation.
#[unsafe(no_mangle)]
pub extern "C" fn rust_efi_crashlog_bind() {
    let _ = PlatformCrashlog::bind(&EFI_CRASHLOG);
}
