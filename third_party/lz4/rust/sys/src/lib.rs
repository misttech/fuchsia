// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Raw FFI bindings to the LZ4 library built by `//third_party/lz4`, which this crate also links.
//!
//! These are hand-written declarations of the subset of `lz4.h` and `lz4hc.h` that is used from
//! Rust, and must be kept in sync with those headers and the defines that `//third_party/lz4` is
//! built with. This crate is `no_std` so that it can also be used by the kernel.

#![no_std]

#[cfg(test)]
extern crate std;

use core::ffi::{c_char, c_int, c_void};

/// `LZ4_MEMORY_USAGE`: log2 of the size of the hash table used for compression.
/// `//third_party/lz4` does not override it, so this is `LZ4_MEMORY_USAGE_DEFAULT`.
pub const LZ4_MEMORY_USAGE: u32 = 14;

/// `LZ4_MAX_INPUT_SIZE`: the maximum number of bytes that LZ4 can compress in one call.
pub const LZ4_MAX_INPUT_SIZE: c_int = 0x7E000000;

/// `LZ4_STREAM_MINSIZE`: the size of [`LZ4_stream_t`].
pub const LZ4_STREAM_MINSIZE: usize = (1 << LZ4_MEMORY_USAGE) + 32;

/// `LZ4HC_CLEVEL_MIN`: the minimum compression level of [`LZ4_compress_HC`].
pub const LZ4HC_CLEVEL_MIN: c_int = 2;
/// `LZ4HC_CLEVEL_DEFAULT`: the default compression level of [`LZ4_compress_HC`].
pub const LZ4HC_CLEVEL_DEFAULT: c_int = 9;
/// `LZ4HC_CLEVEL_OPT_MIN`: the minimum compression level of [`LZ4_compress_HC`] that uses the
/// optimal parser.
pub const LZ4HC_CLEVEL_OPT_MIN: c_int = 10;
/// `LZ4HC_CLEVEL_MAX`: the maximum compression level of [`LZ4_compress_HC`].
pub const LZ4HC_CLEVEL_MAX: c_int = 12;

/// `LZ4_stream_t`: the state used by the LZ4 streaming and external state compression functions.
///
/// The contents are opaque, and this only matches the size and alignment of the C
/// `union LZ4_stream_u`, which is sized by `LZ4_STREAM_MINSIZE` and contains pointers and
/// 32-bit integers. It must be initialized with [`LZ4_initStream`] before being used, which
/// fails if the size or alignment does not satisfy the linked LZ4 library.
///
/// This is intentionally neither `Copy` nor `Clone` as it is large and must not be implicitly
/// copied.
#[repr(C)]
pub struct LZ4_stream_t {
    // `usize` has the same alignment as a pointer on all supported targets, which is the largest
    // alignment of any member of `LZ4_stream_t_internal`.
    _align: [usize; 0],
    _opaque: [u8; LZ4_STREAM_MINSIZE],
}

