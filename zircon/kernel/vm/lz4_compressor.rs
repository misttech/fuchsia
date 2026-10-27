// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::vm::compression::{StrategyCompressResult, VmCompressionStrategy};
use crate::vm::physmap::paddr_to_physmap;
use crate::vm::vm::vm_get_zero_page_paddr;
use boot_options::BootOptions;
use core::ffi::{c_char, c_int, c_void};
use core::mem::MaybeUninit;
use core::pin::Pin;
use kalloc::Box;
use kprint::kprintln;
use ksync::{KMutex, RawMutex, guarded, kcell_init};
use lz4_sys::{
    LZ4_MAX_INPUT_SIZE, LZ4_STREAM_MINSIZE, LZ4_compress_fast_extState_fastReset,
    LZ4_decompress_safe, LZ4_initStream, LZ4_stream_t,
};
use pin_init::{PinInit, pin_data, pin_init};

/// [`VmCompressionStrategy`] implementation using the LZ4 compression algorithm.
#[guarded]
#[pin_data]
pub struct VmLz4Compressor {
    #[mutex]
    compress_lock: KMutex<RawMutex>,

    /// The acceleration factor that is directly passed into the lz4 compress methods. Is set by
    /// the constructor and has no default value. Making this mutable would require a different
    /// form of zero detection.
    acceleration: c_int,

    /// The compressed representation, using the `acceleration` value, of a page of zeroes. Used
    /// to determine if any [`compress`][VmCompressionStrategy::compress] request is actually just
    /// a zero page by comparing with the result.
    compressed_zero: Box<[u8]>,

    /// The `LZ4_stream_t` used to hold compression state.
    // Safety invariant: Once a `VmLz4Compressor` has been returned from `create`, `stream` has
    // been initialized by `LZ4_initStream`, which returned `stream` itself. This verifies that the
    // size and alignment of `LZ4_stream_t` satisfy the requirements of the linked LZ4 library (the
    // alignment check relies on `LZ4_ALIGN_TEST`, which `//third_party/lz4` leaves at its default
    // of 1). After initialization `stream` is only ever passed to
    // `LZ4_compress_fast_extState_fastReset`, which leaves it correctly initialized for the next
    // call, including when it fails and returns 0, so `stream` remains correctly initialized
    // across every call. It must not be passed to any other LZ4 function or modified from Rust.
    #[pin]
    #[guarded_by(compress_lock)]
    stream: LZ4_stream_t,
}

// The opaque `LZ4_stream_t` must be large and aligned enough for the LZ4 library, which also
// checks this at runtime in `LZ4_initStream`.
zr::static_assert!(core::mem::size_of::<LZ4_stream_t>() >= LZ4_STREAM_MINSIZE);
zr::static_assert!(core::mem::align_of::<LZ4_stream_t>() >= core::mem::align_of::<*const u8>());

// LZ4 is always told that the source is `page::SIZE` bytes, which must not exceed
// `LZ4_MAX_INPUT_SIZE`. It must also be at least 4 KiB so that `LZ4_prepareTable` fully resets the
// hash table on every `LZ4_compress_fast_extState_fastReset` call.
zr::static_assert!(page::SIZE >= 4096 && page::SIZE <= LZ4_MAX_INPUT_SIZE as usize);

// These manual impls are only needed because `kalloc::Box`, used for `compressed_zero`, has no
// `Send` or `Sync` impls. Every other field is automatically `Send + Sync`. Since the impls are
// manual the compiler no longer checks the fields, so any new field must be audited against them.
//
// SAFETY: Each field may be moved to another thread:
// - `compress_lock`: `KMutex<RawMutex>` is `Send` because `RawMutex` is.
// - `acceleration`: `c_int` is `Send`.
// - `compressed_zero`: The box uniquely owns its heap allocation, `[u8]` is `Send`, and the kernel
//   heap allows the allocation to be freed from any thread.
// - `stream`: `KCell<LZ4_stream_t, _>` is `Send` because `LZ4_stream_t` is plain data with no
//   pointers into thread-local state.
unsafe impl Send for VmLz4Compressor {}
// SAFETY: Each field may be shared between threads:
// - `compress_lock`: `KMutex<RawMutex>` is `Sync` because `RawMutex` is.
// - `acceleration`: `c_int` is `Sync`, and it is immutable after construction.
// - `compressed_zero`: `&Self` only exposes `&[u8]`, and `[u8]` is `Sync`. It is only written
//   through `Pin<&mut Self>` in `init`, which `create` calls before the object is returned or
//   shared.
// - `stream`: `KCell<LZ4_stream_t, _>` is `Sync` because `LZ4_stream_t` is `Send`, and it is only
//   reachable through the `compress_lock` guard.
unsafe impl Sync for VmLz4Compressor {}

