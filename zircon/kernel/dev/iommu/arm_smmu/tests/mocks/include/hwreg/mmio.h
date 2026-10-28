// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_HWREG_MMIO_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_HWREG_MMIO_H_

#include <lib/mmio-ptr/mmio-ptr.h>
#include <sys/types.h>
#include <zircon/assert.h>

#include <cstdint>
#include <mutex>
#include <type_traits>
#include <vector>

#include <hwreg/internal.h>

class FakeMmioHandler {
 public:
  virtual ~FakeMmioHandler() = default;
  virtual uint32_t Read32(uint64_t offset) = 0;
  virtual void Write32(uint64_t offset, uint32_t val) = 0;
  virtual uint64_t Read64(uint64_t offset) = 0;
  virtual void Write64(uint64_t offset, uint64_t val) = 0;
};

class FakeSmmuMmioRouter {
 public:
  struct Region {
    paddr_t paddr_base = 0;
    vaddr_t vaddr_base = 0;
    size_t size = 0;
    FakeMmioHandler* handler = nullptr;
  };

  static FakeSmmuMmioRouter& Get() {
    static FakeSmmuMmioRouter instance;
    return instance;
  }

  void RegisterRegion(paddr_t paddr_base, vaddr_t vaddr_base, size_t size,
                      FakeMmioHandler* handler) {
    std::lock_guard<std::mutex> guard(lock_);
    UnregisterByPaddrLocked(paddr_base);
    regions_.push_back(Region{
        .paddr_base = paddr_base,
        .vaddr_base = vaddr_base,
        .size = size,
        .handler = handler,
    });
  }

  void UnregisterRegion(paddr_t paddr_base) {
    std::lock_guard<std::mutex> guard(lock_);
    UnregisterByPaddrLocked(paddr_base);
  }

  void Reset() {
    std::lock_guard<std::mutex> guard(lock_);
    regions_.clear();
  }

  vaddr_t PaddrToVaddr(paddr_t paddr) {
    std::lock_guard<std::mutex> guard(lock_);
    for (const auto& r : regions_) {
      if (paddr >= r.paddr_base && paddr < r.paddr_base + r.size) {
        return r.vaddr_base + (paddr - r.paddr_base);
      }
    }
    return 0;
  }

  paddr_t VaddrToPaddr(vaddr_t vaddr) {
    std::lock_guard<std::mutex> guard(lock_);
    for (const auto& r : regions_) {
      if (vaddr >= r.vaddr_base && vaddr < r.vaddr_base + r.size) {
        return r.paddr_base + (vaddr - r.vaddr_base);
      }
    }
    return 0;
  }

  bool TryRead32(vaddr_t vaddr, uint32_t* out_val) {
    FakeMmioHandler* handler = nullptr;
    uint64_t offset = 0;
    {
      std::lock_guard<std::mutex> guard(lock_);
      for (const auto& r : regions_) {
        if (vaddr >= r.vaddr_base && vaddr + sizeof(uint32_t) <= r.vaddr_base + r.size) {
          handler = r.handler;
          offset = vaddr - r.vaddr_base;
          break;
        }
      }
    }
    if (!handler) {
      return false;
    }
    *out_val = handler->Read32(offset);
    return true;
  }

  bool TryWrite32(vaddr_t vaddr, uint32_t val) {
    FakeMmioHandler* handler = nullptr;
    uint64_t offset = 0;
    {
      std::lock_guard<std::mutex> guard(lock_);
      for (const auto& r : regions_) {
        if (vaddr >= r.vaddr_base && vaddr + sizeof(uint32_t) <= r.vaddr_base + r.size) {
          handler = r.handler;
          offset = vaddr - r.vaddr_base;
          break;
        }
      }
    }
    if (!handler) {
      return false;
    }
    handler->Write32(offset, val);
    return true;
  }

  bool TryRead64(vaddr_t vaddr, uint64_t* out_val) {
    FakeMmioHandler* handler = nullptr;
    uint64_t offset = 0;
    {
      std::lock_guard<std::mutex> guard(lock_);
      for (const auto& r : regions_) {
        if (vaddr >= r.vaddr_base && vaddr + sizeof(uint64_t) <= r.vaddr_base + r.size) {
          handler = r.handler;
          offset = vaddr - r.vaddr_base;
          break;
        }
      }
    }
    if (!handler) {
      return false;
    }
    *out_val = handler->Read64(offset);
    return true;
  }

