// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef LIB_DRIVER_MMIO_TESTING_CPP_TEST_HELPER_H_
#define LIB_DRIVER_MMIO_TESTING_CPP_TEST_HELPER_H_

#include <lib/driver/mmio/cpp/mmio-buffer.h>
#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>
#include <zircon/types.h>

#include <utility>

namespace fdf_testing {

// Creates an MmioBuffer backed by a new VMO, for use in tests.
inline fdf::MmioBuffer CreateMmioBuffer(
    size_t size, uint32_t cache_policy = ZX_CACHE_POLICY_UNCACHED_DEVICE,
    const ::fdf::MmioBufferOps* ops = ::fdf::MmioBuffer::GetDefaultOps(),
    const void* ctx = nullptr) {
  zx::vmo vmo;
  ZX_ASSERT(zx::vmo::create(/*size=*/size, 0, &vmo) == ZX_OK);
  zx::result<fdf::MmioBuffer> result =
      fdf::MmioBuffer::Create(0, size, std::move(vmo), cache_policy);
  ZX_ASSERT(result.is_ok());
  return fdf::SetMmioBufferOps(std::move(result.value()), ops, ctx);
}

inline fdf::MmioBuffer CreateMmioBuffer(
    zx::vmo vmo, uint32_t cache_policy = ZX_CACHE_POLICY_UNCACHED_DEVICE,
    const ::fdf::MmioBufferOps* ops = ::fdf::MmioBuffer::GetDefaultOps(),
    const void* ctx = nullptr) {
  zx_info_vmo_t info{};
  ZX_ASSERT(vmo.get_info(ZX_INFO_VMO, &info, sizeof(info), /*actual_count=*/0, /*avail_count=*/0) ==
            ZX_OK);
  zx::result<fdf::MmioBuffer> result =
      fdf::MmioBuffer::Create(0, info.size_bytes, std::move(vmo), cache_policy);
  ZX_ASSERT(result.is_ok());
  return fdf::SetMmioBufferOps(std::move(result.value()), ops, ctx);
}

}  // namespace fdf_testing

#endif  // LIB_DRIVER_MMIO_TESTING_CPP_TEST_HELPER_H_
