// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_CORE_BLOCK_FIFO_H_
#define SRC_DEVICES_BLOCK_DRIVERS_CORE_BLOCK_FIFO_H_

#include <stdint.h>
#include <zircon/types.h>

// LINT.IfChange

// bindgen doesn't like 'using'.
// NOLINTBEGIN(modernize-use-using)
typedef uint32_t reqid_t;
typedef uint16_t groupid_t;
typedef uint16_t vmoid_t;

typedef struct BlockFifoCommand {
  uint8_t opcode;
  uint8_t padding_to_satisfy_zerocopy[3];
  uint32_t flags;
} block_fifo_command_t;

typedef struct BlockFifoRequest {
  block_fifo_command_t command;
  reqid_t reqid;
  groupid_t group;
  vmoid_t vmoid;
  uint32_t length;
  uint32_t padding_to_satisfy_zerocopy;
  uint64_t vmo_offset;
  uint64_t dev_offset;
  uint64_t trace_flow_id;
} block_fifo_request_t;

typedef struct BlockFifoResponse {
  zx_status_t status;
  reqid_t reqid;
  groupid_t group;
  uint16_t padding_to_satisfy_zerocopy;
  uint32_t count;
  uint64_t padding_to_match_request_size_and_alignment[4];
} block_fifo_response_t;

// NOLINTEND(modernize-use-using)

// Notify humans to update Rust bindings because there's no bindgen automation.
// TODO(https://fxbug.dev/42153476): Remove lint when no longer necessary.
// LINT.ThenChange(//src/storage/lib/block_protocol/src/lib.rs)

#endif  // SRC_DEVICES_BLOCK_DRIVERS_CORE_BLOCK_FIFO_H_
