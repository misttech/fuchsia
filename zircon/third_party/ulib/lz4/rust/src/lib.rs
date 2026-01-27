// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::os::raw::{c_char, c_int};

unsafe extern "C" {
    fn LZ4_compress_default(
        src: *const c_char,
        dst: *mut c_char,
        srcSize: c_int,
        dstCapacity: c_int,
    ) -> c_int;
    fn LZ4_decompress_safe(
        src: *const c_char,
        dst: *mut c_char,
        compressedSize: c_int,
        dstCapacity: c_int,
    ) -> c_int;
    fn LZ4_compressBound(inputSize: c_int) -> c_int;
}

/// Compresses the given data using LZ4.
pub fn compress(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    let src_size = data.len() as c_int;
    let bound = unsafe { LZ4_compressBound(src_size) };
    let mut compressed = vec![0u8; bound as usize];
    let compressed_size = unsafe {
        LZ4_compress_default(
            data.as_ptr() as *const c_char,
            compressed.as_mut_ptr() as *mut c_char,
            src_size,
            bound,
        )
    };
    assert!(compressed_size > 0, "LZ4 compression failed");
    compressed.truncate(compressed_size as usize);
    compressed
}

/// Decompresses the given data using LZ4, expecting the given uncompressed size.
pub fn decompress(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>, i32> {
    if uncompressed_size == 0 {
        return Ok(Vec::new());
    }
    let mut decompressed = vec![0u8; uncompressed_size];
    let result = unsafe {
        LZ4_decompress_safe(
            data.as_ptr() as *const c_char,
            decompressed.as_mut_ptr() as *mut c_char,
            data.len() as c_int,
            uncompressed_size as c_int,
        )
    };
    if result < 0 {
        return Err(result);
    }
    if result as usize != uncompressed_size {
        return Err(-1);
    }
    Ok(decompressed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let data = b"Hello, world! LZ4 compression test.";
        let compressed = compress(data);
        let decompressed = decompress(&compressed, data.len()).unwrap();
        assert_eq!(data.as_slice(), decompressed.as_slice());
    }

    #[test]
    fn test_empty() {
        let data = b"";
        let compressed = compress(data);
        let decompressed = decompress(&compressed, 0).unwrap();
        assert_eq!(data.as_slice(), decompressed.as_slice());
    }
}
