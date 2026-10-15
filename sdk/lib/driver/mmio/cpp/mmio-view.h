// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_MMIO_CPP_MMIO_VIEW_H_
#define LIB_DRIVER_MMIO_CPP_MMIO_VIEW_H_

#include <lib/driver/mmio/cpp/mmio-buffer.h>
#include <lib/mmio-ptr/mmio-ptr.h>
#include <lib/zx/vmo.h>
#include <zircon/assert.h>

namespace fdf {

// A sliced view that of an mmio which does not unmap on close. Must not outlive
// mmio buffer it is created from.
class MmioView : public MmioBuffer {
 public:
  MmioView(const MmioBuffer& mmio, zx_off_t offset)
      : MmioView(mmio, offset, mmio.ops(), mmio.ctx()) {}

  MmioView(const MmioBuffer& mmio, zx_off_t offset, const MmioBufferOps* ops,
           const void* ctx = nullptr)
      : MmioBuffer(zx::vmo(), static_cast<MMIO_PTR uint8_t*>(mmio.get()) + offset,
                   mmio.get_offset() + offset, mmio.get_size() - offset, ops, ctx) {
    unowned_vmo() = mmio.get_vmo();
    ZX_ASSERT(offset < mmio.get_size());
  }

  MmioView(const MmioBuffer& mmio, zx_off_t offset, size_t size)
      : MmioView(mmio, offset, size, mmio.ops(), mmio.ctx()) {}

  MmioView(const MmioBuffer& mmio, zx_off_t offset, size_t size, const MmioBufferOps* ops,
           const void* ctx = nullptr)
      : MmioBuffer(zx::vmo(), static_cast<MMIO_PTR uint8_t*>(mmio.get()) + offset,
                   mmio.get_offset() + offset, size, ops, ctx) {
    unowned_vmo() = mmio.get_vmo();
    ZX_ASSERT(offset <= mmio.get_size() && size <= mmio.get_size() - offset);
  }

  MmioView(const MmioView& mmio)
      : MmioBuffer(zx::vmo(), mmio.get(), mmio.get_offset(), mmio.get_size(), mmio.ops(),
                   mmio.ctx()) {
    unowned_vmo() = mmio.get_vmo();
  }
};

// These can't be defined inside the class because they need MmioView
// to be completely defined first.

inline MmioView MmioBuffer::View(zx_off_t off) const { return MmioView(*this, off, ops_, ctx_); }

inline MmioView MmioBuffer::View(zx_off_t off, size_t size) const {
  return MmioView(*this, off, size, ops_, ctx_);
}

}  // namespace fdf

#endif  // LIB_DRIVER_MMIO_CPP_MMIO_VIEW_H_
