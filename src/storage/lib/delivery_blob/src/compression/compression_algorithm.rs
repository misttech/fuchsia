// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Compression algorithms supported by chunked-compression and corresponding compressors and
//! decompressors.
//!
//! The compressors and decompressors are enums rather than traits with multiple implementations
//! because the enums are small and avoid the heap allocation of `Box<dyn Decompressor>`.

use super::{FormatError, ZstdError};
use crate::compression::ChunkedArchiveError;
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use storage_ptr_slice::PtrByteSlice;
use zstd::zstd_safe::zstd_sys;

thread_local! {
    static ZSTD_COMPRESSOR: std::cell::RefCell<zstd::zstd_safe::CCtx<'static>> =
        std::cell::RefCell::new({
            let mut cctx = zstd::zstd_safe::CCtx::create();
            cctx.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true)).unwrap();
            cctx
        });
    static ZSTD_DECOMPRESSOR: std::cell::RefCell<RawZstdDCtx> =
        std::cell::RefCell::new(RawZstdDCtx::new());
    static LZ4_COMPRESSOR: std::cell::RefCell<RawLz4StreamHC> =
        std::cell::RefCell::new(RawLz4StreamHC::new());
}

struct RawZstdDCtx(NonNull<zstd_sys::ZSTD_DCtx>);

// SAFETY: `RawZstdDCtx` owns its `ZSTD_DCtx` pointer exclusively and does not share it
// unsynchronized across threads.
unsafe impl Send for RawZstdDCtx {}

impl RawZstdDCtx {
    fn new() -> Self {
        // SAFETY: Creating a ZSTD decompression context does not access
        // invalid memory or violate invariants.
        let raw_ptr = unsafe { zstd_sys::ZSTD_createDCtx() };
        let ptr = NonNull::new(raw_ptr).expect("ZSTD_createDCtx failed");
        Self(ptr)
    }

    /// Decompresses ZSTD using a pointer slice and a raw destination pointer slice.
    ///
    /// # Safety
    ///
    /// `dst` must point to valid memory allocated for writes of at least
    /// `dst.len()` bytes.
    unsafe fn decompress_ptr(
        &mut self,
        src: PtrByteSlice<'_>,
        dst: *mut [MaybeUninit<u8>],
    ) -> Result<usize, usize> {
        let dctx = self.0.as_ptr();
        // SAFETY: `PtrByteSlice` guarantees `src` is valid for reads. `dst`
        // points to valid memory allocated for writes of at least `dst.len()`
        // bytes (ensured by caller safety preconditions).
        let result = unsafe {
            zstd_sys::ZSTD_decompressDCtx(
                dctx,
                dst as *mut std::os::raw::c_void,
                dst.len(),
                src.as_raw_slice_ptr() as *const std::os::raw::c_void,
                src.len(),
            )
        };
        // SAFETY: Checking an integer return code is pure inspection.
        if unsafe { zstd_sys::ZSTD_isError(result) } != 0 { Err(result) } else { Ok(result) }
    }
}

impl Drop for RawZstdDCtx {
    fn drop(&mut self) {
        // SAFETY: `self.0` is non-null and was allocated via `ZSTD_createDCtx`.
        unsafe {
            zstd_sys::ZSTD_freeDCtx(self.0.as_ptr());
        }
    }
}

/// Decompresses ZSTD using a pointer slice and a raw destination pointer slice.
///
/// # Safety
///
/// `dst` must point to valid memory allocated for writes of at least
/// `dst.len()` bytes.
unsafe fn zstd_decompress_ptr(
    src: PtrByteSlice<'_>,
    dst: *mut [MaybeUninit<u8>],
) -> Result<usize, usize> {
    ZSTD_DECOMPRESSOR.with_borrow_mut(|decompressor| {
        // SAFETY: The caller upholds the safety preconditions for `dst`.
        unsafe { decompressor.decompress_ptr(src, dst) }
    })
}

