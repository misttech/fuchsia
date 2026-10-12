// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fxt::blob::BlobType;
use fxt::fxt_builder::FxtBuilder;
use fxt::metadata::{ProviderInfoMetadataHeader, ProviderSectionMetadataHeader};
use zerocopy::IntoBytes;

/// High provider ID chosen to avoid colliding with trace_manager provider IDs in `ffx trace`.
pub const PROFILER_PROVIDER_ID: u32 = 0xFFFF_0001;

/// Maximum payload bytes per type-5 Perfetto blob record.
/// 32,000 bytes matches `TRACE_MAX_BLOB_SIZE` and leaves room in an FXT record
/// (`fxt::MAX_RECORD_SIZE_WORDS`) for the header and a provider name of up to 255 bytes.
pub const MAX_BLOB_PAYLOAD_BYTES: usize = 32_000;

const FXT_MAGIC_RECORD: [u8; 8] = [0x10, 0x00, 0x04, 0x46, 0x78, 0x54, 0x16, 0x00];
const PROVIDER_INFO_METADATA_TYPE: u8 = 1;
const PROVIDER_SECTION_METADATA_TYPE: u8 = 2;

fn emit_blob_record(out: &mut Vec<u8>, name: &str, payload: &[u8]) {
    let record = fxt::blob::build_blob_record(name, BlobType::Perfetto, payload)
        .expect("a MAX_BLOB_PAYLOAD_BYTES payload and a 255-byte name fit in one record");
    out.extend_from_slice(record.as_slice().as_bytes());
}