impl VmLz4Compressor {
    /// Returns `None` on allocation or other failure.
    pub fn create() -> Option<Pin<Box<Self>>> {
        let acceleration = BootOptions::get().compression_lz4_acceleration as c_int;

        let mut lz4 = Box::try_pin_init(Self::new(acceleration)).ok()?;

        if !lz4.as_mut().init() {
            return None;
        }

        Some(lz4)
    }

    /// Constructor is private to ensure that [`init`][Self::init] gets called to initialize state.
    fn new(lz4_acceleration: c_int) -> impl PinInit<Self> {
        // SAFETY: The closure fully initializes `*slot` by filling it with zeroes, which are a
        // valid bit pattern for the opaque byte array storage of `LZ4_stream_t`. The
        // `slot.write_bytes(0, 1)` is sound because the `PinInit` caller contract guarantees that
        // `slot` is valid for writes of `size_of::<LZ4_stream_t>()` bytes and suitably aligned.
        // `kcell_init` forwards the same `slot`, as `KCell` is `repr(transparent)`.
        let zeroed_stream = unsafe {
            pin_init::init_from_closure(|slot: *mut LZ4_stream_t| {
                slot.write_bytes(0, 1);
                Ok::<(), core::convert::Infallible>(())
            })
        };
        pin_init!(Self {
            compress_lock <- KMutex::init(),
            acceleration: lz4_acceleration,
            compressed_zero: Box::empty_slice(),
            stream <- kcell_init(zeroed_stream),
        })
    }

    /// Internal helper that initializes the `stream` and `compressed_zero`. If this returns false
    /// the object should be destroyed and not used.
    fn init(self: Pin<&mut Self>) -> bool {
        let compressed_zero = {
            ksync::lock!(let mut guard = self.lock_compress_lock());
            let stream: *mut LZ4_stream_t = Pin::get_mut(guard.as_mut().stream_mut());
            // Initialize the stream. As our stream should be exactly sized and aligned there is
            // no reason for this to return anything other than the original pointer.
            // SAFETY: `stream` points to `size_of::<LZ4_stream_t>()` bytes that are exclusively
            // accessed while `compress_lock` is held.
            let initialized = unsafe {
                LZ4_initStream(stream.cast::<c_void>(), core::mem::size_of::<LZ4_stream_t>())
            };
            if initialized != stream {
                return false;
            }

            // Zero page should compress quite well, so just use a small stack allocation.
            const MAX_ZERO_PAGE_STORAGE: usize = 128;
            let mut temp_zero_compress = [MaybeUninit::<u8>::uninit(); MAX_ZERO_PAGE_STORAGE];
            let zero_page = paddr_to_physmap(vm_get_zero_page_paddr());
            // SAFETY: `stream` was initialized by `LZ4_initStream` above and is exclusively
            // accessed while `compress_lock` is held. The zero page is allocated during early VM
            // initialization, so `zero_page` points to `page::SIZE` bytes mapped in the physmap
            // that are valid for reads and never modified. The physmap is memory outside of the
            // Rust abstract machine, so its provenance is exposed and may be recovered with
            // `with_exposed_provenance`, as other physmap users do. LZ4 writes at most
            // `MAX_ZERO_PAGE_STORAGE` bytes to `temp_zero_compress`, which is that size.
            let compress_result = unsafe {
                LZ4_compress_fast_extState_fastReset(
                    stream.cast::<c_void>(),
                    core::ptr::with_exposed_provenance::<c_char>(zero_page.0),
                    temp_zero_compress.as_mut_ptr().cast::<c_char>(),
                    page::SIZE as c_int,
                    MAX_ZERO_PAGE_STORAGE as c_int,
                    self.acceleration,
                )
            };
            if compress_result == 0 {
                kprintln!(
                    "ERROR: LZ4 failed to compress zero page with acceleration {} into {} bytes",
                    self.acceleration,
                    MAX_ZERO_PAGE_STORAGE
                );
                return false;
            }
            debug_assert!(compress_result > 0);
            let compressed_size = compress_result as usize;
            debug_assert!(compressed_size <= MAX_ZERO_PAGE_STORAGE);
            // Now allocate the exact storage.
            let Ok(mut compressed_zero) = Box::<[u8]>::try_new_uninit_slice(compressed_size) else {
                return false;
            };
            compressed_zero.copy_from_slice(&temp_zero_compress[..compressed_size]);
            // SAFETY: `LZ4_compress_fast_extState_fastReset` returned `compress_result > 0`, so it
            // initialized the first `compressed_size` bytes of `temp_zero_compress`, which were
            // copied into `compressed_zero`.
            unsafe { compressed_zero.assume_init() }
        };
        *self.project().compressed_zero = compressed_zero;

        true
    }
}