unsafe extern "C" {
    fn LZ4_decompress_safe(
        src: *const std::os::raw::c_char,
        dst: *mut std::os::raw::c_char,
        compressedSize: std::os::raw::c_int,
        dstCapacity: std::os::raw::c_int,
    ) -> std::os::raw::c_int;
    fn LZ4_compressBound(inputSize: std::os::raw::c_int) -> std::os::raw::c_int;
    fn LZ4_createStreamHC() -> *mut std::os::raw::c_void;
    fn LZ4_freeStreamHC(streamHCPtr: *mut std::os::raw::c_void) -> std::os::raw::c_int;
    fn LZ4_compress_HC_extStateHC_fastReset(
        state: *mut std::os::raw::c_void,
        src: *const std::os::raw::c_char,
        dst: *mut std::os::raw::c_char,
        srcSize: std::os::raw::c_int,
        dstCapacity: std::os::raw::c_int,
        compressionLevel: std::os::raw::c_int,
    ) -> std::os::raw::c_int;
}

struct RawLz4StreamHC(NonNull<std::os::raw::c_void>);

// SAFETY: `RawLz4StreamHC` owns its `LZ4_streamHC_t` pointer exclusively and does not share it
// unsynchronized across threads.
unsafe impl Send for RawLz4StreamHC {}

impl RawLz4StreamHC {
    fn new() -> Self {
        // SAFETY: `LZ4_createStreamHC` allocates and zeroes a new stream state on the heap.
        let raw_ptr = unsafe { LZ4_createStreamHC() };
        let ptr = NonNull::new(raw_ptr).expect("LZ4_createStreamHC failed");
        Self(ptr)
    }

    fn compress_into(
        &mut self,
        data: &[u8],
        dst: &mut Vec<u8>,
        compression_level: lz4::HcCompressionLevel,
    ) -> Result<usize, lz4::Error> {
        if data.is_empty() {
            return Ok(0);
        }
        let data_size =
            std::os::raw::c_int::try_from(data.len()).map_err(|_| lz4::Error::InputTooLarge)?;
        // SAFETY: `LZ4_compressBound` is a pure integer calculation.
        let bound = unsafe { LZ4_compressBound(data_size) };
        if bound <= 0 {
            return Err(lz4::Error::InputTooLarge);
        }
        let bound_usize = bound as usize;
        dst.reserve(bound_usize);
        let prev_len = dst.len();
        // SAFETY:
        //  1. `dst.reserve(bound_usize)` ensures at least `bound_usize` bytes of spare capacity.
        //  2. `self.0` is guaranteed to be in a valid state for
        //     `LZ4_compress_HC_extStateHC_fastReset` per `lz4hc.h`:
        //     - It is initially created and zero-initialized by `LZ4_createStreamHC()` in
        //       `RawLz4StreamHC::new()`.
        //     - `RawLz4StreamHC` exclusively owns the state pointer and only ever mutates it via
        //       `LZ4_compress_HC_extStateHC_fastReset`.
        //     - After any successful compression call, the stream remains in a valid state for
        //       subsequent fast resets. Even if a prior call were to fail, LZ4 marks the stream
        //       `dirty` and `LZ4_resetStreamHC_fast` automatically falls back to a full
        //       `LZ4_initStreamHC` reset.
        //  3. When compression succeeds, LZ4 initializes `compressed_size` bytes in `dst`.
        unsafe {
            let dst_ptr =
                dst.spare_capacity_mut()[..bound_usize].as_mut_ptr().cast::<std::os::raw::c_char>();
            let src_ptr = data.as_ptr().cast::<std::os::raw::c_char>();
            let compressed_size = LZ4_compress_HC_extStateHC_fastReset(
                self.0.as_ptr(),
                src_ptr,
                dst_ptr,
                data_size,
                bound,
                i32::from(compression_level),
            );
            assert!(compressed_size > 0, "LZ4 compression failed");
            let written = compressed_size as usize;
            dst.set_len(prev_len + written);
            Ok(written)
        }
    }
}

impl Drop for RawLz4StreamHC {
    fn drop(&mut self) {
        // SAFETY: `self.0` was allocated by `LZ4_createStreamHC`.
        unsafe {
            LZ4_freeStreamHC(self.0.as_ptr());
        }
    }
}

