// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_FBL_NAME_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_FBL_NAME_H_

#include <stddef.h>
#include <string.h>
#include <zircon/types.h>

#include <algorithm>
#include <mutex>

namespace fbl {

template <size_t Size>
class Name {
 public:
  Name() = default;
  ~Name() = default;

  void get(size_t out_len, char* out_name) const {
    std::lock_guard<std::mutex> guard(lock_);
    if (out_len == 0) {
      return;
    }
    size_t copy_len = std::min(out_len - 1, Size - 1);
    memcpy(out_name, name_, copy_len);
    out_name[copy_len] = '\0';
  }

  zx_status_t set(const char* name, size_t len) {
    std::lock_guard<std::mutex> guard(lock_);
    memset(name_, 0, Size);
    if (name && len > 0) {
      size_t copy_len = std::min(len, Size - 1);
      memcpy(name_, name, copy_len);
      name_[copy_len] = '\0';
    }
    return ZX_OK;
  }

 private:
  mutable std::mutex lock_;
  char name_[Size]{0};
};

}  // namespace fbl

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_FBL_NAME_H_
