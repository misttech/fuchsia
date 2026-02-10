// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_LIB_BLOCK_PROTOCOL_BLOCK_FIFO_H_
#define SRC_STORAGE_LIB_BLOCK_PROTOCOL_BLOCK_FIFO_H_

#include <fidl/fuchsia.storage.block/cpp/fidl.h>
#include <stdint.h>
#include <zircon/types.h>

// LINT.IfChange
using reqid_t = uint32_t;
using groupid_t = uint16_t;
using vmoid_t = uint16_t;

#ifndef BLOCK_VMOID_INVALID
const vmoid_t BLOCK_VMOID_INVALID = fuchsia_storage_block::kVmoidInvalid;

const vmoid_t MAX_TXN_GROUP_COUNT = fuchsia_storage_block::kMaxTxnGroupCount;

const uint32_t BLOCK_IO_FLAG_GROUP_ITEM =
    static_cast<uint32_t>(fuchsia_storage_block::BlockIoFlag::kGroupItem);
const uint32_t BLOCK_IO_FLAG_GROUP_LAST =
    static_cast<uint32_t>(fuchsia_storage_block::BlockIoFlag::kGroupLast);
const uint32_t BLOCK_IO_FLAG_FORCE_ACCESS =
    static_cast<uint32_t>(fuchsia_storage_block::BlockIoFlag::kForceAccess);
const uint32_t BLOCK_IO_FLAG_PRE_BARRIER =
    static_cast<uint32_t>(fuchsia_storage_block::BlockIoFlag::kPreBarrier);
const uint32_t BLOCK_IO_FLAG_DECOMPRESS_WITH_ZSTD =
    static_cast<uint32_t>(fuchsia_storage_block::BlockIoFlag::kDecompressWithZstd);
const uint32_t BLOCK_IO_FLAG_INLINE_ENCRYPTION_ENABLED =
    static_cast<uint32_t>(fuchsia_storage_block::BlockIoFlag::kInlineEncryptionEnabled);

const uint8_t BLOCK_OPCODE_TRIM = static_cast<uint8_t>(fuchsia_storage_block::BlockOpcode::kTrim);
const uint8_t BLOCK_OPCODE_FLUSH = static_cast<uint8_t>(fuchsia_storage_block::BlockOpcode::kFlush);
const uint8_t BLOCK_OPCODE_READ = static_cast<uint8_t>(fuchsia_storage_block::BlockOpcode::kRead);
const uint8_t BLOCK_OPCODE_WRITE = static_cast<uint8_t>(fuchsia_storage_block::BlockOpcode::kWrite);
const uint8_t BLOCK_OPCODE_CLOSE_VMO =
    static_cast<uint8_t>(fuchsia_storage_block::BlockOpcode::kCloseVmo);
#endif

struct BlockFifoCommand {
  uint8_t opcode;
  uint8_t padding_to_satisfy_zerocopy[3];
  uint32_t flags;
};

// * Reads with Decompression *
//
// For a non-fragmented read with decompression (`flags` has `DECOMPRESS_WITH_ZSTD` set)
//  `dev_offset` and `length` should specify the compressed blocks.  `total_compressed_bytes` should
//  be the total number of compressed bytes.  `uncompresed_bytes` is the total number of
//  uncompressed bytes.  `compressed_prefix_bytes` specifies the padding at the beginning before the
//  start of compressed data.
//
// For a fragmented read with decompression:
//
// For the first request in the group: `dev_offset` and `length` should specify the compressed
// blocks for the first fragment.  `total_compressed_bytes` should be set to the total number of
// compressed bytes across all requests, and likewise for `uncompressed_bytes` and
// `compressed_prefix_bytes`.
//
// For subsequent requests, `dev_offset` and `length` should specify the compressed blocks.  All
// other fields should be zero.
//
// The group may only contain read requests applicable to the decompressed read.
//
// There is a 128 MiB limit on the total compressed amount.
struct BlockFifoRequest {
  BlockFifoCommand command;
  reqid_t reqid;
  groupid_t group;
  vmoid_t vmoid;
  uint32_t length;  // In blocks.

  // The total number of compressed bytes across all requests in the group (only applicable for the
  // first request in a group). This does *not* include any padding at either the beginning or end.
  uint32_t total_compressed_bytes;

  uint64_t vmo_offset;
  uint64_t dev_offset;
  uint64_t trace_flow_id;
  // The data unit number used as an inline crypto tweak. Only used if the request flags include
  // `INLINE_ENCRYPTION_ENABLED`.
  uint32_t dun;
  // The keyslot for the key used to encrypt/decrypt the request's data if the request flags include
  // `INLINE_ENCRYPTION_ENABLED`.
  uint8_t slot;
  uint8_t padding;

  // The number of bytes to skip at the beginning (only applicable for the first request in a
  // group).
  uint16_t compressed_prefix_bytes;

  // The total number of uncompressed bytes for this request (only applicable for the first request
  // in a group).
  uint32_t uncompressed_bytes;

  uint32_t padding2;
};

struct BlockFifoResponse {
  zx_status_t status;
  reqid_t reqid;
  groupid_t group;
  uint16_t padding_to_satisfy_zerocopy;
  uint32_t count;
  uint64_t padding_to_match_request_size_and_alignment[6];
};

// Notify humans to update Rust bindings because there's no bindgen automation.
// TODO(https://fxbug.dev/42153476): Remove lint when no longer necessary.
// LINT.ThenChange(//src/storage/lib/block_protocol/src/fifo.rs)

#endif  // SRC_STORAGE_LIB_BLOCK_PROTOCOL_BLOCK_FIFO_H_
