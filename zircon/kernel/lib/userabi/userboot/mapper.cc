// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "mapper.h"

#include <fbl/algorithm.h>

Mapper::Mapper(const zx::vmar* vmar) : vmar_(vmar) {}

Mapper::~Mapper() { Unmap(); }

zx_status_t Mapper::Map(zx_vm_option_t options, const zx::vmo& vmo, uint64_t offset, size_t size) {
  if (start_ != 0u) {
    return ZX_ERR_BAD_STATE;
  }

  const size_t page_size = zx_system_get_page_size();
  uint64_t remainder = offset % page_size;
  uint64_t mapping_offset = offset - remainder;
  size_t mapping_size = fbl::round_up(remainder + size, page_size);

  zx_status_t status = vmar_->map(options, 0u, vmo, mapping_offset, mapping_size, &start_);
  if (status != ZX_OK) {
    return status;
  }
  size_ = mapping_size;
  data_ = reinterpret_cast<std::byte*>(start_ + remainder);
  return ZX_OK;
}

zx_status_t Mapper::Unmap() {
  if (start_ == 0u) {
    return ZX_ERR_BAD_STATE;
  }
  zx_status_t status = vmar_->unmap(start_, size_);
  if (status != ZX_OK) {
    return status;
  }
  start_ = 0u;
  size_ = 0u;
  return ZX_OK;
}
