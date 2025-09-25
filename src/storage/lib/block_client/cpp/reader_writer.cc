// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/lib/block_client/cpp/reader_writer.h"

#include <safemath/checked_math.h>

namespace block_client {
namespace {

constexpr uint64_t kDefaultBufferSize = 128ul * 1024;

}  // namespace

zx_status_t ReaderWriter::Read(uint64_t offset, const size_t count, void* buf) {
  if (zx_status_t status = EnsureBufferInitialized(); status != ZX_OK) {
    return status;
  }
  return DoIo(offset, count, vmoid_.get(), 0, false, buf);
}

zx_status_t ReaderWriter::Read(uint64_t offset, const size_t count, const zx::vmo& vmo,
                               uint64_t vmo_offset) {
  storage::OwnedVmoid vmoid;
  if (zx_status_t status = device_.BlockAttachVmo(vmo, &vmoid.GetReference(&device_));
      status != ZX_OK) {
    return status;
  }
  return DoIo(offset, count, vmoid.get(), vmo_offset, false, std::nullopt);
}

zx_status_t ReaderWriter::Write(uint64_t offset, const size_t count, void* buf) {
  if (zx_status_t status = EnsureBufferInitialized(); status != ZX_OK) {
    return status;
  }
  return DoIo(offset, count, vmoid_.get(), 0, true, buf);
}

zx_status_t ReaderWriter::Write(uint64_t offset, const size_t count, const zx::vmo& vmo,
                                uint64_t vmo_offset) {
  storage::OwnedVmoid vmoid;
  if (zx_status_t status = device_.BlockAttachVmo(vmo, &vmoid.GetReference(&device_));
      status != ZX_OK) {
    return status;
  }
  return DoIo(offset, count, vmoid.get(), vmo_offset, true, std::nullopt);
}

zx_status_t ReaderWriter::EnsureBufferInitialized() {
  if (!buffer_.vmo()) {
    if (zx_status_t status =
            buffer_.CreateAndMap(kDefaultBufferSize, "block_client::ReaderWriteer");
        status != ZX_OK) {
      return status;
    }

    if (zx_status_t status = device_.BlockAttachVmo(buffer_.vmo(), &vmoid_.GetReference(&device_));
        status != ZX_OK) {
      return status;
    }
  }
  return ZX_OK;
}

zx_status_t ReaderWriter::DoIo(uint64_t offset, size_t count, vmoid_t vmoid, uint64_t vmo_offset,
                               bool write, std::optional<void*> buf) {
  if (block_size_ == 0) {
    fuchsia_hardware_block::wire::BlockInfo info;
    if (zx_status_t status = device_.BlockGetInfo(&info); status != ZX_OK)
      return status;

    block_size_ = info.block_size;
  }
  const uint64_t read_size = std::max(kDefaultBufferSize, block_size_);
  if (count % block_size_ != 0 || offset % block_size_ != 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  uint64_t remaining = count;
  while (remaining > 0) {
    size_t amount = std::min(remaining, read_size);
    if (buf && write) {
      memcpy(buffer_.start(), *buf, amount);
    }
    uint8_t opcode = write ? BLOCK_OPCODE_WRITE : BLOCK_OPCODE_READ;
    block_fifo_request_t request = {
        .command = {.opcode = opcode, .flags = 0},
        .vmoid = vmoid,
        .length = safemath::checked_cast<uint32_t>(amount / block_size_),
        .vmo_offset = vmo_offset / block_size_,
        .dev_offset = offset / block_size_,
    };
    if (zx_status_t status = device_.FifoTransaction(&request, 1); status != ZX_OK)
      return status;
    remaining -= amount;
    offset += amount;
    if (buf) {
      if (!write) {
        memcpy(*buf, buffer_.start(), amount);
      }
      *buf = static_cast<uint8_t*>(*buf) + amount;
    } else {
      vmo_offset += amount;
    }
  }

  return ZX_OK;
}

}  // namespace block_client
