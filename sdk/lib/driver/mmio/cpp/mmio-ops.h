// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_MMIO_CPP_MMIO_OPS_H_
#define LIB_DRIVER_MMIO_CPP_MMIO_OPS_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/types.h>

namespace fdf {

class MmioBuffer;

struct MmioBufferOps {
  uint8_t (*Read8)(const void* ctx, const MmioBuffer& mmio, zx_off_t offs);
  uint16_t (*Read16)(const void* ctx, const MmioBuffer& mmio, zx_off_t offs);
  uint32_t (*Read32)(const void* ctx, const MmioBuffer& mmio, zx_off_t offs);
  uint64_t (*Read64)(const void* ctx, const MmioBuffer& mmio, zx_off_t offs);
  void (*ReadBuffer)(const void* ctx, const MmioBuffer& mmio, zx_off_t offs, void* buffer,
                     size_t size);

  void (*Write8)(const void* ctx, const MmioBuffer& mmio, uint8_t val, zx_off_t offs);
  void (*Write16)(const void* ctx, const MmioBuffer& mmio, uint16_t val, zx_off_t offs);
  void (*Write32)(const void* ctx, const MmioBuffer& mmio, uint32_t val, zx_off_t offs);
  void (*Write64)(const void* ctx, const MmioBuffer& mmio, uint64_t val, zx_off_t offs);
  void (*WriteBuffer)(const void* ctx, const MmioBuffer& mmio, zx_off_t offs, const void* buffer,
                      size_t size);
};
}  // namespace fdf

#endif  // LIB_DRIVER_MMIO_CPP_MMIO_OPS_H_
