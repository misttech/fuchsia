// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_MMIO_CPP_MMIO_PINNED_BUFFER_H_
#define LIB_DRIVER_MMIO_CPP_MMIO_PINNED_BUFFER_H_

#include <lib/zx/pmt.h>
#include <zircon/assert.h>

#include <utility>

namespace fdf {

// MmioPinnedBuffer maintains a pinned region of MMIO memory.
class MmioPinnedBuffer {
 public:
  // DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE
  MmioPinnedBuffer(const MmioPinnedBuffer&) = delete;
  MmioPinnedBuffer& operator=(const MmioPinnedBuffer&) = delete;

  explicit MmioPinnedBuffer(zx_paddr_t paddr, zx::pmt pmt) : pmt_(std::move(pmt)), paddr_(paddr) {
    ZX_ASSERT(paddr_ != 0);
  }

  ~MmioPinnedBuffer() { reset(); }

  MmioPinnedBuffer(MmioPinnedBuffer&& other) { transfer(std::move(other)); }

  MmioPinnedBuffer& operator=(MmioPinnedBuffer&& other) {
    if (this != &other) {
      reset();
      transfer(std::move(other));
    }
    return *this;
  }

  void reset() {
    if (pmt_.is_valid()) {
      pmt_.unpin();
    }
    paddr_ = 0;
  }

  zx_paddr_t get_paddr() const { return paddr_; }

 private:
  void transfer(MmioPinnedBuffer&& other) {
    pmt_ = std::move(other.pmt_);
    paddr_ = other.paddr_;
    other.paddr_ = 0;
  }
  zx::pmt pmt_;
  zx_paddr_t paddr_ = 0;
};

}  // namespace fdf

#endif  // LIB_DRIVER_MMIO_CPP_MMIO_PINNED_BUFFER_H_