/// Decompresses LZ4 using a pointer slice and a raw destination pointer slice.
///
/// # Safety
///
/// `dst` must point to valid memory allocated for writes of at least
/// `dst.len()` bytes.
unsafe fn lz4_decompress_ptr(
    src: PtrByteSlice<'_>,
    dst: *mut [MaybeUninit<u8>],
) -> Result<usize, lz4::Error> {
    if src.is_empty() {
        return Ok(0);
    }
    if dst.len() == 0 {
        return Err(lz4::Error::DecompressionFailed);
    }
    // SAFETY: `PtrByteSlice` guarantees `src` is valid for reads. `dst`
    // points to valid memory allocated for writes of at least `dst.len()`
    // bytes (ensured by caller safety preconditions).
    let result = unsafe {
        LZ4_decompress_safe(
            src.as_raw_slice_ptr() as *const std::os::raw::c_char,
            dst as *mut std::os::raw::c_char,
            src.len().try_into().map_err(|_| lz4::Error::InputTooLarge)?,
            dst.len().try_into().map_err(|_| lz4::Error::InputTooLarge)?,
        )
    };
    if result < 0 { Err(lz4::Error::DecompressionFailed) } else { Ok(result as usize) }
}

/// The compression algorithm used to compress the chunks.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CompressionAlgorithm {
    Zstd = 0,
    Lz4 = 1,
}

impl CompressionAlgorithm {
    /// Returns a decompressor that can decompress a chunk compressed with this compression
    /// algorithm.
    pub fn decompressor(&self) -> Decompressor {
        match self {
            Self::Zstd => Decompressor(DecompressorInner::Zstd(RawZstdDCtx::new())),
            Self::Lz4 => Decompressor(DecompressorInner::Lz4),
        }
    }

    /// Returns a decompressor that can decompress a chunk compressed with this compression
    /// algorithm. Some decompressors require a large state object that is expensive to create but
    /// can be reused for many decompressions. A thread-local decompressor stores the state object
    /// in a thread-local variable.
    pub fn thread_local_decompressor(&self) -> ThreadLocalDecompressor {
        match self {
            Self::Zstd => ThreadLocalDecompressor::Zstd,
            Self::Lz4 => ThreadLocalDecompressor::Lz4,
        }
    }
}

impl From<CompressionAlgorithm> for u8 {
    fn from(value: CompressionAlgorithm) -> Self {
        value as u8
    }
}

impl TryFrom<u8> for CompressionAlgorithm {
    type Error = ChunkedArchiveError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(CompressionAlgorithm::Zstd),
            1 => Ok(CompressionAlgorithm::Lz4),
            _ => Err(ChunkedArchiveError::IntegrityError),
        }
    }
}

enum DecompressorInner {
    Zstd(RawZstdDCtx),
    Lz4,
}

/// A decompressor that is capable of decompressing chunks of a compressed archive.
pub struct Decompressor(DecompressorInner);

impl Decompressor {
    /// Returns the compression algorithm used by this decompressor.
    pub fn algorithm(&self) -> CompressionAlgorithm {
        match &self.0 {
            DecompressorInner::Zstd(_) => CompressionAlgorithm::Zstd,
            DecompressorInner::Lz4 => CompressionAlgorithm::Lz4,
        }
    }