unsafe extern "C" {
    /// `LZ4_compress_default()`: compresses `srcSize` bytes from `src` into the `dstCapacity`
    /// byte buffer `dst`. Returns the number of bytes written to `dst`, or 0 if compression fails.
    /// Compression is guaranteed to succeed if `dstCapacity >= LZ4_compressBound(srcSize)`.
    pub fn LZ4_compress_default(
        src: *const c_char,
        dst: *mut c_char,
        srcSize: c_int,
        dstCapacity: c_int,
    ) -> c_int;

    /// `LZ4_decompress_safe()`: decompresses the complete compressed block of `compressedSize`
    /// bytes from `src` into the `dstCapacity` byte buffer `dst`. Returns the number of bytes
    /// written to `dst`, or a negative value if `dst` is too small or `src` is malformed.
    ///
    /// This never reads outside of `src` or writes outside of `dst`, even if `src` is malformed.
    pub fn LZ4_decompress_safe(
        src: *const c_char,
        dst: *mut c_char,
        compressedSize: c_int,
        dstCapacity: c_int,
    ) -> c_int;

    /// `LZ4_compressBound()`: returns the maximum size of the output of compressing `inputSize`
    /// bytes, or 0 if `inputSize` is larger than [`LZ4_MAX_INPUT_SIZE`].
    pub fn LZ4_compressBound(inputSize: c_int) -> c_int;

    /// `LZ4_initStream()`: initializes the `size` byte buffer `buffer` as an [`LZ4_stream_t`].
    /// Returns `buffer` on success, or null if `size` is too small or `buffer` is not aligned
    /// correctly.
    pub fn LZ4_initStream(buffer: *mut c_void, size: usize) -> *mut LZ4_stream_t;

    /// `LZ4_compress_fast_extState_fastReset()`: compresses `srcSize` bytes from `src` into the
    /// `dstCapacity` byte buffer `dst`, using the `acceleration` factor and the [`LZ4_stream_t`]
    /// `state`. Returns the number of bytes written to `dst`, or 0 if compression fails.
    ///
    /// This avoids the expensive initialization of `state` done by
    /// `LZ4_compress_fast_extState()`, and so is only safe to call if `state` is already correctly
    /// initialized, such as by [`LZ4_initStream`] or a previous call to this function. `state` is
    /// left correctly initialized afterwards, even if compression fails.
    ///
    /// This function is only declared by `lz4.h` with `LZ4_STATIC_LINKING_ONLY`, as it is not
    /// part of the stable ABI of shared builds of LZ4, but is always available as this crate
    /// statically links LZ4.
    pub fn LZ4_compress_fast_extState_fastReset(
        state: *mut c_void,
        src: *const c_char,
        dst: *mut c_char,
        srcSize: c_int,
        dstCapacity: c_int,
        acceleration: c_int,
    ) -> c_int;

    /// `LZ4_compress_HC()`: compresses `srcSize` bytes from `src` into the `dstCapacity` byte
    /// buffer `dst` using the high compression (HC) algorithm at `compressionLevel`. Returns the
    /// number of bytes written to `dst`, or 0 if compression fails. Compression is guaranteed to
    /// succeed if `dstCapacity >= LZ4_compressBound(srcSize)`.
    pub fn LZ4_compress_HC(
        src: *const c_char,
        dst: *mut c_char,
        srcSize: c_int,
        dstCapacity: c_int,
        compressionLevel: c_int,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::MaybeUninit;
    use std::boxed::Box;
    use std::vec;

    const ACCELERATION: c_int = 1;

    #[test]
    fn test_stream_size() {
        let mut stream = Box::<LZ4_stream_t>::new_uninit();
        let ptr = stream.as_mut_ptr();
        // SAFETY: `ptr` is valid for writes of `size_of::<LZ4_stream_t>() - 1` bytes.
        let initialized = unsafe {
            LZ4_initStream(ptr.cast::<c_void>(), core::mem::size_of::<LZ4_stream_t>() - 1)
        };
        assert!(initialized.is_null());
        // SAFETY: `ptr` is valid for writes of `size_of::<LZ4_stream_t>()` bytes.
        let initialized =
            unsafe { LZ4_initStream(ptr.cast::<c_void>(), core::mem::size_of::<LZ4_stream_t>()) };
        assert_eq!(initialized, ptr);
    }

    #[test]
    fn test_compress_bound() {
        // SAFETY: No unsafe parameters.
        unsafe {
            assert!(LZ4_compressBound(LZ4_MAX_INPUT_SIZE) > 0);
            assert_eq!(LZ4_compressBound(LZ4_MAX_INPUT_SIZE + 1), 0);
        }
    }

    #[test]
    fn test_ext_state_fast_reset_roundtrip() {
        let mut stream: Box<MaybeUninit<LZ4_stream_t>> = Box::new_uninit();
        let stream = stream.as_mut_ptr();
        // SAFETY: `stream` is valid for writes of `size_of::<LZ4_stream_t>()` bytes.
        let initialized = unsafe {
            LZ4_initStream(stream.cast::<c_void>(), core::mem::size_of::<LZ4_stream_t>())
        };
        assert_eq!(initialized, stream);

        // Compress several inputs to check that the stream is reusable.
        for data in [b"Hello, world! LZ4 compression test.".as_slice(), &[0u8; 4096], &[7u8; 100]] {
            let src_size = c_int::try_from(data.len()).unwrap();
            // SAFETY: No unsafe parameters.
            let bound = unsafe { LZ4_compressBound(src_size) };
            assert!(bound > 0);
            let mut compressed = vec![0u8; bound as usize];
            // SAFETY: `stream` was initialized above, and is left initialized by every call.
            // `data` is valid for reads of `src_size` bytes and `compressed` is valid for writes
            // of `bound` bytes.
            let compressed_size = unsafe {
                LZ4_compress_fast_extState_fastReset(
                    stream.cast::<c_void>(),
                    data.as_ptr().cast::<c_char>(),
                    compressed.as_mut_ptr().cast::<c_char>(),
                    src_size,
                    bound,
                    ACCELERATION,
                )
            };
            assert!(compressed_size > 0 && compressed_size <= bound);

            let mut decompressed = vec![0u8; data.len()];
            // SAFETY: `compressed` is valid for reads of `compressed_size` bytes and
            // `decompressed` is valid for writes of `src_size` bytes.
            let decompressed_size = unsafe {
                LZ4_decompress_safe(
                    compressed.as_ptr().cast::<c_char>(),
                    decompressed.as_mut_ptr().cast::<c_char>(),
                    compressed_size,
                    src_size,
                )
            };
            assert_eq!(decompressed_size, src_size);
            assert_eq!(decompressed.as_slice(), data);
        }
    }

    #[test]
    fn test_ext_state_fast_reset_dst_too_small() {
        let mut stream: Box<MaybeUninit<LZ4_stream_t>> = Box::new_uninit();
        let stream = stream.as_mut_ptr();
        // SAFETY: `stream` is valid for writes of `size_of::<LZ4_stream_t>()` bytes.
        let initialized = unsafe {
            LZ4_initStream(stream.cast::<c_void>(), core::mem::size_of::<LZ4_stream_t>())
        };
        assert_eq!(initialized, stream);

        let data = b"Hello, world! LZ4 compression test.";
        let mut compressed = [0u8; 4];
        // SAFETY: `stream` was initialized above, `data` is valid for reads of `data.len()` bytes
        // and `compressed` is valid for writes of `compressed.len()` bytes.
        let compressed_size = unsafe {
            LZ4_compress_fast_extState_fastReset(
                stream.cast::<c_void>(),
                data.as_ptr().cast::<c_char>(),
                compressed.as_mut_ptr().cast::<c_char>(),
                data.len() as c_int,
                compressed.len() as c_int,
                ACCELERATION,
            )
        };
        assert_eq!(compressed_size, 0);
    }
}
