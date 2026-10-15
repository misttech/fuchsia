// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_char;
use object_constants_rs as object_constants;
use zx_status::Status;

#[allow(unused_imports)]
pub use debuglog_types::{dlog_header_t, dlog_record_t};
pub const DLOG_MAX_RECORD: usize = debuglog_types::DLOG_MAX_RECORD;
pub const DLOG_MAX_DATA: usize = debuglog_types::DLOG_MAX_DATA;

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn cpp_dlog_shutdown(deadline: crate::platform_rs::timer::InstantMono) -> i32;
    fn cpp_dlog_write(severity: u32, flags: u32, ptr: *const u8, len: usize) -> i32;
    fn cpp_dlog_reader_init(
        reader: *mut DlogReaderStorage,
        notify: unsafe extern "C" fn(*mut core::ffi::c_void),
        cookie: *mut core::ffi::c_void,
    );
    fn cpp_dlog_reader_disconnect(reader: *mut DlogReaderStorage);
    fn cpp_dlog_reader_read(
        reader: *mut DlogReaderStorage,
        flags: u32,
        record: *mut dlog_record_t,
        actual: *mut usize,
    ) -> i32;
    fn cpp_dlog_serial_write(ptr: *const c_char, len: usize);
    fn cpp_dlog_serial_write_interruptible(ptr: *const c_char, len: usize) -> i32;
    fn cpp_dlog_sync();
    fn cpp_dlog_panic_start();
    fn cpp_dlog_bluescreen_init();
}

/// Shutdown the debuglog subsystem.
///
/// Blocks, waiting up to |deadline|, for dlog threads to terminate.
pub fn dlog_shutdown(deadline: crate::platform_rs::timer::InstantMono) -> Result<(), Status> {
    // SAFETY: FFI call to C++ dlog shutdown.
    Status::ok(unsafe { cpp_dlog_shutdown(deadline) })
}

/// Writes `bytes` to debuglog.
///
/// Returns `Status::BAD_STATE` if the dlog has already started shutting down.
pub fn dlog_write(severity: u32, flags: u32, bytes: &[u8]) -> Result<(), Status> {
    // SAFETY: `bytes.as_ptr()` points to `bytes.len()` initialized bytes.
    Status::ok(unsafe { cpp_dlog_write(severity, flags, bytes.as_ptr(), bytes.len()) })
}

/// Writes `bytes` directly to the serial debug port, synchronized with the debuglog serial lock.
pub fn dlog_serial_write(bytes: &[u8]) {
    // SAFETY: `bytes.as_ptr()` points to `bytes.len()` initialized bytes.
    unsafe { cpp_dlog_serial_write(bytes.as_ptr() as *const c_char, bytes.len()) }
}

/// Like `dlog_serial_write`, except that if the calling thread must wait for the serial debug port
/// to be resumed (see `zx_debug_suspend`), the wait may be interrupted by the thread being killed
/// (`ZX_ERR_INTERNAL_INTR_KILLED`) or suspended (`ZX_ERR_INTERNAL_INTR_RETRY`), in which case
/// nothing was written.
pub fn dlog_serial_write_interruptible(bytes: &[u8]) -> Result<(), Status> {
    // SAFETY: `bytes.as_ptr()` points to `bytes.len()` initialized bytes.
    Status::ok(unsafe {
        cpp_dlog_serial_write_interruptible(bytes.as_ptr() as *const c_char, bytes.len())
    })
}

/// Blocks until the debuglog dumper thread has dumped all queued log messages. Must only be called
/// from a thread context where blocking is permitted.
pub fn dlog_sync() {
    // SAFETY: FFI call to dlog_sync which is thread-safe and safely handles uninitialized or shutdown states.
    unsafe { cpp_dlog_sync() }
}

/// Informs the debuglog that a panic is in progress, so it stops depending on other threads.
pub fn dlog_panic_start() {
    // SAFETY: takes no arguments; the debuglog owns all the state it changes.
    unsafe { cpp_dlog_panic_start() }
}

/// Switches the debuglog to bluescreen mode for the panic message that follows.
pub fn dlog_bluescreen_init() {
    // SAFETY: takes no arguments; the debuglog owns all the state it changes.
    unsafe { cpp_dlog_bluescreen_init() }
}

#[repr(C, align(8))]
pub struct DlogReaderStorage {
    inner: zr::OpaqueBytes<{ object_constants::kDlogReaderStorageSize }>,
    _pinned: core::marker::PhantomPinned,
}

zr::static_assert_size_and_align!(
    DlogReaderStorage,
    object_constants::kDlogReaderStorageSize,
    object_constants::kDlogReaderStorageAlign,
);

impl DlogReaderStorage {
    /// Creates a `PinInit` initializer for `DlogReaderStorage`.
    ///
    /// Since `DlogReader`s typically capture containing objects via `cookie`, they
    /// use 2-phase initialization to avoid races in the construction of the
    /// `DlogReader` and the containing object.
    ///
    /// # Safety
    ///
    /// The caller must ensure `cookie` is valid for `notify`.
    pub unsafe fn init(
        flags: u32,
        notify: unsafe extern "C" fn(*mut core::ffi::c_void),
        cookie: *mut core::ffi::c_void,
    ) -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        // SAFETY: `slot` is valid memory for `DlogReaderStorage`.
        unsafe {
            pin_init::pin_init_from_closure(move |slot: *mut Self| {
                let reader = &mut *slot;
                *reader = Self {
                    inner: zr::OpaqueBytes::new([0; object_constants::kDlogReaderStorageSize]),
                    _pinned: core::marker::PhantomPinned,
                };
                if (flags & zx_types::ZX_LOG_FLAG_READABLE) != 0 {
                    // SAFETY: `reader` is pinned and points to valid storage for DlogReader.
                    cpp_dlog_reader_init(reader, notify, cookie);
                }
                Ok(())
            })
        }
    }

    /// Safely disconnects DlogReader.
    ///
    /// DlogReaders must be manually stopped via `disconnect` to avoid reentrancy
    /// issues in the DlogReader and its containing object.
    ///
    /// This method must be called before the `DlogReaderStorage` is dropped if it
    /// was initialized.
    pub fn disconnect(self: core::pin::Pin<&mut Self>) {
        // SAFETY: `self` is pinned and initialized.
        unsafe { cpp_dlog_reader_disconnect(self.get_unchecked_mut()) }
    }

    /// Reads one record from debuglog.
    ///
    /// Upon success, returns `Ok(())` and sets `actual` to the record's size.
    /// Returns `Err(Status::SHOULD_WAIT)` if there are no records available to read.
    pub fn read(
        self: core::pin::Pin<&mut Self>,
        flags: u32,
        record: &mut dlog_record_t,
        actual: &mut usize,
    ) -> Result<(), Status> {
        // SAFETY: `self` is pinned and initialized, `record` and `actual` are valid.
        Status::ok(unsafe { cpp_dlog_reader_read(self.get_unchecked_mut(), flags, record, actual) })
    }
}