    /// Decompresses a chunk of a chunked-compression archive.
    pub fn decompress<'a>(
        &mut self,
        data: impl Into<PtrByteSlice<'a>>,
        uncompressed_size: usize,
        chunk_index: usize,
    ) -> Result<Vec<u8>, ChunkedArchiveError> {
        let src = data.into();
        let mut buffer = Vec::with_capacity(uncompressed_size);
        let dst = buffer.spare_capacity_mut() as *mut [MaybeUninit<u8>];
        let len = match &mut self.0 {
            DecompressorInner::Zstd(dctx) => {
                // SAFETY: `dst` points to `uncompressed_size` bytes of capacity
                // in `buffer`.
                unsafe { dctx.decompress_ptr(src, dst) }.map_err(|code| {
                    ChunkedArchiveError::DecompressionError {
                        index: chunk_index,
                        error: FormatError::Zstd(ZstdError(code)),
                    }
                })?
            }
            DecompressorInner::Lz4 => {
                // SAFETY: `dst` points to `uncompressed_size` bytes of capacity
                // in `buffer`.
                unsafe { lz4_decompress_ptr(src, dst) }.map_err(|e| {
                    ChunkedArchiveError::DecompressionError {
                        index: chunk_index,
                        error: FormatError::Lz4(e),
                    }
                })?
            }
        };
        // SAFETY: Decompression wrote `len` initialized bytes into `buffer`.
        unsafe {
            buffer.set_len(len);
        }
        Ok(buffer)
    }

    /// Decompresses a chunk of a chunked-compression archive into a pre-allocated buffer.
    pub fn decompress_into<'a>(
        &mut self,
        data: impl Into<PtrByteSlice<'a>>,
        destination: &mut [u8],
        chunk_index: usize,
    ) -> Result<usize, ChunkedArchiveError> {
        let src = data.into();
        let dst = destination as *mut [u8] as *mut [MaybeUninit<u8>];
        match &mut self.0 {
            DecompressorInner::Zstd(dctx) => {
                // SAFETY: `dst` points to `destination.len()` bytes of valid
                // memory in `destination`.
                unsafe { dctx.decompress_ptr(src, dst) }.map_err(|code| {
                    ChunkedArchiveError::DecompressionError {
                        index: chunk_index,
                        error: FormatError::Zstd(ZstdError(code)),
                    }
                })
            }
            DecompressorInner::Lz4 => {
                // SAFETY: `dst` points to `destination.len()` bytes of valid
                // memory in `destination`.
                unsafe { lz4_decompress_ptr(src, dst) }.map_err(|e| {
                    ChunkedArchiveError::DecompressionError {
                        index: chunk_index,
                        error: FormatError::Lz4(e),
                    }
                })
            }
        }
    }
}

#[derive(Copy, Clone)]
/// A decompressor that uses thread-local storage to avoid reallocation of large state objects.
pub enum ThreadLocalDecompressor {
    Zstd,
    Lz4,
}

impl ThreadLocalDecompressor {
    /// Decompresses a chunk of a chunked-compression archive.
    pub fn decompress<'a>(
        &self,
        data: impl Into<PtrByteSlice<'a>>,
        uncompressed_size: usize,
        chunk_index: usize,
    ) -> Result<Vec<u8>, ChunkedArchiveError> {
        let src = data.into();
        let mut buffer = Vec::with_capacity(uncompressed_size);
        let dst = buffer.spare_capacity_mut() as *mut [MaybeUninit<u8>];
        let len = match self {
            Self::Zstd => {
                // SAFETY: `dst` points to `uncompressed_size` bytes of capacity
                // in `buffer`.
                unsafe { zstd_decompress_ptr(src, dst) }.map_err(|code| {
                    ChunkedArchiveError::DecompressionError {
                        index: chunk_index,
                        error: FormatError::Zstd(ZstdError(code)),
                    }
                })?
            }
            Self::Lz4 => {
                // SAFETY: `dst` points to `uncompressed_size` bytes of capacity
                // in `buffer`.
                unsafe { lz4_decompress_ptr(src, dst) }.map_err(|e| {
                    ChunkedArchiveError::DecompressionError {
                        index: chunk_index,
                        error: FormatError::Lz4(e),
                    }
                })?
            }
        };
        // SAFETY: Decompression wrote `len` initialized bytes into `buffer`.
        unsafe {
            buffer.set_len(len);
        }
        Ok(buffer)
    }

    /// Decompresses a chunk of a chunked-compression archive into a pre-allocated buffer.
    pub fn decompress_into<'a>(
        &self,
        data: impl Into<PtrByteSlice<'a>>,
        destination: &mut [u8],
        chunk_index: usize,
    ) -> Result<usize, ChunkedArchiveError> {
        let src = data.into();
        let dst = destination as *mut [u8] as *mut [MaybeUninit<u8>];
        match self {
            Self::Zstd => {
                // SAFETY: `dst` points to `destination.len()` bytes of valid
                // memory in `destination`.
                unsafe { zstd_decompress_ptr(src, dst) }.map_err(|code| {
                    ChunkedArchiveError::DecompressionError {
                        index: chunk_index,
                        error: FormatError::Zstd(ZstdError(code)),
                    }
                })
            }
            Self::Lz4 => {
                // SAFETY: `dst` points to `destination.len()` bytes of valid
                // memory in `destination`.
                unsafe { lz4_decompress_ptr(src, dst) }.map_err(|e| {
                    ChunkedArchiveError::DecompressionError {
                        index: chunk_index,
                        error: FormatError::Lz4(e),
                    }
                })
            }
        }
    }
}