  bool TryWrite64(vaddr_t vaddr, uint64_t val) {
    FakeMmioHandler* handler = nullptr;
    uint64_t offset = 0;
    {
      std::lock_guard<std::mutex> guard(lock_);
      for (const auto& r : regions_) {
        if (vaddr >= r.vaddr_base && vaddr + sizeof(uint64_t) <= r.vaddr_base + r.size) {
          handler = r.handler;
          offset = vaddr - r.vaddr_base;
          break;
        }
      }
    }
    if (!handler) {
      return false;
    }
    handler->Write64(offset, val);
    return true;
  }

 private:
  void UnregisterByPaddrLocked(paddr_t paddr_base) {
    for (auto it = regions_.begin(); it != regions_.end();) {
      if (it->paddr_base == paddr_base) {
        it = regions_.erase(it);
      } else {
        ++it;
      }
    }
  }

  std::mutex lock_;
  std::vector<Region> regions_;
};

namespace hwreg {

template <typename ForcedAccessType = void>
class RegisterMmioScaled {
 public:
  RegisterMmioScaled(volatile void* mmio) : mmio_(InitPtr(mmio)) {}

  template <typename IntType>
  void Write(IntType val, uint32_t offset) {
    static_assert(sizeof(IntType) <= sizeof(IoType<IntType>));
    using AccessInt = IoType<IntType>;
    const vaddr_t vaddr = base() + (offset * kScale);
    if constexpr (sizeof(AccessInt) == sizeof(uint32_t)) {
      if (FakeSmmuMmioRouter::Get().TryWrite32(vaddr, static_cast<uint32_t>(val))) {
        return;
      }
    } else if constexpr (sizeof(AccessInt) == sizeof(uint64_t)) {
      if (FakeSmmuMmioRouter::Get().TryWrite64(vaddr, static_cast<uint64_t>(val))) {
        return;
      }
    }
    MmioWrite(static_cast<AccessInt>(val), MmioPtr<AccessInt>(offset));
  }

  template <typename IntType>
  IntType Read(uint32_t offset) {
    static_assert(sizeof(IntType) <= sizeof(IoType<IntType>));
    using AccessInt = IoType<IntType>;
    const vaddr_t vaddr = base() + (offset * kScale);
    if constexpr (sizeof(AccessInt) == sizeof(uint32_t)) {
      uint32_t out = 0;
      if (FakeSmmuMmioRouter::Get().TryRead32(vaddr, &out)) {
        return static_cast<IntType>(out);
      }
    } else if constexpr (sizeof(AccessInt) == sizeof(uint64_t)) {
      uint64_t out = 0;
      if (FakeSmmuMmioRouter::Get().TryRead64(vaddr, &out)) {
        return static_cast<IntType>(out);
      }
    }
    return static_cast<IntType>(MmioRead(MmioPtr<const AccessInt>(offset)));
  }

  uintptr_t base() const { return reinterpret_cast<uintptr_t>(mmio_); }

 private:
  static_assert(std::is_void_v<ForcedAccessType> ||
                    hwreg::internal::IsSupportedInt<ForcedAccessType>::value,
                "Unsupported type.");

  template <typename IntType>
  using IoType = std::conditional_t<std::is_void_v<ForcedAccessType>, IntType, ForcedAccessType>;

  static constexpr uint32_t kScale =
      sizeof(std::conditional_t<std::is_void_v<ForcedAccessType>, char, ForcedAccessType>);

  MMIO_PTR volatile std::byte* InitPtr(volatile void* ptr) {
    uintptr_t addr = reinterpret_cast<uintptr_t>(ptr);
    return reinterpret_cast<MMIO_PTR volatile std::byte*>(addr);
  }

  template <typename U>
  MMIO_PTR volatile U* MmioPtr(uint32_t offset) {
    using PtrType = MMIO_PTR volatile U*;
    using IntType = std::remove_const_t<U>;
    static_assert(internal::IsSupportedInt<IntType>::value, "unsupported register access width");
    MMIO_PTR volatile std::byte* addr = mmio_ + (offset * kScale);
    return reinterpret_cast<PtrType>(addr);
  }

  MMIO_PTR volatile std::byte* mmio_ = nullptr;
};

using RegisterMmio = RegisterMmioScaled<>;
static_assert(std::is_copy_constructible_v<RegisterMmio>);

}  // namespace hwreg

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_HWREG_MMIO_H_