// SAFETY:
// 1. When `compress` returns `StrategyCompressResult::Compressed(compressed_size)`,
//    `LZ4_compress_fast_extState_fastReset` returned `compressed_result > 0` with
//    `compressed_result <= threshold <= dst.len()`, having written `compressed_size` bytes to
//    `dst`.
// 2. `decompress` asserts that `LZ4_decompress_safe` returned `page::SIZE`, which means all
//    `page::SIZE` bytes of `dst` were written.
unsafe impl VmCompressionStrategy for VmLz4Compressor {
    fn compress(&self, src: &[u8], dst: &mut [MaybeUninit<u8>]) -> StrategyCompressResult {
        // LZ4 reads `page::SIZE` bytes from `src`, so this check is required for soundness.
        assert!(src.len() == page::SIZE);
        // Saturate so that LZ4 is never told `dst` is larger than it is, or given a negative
        // capacity if `dst.len() > c_int::MAX as usize`.
        let threshold = c_int::try_from(dst.len()).unwrap_or(c_int::MAX);
        let compressed_result = {
            ksync::lock!(let mut guard = self.lock_compress_lock());
            let stream: *mut LZ4_stream_t = Pin::get_mut(guard.as_mut().stream_mut());
            // SAFETY: By the `stream` safety invariant, `stream` is correctly initialized, sized
            // and aligned for the LZ4 library, and it is exclusively accessed while
            // `compress_lock` is held.
            // `src` is valid for reads of `page::SIZE` bytes, as asserted above, and LZ4 writes at
            // most `threshold <= dst.len()` bytes to `dst`. If `dst` is empty then
            // `dst.as_mut_ptr()` is dangling but non-null and `threshold` is 0, so LZ4 writes
            // nothing. `src` and `dst` do not overlap as they are `&` and `&mut` borrows.
            unsafe {
                LZ4_compress_fast_extState_fastReset(
                    stream.cast::<c_void>(),
                    src.as_ptr().cast::<c_char>(),
                    dst.as_mut_ptr().cast::<c_char>(),
                    page::SIZE as c_int,
                    threshold,
                    self.acceleration,
                )
            }
        };
        if compressed_result == 0 {
            return StrategyCompressResult::Fail;
        }
        debug_assert!(compressed_result > 0 && compressed_result <= threshold);
        let compressed_size = compressed_result as usize;
        // SAFETY: `LZ4_compress_fast_extState_fastReset` returned `compressed_result > 0`, so it
        // initialized the first `compressed_size` bytes of `dst`.
        let compressed = unsafe { dst[..compressed_size].assume_init_ref() };
        if compressed == &*self.compressed_zero {
            return StrategyCompressResult::Zero;
        }
        StrategyCompressResult::Compressed(compressed_size)
    }

    fn decompress(&self, src: &[u8], dst: &mut [MaybeUninit<u8>]) {
        // LZ4 may write up to `page::SIZE` bytes to `dst`, so this check is required for
        // soundness.
        assert!(dst.len() == page::SIZE);
        // Saturate so that LZ4 is never told `src` is larger than it is, or given a negative size
        // if `src.len() > c_int::MAX as usize` (`LZ4_decompress_safe` does not check for negative
        // `compressedSize` before computing `src + compressedSize`).
        let src_len = c_int::try_from(src.len()).unwrap_or(c_int::MAX);
        // SAFETY: `src` is valid for reads of `src_len <= src.len()` bytes, and `dst` is valid for
        // writes of `page::SIZE` bytes, as asserted above. `LZ4_decompress_safe` never reads or
        // writes outside of these bounds, even if `src` is malformed.
        let result = unsafe {
            LZ4_decompress_safe(
                src.as_ptr().cast::<c_char>(),
                dst.as_mut_ptr().cast::<c_char>(),
                src_len,
                page::SIZE as c_int,
            )
        };
        assert!(result == page::SIZE as c_int);
    }

    fn dump(&self) {}
}