fn zstd_compress_into(
    cctx: &mut zstd::zstd_safe::CCtx<'_>,
    data: &[u8],
    dst: &mut Vec<u8>,
    chunk_index: usize,
) -> Result<usize, ChunkedArchiveError> {
    let buffer_len = zstd::zstd_safe::compress_bound(data.len());
    dst.reserve(buffer_len);
    let prev_len = dst.len();
    let mut cursor = std::io::Cursor::new(dst);
    cursor.set_position(prev_len as u64);
    match cctx.compress2(&mut cursor, data) {
        Ok(written) => Ok(written),
        Err(code) => Err(ChunkedArchiveError::CompressionError {
            index: chunk_index,
            error: FormatError::Zstd(ZstdError(code)),
        }),
    }
}

enum CompressorInner {
    Zstd(zstd::zstd_safe::CCtx<'static>),
    Lz4 { state: RawLz4StreamHC, compression_level: lz4::HcCompressionLevel },
}

/// A compressor that is capable of compressing chunks of a chunked-compression archive.
pub struct Compressor(CompressorInner);

impl Compressor {
    pub(super) fn new_zstd(compression_level: i32) -> Self {
        let mut cctx = zstd::zstd_safe::CCtx::create();
        cctx.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(compression_level))
            .expect("setting the compression level should never fail");
        Self(CompressorInner::Zstd(cctx))
    }

    pub(super) fn new_lz4(compression_level: lz4::HcCompressionLevel) -> Self {
        Self(CompressorInner::Lz4 { state: RawLz4StreamHC::new(), compression_level })
    }

    /// Compresses a chunk of a chunked-compression archive directly into `dst`, appending the
    /// compressed bytes and returning the number of bytes written.
    pub fn compress_into(
        &mut self,
        data: &[u8],
        dst: &mut Vec<u8>,
        chunk_index: usize,
    ) -> Result<usize, ChunkedArchiveError> {
        match &mut self.0 {
            CompressorInner::Zstd(cctx) => zstd_compress_into(cctx, data, dst, chunk_index),
            CompressorInner::Lz4 { state, compression_level } => state
                .compress_into(data, dst, *compression_level)
                .map_err(|error| ChunkedArchiveError::CompressionError {
                    index: chunk_index,
                    error: FormatError::Lz4(error),
                }),
        }
    }

    /// Compresses a chunk of a chunked-compression archive.
    pub fn compress(
        &mut self,
        data: &[u8],
        chunk_index: usize,
    ) -> Result<Vec<u8>, ChunkedArchiveError> {
        let mut buffer = Vec::new();
        self.compress_into(data, &mut buffer, chunk_index)?;
        buffer.shrink_to_fit();
        Ok(buffer)
    }
}

#[derive(Copy, Clone)]
/// A compressor that uses thread-local storage to avoid reallocation of large state objects.
pub enum ThreadLocalCompressor {
    Zstd { compression_level: i32 },
    Lz4 { compression_level: lz4::HcCompressionLevel },
}

