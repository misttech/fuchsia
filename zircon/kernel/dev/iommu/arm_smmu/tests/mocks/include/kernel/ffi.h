// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_KERNEL_FFI_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_KERNEL_FFI_H_

#ifdef __clang__
#define FFI_ALWAYS_INLINE [[gnu::always_inline]]
#else
#define FFI_ALWAYS_INLINE
#endif

namespace ffi {
template <typename T>
struct Uninitialized {};
}  // namespace ffi

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_KERNEL_FFI_H_