/// Wraps a `Trace.packet` byte stream in FXT metadata and type-5 (`BlobType::Perfetto`) records
/// so the output is both a valid standalone FXT session and safe to concatenate after an
/// `ffx trace` `.fxt` file. `packets` is split blindly into `MAX_BLOB_PAYLOAD_BYTES` chunks.
pub fn wrap_perfetto_blobs(packets: &[u8], provider_name: &str) -> Vec<u8> {
    let max_len = provider_name.floor_char_boundary(u8::MAX as usize);
    let name = &provider_name[..max_len];
    let name_bytes = name.as_bytes();
    let num_blobs = packets.len().div_ceil(MAX_BLOB_PAYLOAD_BYTES);
    let per_blob_overhead = 8 + name_bytes.len().next_multiple_of(8) + 8;
    let mut out = Vec::with_capacity(packets.len() + 64 + num_blobs * per_blob_overhead);

    // 1. Magic number record (type 0, metadata_type 4: 0x0016547846040010).
    // Required by `fxt::session::SessionParser` at the start of a session and ignored by
    // Perfetto's `FuchsiaTraceTokenizer` when encountered mid-stream after `cat trace.fxt ...`.
    out.extend_from_slice(&FXT_MAGIC_RECORD);

    // 2. ProviderInfo metadata record (type 0, metadata_type 1).
    let mut provider_info = ProviderInfoMetadataHeader::empty();
    provider_info.set_metadata_type(PROVIDER_INFO_METADATA_TYPE);
    provider_info.set_provider_id(PROFILER_PROVIDER_ID);
    provider_info.set_name_len(name_bytes.len() as u8);
    let provider_info_record = FxtBuilder::new(provider_info).atom(name_bytes).build();
    out.extend_from_slice(provider_info_record.as_slice().as_bytes());

    // 3. ProviderSection metadata record (type 0, metadata_type 2).
    let mut provider_section = ProviderSectionMetadataHeader::empty();
    provider_section.set_metadata_type(PROVIDER_SECTION_METADATA_TYPE);
    provider_section.set_provider_id(PROFILER_PROVIDER_ID);
    let provider_section_record = FxtBuilder::new(provider_section).build();
    out.extend_from_slice(provider_section_record.as_slice().as_bytes());

    // 4. Type-5 Blob records (BlobType::Perfetto). Perfetto's FXT tokenizer appends every
    // Perfetto blob payload into one contiguous buffer before parsing protos, so chunk
    // boundaries need not align with `TracePacket` boundaries (Starnix's perfetto_consumer
    // chunks blindly the same way).
    for chunk in packets.chunks(MAX_BLOB_PAYLOAD_BYTES) {
        emit_blob_record(&mut out, name, chunk);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxt::TraceRecord;
    use fxt::session::parse_full_session;

    fn verify_fxt_container_invariants(fxt_bytes: &[u8]) {
        assert_eq!(fxt_bytes.len() % 8, 0, "total FXT stream must be 8-byte aligned");
        let mut offset = 0;
        while offset < fxt_bytes.len() {
            let header = u64::from_le_bytes(fxt_bytes[offset..offset + 8].try_into().unwrap());
            let size_words = ((header >> 4) & 0xfff) as usize;
            assert!(
                size_words > 0 && size_words <= fxt::MAX_RECORD_SIZE_WORDS,
                "invalid size_words: {size_words}"
            );
            let record_len = size_words * 8;
            assert_eq!(record_len % 8, 0);
            assert!(offset + record_len <= fxt_bytes.len());
            offset += record_len;
        }
        assert_eq!(offset, fxt_bytes.len());
    }

    #[test]
    fn test_wrap_perfetto_blobs_empty_input() {
        let wrapped = wrap_perfetto_blobs(&[], "ffx_profiler");
        verify_fxt_container_invariants(&wrapped);

        let (records, warnings) = parse_full_session(&wrapped).expect("valid FXT session");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        let blob_bytes: Vec<u8> = records
            .into_iter()
            .filter_map(|r| match r {
                TraceRecord::Blob(b) => Some(b.bytes),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(blob_bytes.is_empty());
    }

    #[test]
    fn test_wrap_perfetto_blobs_exact_limit() {
        let input: Vec<u8> = (0..MAX_BLOB_PAYLOAD_BYTES).map(|i| (i % 251) as u8).collect();
        let wrapped = wrap_perfetto_blobs(&input, "ffx_profiler");
        verify_fxt_container_invariants(&wrapped);

        let (records, warnings) = parse_full_session(&wrapped).expect("valid FXT session");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        let blobs: Vec<_> = records
            .into_iter()
            .filter_map(|r| match r {
                TraceRecord::Blob(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].ty, BlobType::Perfetto);
        assert_eq!(blobs[0].name.as_str(), "ffx_profiler");
        assert_eq!(blobs[0].bytes, input);
    }

    #[test]
    fn test_wrap_perfetto_blobs_limit_plus_one() {
        let input: Vec<u8> = (0..(MAX_BLOB_PAYLOAD_BYTES + 1)).map(|i| (i % 253) as u8).collect();
        let wrapped = wrap_perfetto_blobs(&input, "ffx_profiler");
        verify_fxt_container_invariants(&wrapped);

        let (records, warnings) = parse_full_session(&wrapped).expect("valid FXT session");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        let mut concatenated = Vec::new();
        let mut blob_count = 0;
        for record in records {
            if let TraceRecord::Blob(b) = record {
                assert_eq!(b.ty, BlobType::Perfetto);
                assert_eq!(b.name.as_str(), "ffx_profiler");
                assert!(b.bytes.len() <= MAX_BLOB_PAYLOAD_BYTES);
                concatenated.extend_from_slice(&b.bytes);
                blob_count += 1;
            }
        }
        assert_eq!(blob_count, 2);
        assert_eq!(concatenated, input);
    }

    #[test]
    fn test_wrap_perfetto_blobs_multi_chunk_roundtrip() {
        // Pseudo-random bytes (a 32-bit LCG) so no chunk is trivially repetitive.
        let mut state: u32 = 0x1234_5678;
        let input: Vec<u8> = (0..(2 * MAX_BLOB_PAYLOAD_BYTES + 1))
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect();
        let wrapped = wrap_perfetto_blobs(&input, "ffx_profiler");
        verify_fxt_container_invariants(&wrapped);

        let (records, warnings) = parse_full_session(&wrapped).expect("valid FXT session");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        let blobs: Vec<_> = records
            .into_iter()
            .filter_map(|r| match r {
                TraceRecord::Blob(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(blobs.len(), 3);
        for blob in &blobs {
            assert_eq!(blob.ty, BlobType::Perfetto);
            assert_eq!(blob.name.as_str(), "ffx_profiler");
        }
        assert_eq!(blobs[0].bytes.len(), MAX_BLOB_PAYLOAD_BYTES);
        assert_eq!(blobs[1].bytes.len(), MAX_BLOB_PAYLOAD_BYTES);
        assert_eq!(blobs[2].bytes.len(), 1);

        let concatenated: Vec<u8> = blobs.into_iter().flat_map(|b| b.bytes).collect();
        assert_eq!(concatenated, input);
    }

    #[test]
    fn test_wrap_perfetto_blobs_truncates_name_on_char_boundary() {
        // 200 x "é" (2 bytes each) = 400 bytes; the 255-byte limit falls mid-character, so the
        // name must be cut back to 254 bytes (127 x "é") rather than at a non-UTF-8 boundary.
        let provider_name = "é".repeat(200);
        let input: Vec<u8> = (0..(MAX_BLOB_PAYLOAD_BYTES + 1)).map(|i| (i % 7) as u8).collect();
        let wrapped = wrap_perfetto_blobs(&input, &provider_name);
        verify_fxt_container_invariants(&wrapped);

        let (records, warnings) = parse_full_session(&wrapped).expect("valid FXT session");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        let expected_name = "é".repeat(127);
        assert_eq!(expected_name.len(), 254);
        let mut blob_count = 0;
        for record in records {
            if let TraceRecord::Blob(b) = record {
                assert_eq!(b.name.as_str().len(), 254);
                assert_eq!(b.name.as_str(), expected_name);
                blob_count += 1;
            }
        }
        assert_eq!(blob_count, 2);
    }
}