impl ThreadLocalCompressor {
    /// Compresses a chunk of a chunked-compression archive directly into `dst`, appending the
    /// compressed bytes and returning the number of bytes written.
    pub fn compress_into(
        &self,
        data: &[u8],
        dst: &mut Vec<u8>,
        chunk_index: usize,
    ) -> Result<usize, ChunkedArchiveError> {
        match self {
            Self::Zstd { compression_level } => ZSTD_COMPRESSOR.with_borrow_mut(|cctx| {
                cctx.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(
                    *compression_level,
                ))
                .expect("setting the compression level should never fail");
                zstd_compress_into(cctx, data, dst, chunk_index)
            }),
            Self::Lz4 { compression_level } => LZ4_COMPRESSOR.with_borrow_mut(|state| {
                state.compress_into(data, dst, *compression_level).map_err(|error| {
                    ChunkedArchiveError::CompressionError {
                        index: chunk_index,
                        error: FormatError::Lz4(error),
                    }
                })
            }),
        }
    }

    /// Compresses a chunk of a chunked-compression archive.
    pub fn compress(
        &self,
        data: &[u8],
        chunk_index: usize,
    ) -> Result<Vec<u8>, ChunkedArchiveError> {
        let mut buffer = Vec::new();
        self.compress_into(data, &mut buffer, chunk_index)?;
        buffer.shrink_to_fit();
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression::ChunkedArchiveOptions;

    const TEST_DATA: &[u8] = b"hello world this is some test data to compress and decompress";

    #[test]
    fn test_zstd_roundtrip() {
        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Zstd };
        let mut compressor = options.compressor();
        let compressed = compressor.compress(TEST_DATA, 0).unwrap();

        let mut decompressor = CompressionAlgorithm::Zstd.decompressor();
        let decompressed = decompressor.decompress(&compressed, TEST_DATA.len(), 0).unwrap();

        assert_eq!(decompressed, TEST_DATA);
    }

    #[test]
    fn test_lz4_roundtrip() {
        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Lz4 };
        let mut compressor = options.compressor();
        let compressed = compressor.compress(TEST_DATA, 0).unwrap();

        let mut decompressor = CompressionAlgorithm::Lz4.decompressor();
        let decompressed = decompressor.decompress(&compressed, TEST_DATA.len(), 0).unwrap();

        assert_eq!(decompressed, TEST_DATA);
    }

    #[test]
    fn test_thread_local_zstd_roundtrip() {
        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Zstd };
        let compressor = options.thread_local_compressor();
        let compressed = compressor.compress(TEST_DATA, 0).unwrap();

        let decompressor = CompressionAlgorithm::Zstd.thread_local_decompressor();
        let decompressed = decompressor.decompress(&compressed, TEST_DATA.len(), 0).unwrap();

        assert_eq!(decompressed, TEST_DATA);
    }

    #[test]
    fn test_thread_local_lz4_roundtrip() {
        let options =
            ChunkedArchiveOptions::V3 { compression_algorithm: CompressionAlgorithm::Lz4 };
        let compressor = options.thread_local_compressor();
        let compressed = compressor.compress(TEST_DATA, 0).unwrap();

        let decompressor = CompressionAlgorithm::Lz4.thread_local_decompressor();
        let decompressed = decompressor.decompress(&compressed, TEST_DATA.len(), 0).unwrap();

        assert_eq!(decompressed, TEST_DATA);
    }

    #[test]
    fn test_decompress_into() {
        let options = ChunkedArchiveOptions::V2 {
            minimum_chunk_size: 0,
            chunk_alignment: 0,
            compression_level: 1,
        };
        let mut compressor = options.compressor();
        let compressed = compressor.compress(TEST_DATA, 0).unwrap();

        let mut decompressor = CompressionAlgorithm::Zstd.decompressor();
        let mut buffer = vec![0u8; TEST_DATA.len()];
        let len = decompressor.decompress_into(&compressed, &mut buffer, 0).unwrap();

        assert_eq!(len, TEST_DATA.len());
        assert_eq!(buffer, TEST_DATA);
    }

    #[test]
    fn test_algorithm_conversion() {
        assert_eq!(u8::from(CompressionAlgorithm::Zstd), 0);
        assert_eq!(u8::from(CompressionAlgorithm::Lz4), 1);

        assert_eq!(CompressionAlgorithm::try_from(0).unwrap(), CompressionAlgorithm::Zstd);
        assert_eq!(CompressionAlgorithm::try_from(1).unwrap(), CompressionAlgorithm::Lz4);
        assert!(CompressionAlgorithm::try_from(2).is_err());
    }

    #[test]
    fn test_decompress_into_ptr() {
        for algorithm in [CompressionAlgorithm::Zstd, CompressionAlgorithm::Lz4] {
            let options = ChunkedArchiveOptions::V3 { compression_algorithm: algorithm };
            let compressor = options.thread_local_compressor();
            let compressed = compressor.compress(TEST_DATA, 0).unwrap();

            let decompressor = algorithm.thread_local_decompressor();
            let mut buffer = vec![0u8; TEST_DATA.len()];
            let len = decompressor
                .decompress_into(PtrByteSlice::from(compressed.as_slice()), &mut buffer, 0)
                .unwrap();

            assert_eq!(len, TEST_DATA.len());
            assert_eq!(buffer, TEST_DATA);
        }
    }
}
