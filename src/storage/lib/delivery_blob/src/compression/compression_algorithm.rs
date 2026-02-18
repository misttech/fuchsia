// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Compression algorithms supported by chunked-compression and corresponding compressors and
//! decompressors.
//!
//! The compressors and decompressors are enums rather than traits with multiple implementations
//! because the enums are small and avoid the heap allocation of `Box<dyn Decompressor>`.

use crate::compression::ChunkedArchiveError;

thread_local! {
    static ZSTD_COMPRESSOR: std::cell::RefCell<zstd::bulk::Compressor<'static>> =
        std::cell::RefCell::new({
            let mut compressor = zstd::bulk::Compressor::default();
            compressor.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true)).unwrap();
            compressor
        });
    static ZSTD_DECOMPRESSOR: std::cell::RefCell<zstd::bulk::Decompressor<'static>> =
        std::cell::RefCell::new(zstd::bulk::Decompressor::default());
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
            Self::Zstd => Decompressor::Zstd(zstd::bulk::Decompressor::default()),
            Self::Lz4 => Decompressor::Lz4,
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

/// A decompressor that is capable of decompressing chunks of a compressed archive.
pub enum Decompressor {
    Zstd(zstd::bulk::Decompressor<'static>),
    Lz4,
}

impl Decompressor {
    /// Decompresses a chunk of a chunked-compression archive.
    pub fn decompress(
        &mut self,
        data: &[u8],
        uncompressed_size: usize,
        chunk_index: usize,
    ) -> Result<Vec<u8>, ChunkedArchiveError> {
        match self {
            Self::Zstd(decompressor) => {
                decompressor.decompress(data, uncompressed_size).map_err(|error| {
                    ChunkedArchiveError::DecompressionError { index: chunk_index, error }
                })
            }
            Self::Lz4 => lz4::decompress(data, uncompressed_size).map_err(|_| {
                ChunkedArchiveError::DecompressionError {
                    index: chunk_index,
                    error: std::io::Error::other("LZ4 decompression error"),
                }
            }),
        }
    }

    /// Decompresses a chunk of a chunked-compression archive into a pre-allocated buffer.
    pub fn decompress_into<'a>(
        &mut self,
        data: &[u8],
        destination: &'a mut [u8],
        chunk_index: usize,
    ) -> Result<usize, ChunkedArchiveError> {
        match self {
            Self::Zstd(decompressor) => {
                decompressor.decompress_to_buffer(data, destination).map_err(|error| {
                    ChunkedArchiveError::DecompressionError { index: chunk_index, error }
                })
            }
            Self::Lz4 => lz4::decompress_into(data, destination).map_err(|e| {
                ChunkedArchiveError::DecompressionError {
                    index: chunk_index,
                    error: std::io::Error::other(e),
                }
            }),
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
    pub fn decompress(
        &self,
        data: &[u8],
        uncompressed_size: usize,
        chunk_index: usize,
    ) -> Result<Vec<u8>, ChunkedArchiveError> {
        match self {
            Self::Zstd => ZSTD_DECOMPRESSOR.with(|decompressor| {
                decompressor.borrow_mut().decompress(data, uncompressed_size).map_err(|error| {
                    ChunkedArchiveError::DecompressionError { index: chunk_index, error }
                })
            }),
            Self::Lz4 => lz4::decompress(data, uncompressed_size).map_err(|_| {
                ChunkedArchiveError::DecompressionError {
                    index: chunk_index,
                    error: std::io::Error::other("LZ4 decompression error"),
                }
            }),
        }
    }

    /// Decompresses a chunk of a chunked-compression archive into a pre-allocated buffer.
    pub fn decompress_into<'a>(
        &self,
        data: &[u8],
        destination: &'a mut [u8],
        chunk_index: usize,
    ) -> Result<usize, ChunkedArchiveError> {
        match self {
            Self::Zstd => ZSTD_DECOMPRESSOR.with(|decompressor| {
                decompressor.borrow_mut().decompress_to_buffer(data, destination).map_err(|error| {
                    ChunkedArchiveError::DecompressionError { index: chunk_index, error }
                })
            }),
            Self::Lz4 => lz4::decompress_into(data, destination).map_err(|e| {
                ChunkedArchiveError::DecompressionError {
                    index: chunk_index,
                    error: std::io::Error::other(e),
                }
            }),
        }
    }
}

/// A compressor that is capable of compressing chunks of a chunked-compression archive.
pub enum Compressor {
    Zstd(zstd::bulk::Compressor<'static>),
    Lz4 { compression_level: lz4::HcCompressionLevel },
}

impl Compressor {
    /// Compresses a chunk of a chunked-compression archive.
    pub fn compress(
        &mut self,
        data: &[u8],
        chunk_index: usize,
    ) -> Result<Vec<u8>, ChunkedArchiveError> {
        match self {
            Self::Zstd(compressor) => compressor.compress(data).map_err(|error| {
                ChunkedArchiveError::CompressionError { index: chunk_index, error }
            }),
            Self::Lz4 { compression_level } => Ok(lz4::compress_hc(data, *compression_level)
                .expect("chunk size is less than max LZ4 input")),
        }
    }
}

#[derive(Copy, Clone)]
/// A compressor that uses thread-local storage to avoid reallocation of large state objects.
pub enum ThreadLocalCompressor {
    Zstd { compression_level: i32 },
    Lz4 { compression_level: lz4::HcCompressionLevel },
}

impl ThreadLocalCompressor {
    /// Compresses a chunk of a chunked-compression archive.
    pub fn compress(
        &self,
        data: &[u8],
        chunk_index: usize,
    ) -> Result<Vec<u8>, ChunkedArchiveError> {
        match self {
            Self::Zstd { compression_level } => ZSTD_COMPRESSOR.with(|compressor| {
                let mut compressor = compressor.borrow_mut();
                compressor
                    .set_compression_level(*compression_level)
                    .expect("setting the compression level should never fail");
                compressor.compress(data).map_err(|error| ChunkedArchiveError::CompressionError {
                    index: chunk_index,
                    error,
                })
            }),
            Self::Lz4 { compression_level } => Ok(lz4::compress_hc(data, *compression_level)
                .expect("chunk size is less than max LZ4 input")),
        }
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
}
