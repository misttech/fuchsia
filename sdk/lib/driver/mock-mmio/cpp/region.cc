// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/mock-mmio/cpp/region.h>
#include <lib/zx/result.h>

#include <utility>

namespace mock_mmio {

void Region::VerifyAll() {
  for (auto& reg : regs_) {
    reg.VerifyAndClear();
  }
}

fdf::MmioBuffer Region::GetMmioBuffer() {
  size_t size = (reg_offset_ + reg_count_) * reg_size_;

  // TODO(https://fxbug.dev/335906001): This creates an unneeded VMO for now.
  zx::vmo vmo;
  ZX_ASSERT(zx::vmo::create(/*size=*/size, 0, &vmo) == ZX_OK);
  zx::result<fdf::MmioBuffer> result =
      fdf::MmioBuffer::Create(0, size, std::move(vmo), ZX_CACHE_POLICY_CACHED);
  ZX_ASSERT(result.is_ok());
  return fdf::SetMmioBufferOps(std::move(result.value()), &Region::kMockMmioOps, this);
}

void Region::CheckOffset(zx_off_t offsets) const {
  ZX_ASSERT(offsets / reg_size_ >= reg_offset_);
  ZX_ASSERT((offsets / reg_size_) - reg_offset_ < reg_count_);
}

}  // namespace mock_mmio
