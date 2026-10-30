// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <lib/fit/defer.h>
#include <stdio.h>
#include <zircon/errors.h>
#include <zircon/syscalls/iommu.h>
#include <zircon/types.h>

#include <chrono>
#include <map>
#include <memory>
#include <optional>
#include <random>
#include <utility>
#include <vector>

#include <dev/arm_smmu/context_bank.h>
#include <dev/arm_smmu/smmu.h>
#include <dev/arm_smmu/smmu_bti.h>
#include <dev/arm_smmu/smmu_mode.h>
#include <dev/arm_smmu/smmu_pmt.h>
#include <dev/arm_smmu/stream_match_reg_group.h>
#include <dev/iommu/bti.h>
#include <dev/iommu/iommu.h>
#include <dev/iommu/pmt.h>
#include <fbl/ref_ptr.h>
#include <vm/pinned_vm_object.h>
#include <vm/pmm.h>
#include <zxtest/zxtest.h>

#include "cross_test_ffi.h"
#include "sim_smmu_hardware_model.h"

namespace arm_smmu {

// Cleanly tears down an Smmu instance between host tests and removes it from
// the static Smmu::instances_ list while verifying destructor invariants.
void SmmuTestAccess::DestroySmmu(fbl::RefPtr<iommu::Iommu>&& iommu) {
  if (!iommu) {
    return;
  }
  fbl::RefPtr<Smmu> target;
  {
    Guard<Mutex> guard{Smmu::InstanceLock::Get()};
    for (Smmu& s : Smmu::instances_) {
      if (static_cast<iommu::Iommu*>(&s) == iommu.get()) {
        target = Smmu::instances_.erase(s);
        break;
      }
    }
  }
  iommu.reset();
  if (!target) {
    return;
  }

  // Shut down any remaining adopted or quarantined BTIs before unregistering
  // interrupts so all ContextBank, SMRG, and DeviceAspace resources are
  // cleanly released.
  while (true) {
    fbl::RefPtr<SmmuBti> bti;
    {
      Guard<Mutex> guard{&target->lock_};
      if (!target->bti_list_.is_empty()) {
        bti = fbl::RefPtr<SmmuBti>{&target->bti_list_.front()};
      }
    }
    if (!bti) {
      break;
    }
    bool handled_adopted_without_cb = false;
    fbl::DoublyLinkedList<ktl::unique_ptr<StreamMatchRegGroup>> local_smrg_list;
    {
      Guard<Mutex> pmt_guard{&bti->pmt_lock_};
      Guard<SpinLock, IrqSave> bti_guard{&bti->lock_};
      bti->quarantined_pmt_count_ = 0;
      bti->quarantined_page_count_ = 0;
      if (bti->mode_ == BtiMode::kAdopted) {
        if (bti->context_bank_) {
          bti->mode_ = BtiMode::kFault;
          bti->context_bank_->mode_ = BtiMode::kFault;
        } else {
          // Adopted BTI in bypass mode (no ContextBank): disable its SMRGs directly.
          for (StreamMatchRegGroup& smrg : bti->smrg_list_) {
            smrg.Disable(*bti);
          }
          local_smrg_list = ktl::move(bti->smrg_list_);
          bti->mode_ = BtiMode::kShutdown;
          handled_adopted_without_cb = true;
        }
      }
    }
    local_smrg_list.clear();
    if (handled_adopted_without_cb) {
      {
        Guard<Mutex> guard{&target->lock_};
        target->bti_list_.erase(*bti);
      }
      const_cast<fbl::RefPtr<Smmu>&>(bti->smmu_).reset();
    } else {
      bti->OnEndOfLife();
    }
  }

  {
    Guard<Mutex> guard{&target->lock_};
    target->UnregisterInterrupts();
  }
  target.reset();
}

bool SmmuTestAccess::IsRegisteredSmmu(const iommu::Iommu* iommu) {
  if (iommu == nullptr) {
    return false;
  }
  Guard<Mutex> guard{Smmu::InstanceLock::Get()};
  for (const Smmu& s : Smmu::instances_) {
    if (static_cast<const iommu::Iommu*>(&s) == iommu) {
      return true;
    }
  }
  return false;
}

zx_status_t SmmuTestAccess::CreateCppSmmu(const zbi_dcfg_arm_smmu_driver_t* config,
                                          iommu::Iommu** out_smmu) {
  if (out_smmu == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  *out_smmu = nullptr;
  if (config == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto res = Smmu::Create(*config);
  if (res.is_error()) {
    return res.status_value();
  }
  *out_smmu = fbl::ExportToRawPtr(&*res);
  return ZX_OK;
}

zx_status_t SmmuTestAccess::FetchCppSmmu(const uint8_t* desc, size_t desc_len,
                                         iommu::Iommu** out_smmu) {
  if (out_smmu == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  *out_smmu = nullptr;
  ktl::unique_ptr<const uint8_t[]> desc_copy;
  if (desc != nullptr) {
    fbl::AllocChecker ac;
    ktl::unique_ptr<uint8_t[]> buf{new (&ac) uint8_t[desc_len]};
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }
    if (desc_len != 0) {
      memcpy(buf.get(), desc, desc_len);
    }
    desc_copy.reset(buf.release());
  }
  auto res = Smmu::Fetch(ktl::move(desc_copy), desc_len);
  if (res.is_error()) {
    return res.status_value();
  }
  // In Disabled mode (global or per-instance override), C++ `Smmu::Fetch` returns a fresh
  // `StubIommu` rather than an `arm_smmu::Smmu` from `Smmu::instances_`. Match `rust_smmu_fetch`
  // and `cpp_smmu_fetch`'s contract by dropping the temporary `StubIommu` and leaving
  // `*out_smmu == nullptr`.
  if (!IsRegisteredSmmu(res.value().get())) {
    return ZX_OK;
  }
  *out_smmu = fbl::ExportToRawPtr(&*res);
  return ZX_OK;
}

void SmmuTestAccess::DestroyCppSmmu(iommu::Iommu* smmu) {
  if (smmu == nullptr) {
    return;
  }
  DestroySmmu(fbl::ImportFromRawPtr(smmu));
}

void SmmuTestAccess::ReleaseCppSmmu(iommu::Iommu* smmu) {
  if (smmu == nullptr) {
    return;
  }
  (void)fbl::ImportFromRawPtr(smmu);
}

zx_status_t SmmuTestAccess::CreateCppBti(iommu::Iommu* smmu, uint64_t bti_id,
                                         iommu::Bti** out_bti) {
  if (out_bti == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  *out_bti = nullptr;
  if (smmu == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto res = smmu->CreateBti(bti_id);
  if (res.is_error()) {
    return res.status_value();
  }
  *out_bti = fbl::ExportToRawPtr(&*res);
  return ZX_OK;
}

void SmmuTestAccess::OnZeroHandlesCppBti(iommu::Bti* bti) {
  if (bti != nullptr) {
    bti->OnDispatcherZeroHandles();
  }
}

void SmmuTestAccess::ReleaseCppBti(iommu::Bti* bti) {
  if (bti != nullptr) {
    (void)fbl::ImportFromRawPtr(bti);
  }
}

void SmmuTestAccess::ReleaseQuarantineCppBti(iommu::Bti* bti) {
  if (bti != nullptr) {
    bti->ReleaseQuarantine();
  }
}

bool SmmuTestAccess::InFaultStateCppBti(const iommu::Bti* bti) {
  return bti != nullptr && bti->in_fault_state();
}

size_t SmmuTestAccess::QuarantineCountCppBti(const iommu::Bti* bti) {
  return bti != nullptr ? bti->quarantine_count() : 0;
}

uint64_t SmmuTestAccess::PmoCountCppBti(const iommu::Bti* bti) {
  return bti != nullptr ? bti->pmo_count() : 0;
}

uint64_t SmmuTestAccess::MinimumContiguityCppBti(const iommu::Bti* bti) {
  return bti != nullptr ? bti->minimum_contiguity() : 0;
}

uint64_t SmmuTestAccess::AspaceSizeCppBti(const iommu::Bti* bti) {
  return bti != nullptr ? bti->aspace_size() : 0;
}

uint64_t SmmuTestAccess::BtiIdCppBti(const iommu::Bti* bti) {
  return bti != nullptr ? bti->bti_id() : 0;
}

void SmmuTestAccess::GetNameCppBti(const iommu::Bti* bti, char* out_name) {
  if (bti == nullptr || out_name == nullptr) {
    return;
  }
  auto& name_arr = *reinterpret_cast<char (*)[ZX_MAX_NAME_LEN]>(out_name);
  static_cast<void>(bti->get_name(name_arr));
}

void SmmuTestAccess::SetNameCppBti(iommu::Bti* bti, const char* name, size_t name_len) {
  if (bti != nullptr && (name != nullptr || name_len == 0)) {
    static_cast<void>(bti->set_name(name, name_len));
  }
}

zx_status_t SmmuTestAccess::MapCppBti(iommu::Bti* bti, PinnedVmObject* pinned_vmo, uint32_t perms,
                                      bool require_contiguous, iommu::Pmt** out_pmt) {
  if (out_pmt == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  *out_pmt = nullptr;
  if (bti == nullptr || pinned_vmo == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  const auto req = require_contiguous ? iommu::RequireContiguousMapping::Yes
                                      : iommu::RequireContiguousMapping::No;
  auto res = bti->Map(ktl::move(*pinned_vmo), perms, req);
  if (res.is_error()) {
    return res.status_value();
  }
  *out_pmt = fbl::ExportToRawPtr(&*res);
  return ZX_OK;
}

zx_status_t SmmuTestAccess::QueryAddressCppPmt(iommu::Pmt* pmt, uint64_t offset, size_t size,
                                               uint64_t* out_device_vaddr, size_t* out_size) {
  if (pmt == nullptr || out_device_vaddr == nullptr || out_size == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto res = pmt->QueryAddress(offset, size);
  if (res.is_error()) {
    *out_device_vaddr = 0;
    *out_size = 0;
    return res.status_value();
  }
  *out_device_vaddr = res->device_vaddr;
  *out_size = res->size;
  return ZX_OK;
}

void SmmuTestAccess::ReleasePinnedMemoryCppPmt(iommu::Pmt* pmt) {
  if (pmt != nullptr) {
    pmt->ReleasePinnedMemory();
  }
}

void SmmuTestAccess::OnZeroHandlesCppPmt(iommu::Pmt* pmt) {
  if (pmt != nullptr) {
    pmt->OnDispatcherZeroHandles();
  }
}

void SmmuTestAccess::ReleaseCppPmt(iommu::Pmt* pmt) {
  if (pmt != nullptr) {
    (void)fbl::ImportFromRawPtr(pmt);
  }
}

bool SmmuTestAccess::ValidateCppModeString(const char* mode_str) {
  return mode_str != nullptr && ValidateSmmuModeString(mode_str);
}

bool SmmuTestAccess::GetCppModeString(const char* mode_str, bool has_base_addr, uint64_t base_addr,
                                      uint32_t* out_mode) {
  if (mode_str == nullptr || out_mode == nullptr) {
    return false;
  }
  const ktl::optional<uint64_t> addr =
      has_base_addr ? ktl::optional<uint64_t>{base_addr} : ktl::nullopt;
  auto res = GetSmmuMode(mode_str, addr);
  if (!res.has_value()) {
    return false;
  }
  *out_mode = static_cast<uint32_t>(*res);
  return true;
}

namespace testing {
namespace {

class ModelTestVmObject final : public VmObject {
 public:
  explicit ModelTestVmObject(std::vector<paddr_t> page_paddrs, bool contiguous = false)
      : page_paddrs_(std::move(page_paddrs)), contiguous_(contiguous) {
    SetLookupHook([this](size_t offset, size_t size, paddr_t* out_paddr) -> zx_status_t {
      if ((offset & (kPageSize - 1)) != 0 || (size & (kPageSize - 1)) != 0 || size == 0) {
        return ZX_ERR_INVALID_ARGS;
      }
      const size_t start_page = offset / kPageSize;
      const size_t num_pages = size / kPageSize;
      if (start_page >= page_paddrs_.size() || num_pages > page_paddrs_.size() - start_page) {
        return ZX_ERR_OUT_OF_RANGE;
      }
      if (num_pages > 1 && !contiguous_) {
        for (size_t i = 1; i < num_pages; ++i) {
          if (page_paddrs_[start_page + i] != page_paddrs_[start_page] + i * kPageSize) {
            return ZX_ERR_NEXT;
          }
        }
      }
      *out_paddr = page_paddrs_[start_page];
      return ZX_OK;
    });
  }

  static fbl::RefPtr<ModelTestVmObject> CreateContiguous(paddr_t base_pa, size_t num_pages) {
    std::vector<paddr_t> pages(num_pages);
    for (size_t i = 0; i < num_pages; ++i) {
      pages[i] = base_pa + i * kPageSize;
    }
    return fbl::MakeRefCounted<ModelTestVmObject>(std::move(pages), true);
  }

  static fbl::RefPtr<ModelTestVmObject> CreateDiscontiguous(std::vector<paddr_t> pages) {
    return fbl::MakeRefCounted<ModelTestVmObject>(std::move(pages), false);
  }

  const std::vector<paddr_t>& page_paddrs() const { return page_paddrs_; }

 private:
  std::vector<paddr_t> page_paddrs_;
  bool contiguous_ = false;
};

// Outer-boundary backend adapter driving either the C++ Smmu/SmmuBti/SmmuPmt
// implementation (`BackendKind::kCpp`) or the Rust Smmu/SmmuBti/SmmuPmt
// implementation through cross_test_ffi.{rs,cc} (`BackendKind::kRust`).
class SmmuBackendRunner {
 public:
  struct SmmuHandle {
    fbl::RefPtr<iommu::Iommu> cpp_iommu;
    const RustSmmu* rust_smmu = nullptr;
    bool owns_registration = false;

    SmmuHandle() = default;
    SmmuHandle(const SmmuHandle&) = delete;
    SmmuHandle& operator=(const SmmuHandle&) = delete;
    SmmuHandle(SmmuHandle&& other) noexcept
        : cpp_iommu(std::move(other.cpp_iommu)),
          rust_smmu(std::exchange(other.rust_smmu, nullptr)),
          owns_registration(std::exchange(other.owns_registration, false)) {}
    SmmuHandle& operator=(SmmuHandle&& other) noexcept {
      if (this != &other) {
        Reset();
        cpp_iommu = std::move(other.cpp_iommu);
        rust_smmu = std::exchange(other.rust_smmu, nullptr);
        owns_registration = std::exchange(other.owns_registration, false);
      }
      return *this;
    }
    ~SmmuHandle() { Reset(); }

    void Reset() {
      const bool should_destroy = std::exchange(owns_registration, false);
      if (should_destroy) {
        if (rust_smmu != nullptr) {
          rust_smmu_destroy(std::exchange(rust_smmu, nullptr));
        }
        if (cpp_iommu) {
          SmmuTestAccess::DestroySmmu(std::move(cpp_iommu));
        }
        return;
      }
      cpp_iommu.reset();
      if (rust_smmu != nullptr) {
        rust_smmu_release(std::exchange(rust_smmu, nullptr));
      }
    }
  };

  struct BtiHandle {
    fbl::RefPtr<iommu::Bti> cpp_bti;
    const RustSmmuBti* rust_bti = nullptr;
    bool dispatcher_closed = false;

    BtiHandle() = default;
    BtiHandle(const BtiHandle&) = delete;
    BtiHandle& operator=(const BtiHandle&) = delete;
    BtiHandle(BtiHandle&& other) noexcept
        : cpp_bti(std::move(other.cpp_bti)),
          rust_bti(std::exchange(other.rust_bti, nullptr)),
          dispatcher_closed(std::exchange(other.dispatcher_closed, true)) {}
    BtiHandle& operator=(BtiHandle&& other) noexcept {
      if (this != &other) {
        Reset();
        cpp_bti = std::move(other.cpp_bti);
        rust_bti = std::exchange(other.rust_bti, nullptr);
        dispatcher_closed = std::exchange(other.dispatcher_closed, true);
      }
      return *this;
    }
    ~BtiHandle() { Reset(); }

    void Reset() {
      if (cpp_bti) {
        if (!dispatcher_closed) {
          dispatcher_closed = true;
          if (cpp_bti->in_fault_state()) {
            cpp_bti->ReleaseQuarantine();
          }
          cpp_bti->OnDispatcherZeroHandles();
        }
        cpp_bti.reset();
      }
      if (rust_bti != nullptr) {
        const RustSmmuBti* raw = std::exchange(rust_bti, nullptr);
        if (!dispatcher_closed) {
          dispatcher_closed = true;
          if (rust_smmu_bti_in_fault_state(raw)) {
            rust_smmu_bti_release_quarantine(raw);
          }
          rust_smmu_bti_on_zero_handles(raw);
        }
        rust_smmu_bti_release(raw);
      }
    }
  };

  struct PmtHandle {
    fbl::RefPtr<iommu::Pmt> cpp_pmt;
    const RustSmmuPmt* rust_pmt = nullptr;

    PmtHandle() = default;
    PmtHandle(const PmtHandle&) = delete;
    PmtHandle& operator=(const PmtHandle&) = delete;
    PmtHandle(PmtHandle&& other) noexcept
        : cpp_pmt(std::move(other.cpp_pmt)), rust_pmt(std::exchange(other.rust_pmt, nullptr)) {}
    PmtHandle& operator=(PmtHandle&& other) noexcept {
      if (this != &other) {
        Reset();
        cpp_pmt = std::move(other.cpp_pmt);
        rust_pmt = std::exchange(other.rust_pmt, nullptr);
      }
      return *this;
    }
    ~PmtHandle() { Reset(); }

    void Reset() {
      if (cpp_pmt) {
        cpp_pmt->ReleasePinnedMemory();
        cpp_pmt->OnDispatcherZeroHandles();
        cpp_pmt.reset();
      }
      if (rust_pmt != nullptr) {
        const RustSmmuPmt* raw = std::exchange(rust_pmt, nullptr);
        rust_smmu_pmt_release_pinned_memory(raw);
        rust_smmu_pmt_on_zero_handles(raw);
        rust_smmu_pmt_release(raw);
      }
    }
  };

  explicit SmmuBackendRunner(BackendKind kind) : kind_(kind) {}

  BackendKind kind() const { return kind_; }

  const char* Tag() const { return BackendTag(kind_); }

  void LogTestStart(const char* test_name) const {
    printf("[Backend: %s] %s\n", Tag(), test_name);
    fflush(stdout);
  }

  zx::result<SmmuHandle> CreateSmmu(const zbi_dcfg_arm_smmu_driver_t& config) {
    if (kind_ == BackendKind::kRust) {
      const RustSmmu* raw = nullptr;
      const zx_status_t status = rust_smmu_create(&config, &raw);
      if (status != ZX_OK) {
        return zx::error(status);
      }
      SmmuHandle handle;
      handle.rust_smmu = raw;
      handle.owns_registration = true;
      return zx::ok(std::move(handle));
    }
    auto res = Smmu::Create(config);
    if (res.is_error()) {
      return res.take_error();
    }
    SmmuHandle handle;
    handle.cpp_iommu = std::move(*res);
    handle.owns_registration = true;
    return zx::ok(std::move(handle));
  }

  zx::result<SmmuHandle> FetchSmmu(uint64_t mmio_phys) {
    if (kind_ == BackendKind::kRust) {
      zx_iommu_desc_arm_smmu_t desc{.register_base = mmio_phys};
      const RustSmmu* raw = nullptr;
      const zx_status_t status =
          rust_smmu_fetch(reinterpret_cast<const uint8_t*>(&desc), sizeof(desc), &raw);
      if (status != ZX_OK) {
        return zx::error(status);
      }
      SmmuHandle handle;
      handle.rust_smmu = raw;
      return zx::ok(std::move(handle));
    }
    fbl::AllocChecker ac;
    ktl::unique_ptr<uint8_t[]> buf{new (&ac) uint8_t[sizeof(zx_iommu_desc_arm_smmu_t)]};
    ZX_ASSERT(ac.check());
    auto* desc = reinterpret_cast<zx_iommu_desc_arm_smmu_t*>(buf.get());
    desc->register_base = mmio_phys;
    auto res = Smmu::Fetch(ktl::unique_ptr<const uint8_t[]>{buf.release()}, sizeof(*desc));
    if (res.is_error()) {
      return res.take_error();
    }
    SmmuHandle handle;
    if (SmmuTestAccess::IsRegisteredSmmu(res.value().get())) {
      handle.cpp_iommu = std::move(*res);
    }
    return zx::ok(std::move(handle));
  }

  void DestroySmmu(SmmuHandle& smmu) {
    smmu.owns_registration = false;
    if (kind_ == BackendKind::kRust) {
      if (smmu.rust_smmu != nullptr) {
        rust_smmu_destroy(std::exchange(smmu.rust_smmu, nullptr));
      }
      return;
    }
    SmmuTestAccess::DestroySmmu(std::move(smmu.cpp_iommu));
  }

  zx::result<BtiHandle> CreateBti(SmmuHandle& smmu, uint64_t bti_id) {
    if (kind_ == BackendKind::kRust) {
      const RustSmmuBti* raw = nullptr;
      const zx_status_t status = rust_smmu_create_bti(smmu.rust_smmu, bti_id, &raw);
      if (status != ZX_OK) {
        return zx::error(status);
      }
      BtiHandle handle;
      handle.rust_bti = raw;
      handle.dispatcher_closed = false;
      return zx::ok(std::move(handle));
    }
    auto res = smmu.cpp_iommu->CreateBti(bti_id);
    if (res.is_error()) {
      return res.take_error();
    }
    BtiHandle handle;
    handle.cpp_bti = std::move(*res);
    handle.dispatcher_closed = false;
    return zx::ok(std::move(handle));
  }

  // Simulates closing the last user-mode handle to BusTransactionInitiatorDispatcher.
  void DropBti(BtiHandle& bti) {
    if (kind_ == BackendKind::kRust) {
      if (bti.rust_bti != nullptr) {
        const RustSmmuBti* raw = std::exchange(bti.rust_bti, nullptr);
        if (!bti.dispatcher_closed) {
          bti.dispatcher_closed = true;
          rust_smmu_bti_on_zero_handles(raw);
        }
        rust_smmu_bti_release(raw);
      }
      return;
    }
    if (bti.cpp_bti && !bti.dispatcher_closed) {
      bti.dispatcher_closed = true;
      bti.cpp_bti->OnDispatcherZeroHandles();
    }
    bti.cpp_bti.reset();
  }

  void ReleaseQuarantine(BtiHandle& bti) {
    if (kind_ == BackendKind::kRust) {
      rust_smmu_bti_release_quarantine(bti.rust_bti);
      return;
    }
    bti.cpp_bti->ReleaseQuarantine();
  }

  bool BtiInFaultState(const BtiHandle& bti) const {
    if (kind_ == BackendKind::kRust) {
      return rust_smmu_bti_in_fault_state(bti.rust_bti);
    }
    return bti.cpp_bti->in_fault_state();
  }

  size_t BtiQuarantineCount(const BtiHandle& bti) const {
    if (kind_ == BackendKind::kRust) {
      return rust_smmu_bti_quarantine_count(bti.rust_bti);
    }
    return bti.cpp_bti->quarantine_count();
  }

  uint64_t BtiPmoCount(const BtiHandle& bti) const {
    if (kind_ == BackendKind::kRust) {
      return rust_smmu_bti_pmo_count(bti.rust_bti);
    }
    return bti.cpp_bti->pmo_count();
  }

  uint64_t BtiMinimumContiguity(const BtiHandle& bti) const {
    if (kind_ == BackendKind::kRust) {
      return rust_smmu_bti_minimum_contiguity(bti.rust_bti);
    }
    return bti.cpp_bti->minimum_contiguity();
  }

  uint64_t BtiAspaceSize(const BtiHandle& bti) const {
    if (kind_ == BackendKind::kRust) {
      return rust_smmu_bti_aspace_size(bti.rust_bti);
    }
    return bti.cpp_bti->aspace_size();
  }

  zx::result<PmtHandle> BtiPin(BtiHandle& bti, PinnedVmObject pinned_vmo, uint32_t perms,
                               bool require_contiguous) {
    if (kind_ == BackendKind::kRust) {
      const RustSmmuPmt* raw = nullptr;
      const zx_status_t status =
          rust_smmu_bti_map(bti.rust_bti, &pinned_vmo, perms, require_contiguous, &raw);
      if (status != ZX_OK) {
        return zx::error(status);
      }
      PmtHandle handle;
      handle.rust_pmt = raw;
      return zx::ok(std::move(handle));
    }
    const auto req = require_contiguous ? iommu::RequireContiguousMapping::Yes
                                        : iommu::RequireContiguousMapping::No;
    auto res = bti.cpp_bti->Map(std::move(pinned_vmo), perms, req);
    if (res.is_error()) {
      return res.take_error();
    }
    PmtHandle handle;
    handle.cpp_pmt = std::move(*res);
    return zx::ok(std::move(handle));
  }

  zx::result<iommu::QueryAddressResult> PmtQueryAddress(PmtHandle& pmt, uint64_t offset,
                                                        size_t size) {
    if (kind_ == BackendKind::kRust) {
      uint64_t device_vaddr = 0;
      size_t mapped_size = 0;
      const zx_status_t status =
          rust_smmu_pmt_query_address(pmt.rust_pmt, offset, size, &device_vaddr, &mapped_size);
      if (status != ZX_OK) {
        return zx::error(status);
      }
      return zx::ok(iommu::QueryAddressResult{.device_vaddr = device_vaddr, .size = mapped_size});
    }
    return pmt.cpp_pmt->QueryAddress(offset, size);
  }

  // Unpins |pmt| (ReleasePinnedMemory()), leaving its handle open. This is the
  // first half of zx_pmt_unpin(), which unpins and only then drops the handle;
  // other operations (including closing the BTI) can happen in between. Follow
  // with PmtClose().
  void PmtUnpin(PmtHandle& pmt) {
    if (kind_ == BackendKind::kRust) {
      if (pmt.rust_pmt != nullptr) {
        rust_smmu_pmt_release_pinned_memory(pmt.rust_pmt);
      }
      return;
    }
    if (pmt.cpp_pmt) {
      pmt.cpp_pmt->ReleasePinnedMemory();
    }
  }

  // Closes the last handle to |pmt| (OnDispatcherZeroHandles()) and drops the
  // reference. If the PMT was unpinned first this is the normal end of its
  // life. If it is still pinned this is a leak, which quarantines it and puts
  // its BTI into the fault state.
  void PmtClose(PmtHandle& pmt) {
    if (kind_ == BackendKind::kRust) {
      if (pmt.rust_pmt != nullptr) {
        const RustSmmuPmt* raw = std::exchange(pmt.rust_pmt, nullptr);
        rust_smmu_pmt_on_zero_handles(raw);
        rust_smmu_pmt_release(raw);
      }
      return;
    }
    if (pmt.cpp_pmt) {
      pmt.cpp_pmt->OnDispatcherZeroHandles();
      pmt.cpp_pmt.reset();
    }
  }

  // Normal PMT lifecycle in one step: zx_pmt_unpin().
  void PmtUnpinAndDrop(PmtHandle& pmt) {
    PmtUnpin(pmt);
    PmtClose(pmt);
  }

  // Leaked PMT lifecycle: closing the last PMT handle WITHOUT calling zx_pmt_unpin(),
  // triggering BTI quarantine.
  void PmtLeakAndDrop(PmtHandle& pmt) { PmtClose(pmt); }

  bool ValidateModeString(const char* mode_str) const {
    if (kind_ == BackendKind::kRust) {
      return rust_smmu_validate_mode_string(mode_str);
    }
    return ValidateSmmuModeString(mode_str);
  }

  ktl::optional<ArmSmmuMode> ParseMode(const char* mode_str,
                                       ktl::optional<uint64_t> base_addr = ktl::nullopt) const {
    if (kind_ == BackendKind::kRust) {
      uint32_t raw_mode = 0;
      if (!rust_smmu_get_mode(mode_str, base_addr.has_value(), base_addr.value_or(0), &raw_mode)) {
        return ktl::nullopt;
      }
      return static_cast<ArmSmmuMode>(raw_mode);
    }
    return GetSmmuMode(mode_str, base_addr);
  }

 private:
  BackendKind kind_;
};

// Abstract reference specification of expected IOMMU translation behavior,
// verified after every operation against SimSmmuHardwareModel::TranslateDma.
class AbstractIommuSpec {
 public:
  struct MappedPage {
    paddr_t paddr = 0;
    uint32_t perms = 0;
  };

  struct BtiSpec {
    uint64_t bti_id = 0;
    uint16_t sid_val = 0;
    uint16_t sid_mask = 0;
    bool passthru = false;
    bool in_fault = false;
    std::map<uint64_t, MappedPage> iova_to_page;

    bool MatchesStreamId(uint16_t sid) const { return ((sid ^ sid_val) & ~sid_mask) == 0; }
  };

  explicit AbstractIommuSpec(ArmSmmuMode mode) : mode_(mode) {}

  void AddBti(uint64_t bti_id) {
    BtiSpec spec;
    spec.bti_id = bti_id;
    spec.sid_val = static_cast<uint16_t>(bti_id & 0x7fffu);
    spec.sid_mask = static_cast<uint16_t>((bti_id >> 16) & 0x7fffu);
    spec.passthru = (mode_ == ArmSmmuMode::kPassthru);
    btis_[bti_id] = std::move(spec);
  }

  void RemoveBti(uint64_t bti_id) { btis_.erase(bti_id); }

  void SetBtiFault(uint64_t bti_id, bool in_fault) {
    auto it = btis_.find(bti_id);
    if (it != btis_.end()) {
      it->second.in_fault = in_fault;
    }
  }

  void AddMapping(uint64_t bti_id, uint64_t base_iova, const std::vector<paddr_t>& paddrs,
                  uint32_t perms) {
    auto& bti = btis_.at(bti_id);
    for (size_t i = 0; i < paddrs.size(); ++i) {
      // The specification learns each mapping's device address from the
      // driver, so it cannot predict the address. When translating, it can
      // still require that the driver never hands out a device address which
      // is already part of a live mapping of the same BTI. (Without
      // translation the device address is the physical address, and two pins
      // of the same memory legitimately coincide.)
      if (!bti.passthru) {
        ZX_ASSERT_MSG(bti.iova_to_page.find(base_iova + i * kPageSize) == bti.iova_to_page.end(),
                      "driver mapped device address 0x%lx of BTI 0x%lx twice",
                      base_iova + i * kPageSize, bti_id);
      }
      bti.iova_to_page[base_iova + i * kPageSize] = MappedPage{
          .paddr = paddrs[i],
          .perms = perms,
      };
    }
  }

  void RemoveMapping(uint64_t bti_id, uint64_t base_iova, size_t num_pages) {
    auto it = btis_.find(bti_id);
    if (it == btis_.end()) {
      return;
    }
    for (size_t i = 0; i < num_pages; ++i) {
      it->second.iova_to_page.erase(base_iova + i * kPageSize);
    }
  }

  DmaTranslationResult TranslateDma(uint16_t stream_id, uint64_t iova,
                                    uint32_t access_perms) const {
    if (mode_ == ArmSmmuMode::kDisabled) {
      return DmaTranslationResult::Ok(iova);
    }

    const BtiSpec* matched = nullptr;
    for (const auto& [id, bti] : btis_) {
      if (bti.MatchesStreamId(stream_id)) {
        if (matched != nullptr) {
          return DmaTranslationResult::Fault(DmaFaultType::kStreamMatchConflict);
        }
        matched = &bti;
      }
    }

    if (matched == nullptr) {
      return DmaTranslationResult::Fault(DmaFaultType::kUnidentifiedStream);
    }

    if (matched->passthru) {
      if (matched->in_fault) {
        return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
      }
      return DmaTranslationResult::Ok(iova);
    }

    // Enforced Stage-1 translation mode:
    const uint64_t page_iova = iova & ~uint64_t{kPageSize - 1};
    const uint64_t page_off = iova & uint64_t{kPageSize - 1};
    auto it = matched->iova_to_page.find(page_iova);
    if (it == matched->iova_to_page.end()) {
      return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
    }

    const uint32_t pte_perms = it->second.perms;
    // Note: in VMSAv8-64 (see GetPageEntryPerms in vmsav8_64.h), Write permission
    // implies Read permission, while Execute requires IOMMU_FLAG_PERM_EXECUTE and
    // Write requires IOMMU_FLAG_PERM_WRITE.
    const bool has_read = (pte_perms & (IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE)) != 0;
    const bool has_write = (pte_perms & IOMMU_FLAG_PERM_WRITE) != 0;
    const bool has_exec = (pte_perms & IOMMU_FLAG_PERM_EXECUTE) != 0;

    if ((access_perms & IOMMU_FLAG_PERM_READ) && !has_read) {
      return DmaTranslationResult::Fault(DmaFaultType::kPermission);
    }
    if ((access_perms & IOMMU_FLAG_PERM_WRITE) && !has_write) {
      return DmaTranslationResult::Fault(DmaFaultType::kPermission);
    }
    if ((access_perms & IOMMU_FLAG_PERM_EXECUTE) && !has_exec) {
      return DmaTranslationResult::Fault(DmaFaultType::kPermission);
    }

    return DmaTranslationResult::Ok(it->second.paddr | page_off);
  }

  // Verifies that the hardware model translates every mapped page of every
  // translating BTI the way this specification says, for the mapping's own
  // permissions and for each kind of access on its own (which, for a mapping
  // that does not grant it, is a probe that must be denied), and that the
  // pages just outside each mapping behave as the specification says too.
  bool VerifyAllMappings(SimSmmuHardwareModel& hw) const {
    bool ok = true;
    const auto check = [&](uint16_t sid, uint64_t iova, uint32_t perms) {
      const DmaTranslationResult expected = TranslateDma(sid, iova, perms);
      const DmaTranslationResult actual = hw.TranslateDma(sid, iova, perms);
      if (actual != expected) {
        printf(
            "spec mismatch: sid=0x%x iova=0x%lx perms=0x%x: actual fault %u pa 0x%lx, expected "
            "fault %u pa 0x%lx\n",
            sid, iova, perms, static_cast<uint32_t>(actual.fault), actual.paddr,
            static_cast<uint32_t>(expected.fault), expected.paddr);
        ok = false;
      }
    };
    for (const auto& [id, bti] : btis_) {
      if (bti.passthru) {
        continue;
      }
      size_t n = 0;
      for (const auto& [iova, page] : bti.iova_to_page) {
        // Vary the offset within the page so that it is not always the same low bits.
        const uint64_t offset = (n++ & 1) == 0 ? 0x40 : 0xfc8;
        check(bti.sid_val, iova + offset, page.perms);
        for (uint32_t perm :
             {IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE, IOMMU_FLAG_PERM_EXECUTE}) {
          check(bti.sid_val, iova + 0x10, perm);
        }
        if (iova >= kPageSize &&
            bti.iova_to_page.find(iova - kPageSize) == bti.iova_to_page.end()) {
          check(bti.sid_val, iova - 8, IOMMU_FLAG_PERM_READ);
        }
        if (bti.iova_to_page.find(iova + kPageSize) == bti.iova_to_page.end()) {
          check(bti.sid_val, iova + kPageSize, IOMMU_FLAG_PERM_READ);
        }
      }
    }
    return ok;
  }

  // Verifies that no two BTIs share a translation context. Each BTI owns its
  // own context; two BTIs resolving to the same one means a device can reach
  // another device's mappings.
  bool VerifyDistinctContexts(const SimSmmuHardwareModel& hw) const {
    if (mode_ == ArmSmmuMode::kDisabled) {
      return true;
    }
    std::map<uint32_t, uint64_t> owner_of_context;
    for (const auto& [id, bti] : btis_) {
      const std::optional<uint32_t> cb = hw.FindContextBankForStreamId(bti.sid_val);
      if (!cb.has_value()) {
        continue;
      }
      const auto [it, inserted] = owner_of_context.emplace(*cb, id);
      if (!inserted) {
        printf("BTIs 0x%lx and 0x%lx are both routed to context bank %u\n", it->second, id, *cb);
        return false;
      }
    }
    return true;
  }

 private:
  ArmSmmuMode mode_;
  std::map<uint64_t, BtiSpec> btis_;
};

std::vector<BackendKind> GetBackendsToTest() { return {BackendKind::kCpp, BackendKind::kRust}; }

// ============================================================================
// 1. Designed ("White-Box") Sequence: Init, IDR Validation, & Global Config
// ============================================================================
void RunDesignedInitAndGlobalConfig(SmmuBackendRunner& backend) {
  backend.LogTestStart("DesignedInitAndGlobalConfig");
  PmmMock::Get().ClearAllocationLimit();
  InterruptMock::Get().Reset();

  // 1a. Verify mode string validation & parsing through the backend runner.
  EXPECT_TRUE(backend.ValidateModeString("enforced"));
  EXPECT_TRUE(backend.ValidateModeString("passthru,0x9000000=enforced"));
  EXPECT_FALSE(backend.ValidateModeString("invalid_mode"));
  EXPECT_EQ(ArmSmmuMode::kEnforced,
            backend.ParseMode("passthru,0x9000000=enforced", 0x9000000).value());

  // 1b. Reject unsupported SMMU major version (IDR7.major != 2).
  {
    BootOptions::Get()->SetArmSmmuMode("enforced");
    SimSmmuHardwareModel::Config bad_ver_cfg{};
    bad_ver_cfg.major_version = 1;
    SimSmmuHardwareModel hw_model(bad_ver_cfg);
    auto create_res = backend.CreateSmmu(hw_model.MakeZbiConfig());
    EXPECT_EQ(ZX_ERR_INVALID_ARGS, create_res.status_value());
  }

  // 1c. Reject SMMU without stream matching support (IDR0.SMS == 0).
  {
    BootOptions::Get()->SetArmSmmuMode("enforced");
    SimSmmuHardwareModel::Config no_sms_cfg{};
    no_sms_cfg.sms = false;
    SimSmmuHardwareModel hw_model(no_sms_cfg);
    auto create_res = backend.CreateSmmu(hw_model.MakeZbiConfig());
    EXPECT_EQ(ZX_ERR_NOT_SUPPORTED, create_res.status_value());
  }

  // 1d. Reject SMMU without dedicated interrupt per context bank (IDR0.NUMIRPT != 1).
  {
    BootOptions::Get()->SetArmSmmuMode("enforced");
    SimSmmuHardwareModel::Config bad_irpt_cfg{};
    bad_irpt_cfg.numirpt = 2;
    SimSmmuHardwareModel hw_model(bad_irpt_cfg);
    auto create_res = backend.CreateSmmu(hw_model.MakeZbiConfig());
    EXPECT_EQ(ZX_ERR_NOT_SUPPORTED, create_res.status_value());
  }

  // 1e. Successful Init in Enforced mode: verify global TLBGSYNC, CR0, Fetch,
  // and duplicate Create rejection.
  {
    BootOptions::Get()->SetArmSmmuMode("enforced");
    SimSmmuHardwareModel hw_model;
    auto zbi_cfg = hw_model.MakeZbiConfig();

    auto smmu_res = backend.CreateSmmu(zbi_cfg);
    ASSERT_OK(smmu_res.status_value());
    auto smmu = std::move(*smmu_res);

    EXPECT_TRUE(hw_model.VerifyQuiescent());
    EXPECT_GE(hw_model.tlb_gsync_call_count(), 1u);
    EXPECT_FALSE(InterruptMock::Get().IsMasked(hw_model.config().global_irq_num));

    // Creating a second SMMU at the same physical base address must fail with ZX_ERR_ALREADY_BOUND.
    auto dup_res = backend.CreateSmmu(zbi_cfg);
    EXPECT_EQ(ZX_ERR_ALREADY_BOUND, dup_res.status_value());

    // First FetchSmmu succeeds; second FetchSmmu returns ZX_ERR_ALREADY_BOUND.
    auto fetch1 = backend.FetchSmmu(hw_model.config().mmio_phys);
    ASSERT_OK(fetch1.status_value());
    auto fetch2 = backend.FetchSmmu(hw_model.config().mmio_phys);
    EXPECT_EQ(ZX_ERR_ALREADY_BOUND, fetch2.status_value());
    fetch1->Reset();

    // Unidentified streams must fault when CR0.CLIENTPD == 0 && CR0.USFCFG == 1.
    EXPECT_EQ(DmaTranslationResult::Fault(DmaFaultType::kUnidentifiedStream),
              hw_model.TranslateDma(0x0123, 0x20'0000, IOMMU_FLAG_PERM_READ));

    backend.DestroySmmu(smmu);
    EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
  }

  // 1f. Disabled mode (global and per-instance override): Smmu::Create still
  // registers the instance for diagnostics, while FetchSmmu / *_smmu_fetch
  // succeed repeatedly with a null/stub handle rather than exporting a
  // StubIommu as an arm_smmu::Smmu, and raw FFI fetch rejects malformed
  // descriptor sizes when global mode is not disabled.
  {
    SimSmmuHardwareModel hw_model;
    const uint64_t mmio_phys = hw_model.config().mmio_phys;

    char override_mode[64];
    snprintf(override_mode, sizeof(override_mode), "enforced,0x%lx=disabled", mmio_phys);
    BootOptions::Get()->SetArmSmmuMode(override_mode);

    auto create_disabled = backend.CreateSmmu(hw_model.MakeZbiConfig());
    ASSERT_OK(create_disabled.status_value());
    if (backend.kind() == BackendKind::kRust) {
      EXPECT_NE(nullptr, create_disabled->rust_smmu);
    } else {
      EXPECT_NE(nullptr, create_disabled->cpp_iommu.get());
    }

    // Per-instance disabled override (global mode == Enforced, instance op_mode == Disabled).
    auto fetch_override_1 = backend.FetchSmmu(mmio_phys);
    ASSERT_OK(fetch_override_1.status_value());
    EXPECT_EQ(nullptr, fetch_override_1->cpp_iommu.get());
    EXPECT_EQ(nullptr, fetch_override_1->rust_smmu);
    auto fetch_override_2 = backend.FetchSmmu(mmio_phys);
    ASSERT_OK(fetch_override_2.status_value());

    zx_iommu_desc_arm_smmu_t valid_desc{.register_base = mmio_phys};
    if (backend.kind() == BackendKind::kRust) {
      const RustSmmu* rust_out = reinterpret_cast<const RustSmmu*>(uintptr_t{1});
      EXPECT_EQ(ZX_OK, rust_smmu_fetch(reinterpret_cast<const uint8_t*>(&valid_desc),
                                       sizeof(valid_desc), &rust_out));
      EXPECT_EQ(nullptr, rust_out);
    } else {
      iommu::Iommu* cpp_out = reinterpret_cast<iommu::Iommu*>(uintptr_t{1});
      EXPECT_EQ(ZX_OK, cpp_smmu_fetch(reinterpret_cast<const uint8_t*>(&valid_desc),
                                      sizeof(valid_desc), &cpp_out));
      EXPECT_EQ(nullptr, cpp_out);
    }

    // Malformed descriptor lengths must return ZX_ERR_INVALID_ARGS across both FFI surfaces.
    uint8_t short_desc[4] = {};
    iommu::Iommu* cpp_out = nullptr;
    const RustSmmu* rust_out = nullptr;
    EXPECT_EQ(ZX_ERR_INVALID_ARGS,
              cpp_smmu_fetch(nullptr, sizeof(zx_iommu_desc_arm_smmu_t), &cpp_out));
    EXPECT_EQ(ZX_ERR_INVALID_ARGS,
              rust_smmu_fetch(nullptr, sizeof(zx_iommu_desc_arm_smmu_t), &rust_out));
    EXPECT_EQ(ZX_ERR_INVALID_ARGS, cpp_smmu_fetch(short_desc, sizeof(short_desc), &cpp_out));
    EXPECT_EQ(ZX_ERR_INVALID_ARGS, rust_smmu_fetch(short_desc, sizeof(short_desc), &rust_out));

    // Global disabled mode also returns ZX_OK with nullptr.
    BootOptions::Get()->SetArmSmmuMode("disabled");
    auto fetch_global_disabled = backend.FetchSmmu(mmio_phys);
    ASSERT_OK(fetch_global_disabled.status_value());
    EXPECT_EQ(nullptr, fetch_global_disabled->cpp_iommu.get());
    EXPECT_EQ(nullptr, fetch_global_disabled->rust_smmu);

    backend.DestroySmmu(*create_disabled);
    EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
  }
}

// ============================================================================
// 2. Designed ("White-Box") Sequence: L3/L2 Boundary Straddling, Partial vs.
//    Cascading Table Reclamation, & Large Region TLBIASID Threshold
// ============================================================================
void RunDesignedBoundaryStraddlingAndReclamation(SmmuBackendRunner& backend) {
  backend.LogTestStart("DesignedBoundaryStraddlingAndReclamation");
  BootOptions::Get()->SetArmSmmuMode("enforced");
  PmmMock::Get().ClearAllocationLimit();
  InterruptMock::Get().Reset();

  SimSmmuHardwareModel hw_model;
  AbstractIommuSpec spec(ArmSmmuMode::kEnforced);

  auto smmu_res = backend.CreateSmmu(hw_model.MakeZbiConfig());
  ASSERT_OK(smmu_res.status_value());
  auto smmu = std::move(*smmu_res);

  constexpr uint16_t kStreamId = 0x0017;
  auto bti_res = backend.CreateBti(smmu, kStreamId);
  ASSERT_OK(bti_res.status_value());
  auto bti = std::move(*bti_res);
  spec.AddBti(kStreamId);

  EXPECT_EQ(ktl::numeric_limits<uint64_t>::max(), backend.BtiMinimumContiguity(bti));
  EXPECT_EQ(uint64_t{1} << 48, backend.BtiAspaceSize(bti));

  // DeviceAspace starts at 2 MiB (index 1 in L2).
  // Pin #1: 511 pages filling the rest of the first L3 table [2 MiB, 4 MiB - 4 KiB).
  const size_t kPin1Pages = 511;
  auto vmo1 = ModelTestVmObject::CreateContiguous(0x4000'0000, kPin1Pages);
  auto pmt1_res = backend.BtiPin(bti, PinnedVmObject(vmo1, 0, kPin1Pages * kPageSize),
                                 IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE, true);
  ASSERT_OK(pmt1_res.status_value());
  auto pmt1 = std::move(*pmt1_res);

  auto q1 = backend.PmtQueryAddress(pmt1, 0, kPin1Pages * kPageSize);
  ASSERT_OK(q1.status_value());
  EXPECT_EQ(0x20'0000ULL, q1->device_vaddr);
  spec.AddMapping(kStreamId, q1->device_vaddr, vmo1->page_paddrs(),
                  IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE);

  // Pin #2: 4 pages straddling the exact 4 MiB boundary between L2[1]'s L3 table
  // and L2[2]'s L3 table ([4 MiB - 4 KiB, 4 MiB + 12 KiB)).
  const size_t kPin2Pages = 4;
  auto vmo2 =
      ModelTestVmObject::CreateDiscontiguous({0x8000'1000, 0x8000'9000, 0x8002'3000, 0x8005'7000});
  auto pmt2_res = backend.BtiPin(bti, PinnedVmObject(vmo2, 0, kPin2Pages * kPageSize),
                                 IOMMU_FLAG_PERM_READ, false);
  ASSERT_OK(pmt2_res.status_value());
  auto pmt2 = std::move(*pmt2_res);

  auto q2 = backend.PmtQueryAddress(pmt2, 0, kPin2Pages * kPageSize);
  ASSERT_OK(q2.status_value());
  EXPECT_EQ(0x40'0000ULL - kPageSize, q2->device_vaddr);
  spec.AddMapping(kStreamId, q2->device_vaddr, vmo2->page_paddrs(), IOMMU_FLAG_PERM_READ);

  // Verify translations across the L3 -> L2 boundary against AbstractIommuSpec.
  for (uint64_t probe_iova :
       {uint64_t{0x20'0000}, uint64_t{0x3f'e000}, uint64_t{0x3f'f000}, uint64_t{0x40'0000},
        uint64_t{0x40'1000}, uint64_t{0x40'2000}, uint64_t{0x40'3000}}) {
    EXPECT_EQ(spec.TranslateDma(kStreamId, probe_iova, IOMMU_FLAG_PERM_READ),
              hw_model.TranslateDma(kStreamId, probe_iova, IOMMU_FLAG_PERM_READ));
  }

  // Unpin #1: L2[1]'s L3 table STILL has 1 page mapped by pmt2 at 0x3f'f000, so
  // L2[1] must NOT be reclaimed yet!
  backend.PmtUnpinAndDrop(pmt1);
  spec.RemoveMapping(kStreamId, q1->device_vaddr, kPin1Pages);
  EXPECT_TRUE(hw_model.VerifyQuiescent());

  EXPECT_EQ(DmaTranslationResult::Fault(DmaFaultType::kTranslation),
            hw_model.TranslateDma(kStreamId, 0x20'0000, IOMMU_FLAG_PERM_READ));
  EXPECT_EQ(spec.TranslateDma(kStreamId, 0x3f'f000, IOMMU_FLAG_PERM_READ),
            hw_model.TranslateDma(kStreamId, 0x3f'f000, IOMMU_FLAG_PERM_READ));
  EXPECT_EQ(spec.TranslateDma(kStreamId, 0x40'0000, IOMMU_FLAG_PERM_READ),
            hw_model.TranslateDma(kStreamId, 0x40'0000, IOMMU_FLAG_PERM_READ));

  // Unpin #2: Now BOTH L2[1]'s L3 table and L2[2]'s L3 table (and L2 + L1) become
  // completely empty and are reclaimed in a cascading FinishLevel walk!
  backend.PmtUnpinAndDrop(pmt2);
  spec.RemoveMapping(kStreamId, q2->device_vaddr, kPin2Pages);
  EXPECT_TRUE(hw_model.VerifyQuiescent());

  for (uint64_t probe_iova : {uint64_t{0x3f'f000}, uint64_t{0x40'0000}, uint64_t{0x40'2000}}) {
    EXPECT_EQ(DmaTranslationResult::Fault(DmaFaultType::kTranslation),
              hw_model.TranslateDma(kStreamId, probe_iova, IOMMU_FLAG_PERM_READ));
  }

  // Pin #3: Large mapping (> 256 pages, specifically 260 pages) to exercise the
  // TLBIASID whole-ASID invalidation path in ContextBank::TLBInvalidateRegion.
  const size_t kPin3Pages = 260;
  auto vmo3 = ModelTestVmObject::CreateContiguous(0x9000'0000, kPin3Pages);
  auto pmt3_res = backend.BtiPin(bti, PinnedVmObject(vmo3, 0, kPin3Pages * kPageSize),
                                 IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE, true);
  ASSERT_OK(pmt3_res.status_value());
  auto pmt3 = std::move(*pmt3_res);
  auto q3 = backend.PmtQueryAddress(pmt3, 0, kPin3Pages * kPageSize);
  ASSERT_OK(q3.status_value());
  EXPECT_EQ(DmaTranslationResult::Ok(0x9000'0000 + (kPin3Pages - 1) * kPageSize),
            hw_model.TranslateDma(kStreamId, q3->device_vaddr + (kPin3Pages - 1) * kPageSize,
                                  IOMMU_FLAG_PERM_WRITE));

  backend.PmtUnpinAndDrop(pmt3);
  EXPECT_TRUE(hw_model.VerifyQuiescent());

  backend.DropBti(bti);
  backend.DestroySmmu(smmu);
  EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
}

// ============================================================================
// 3. Designed ("White-Box") Sequence: Permission Encoding & Enforcement (R/W/X)
// ============================================================================
void RunDesignedPermissionsEnforcement(SmmuBackendRunner& backend) {
  backend.LogTestStart("DesignedPermissionsEnforcement");
  BootOptions::Get()->SetArmSmmuMode("enforced");
  PmmMock::Get().ClearAllocationLimit();
  InterruptMock::Get().Reset();

  SimSmmuHardwareModel hw_model;
  AbstractIommuSpec spec(ArmSmmuMode::kEnforced);

  auto smmu_res = backend.CreateSmmu(hw_model.MakeZbiConfig());
  ASSERT_OK(smmu_res.status_value());
  auto smmu = std::move(*smmu_res);

  constexpr uint16_t kStreamId = 0x002a;
  auto bti_res = backend.CreateBti(smmu, kStreamId);
  ASSERT_OK(bti_res.status_value());
  auto bti = std::move(*bti_res);
  spec.AddBti(kStreamId);

  auto vmo_ro = ModelTestVmObject::CreateContiguous(0x5000'0000, 1);
  auto vmo_rw = ModelTestVmObject::CreateContiguous(0x5001'0000, 1);
  auto vmo_rx = ModelTestVmObject::CreateContiguous(0x5002'0000, 1);

  // Invalid permission flags (0 or unknown bit) must fail with ZX_ERR_INVALID_ARGS.
  EXPECT_EQ(ZX_ERR_INVALID_ARGS,
            backend.BtiPin(bti, PinnedVmObject(vmo_ro, 0, kPageSize), 0, false).status_value());
  EXPECT_EQ(ZX_ERR_INVALID_ARGS,
            backend.BtiPin(bti, PinnedVmObject(vmo_ro, 0, kPageSize), 0x80, false).status_value());

  auto pmt_ro =
      backend.BtiPin(bti, PinnedVmObject(vmo_ro, 0, kPageSize), IOMMU_FLAG_PERM_READ, false);
  ASSERT_OK(pmt_ro.status_value());
  auto q_ro = backend.PmtQueryAddress(*pmt_ro, 0, kPageSize);
  ASSERT_OK(q_ro.status_value());
  spec.AddMapping(kStreamId, q_ro->device_vaddr, vmo_ro->page_paddrs(), IOMMU_FLAG_PERM_READ);

  auto pmt_rw = backend.BtiPin(bti, PinnedVmObject(vmo_rw, 0, kPageSize),
                               IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE, false);
  ASSERT_OK(pmt_rw.status_value());
  auto q_rw = backend.PmtQueryAddress(*pmt_rw, 0, kPageSize);
  ASSERT_OK(q_rw.status_value());
  spec.AddMapping(kStreamId, q_rw->device_vaddr, vmo_rw->page_paddrs(),
                  IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE);

  auto pmt_rx = backend.BtiPin(bti, PinnedVmObject(vmo_rx, 0, kPageSize),
                               IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_EXECUTE, false);
  ASSERT_OK(pmt_rx.status_value());
  auto q_rx = backend.PmtQueryAddress(*pmt_rx, 0, kPageSize);
  ASSERT_OK(q_rx.status_value());
  spec.AddMapping(kStreamId, q_rx->device_vaddr, vmo_rx->page_paddrs(),
                  IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_EXECUTE);

  for (uint64_t iova : {q_ro->device_vaddr, q_rw->device_vaddr, q_rx->device_vaddr}) {
    for (uint32_t perm : {uint32_t{IOMMU_FLAG_PERM_READ}, uint32_t{IOMMU_FLAG_PERM_WRITE},
                          uint32_t{IOMMU_FLAG_PERM_EXECUTE},
                          uint32_t{IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE}}) {
      EXPECT_EQ(spec.TranslateDma(kStreamId, iova + 0x120, perm),
                hw_model.TranslateDma(kStreamId, iova + 0x120, perm));
    }
  }

  backend.PmtUnpinAndDrop(*pmt_ro);
  backend.PmtUnpinAndDrop(*pmt_rw);
  backend.PmtUnpinAndDrop(*pmt_rx);
  backend.DropBti(bti);
  backend.DestroySmmu(smmu);
  EXPECT_TRUE(hw_model.VerifyQuiescent());
  EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
}

// ============================================================================
// 4. Designed ("White-Box") Sequence: Deterministic PMM OOM Rollback Mid-Map
//    Without Disturbing Pre-Existing Adjacent Mappings
// ============================================================================
void RunDesignedOomRollbackMidMap(SmmuBackendRunner& backend) {
  backend.LogTestStart("DesignedOomRollbackMidMap");
  BootOptions::Get()->SetArmSmmuMode("enforced");
  PmmMock::Get().ClearAllocationLimit();
  InterruptMock::Get().Reset();

  SimSmmuHardwareModel hw_model;
  AbstractIommuSpec spec(ArmSmmuMode::kEnforced);

  auto smmu_res = backend.CreateSmmu(hw_model.MakeZbiConfig());
  ASSERT_OK(smmu_res.status_value());
  auto smmu = std::move(*smmu_res);

  constexpr uint16_t kStreamId = 0x0033;
  auto bti_res = backend.CreateBti(smmu, kStreamId);
  ASSERT_OK(bti_res.status_value());
  auto bti = std::move(*bti_res);
  spec.AddBti(kStreamId);

  // Establish a baseline mapping of 511 pages at [2 MiB, 4 MiB - 4 KiB) to fill
  // L2[1][0..510], leaving 1 free slot at L2[1][511] and 1 page cached in PageCache.
  auto vmo_base = ModelTestVmObject::CreateContiguous(0x6000'0000, 511);
  auto pmt_base_res = backend.BtiPin(bti, PinnedVmObject(vmo_base, 0, 511 * kPageSize),
                                     IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE, true);
  ASSERT_OK(pmt_base_res.status_value());
  auto pmt_base = std::move(*pmt_base_res);
  auto q_base = backend.PmtQueryAddress(pmt_base, 0, 511 * kPageSize);
  ASSERT_OK(q_base.status_value());
  spec.AddMapping(kStreamId, q_base->device_vaddr, vmo_base->page_paddrs(),
                  IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE);

  // Now attempt a 1538-page mapping (1 page in L2[1][511] + 512 in L2[2] + 512 in L2[3] +
  // 512 in L2[4] + 1 in L2[5][0], requiring 3 new L3 tables), injecting OOM at
  // progressively deeper table allocations (accounting for the 1 page cached in
  // DeviceAspace::PageCache after the second iteration's rollback).
  constexpr size_t kBigPages = 1538;
  auto vmo_big = ModelTestVmObject::CreateContiguous(0x7000'0000, kBigPages);
  for (uint64_t alloc_limit : {uint64_t{0}, uint64_t{1}, uint64_t{1}}) {
    PmmMock::Get().SetAllocationLimit(alloc_limit);
    auto fail_res = backend.BtiPin(bti, PinnedVmObject(vmo_big, 0, kBigPages * kPageSize),
                                   IOMMU_FLAG_PERM_READ, true);
    PmmMock::Get().ClearAllocationLimit();
    EXPECT_EQ(ZX_ERR_NO_MEMORY, fail_res.status_value());
    EXPECT_TRUE(hw_model.VerifyQuiescent());

    // Pre-existing mapping must remain 100% intact, and the rolled-back region
    // must fault cleanly with no stale TLB entries!
    EXPECT_EQ(spec.TranslateDma(kStreamId, q_base->device_vaddr, IOMMU_FLAG_PERM_READ),
              hw_model.TranslateDma(kStreamId, q_base->device_vaddr, IOMMU_FLAG_PERM_READ));
    EXPECT_EQ(
        spec.TranslateDma(kStreamId, q_base->device_vaddr + 510 * kPageSize, IOMMU_FLAG_PERM_WRITE),
        hw_model.TranslateDma(kStreamId, q_base->device_vaddr + 510 * kPageSize,
                              IOMMU_FLAG_PERM_WRITE));
    EXPECT_EQ(DmaTranslationResult::Fault(DmaFaultType::kTranslation),
              hw_model.TranslateDma(kStreamId, 0x40'0000, IOMMU_FLAG_PERM_READ));
  }

  // With the allocation limit cleared, the 1538-page mapping now succeeds!
  auto ok_res = backend.BtiPin(bti, PinnedVmObject(vmo_big, 0, kBigPages * kPageSize),
                               IOMMU_FLAG_PERM_READ, true);
  ASSERT_OK(ok_res.status_value());
  auto pmt_big = std::move(*ok_res);
  auto q_big = backend.PmtQueryAddress(pmt_big, 0, kBigPages * kPageSize);
  ASSERT_OK(q_big.status_value());
  EXPECT_EQ(DmaTranslationResult::Ok(0x7000'0000 + (kBigPages - 1) * kPageSize),
            hw_model.TranslateDma(kStreamId, q_big->device_vaddr + (kBigPages - 1) * kPageSize,
                                  IOMMU_FLAG_PERM_READ));

  backend.PmtUnpinAndDrop(pmt_big);
  backend.PmtUnpinAndDrop(pmt_base);
  backend.DropBti(bti);
  backend.DestroySmmu(smmu);
  EXPECT_TRUE(hw_model.VerifyQuiescent());
  EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
}

// ============================================================================
// 5. Designed ("White-Box") Sequence: Full BTI/PMT Lifecycle, Quarantine,
//    Orphaned BTI, & ReleaseQuarantine in Both Enforced and Passthru Modes
// ============================================================================
void RunDesignedBtiLifecycleAndQuarantine(SmmuBackendRunner& backend) {
  backend.LogTestStart("DesignedBtiLifecycleAndQuarantine");

  // --- Part A: Enforced Mode ---
  {
    BootOptions::Get()->SetArmSmmuMode("enforced");
    PmmMock::Get().ClearAllocationLimit();
    InterruptMock::Get().Reset();

    SimSmmuHardwareModel hw_model;
    auto smmu_res = backend.CreateSmmu(hw_model.MakeZbiConfig());
    ASSERT_OK(smmu_res.status_value());
    auto smmu = std::move(*smmu_res);

    constexpr uint16_t kStreamId = 0x0044;
    auto bti_res = backend.CreateBti(smmu, kStreamId);
    ASSERT_OK(bti_res.status_value());
    auto bti = std::move(*bti_res);

    // Case A1: Leak PMT1 while PMT2 remains active -> BTI enters BtiMode::kFault,
    // PMT1's IOVA is unmapped, PMT2 remains mapped, new BtiPin fails with
    // ZX_ERR_BAD_STATE until ReleaseQuarantine() is called.
    auto vmo1 = ModelTestVmObject::CreateContiguous(0xa000'0000, 2);
    auto vmo2 = ModelTestVmObject::CreateContiguous(0xb000'0000, 2);
    auto pmt1 = backend.BtiPin(bti, PinnedVmObject(vmo1, 0, 2 * kPageSize),
                               IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE, true);
    ASSERT_OK(pmt1.status_value());
    auto q1 = backend.PmtQueryAddress(*pmt1, 0, 2 * kPageSize);
    ASSERT_OK(q1.status_value());

    auto pmt2 =
        backend.BtiPin(bti, PinnedVmObject(vmo2, 0, 2 * kPageSize), IOMMU_FLAG_PERM_READ, true);
    ASSERT_OK(pmt2.status_value());
    auto q2 = backend.PmtQueryAddress(*pmt2, 0, 2 * kPageSize);
    ASSERT_OK(q2.status_value());

    // Leak pmt1 (close handle without ReleasePinnedMemory).
    backend.PmtLeakAndDrop(*pmt1);
    EXPECT_TRUE(backend.BtiInFaultState(bti));
    EXPECT_EQ(1u, backend.BtiQuarantineCount(bti));
    EXPECT_EQ(2u, backend.BtiPmoCount(bti));

    // PMT1 is unmapped, PMT2 is still mapped.
    EXPECT_EQ(DmaTranslationResult::Fault(DmaFaultType::kTranslation),
              hw_model.TranslateDma(kStreamId, q1->device_vaddr, IOMMU_FLAG_PERM_READ));
    EXPECT_EQ(DmaTranslationResult::Ok(0xb000'0000),
              hw_model.TranslateDma(kStreamId, q2->device_vaddr, IOMMU_FLAG_PERM_READ));

    // Pinning while in fault state must fail with ZX_ERR_BAD_STATE.
    EXPECT_EQ(
        ZX_ERR_BAD_STATE,
        backend.BtiPin(bti, PinnedVmObject(vmo1, 0, 2 * kPageSize), IOMMU_FLAG_PERM_READ, false)
            .status_value());

    // ReleaseQuarantine restores BTI to kTranslation mode.
    backend.ReleaseQuarantine(bti);
    EXPECT_FALSE(backend.BtiInFaultState(bti));
    EXPECT_EQ(0u, backend.BtiQuarantineCount(bti));
    EXPECT_EQ(1u, backend.BtiPmoCount(bti));

    // Case A2: Drop BTI while PMT2 is still active -> BTI becomes orphaned,
    // CreateBti(kStreamId) fails with ZX_ERR_ALREADY_EXISTS while PMT2 is active,
    // and succeeds once PMT2 is unpinned!
    backend.DropBti(bti);
    EXPECT_EQ(DmaTranslationResult::Ok(0xb000'0000),
              hw_model.TranslateDma(kStreamId, q2->device_vaddr, IOMMU_FLAG_PERM_READ));
    EXPECT_EQ(ZX_ERR_ALREADY_EXISTS, backend.CreateBti(smmu, kStreamId).status_value());

    backend.PmtUnpinAndDrop(*pmt2);
    EXPECT_EQ(DmaTranslationResult::Fault(DmaFaultType::kUnidentifiedStream),
              hw_model.TranslateDma(kStreamId, q2->device_vaddr, IOMMU_FLAG_PERM_READ));

    // Re-creating BTI with the same kStreamId now succeeds!
    auto bti_recreated = backend.CreateBti(smmu, kStreamId);
    ASSERT_OK(bti_recreated.status_value());
    backend.DropBti(*bti_recreated);

    backend.DestroySmmu(smmu);
    EXPECT_TRUE(hw_model.VerifyQuiescent());
    EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
  }

  // --- Part B: Passthru Mode ---
  {
    BootOptions::Get()->SetArmSmmuMode("passthru");
    PmmMock::Get().ClearAllocationLimit();
    InterruptMock::Get().Reset();

    SimSmmuHardwareModel hw_model;
    auto smmu_res = backend.CreateSmmu(hw_model.MakeZbiConfig());
    ASSERT_OK(smmu_res.status_value());
    auto smmu = std::move(*smmu_res);

    constexpr uint16_t kStreamId = 0x0055;
    auto bti_res = backend.CreateBti(smmu, kStreamId);
    ASSERT_OK(bti_res.status_value());
    auto bti = std::move(*bti_res);

    EXPECT_EQ(kPageSize, backend.BtiMinimumContiguity(bti));

    // In Passthru mode, requiring a contiguous mapping on a discontiguous VMO
    // must fail, while non-contiguous or physically contiguous requests succeed.
    auto vmo_discontig = ModelTestVmObject::CreateDiscontiguous({0xc000'0000, 0xc000'5000});
    EXPECT_NOT_OK(backend
                      .BtiPin(bti, PinnedVmObject(vmo_discontig, 0, 2 * kPageSize),
                              IOMMU_FLAG_PERM_READ, true)
                      .status_value());

    auto pmt_res = backend.BtiPin(bti, PinnedVmObject(vmo_discontig, 0, 2 * kPageSize),
                                  IOMMU_FLAG_PERM_READ, false);
    ASSERT_OK(pmt_res.status_value());
    auto pmt = std::move(*pmt_res);

    auto q_page0 = backend.PmtQueryAddress(pmt, 0, 2 * kPageSize);
    ASSERT_OK(q_page0.status_value());
    EXPECT_EQ(0xc000'0000ULL, q_page0->device_vaddr);
    EXPECT_EQ(kPageSize, q_page0->size);

    auto q_page1 = backend.PmtQueryAddress(pmt, kPageSize, kPageSize);
    ASSERT_OK(q_page1.status_value());
    EXPECT_EQ(0xc000'5000ULL, q_page1->device_vaddr);

    // In Passthru mode, any address bypasses Stage-1 translation while BTI is in kBypass.
    EXPECT_EQ(DmaTranslationResult::Ok(0xc000'0000),
              hw_model.TranslateDma(kStreamId, 0xc000'0000, IOMMU_FLAG_PERM_READ));

    // Leak PMT in Passthru mode -> Context Bank switches to BtiMode::kFault
    // (SCTLR.M=1, EPD0=1, EPD1=1), blocking ALL DMA from kStreamId!
    backend.PmtLeakAndDrop(pmt);
    EXPECT_TRUE(backend.BtiInFaultState(bti));
    EXPECT_EQ(DmaTranslationResult::Fault(DmaFaultType::kTranslation),
              hw_model.TranslateDma(kStreamId, 0xc000'0000, IOMMU_FLAG_PERM_READ));

    // ReleaseQuarantine in Passthru mode restores Context Bank to kBypass!
    backend.ReleaseQuarantine(bti);
    EXPECT_FALSE(backend.BtiInFaultState(bti));
    EXPECT_EQ(DmaTranslationResult::Ok(0xc000'0000),
              hw_model.TranslateDma(kStreamId, 0xc000'0000, IOMMU_FLAG_PERM_READ));

    backend.DropBti(bti);
    backend.DestroySmmu(smmu);
    EXPECT_TRUE(hw_model.VerifyQuiescent());
    EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
  }
}

// ============================================================================
// 6. Designed ("White-Box") Sequence: Bootloader SMRG & Context Bank Adoption
// ============================================================================
void RunDesignedBootloaderAdoption(SmmuBackendRunner& backend) {
  backend.LogTestStart("DesignedBootloaderAdoption");

  for (const char* mode_str : {"passthru", "enforced"}) {
    BootOptions::Get()->SetArmSmmuMode(mode_str);
    PmmMock::Get().ClearAllocationLimit();
    InterruptMock::Get().Reset();

    SimSmmuHardwareModel hw_model;
    // SMRG 0: Bypass mode for SID 0x0042 (in handoff list -> adopted).
    hw_model.PreconfigureBootloaderSmrg(0, 0x0042, 0x0000, S2CR_Type::kBypass);
    // SMRG 1 & SMRG 2: Translation mode sharing Context Bank 2 (SCTLR.M=0 bypass)
    // for SIDs 0x0050 and 0x0051 (in handoff list -> both adopted into one BTI).
    hw_model.PreconfigureBootloaderSmrg(1, 0x0050, 0x0000, S2CR_Type::kTranslation, 2,
                                        CBAR_Type::kS1TS2Bypass, false, true, true);
    hw_model.PreconfigureBootloaderSmrg(2, 0x0051, 0x0000, S2CR_Type::kTranslation, 2,
                                        CBAR_Type::kS1TS2Bypass, false, true, true);
    // SMRG 3: Active in bootloader for SID 0x0099, but NOT in handoff list -> locked down!
    hw_model.PreconfigureBootloaderSmrg(3, 0x0099, 0x0000, S2CR_Type::kBypass);

    auto zbi_cfg = hw_model.MakeZbiConfig({0x0042, 0x0050, 0x0051});
    auto smmu_res = backend.CreateSmmu(zbi_cfg);
    ASSERT_OK(smmu_res.status_value());
    auto smmu = std::move(*smmu_res);

    // Adopted SIDs 0x0042, 0x0050, 0x0051 remain active in bypass; non-handoff
    // SID 0x0099 was locked down and faults as an unidentified stream.
    EXPECT_EQ(DmaTranslationResult::Ok(0x1234'0000),
              hw_model.TranslateDma(0x0042, 0x1234'0000, IOMMU_FLAG_PERM_READ));
    EXPECT_EQ(DmaTranslationResult::Ok(0x1234'0000),
              hw_model.TranslateDma(0x0050, 0x1234'0000, IOMMU_FLAG_PERM_READ));
    EXPECT_EQ(DmaTranslationResult::Ok(0x1234'0000),
              hw_model.TranslateDma(0x0051, 0x1234'0000, IOMMU_FLAG_PERM_READ));
    EXPECT_EQ(DmaTranslationResult::Fault(DmaFaultType::kUnidentifiedStream),
              hw_model.TranslateDma(0x0099, 0x1234'0000, IOMMU_FLAG_PERM_READ));

    // Creating a BTI for an adopted StreamID must return ZX_ERR_ALREADY_EXISTS,
    // while creating a BTI for the locked-down StreamID 0x0099 succeeds!
    EXPECT_EQ(ZX_ERR_ALREADY_EXISTS, backend.CreateBti(smmu, 0x0042).status_value());
    EXPECT_EQ(ZX_ERR_ALREADY_EXISTS, backend.CreateBti(smmu, 0x0050).status_value());
    EXPECT_EQ(ZX_ERR_ALREADY_EXISTS, backend.CreateBti(smmu, 0x0051).status_value());

    auto bti_99 = backend.CreateBti(smmu, 0x0099);
    ASSERT_OK(bti_99.status_value());
    backend.DropBti(*bti_99);

    backend.DestroySmmu(smmu);
    EXPECT_TRUE(hw_model.VerifyQuiescent());
    EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
  }
}

// ============================================================================
// 7. Designed ("White-Box") Sequence: Global & Context Bank Fault IRQ Handling
// ============================================================================
void RunDesignedFaultIrqHandling(SmmuBackendRunner& backend) {
  backend.LogTestStart("DesignedFaultIrqHandling");
  BootOptions::Get()->SetArmSmmuMode("enforced");
  PmmMock::Get().ClearAllocationLimit();
  InterruptMock::Get().Reset();

  SimSmmuHardwareModel hw_model;
  auto smmu_res = backend.CreateSmmu(hw_model.MakeZbiConfig());
  ASSERT_OK(smmu_res.status_value());
  auto smmu = std::move(*smmu_res);

  constexpr uint16_t kStreamId = 0x0066;
  auto bti_res = backend.CreateBti(smmu, kStreamId);
  ASSERT_OK(bti_res.status_value());
  auto bti = std::move(*bti_res);

  // 7a. Trigger a Global Fault IRQ (USF = bit 1, MULTI = bit 31).
  ASSERT_TRUE(
      hw_model.InjectGlobalFaultIrq((1u << 31) | (1u << 1), 0xdead'beef'0000ULL, 0x3u, 0x0bad));
  // Handler must clear GFSR via W1C and mask the global IRQ.
  EXPECT_EQ(0u, hw_model.gfsr());
  EXPECT_TRUE(InterruptMock::Get().IsMasked(hw_model.config().global_irq_num));

  // 7b. Trigger a Context Bank Fault IRQ (TF = bit 1,PF = bit 3) on kStreamId's CB.
  auto maybe_cb = hw_model.FindContextBankForStreamId(kStreamId);
  ASSERT_TRUE(maybe_cb.has_value());
  const uint32_t cb_ndx = *maybe_cb;
  EXPECT_FALSE(InterruptMock::Get().IsMasked(hw_model.config().base_context_irq_num + cb_ndx));

  ASSERT_TRUE(hw_model.InjectContextFaultIrq(cb_ndx, (1u << 1) | (1u << 3), 0xfeed'face'0000ULL,
                                             0x10u, kStreamId));
  // Handler must clear FSR via W1C, clear SCTLR.CFIE (bit 6), mask the CB IRQ,
  // and place the BTI into BtiMode::kFault!
  EXPECT_EQ(0u, hw_model.cb_fsr(cb_ndx));
  EXPECT_EQ(0u, hw_model.cb_sctlr(cb_ndx) & (1u << 6));
  EXPECT_TRUE(InterruptMock::Get().IsMasked(hw_model.config().base_context_irq_num + cb_ndx));
  EXPECT_TRUE(backend.BtiInFaultState(bti));

  auto vmo = ModelTestVmObject::CreateContiguous(0xd000'0000, 1);
  EXPECT_EQ(ZX_ERR_BAD_STATE,
            backend.BtiPin(bti, PinnedVmObject(vmo, 0, kPageSize), IOMMU_FLAG_PERM_READ, false)
                .status_value());

  // Calling ReleaseQuarantine recovers the BTI from fault mode and re-enables
  // SCTLR.CFIE and unmasks the CB interrupt!
  backend.ReleaseQuarantine(bti);
  EXPECT_FALSE(backend.BtiInFaultState(bti));
  EXPECT_NE(0u, hw_model.cb_sctlr(cb_ndx) & (1u << 6));
  EXPECT_FALSE(InterruptMock::Get().IsMasked(hw_model.config().base_context_irq_num + cb_ndx));

  auto pmt_res =
      backend.BtiPin(bti, PinnedVmObject(vmo, 0, kPageSize), IOMMU_FLAG_PERM_READ, false);
  ASSERT_OK(pmt_res.status_value());
  auto q = backend.PmtQueryAddress(*pmt_res, 0, kPageSize);
  ASSERT_OK(q.status_value());
  const uint32_t cb_irq = hw_model.config().base_context_irq_num + cb_ndx;

  // 7c. A device access, rather than an injected syndrome. An access the
  // mapping permits raises nothing.
  EXPECT_TRUE(
      hw_model.DeviceAccess(kStreamId, q->device_vaddr + 0x80, IOMMU_FLAG_PERM_READ).is_ok());
  EXPECT_FALSE(backend.BtiInFaultState(bti));
  EXPECT_TRUE(hw_model.VerifyQuiescent());

  // A write to the read-only mapping is a permission fault. The hardware
  // records it and raises the context interrupt; the handler must clear the
  // status, disable and mask the interrupt, and put the BTI into its fault
  // state, exactly as for the injected fault above.
  EXPECT_EQ(DmaFaultType::kPermission,
            hw_model.DeviceAccess(kStreamId, q->device_vaddr + 0x80, IOMMU_FLAG_PERM_WRITE).fault);
  EXPECT_EQ(0u, hw_model.cb_fsr(cb_ndx));
  EXPECT_EQ(0u, hw_model.cb_sctlr(cb_ndx) & (1u << 6));
  EXPECT_TRUE(InterruptMock::Get().IsMasked(cb_irq));
  EXPECT_TRUE(backend.BtiInFaultState(bti));
  EXPECT_TRUE(hw_model.VerifyQuiescent());

  backend.ReleaseQuarantine(bti);
  EXPECT_FALSE(backend.BtiInFaultState(bti));

  // An access to an address with no mapping is a translation fault, reported
  // the same way.
  EXPECT_EQ(
      DmaFaultType::kTranslation,
      hw_model.DeviceAccess(kStreamId, q->device_vaddr + 0x10'0000, IOMMU_FLAG_PERM_READ).fault);
  EXPECT_TRUE(backend.BtiInFaultState(bti));
  backend.ReleaseQuarantine(bti);
  EXPECT_FALSE(backend.BtiInFaultState(bti));
  EXPECT_TRUE(hw_model.VerifyQuiescent());

  backend.PmtUnpinAndDrop(*pmt_res);

  backend.DropBti(bti);
  backend.DestroySmmu(smmu);
  EXPECT_TRUE(hw_model.VerifyQuiescent());
  EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
}

// ============================================================================
// 7b. Designed ("White-Box") Sequence: The Two Halves of a PMT Unpin
// ============================================================================

// Exercises what can happen between the two halves of zx_pmt_unpin(), which
// unpins a PMT and only afterwards drops its handle: other pins, and the BTI's
// own last handle going away.
void RunDesignedSplitPmtLifecycle(SmmuBackendRunner& backend) {
  backend.LogTestStart("DesignedSplitPmtLifecycle");
  for (ArmSmmuMode mode : {ArmSmmuMode::kEnforced, ArmSmmuMode::kPassthru}) {
    const bool enforced = (mode == ArmSmmuMode::kEnforced);
    BootOptions::Get()->SetArmSmmuMode(enforced ? "enforced" : "passthru");
    PmmMock::Get().ClearAllocationLimit();
    InterruptMock::Get().Reset();

    SimSmmuHardwareModel hw_model;
    auto smmu_res = backend.CreateSmmu(hw_model.MakeZbiConfig());
    ASSERT_OK(smmu_res.status_value());
    auto smmu = std::move(*smmu_res);

    constexpr uint16_t kStreamId = 0x0071;
    auto bti_res = backend.CreateBti(smmu, kStreamId);
    ASSERT_OK(bti_res.status_value());
    auto bti = std::move(*bti_res);
    const uint32_t perms = IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE;

    // Case 1: unpin, do other work, then close.
    auto vmo1 = ModelTestVmObject::CreateContiguous(0xa000'0000, 2);
    auto pmt1 = backend.BtiPin(bti, PinnedVmObject(vmo1, 0, 2 * kPageSize), perms, true);
    ASSERT_OK(pmt1.status_value());
    auto q1 = backend.PmtQueryAddress(*pmt1, 0, 2 * kPageSize);
    ASSERT_OK(q1.status_value());
    EXPECT_EQ(1u, backend.BtiPmoCount(bti));
    backend.PmtUnpin(*pmt1);
    // Unpinning alone removes the mapping and the PMT from the BTI's count.
    EXPECT_EQ(0u, backend.BtiPmoCount(bti));
    EXPECT_FALSE(backend.BtiInFaultState(bti));
    if (enforced) {
      EXPECT_EQ(DmaFaultType::kTranslation,
                hw_model.TranslateDma(kStreamId, q1->device_vaddr, IOMMU_FLAG_PERM_READ).fault);
    }
    EXPECT_TRUE(hw_model.VerifyQuiescent());

    // A new pin while the old PMT's handle is still open (it may reuse the address).
    auto vmo2 = ModelTestVmObject::CreateContiguous(0xb000'0000, 2);
    auto pmt2 = backend.BtiPin(bti, PinnedVmObject(vmo2, 0, 2 * kPageSize), perms, true);
    ASSERT_OK(pmt2.status_value());
    EXPECT_TRUE(hw_model.VerifyQuiescent());

    // Closing an unpinned PMT is not a leak.
    backend.PmtClose(*pmt1);
    EXPECT_FALSE(backend.BtiInFaultState(bti));
    EXPECT_EQ(0u, backend.BtiQuarantineCount(bti));
    EXPECT_EQ(1u, backend.BtiPmoCount(bti));
    EXPECT_TRUE(hw_model.VerifyQuiescent());
    backend.PmtUnpinAndDrop(*pmt2);

    // Case 2: unpin, close the BTI's last handle, then close the PMT. With no
    // pinned or quarantined PMT the BTI is not orphaned and reaches the end of
    // its life at once; the PMT's close arrives afterwards and must be
    // harmless.
    auto vmo3 = ModelTestVmObject::CreateContiguous(0xc000'0000, 1);
    auto pmt3 = backend.BtiPin(bti, PinnedVmObject(vmo3, 0, kPageSize), perms, true);
    ASSERT_OK(pmt3.status_value());
    backend.PmtUnpin(*pmt3);
    backend.DropBti(bti);
    EXPECT_EQ(DmaFaultType::kUnidentifiedStream,
              hw_model.TranslateDma(kStreamId, 0xc000'0000, IOMMU_FLAG_PERM_READ).fault);
    EXPECT_TRUE(hw_model.VerifyQuiescent());
    backend.PmtClose(*pmt3);
    EXPECT_TRUE(hw_model.VerifyQuiescent());

    // The stream is free again.
    auto bti_again = backend.CreateBti(smmu, kStreamId);
    ASSERT_OK(bti_again.status_value());
    backend.DropBti(*bti_again);

    backend.DestroySmmu(smmu);
    EXPECT_TRUE(hw_model.VerifyQuiescent());
    EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
  }
}

// ============================================================================
// 8. Time-Bound Randomized State-Machine Sequence (MiniStress)
// ============================================================================
// |min_steps| and |budget| bound the run: it continues until both the step
// count and the time budget are met.
void RunRandomizedMiniStress(SmmuBackendRunner& backend, ArmSmmuMode mode, uint64_t seed,
                             size_t min_steps = 150,
                             std::chrono::milliseconds budget = std::chrono::milliseconds(120)) {
  char title[128];
  snprintf(title, sizeof(title), "RandomizedMiniStress(%s, seed=0x%lx)",
           (mode == ArmSmmuMode::kEnforced) ? "Enforced" : "Passthru", seed);
  backend.LogTestStart(title);

  BootOptions::Get()->SetArmSmmuMode((mode == ArmSmmuMode::kEnforced) ? "enforced" : "passthru");
  PmmMock::Get().ClearAllocationLimit();
  InterruptMock::Get().Reset();

  SimSmmuHardwareModel hw_model;
  AbstractIommuSpec spec(mode);

  auto smmu_res = backend.CreateSmmu(hw_model.MakeZbiConfig());
  ASSERT_OK(smmu_res.status_value());
  auto smmu = std::move(*smmu_res);

  struct ActivePmtInfo {
    SmmuBackendRunner::PmtHandle handle;
    uint64_t base_iova = 0;
    std::vector<paddr_t> paddrs;
    uint32_t perms = 0;
  };

  struct ActiveBtiInfo {
    uint64_t bti_id = 0;
    // A stream ID the BTI matches (its ID with the masked bits cleared).
    uint16_t sid = 0;
    // The BTI's stream ID mask: bits which are ignored when matching.
    uint16_t mask = 0;
    // Declared before `pmts` and `unpinned_open` so reverse member destruction on an early
    // assertion return unpins/closes all PMTs before closing the BTI handle.
    SmmuBackendRunner::BtiHandle handle;
    std::vector<ActivePmtInfo> pmts;
    // A PMT of this BTI which has been unpinned but whose handle is still open.
    //
    // At most one per BTI. Two or more, on a BTI whose own handle has been
    // closed, is a sequence both drivers are known to mishandle (see "can run
    // on_end_of_life twice" in pending_bugs.md), so the generator does not
    // produce it.
    std::optional<SmmuBackendRunner::PmtHandle> unpinned_open;
  };

  std::vector<ActiveBtiInfo> active_btis;
  std::mt19937_64 rng(seed);
  uint64_t next_phys_page = 0x1'0000'0000ULL;

  // Device address ranges which were recently unmapped. They are probed after
  // every later step: an address which was unmapped must fault until something
  // is mapped there again, whether or not the driver ever reuses it.
  struct UnmappedRange {
    uint16_t sid = 0;
    uint64_t base = 0;
    size_t pages = 0;
  };
  std::array<std::optional<UnmappedRange>, 8> recently_unmapped;
  size_t recent_next = 0;
  const auto note_unmapped = [&](uint16_t sid, uint64_t base, size_t pages) {
    recently_unmapped[recent_next++ % recently_unmapped.size()] =
        UnmappedRange{.sid = sid, .base = base, .pages = pages};
  };

  const auto start_time = std::chrono::steady_clock::now();
  size_t step_count = 0;

  while (std::chrono::steady_clock::now() - start_time < budget || step_count < min_steps) {
    ++step_count;
    const uint32_t action = static_cast<uint32_t>(rng() % 100);

    if (active_btis.empty() || (action < 15 && active_btis.size() < 6)) {
      // Action: CreateBti. One in four BTIs covers a group of 2 or 4 stream IDs
      // through a stream ID mask, so that masked matching and overlap detection
      // are exercised.
      const uint16_t candidate_mask =
          (rng() % 4 == 0) ? static_cast<uint16_t>((rng() % 2 == 0) ? 0x1 : 0x3) : uint16_t{0};
      const uint16_t candidate_sid =
          static_cast<uint16_t>((0x100 + (rng() % 32)) & ~uint32_t{candidate_mask});
      const uint64_t candidate_id = (uint64_t{candidate_mask} << 16) | candidate_sid;
      auto res = backend.CreateBti(smmu, candidate_id);
      // Two BTIs collide if some stream ID matches both: their IDs agree on
      // every bit that neither masks out.
      bool expected_exists = false;
      for (const auto& b : active_btis) {
        if (((b.sid ^ candidate_sid) & ~(b.mask | candidate_mask) & 0x7fff) == 0) {
          expected_exists = true;
          break;
        }
      }
      if (expected_exists) {
        EXPECT_EQ(ZX_ERR_ALREADY_EXISTS, res.status_value());
      } else {
        ASSERT_OK(res.status_value());
        spec.AddBti(candidate_id);
        active_btis.push_back(ActiveBtiInfo{
            .bti_id = candidate_id,
            .sid = candidate_sid,
            .mask = candidate_mask,
            .handle = std::move(*res),
            .pmts = {},
        });
      }
    } else if (action < 55) {
      // Action: BtiPin (sometimes with injected OOM)
      auto& bti_info = active_btis[rng() % active_btis.size()];
      const bool contig = (rng() % 2) == 0;
      // Mostly small mappings. One contiguous mapping in eight is large enough
      // to span several last-level tables and to make the driver invalidate by
      // ASID rather than page by page when it is unmapped.
      const size_t num_pages =
          (contig && (rng() % 8) == 0) ? (257 + (rng() % 512)) : (1 + (rng() % 16));
      const uint32_t perm_choices[] = {
          IOMMU_FLAG_PERM_READ,
          IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE,
          IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_EXECUTE,
          IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE | IOMMU_FLAG_PERM_EXECUTE,
      };
      const uint32_t perms = perm_choices[rng() % 4];

      std::vector<paddr_t> pages(num_pages);
      for (size_t i = 0; i < num_pages; ++i) {
        pages[i] = next_phys_page;
        next_phys_page += contig ? kPageSize : (2 * kPageSize);
      }
      auto vmo = contig ? ModelTestVmObject::CreateContiguous(pages[0], num_pages)
                        : ModelTestVmObject::CreateDiscontiguous(pages);

      const bool inject_oom = (mode == ArmSmmuMode::kEnforced) && ((rng() % 8) == 0);
      if (inject_oom) {
        // Fail the 1st to 5th page table page allocation of this pin.
        PmmMock::Get().SetAllocationLimit(rng() % 5);
      }
      auto pmt_res = backend.BtiPin(bti_info.handle, PinnedVmObject(vmo, 0, num_pages * kPageSize),
                                    perms, contig);
      if (inject_oom) {
        PmmMock::Get().ClearAllocationLimit();
      }

      if (backend.BtiInFaultState(bti_info.handle)) {
        EXPECT_EQ(ZX_ERR_BAD_STATE, pmt_res.status_value());
      } else if (pmt_res.is_ok()) {
        auto q_res = backend.PmtQueryAddress(*pmt_res, 0, num_pages * kPageSize);
        ASSERT_OK(q_res.status_value());
        if (mode == ArmSmmuMode::kEnforced) {
          spec.AddMapping(bti_info.bti_id, q_res->device_vaddr, pages, perms);
        }
        bti_info.pmts.push_back(ActivePmtInfo{
            .handle = std::move(*pmt_res),
            .base_iova = q_res->device_vaddr,
            .paddrs = std::move(pages),
            .perms = perms,
        });
      }
    } else if (action < 75) {
      // Action: PmtUnpinAndDrop
      auto& bti_info = active_btis[rng() % active_btis.size()];
      // Half the time, close a PMT which was unpinned by an earlier step.
      if (bti_info.unpinned_open.has_value() && (bti_info.pmts.empty() || (rng() % 2) == 0)) {
        backend.PmtClose(*bti_info.unpinned_open);
        bti_info.unpinned_open.reset();
      } else if (!bti_info.pmts.empty()) {
        const size_t idx = rng() % bti_info.pmts.size();
        ActivePmtInfo pmt = std::move(bti_info.pmts[idx]);
        bti_info.pmts.erase(bti_info.pmts.begin() + idx);
        // One time in three, unpin now and leave the handle open, so that later
        // steps run between the two halves of zx_pmt_unpin().
        if (!bti_info.unpinned_open.has_value() && (rng() % 3) == 0) {
          backend.PmtUnpin(pmt.handle);
          bti_info.unpinned_open = std::move(pmt.handle);
        } else {
          backend.PmtUnpinAndDrop(pmt.handle);
        }
        if (mode == ArmSmmuMode::kEnforced) {
          spec.RemoveMapping(bti_info.bti_id, pmt.base_iova, pmt.paddrs.size());
          note_unmapped(bti_info.sid, pmt.base_iova, pmt.paddrs.size());
        }
      }
    } else if (action < 85) {
      // Action: PmtLeakAndDrop (Quarantine)
      auto& bti_info = active_btis[rng() % active_btis.size()];
      if (!bti_info.pmts.empty()) {
        const size_t idx = rng() % bti_info.pmts.size();
        ActivePmtInfo pmt = std::move(bti_info.pmts[idx]);
        bti_info.pmts.erase(bti_info.pmts.begin() + idx);
        backend.PmtLeakAndDrop(pmt.handle);
        if (mode == ArmSmmuMode::kEnforced) {
          spec.RemoveMapping(bti_info.bti_id, pmt.base_iova, pmt.paddrs.size());
          note_unmapped(bti_info.sid, pmt.base_iova, pmt.paddrs.size());
        }
        spec.SetBtiFault(bti_info.bti_id, true);
        EXPECT_TRUE(backend.BtiInFaultState(bti_info.handle));
      }
    } else if (action < 92) {
      // Action: ReleaseQuarantine
      auto& bti_info = active_btis[rng() % active_btis.size()];
      backend.ReleaseQuarantine(bti_info.handle);
      spec.SetBtiFault(bti_info.bti_id, false);
      EXPECT_FALSE(backend.BtiInFaultState(bti_info.handle));
    } else if (action < 96) {
      // Action: Hardware Context Fault IRQ
      auto& bti_info = active_btis[rng() % active_btis.size()];
      if (auto maybe_cb = hw_model.FindContextBankForStreamId(bti_info.sid); maybe_cb.has_value()) {
        // The handler only runs if the driver has the interrupt enabled
        // (SCTLR.CFIE) and unmasked. When it does not run, the fault stays
        // recorded and the BTI's state does not change. That happens when the
        // BTI is already faulted, and also in a state the driver should not be
        // in: see "Context fault interrupt left disabled" in pending_bugs.md.
        if (hw_model.InjectContextFaultIrq(*maybe_cb, 0x2u, 0xbad0'0000ULL, 0x1u, bti_info.sid)) {
          spec.SetBtiFault(bti_info.bti_id, true);
          EXPECT_TRUE(backend.BtiInFaultState(bti_info.handle));
        }
      }
    } else {
      // Action: Unpin all PMTs on a BTI and DropBti
      const size_t idx = rng() % active_btis.size();
      ActiveBtiInfo bti_info = std::move(active_btis[idx]);
      active_btis.erase(active_btis.begin() + idx);
      for (auto& pmt : bti_info.pmts) {
        backend.PmtUnpinAndDrop(pmt.handle);
      }
      if (backend.BtiInFaultState(bti_info.handle)) {
        backend.ReleaseQuarantine(bti_info.handle);
      }
      // A PMT which is unpinned but still open may be closed on either side of
      // the BTI's last handle going away.
      const bool close_first = (rng() % 2) == 0;
      if (bti_info.unpinned_open.has_value() && close_first) {
        backend.PmtClose(*bti_info.unpinned_open);
      }
      backend.DropBti(bti_info.handle);
      if (bti_info.unpinned_open.has_value() && !close_first) {
        backend.PmtClose(*bti_info.unpinned_open);
      }
      spec.RemoveBti(bti_info.bti_id);
    }

    // After EVERY step: verify zero hardware hazards and lockstep agreement
    // between SimSmmuHardwareModel::TranslateDma and AbstractIommuSpec::TranslateDma.
    // A fault interrupt which became deliverable during this step (for example
    // one recorded while the interrupt was disabled, and since re-enabled)
    // fires now, and its handler puts the owning BTI into its fault state.
    for (uint32_t cb : hw_model.ServicePendingInterrupts().contexts) {
      for (const auto& bti_info : active_btis) {
        if (hw_model.FindContextBankForStreamId(bti_info.sid) == std::optional<uint32_t>(cb)) {
          spec.SetBtiFault(bti_info.bti_id, true);
        }
      }
    }
    ASSERT_TRUE(hw_model.VerifyQuiescent());
    ASSERT_TRUE(spec.VerifyDistinctContexts(hw_model));
    ASSERT_TRUE(spec.VerifyAllMappings(hw_model));
    for (const auto& range : recently_unmapped) {
      if (!range.has_value()) {
        continue;
      }
      const uint64_t page_off = rng() % range->pages;
      for (uint64_t iova : {range->base + 0x18, range->base + page_off * kPageSize + 0x7f0}) {
        ASSERT_EQ(spec.TranslateDma(range->sid, iova, IOMMU_FLAG_PERM_READ),
                  hw_model.TranslateDma(range->sid, iova, IOMMU_FLAG_PERM_READ));
      }
    }
    for (const auto& bti_info : active_btis) {
      if (mode == ArmSmmuMode::kEnforced) {
        for (const auto& pmt : bti_info.pmts) {
          const uint64_t probe_iova =
              pmt.base_iova + (rng() % pmt.paddrs.size()) * kPageSize + 0x40;
          for (uint32_t perm : {uint32_t{IOMMU_FLAG_PERM_READ}, uint32_t{IOMMU_FLAG_PERM_WRITE},
                                uint32_t{IOMMU_FLAG_PERM_EXECUTE}}) {
            ASSERT_EQ(spec.TranslateDma(bti_info.sid, probe_iova, perm),
                      hw_model.TranslateDma(bti_info.sid, probe_iova, perm));
          }
        }
      } else {
        ASSERT_EQ(spec.TranslateDma(bti_info.sid, 0x1234'5000, IOMMU_FLAG_PERM_READ),
                  hw_model.TranslateDma(bti_info.sid, 0x1234'5000, IOMMU_FLAG_PERM_READ));
      }
    }
  }

  // Clean up all remaining active BTIs and PMTs.
  for (auto& bti_info : active_btis) {
    for (auto& pmt : bti_info.pmts) {
      backend.PmtUnpinAndDrop(pmt.handle);
    }
    if (bti_info.unpinned_open.has_value()) {
      backend.PmtClose(*bti_info.unpinned_open);
    }
    if (backend.BtiInFaultState(bti_info.handle)) {
      backend.ReleaseQuarantine(bti_info.handle);
    }
    backend.DropBti(bti_info.handle);
  }
  backend.DestroySmmu(smmu);
  EXPECT_TRUE(hw_model.VerifyQuiescent());
  EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
}

TEST(SmmuModelTest, DesignedInitAndGlobalConfig) {
  for (BackendKind kind : GetBackendsToTest()) {
    SmmuBackendRunner backend(kind);
    RunDesignedInitAndGlobalConfig(backend);
  }
}

TEST(SmmuModelTest, DesignedBoundaryStraddlingAndReclamation) {
  for (BackendKind kind : GetBackendsToTest()) {
    SmmuBackendRunner backend(kind);
    RunDesignedBoundaryStraddlingAndReclamation(backend);
  }
}

TEST(SmmuModelTest, DesignedPermissionsEnforcement) {
  for (BackendKind kind : GetBackendsToTest()) {
    SmmuBackendRunner backend(kind);
    RunDesignedPermissionsEnforcement(backend);
  }
}

TEST(SmmuModelTest, DesignedOomRollbackMidMap) {
  for (BackendKind kind : GetBackendsToTest()) {
    SmmuBackendRunner backend(kind);
    RunDesignedOomRollbackMidMap(backend);
  }
}

TEST(SmmuModelTest, DesignedBtiLifecycleAndQuarantine) {
  for (BackendKind kind : GetBackendsToTest()) {
    SmmuBackendRunner backend(kind);
    RunDesignedBtiLifecycleAndQuarantine(backend);
  }
}

TEST(SmmuModelTest, DesignedBootloaderAdoption) {
  for (BackendKind kind : GetBackendsToTest()) {
    SmmuBackendRunner backend(kind);
    RunDesignedBootloaderAdoption(backend);
  }
}

TEST(SmmuModelTest, DesignedSplitPmtLifecycle) {
  for (BackendKind kind : GetBackendsToTest()) {
    SmmuBackendRunner backend(kind);
    RunDesignedSplitPmtLifecycle(backend);
  }
}

TEST(SmmuModelTest, DesignedFaultIrqHandling) {
  for (BackendKind kind : GetBackendsToTest()) {
    SmmuBackendRunner backend(kind);
    RunDesignedFaultIrqHandling(backend);
  }
}

TEST(SmmuModelTest, RandomizedMiniStressEnforced) {
  for (BackendKind kind : GetBackendsToTest()) {
    SmmuBackendRunner backend(kind);
    RunRandomizedMiniStress(backend, ArmSmmuMode::kEnforced, 0x534d'4d55'7632'0001ULL);
  }
}

TEST(SmmuModelTest, RandomizedMiniStressPassthru) {
  for (BackendKind kind : GetBackendsToTest()) {
    SmmuBackendRunner backend(kind);
    RunRandomizedMiniStress(backend, ArmSmmuMode::kPassthru, 0x534d'4d55'7632'0002ULL);
  }
}

// Runs the randomized sequences with a seed which changes on every run, so that
// repeated runs explore different sequences. The seed is printed; set
// ARM_SMMU_MODEL_SEED to replay a run.
TEST(SmmuModelTest, RandomizedMiniStressFreshSeed) {
  const uint64_t seed = FreshModelSeed();
  printf("SmmuModelTest.RandomizedMiniStressFreshSeed: ARM_SMMU_MODEL_SEED=0x%lx\n", seed);
  for (BackendKind kind : GetBackendsToTest()) {
    SmmuBackendRunner backend(kind);
    // A fixed number of steps, so that a seed replays the same sequence.
    RunRandomizedMiniStress(backend, ArmSmmuMode::kEnforced, seed, 300,
                            std::chrono::milliseconds(0));
    RunRandomizedMiniStress(backend, ArmSmmuMode::kPassthru, seed + 1, 300,
                            std::chrono::milliseconds(0));
  }
}

TEST(SmmuModelTest, HardwareModelDetectsOrderingAndCoherencyHazards) {
  BootOptions::Get()->SetArmSmmuMode("enforced");
  PmmMock::Get().ClearAllocationLimit();
  InterruptMock::Get().Reset();

  SimSmmuHardwareModel hw_model;
  SmmuBackendRunner backend(BackendKind::kCpp);
  auto smmu_res = backend.CreateSmmu(hw_model.MakeZbiConfig());
  ASSERT_OK(smmu_res.status_value());
  auto smmu = std::move(*smmu_res);

  constexpr uint16_t kStreamId = 0x0077;
  auto bti_res = backend.CreateBti(smmu, kStreamId);
  ASSERT_OK(bti_res.status_value());
  auto bti = std::move(*bti_res);

  auto vmo = ModelTestVmObject::CreateContiguous(0xe000'0000, 1);
  auto pmt_res = backend.BtiPin(bti, PinnedVmObject(vmo, 0, kPageSize),
                                IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE, true);
  ASSERT_OK(pmt_res.status_value());
  auto pmt = std::move(*pmt_res);
  EXPECT_FALSE(hw_model.has_hazard());

  // Allocate a dummy page table page to test hazard detection hooks directly.
  hw_model.set_assert_on_hazard(false);
  vm_page_t* page = nullptr;
  paddr_t pa = 0;
  ASSERT_OK(pmm_alloc_page(0, &page, &pa));
  auto* table_vaddr = reinterpret_cast<uint64_t*>(paddr_to_physmap(pa));
  auto free_page_guard = fit::defer([&]() {
    memset(table_vaddr, 0, kPageSize);
    arch::CleanDataCacheRange(reinterpret_cast<vaddr_t>(table_vaddr), kPageSize);
    VmPageDoublyLinkedList free_list;
    free_list.push_back(page);
    pmm_free(&free_list);
  });
  memset(table_vaddr, 0, kPageSize);
  arch::CleanDataCacheRange(reinterpret_cast<vaddr_t>(table_vaddr), kPageSize);

  // 1. Writing a valid L3 PTE with AF == 0 must trigger a hardware-writable PTE hazard.
  vmsav8_64::WritePte(&table_vaddr[0], 0xe000'0003ULL /* valid L3, AF=0 */, 3);
  EXPECT_TRUE(hw_model.has_hazard());
  hw_model.ClearHazards();

  // 2. Overwriting a valid PTE with a different valid PTE without breaking to 0
  // first must trigger a Make-Before-Break hazard.
  vmsav8_64::WritePte(&table_vaddr[0], 0, 3);
  hw_model.ClearHazards();
  vmsav8_64::WritePte(&table_vaddr[0], 0xe000'0403ULL /* valid L3, AF=1, DBM=0 */, 3);
  EXPECT_FALSE(hw_model.has_hazard());
  vmsav8_64::WritePte(&table_vaddr[0], 0xe000'1403ULL /* different valid L3, AF=1, DBM=0 */, 3);
  EXPECT_TRUE(hw_model.has_hazard());
  hw_model.ClearHazards();
  hw_model.set_assert_on_hazard(true);

  // 3. Before arch::CleanDataCacheRange, poc_dram_view still holds 0 while
  // cpu_cache_view holds the dirty PTE.
  EXPECT_EQ(0ULL, PmmMock::Get().ReadPocDram64(pa).value_or(0xffffULL));
  EXPECT_EQ(0xe000'1403ULL, PmmMock::Get().ReadCpuCache64(pa).value_or(0ULL));
  arch::CleanDataCacheRange(reinterpret_cast<vaddr_t>(table_vaddr), kPageSize);
  EXPECT_EQ(0xe000'1403ULL, PmmMock::Get().ReadPocDram64(pa).value_or(0ULL));

  memset(table_vaddr, 0, kPageSize);
  arch::CleanDataCacheRange(reinterpret_cast<vaddr_t>(table_vaddr), kPageSize);
  VmPageDoublyLinkedList free_list;
  free_list.push_back(page);
  pmm_free(&free_list);
  free_page_guard.cancel();

  backend.PmtUnpinAndDrop(pmt);
  backend.DropBti(bti);
  backend.DestroySmmu(smmu);
  EXPECT_FALSE(hw_model.has_hazard());
  EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
}

// ============================================================================
// Detector self-tests
//
// The hardware model exists to catch driver mistakes, so each of its detectors
// needs a test that makes the mistake on purpose and checks that the detector
// fires. The driver does not make these mistakes, so the tests program the
// simulated registers and write descriptors directly. They mirror the Rust
// detector self-tests in v2/test_model.rs.
// ============================================================================

class DetectorFixture {
 public:
  static constexpr uint32_t kCb = 0;
  static constexpr uint16_t kAsid = 0x5a00;

  explicit DetectorFixture(const SimSmmuHardwareModel::Config& cfg = {}) : hw_(cfg) {
    PmmMock::Get().ClearAllocationLimit();
    // The routing checks depend on the operating mode the driver was asked for.
    BootOptions::Get()->SetArmSmmuMode("enforced");
    hw_.set_assert_on_hazard(false);
  }

  ~DetectorFixture() {
    // Stop routing and translation and flush the cache first so that tearing
    // the tables down is not itself reported.
    hw_.Write32(0x000, 1);  // sCR0.CLIENTPD
    WriteCb32(0x000, 0);
    TlbiAsid();
    Sync();
    while (!pages_.empty()) {
      const paddr_t pa = pages_.back()->paddr();
      ZeroAndClean(pa);
      Free(pa);
    }
  }

  // Allocates a translation table page which is zero in both the CPU and PoC views.
  paddr_t AllocTable() {
    vm_page_t* page = nullptr;
    paddr_t pa = 0;
    ZX_ASSERT(pmm_alloc_page(0, &page, &pa) == ZX_OK);
    pages_.push_back(page);
    ZeroAndClean(pa);
    return pa;
  }

  // Writes |val| into the CPU view of |table_pa|[idx], as the driver would.
  void Write(paddr_t table_pa, size_t idx, uint64_t val, uint32_t level) {
    vmsav8_64::WritePte(&Table(table_pa)[idx], val, level);
  }

  void Clean(paddr_t table_pa) {
    arch::CleanDataCacheRange(reinterpret_cast<vaddr_t>(Table(table_pa)), kPageSize);
  }

  // Returns |table_pa| to the PMM. The page must already be zero in both views.
  void Free(paddr_t table_pa) {
    for (auto it = pages_.begin(); it != pages_.end(); ++it) {
      if ((*it)->paddr() == table_pa) {
        VmPageDoublyLinkedList free_list;
        free_list.push_back(*it);
        pages_.erase(it);
        pmm_free(&free_list);
        return;
      }
    }
    ZX_PANIC("fixture does not own page 0x%lx", table_pa);
  }

  // Programs context bank kCb for stage 1 AArch64 translation through |root_pa|.
  void EnableContext(paddr_t root_pa) {
    hw_.Write32(GR1Offset() + 4 * kCb, 1u << 16);         // CBAR.TYPE = S1 with S2 bypass
    hw_.Write32(GR1Offset() + 0x800 + 4 * kCb, 1);        // CBA2R.VA64
    WriteCb64(0x020, (uint64_t{kAsid} << 48) | root_pa);  // TTBR0
    WriteCb32(0x030, (1u << 23) | 16u);                   // TCR: EPD1, T0SZ = 16
    WriteCb32(0x000, 1);                                  // SCTLR.M
    // Route kStreamId to the context so TranslateDma() reaches it.
    hw_.Write32(0x000, 1u << 10);                // sCR0: CLIENTPD = 0, USFCFG
    RouteStream(2);                              // S2CR0, forced to unprivileged as the driver does
    hw_.Write32(0x800, (1u << 31) | kStreamId);  // SMR0: valid
  }

  // Points S2CR0 at the fixture's context with the given PRIVCFG encoding.
  void RouteStream(uint32_t privcfg) { hw_.Write32(0xc00, (privcfg << 24) | kCb); }

  void WriteCbar(uint32_t val) { hw_.Write32(GR1Offset() + 4 * kCb, val); }
  void WriteTcr(uint32_t val) { WriteCb32(0x030, val); }
  void WriteTcr2(uint32_t val) { WriteCb32(0x010, val); }
  void WriteTtbr1(uint64_t val) { WriteCb64(0x028, val); }
  void WriteSctlr(uint32_t val) { WriteCb32(0x000, val); }
  void WriteGr0(uint64_t reg, uint32_t val) { hw_.Write32(reg, val); }
  static constexpr uint16_t kStreamId = 0x0010;

  void TlbiVa(uint64_t iova) { WriteCb64(0x600, (uint64_t{kAsid} << 48) | (iova >> 12)); }
  void TlbiAsid() { WriteCb32(0x610, kAsid); }
  // Posts a "last level only" invalidation by address (TLBIVAL).
  void TlbiVaLeaf(uint64_t iova) { WriteCb64(0x620, (uint64_t{kAsid} << 48) | (iova >> 12)); }
  void WriteCbRaw32(uint64_t reg, uint32_t val) { WriteCb32(reg, val); }

  // Issues TLBSYNC and polls TLBSTATUS until the sync is complete.
  void Sync() { SyncOn(kCb); }

  // Issues TLBSYNC on context bank |cb| and polls that bank's TLBSTATUS to completion.
  void SyncOn(uint32_t cb) {
    hw_.Write32(CbOffset(cb) + 0x7f0, 0);
    while (hw_.Read32(CbOffset(cb) + 0x7f4) != 0) {
    }
  }

  // Writes TLBSYNC without waiting for it to complete.
  void SyncWithoutWaiting() { WriteCb32(0x7f0, 0); }

  // Polls TLBSTATUS until it reads idle.
  void PollSync() {
    while (hw_.Read32(CbOffset() + 0x7f4) != 0) {
    }
  }

  // Issues sTLBGSYNC and polls sTLBGSTATUS until the sync is complete.
  void GlobalSync() {
    hw_.Write32(0x070, 0);
    while (hw_.Read32(0x074) != 0) {
    }
  }

  DmaTranslationResult Translate(uint64_t iova, uint32_t perms = IOMMU_FLAG_PERM_READ) {
    return hw_.TranslateDma(kStreamId, iova, perms);
  }

  bool HasNoHazard() const { return !hw_.has_hazard(); }

  // Returns true if a hazard whose text contains |needle| was recorded, then clears all hazards.
  bool TakeHazard(const char* needle) {
    bool found = false;
    for (const std::string& h : hw_.hazards()) {
      if (h.find(needle) != std::string::npos) {
        found = true;
      }
    }
    if (!found) {
      printf("expected a hazard containing \"%s\"; %zu recorded%s%s\n", needle,
             hw_.hazards().size(), hw_.hazards().empty() ? "" : ", first: ",
             hw_.hazards().empty() ? "" : hw_.hazards().front().c_str());
    }
    hw_.ClearHazards();
    return found;
  }

  SimSmmuHardwareModel& hw() { return hw_; }

 private:
  uint64_t GR1Offset() const { return hw_.config().page_size; }
  uint64_t CbOffset(uint32_t cb = kCb) const {
    return uint64_t{hw_.config().page_size} * (hw_.config().num_pages + cb);
  }
  void WriteCb32(uint64_t reg, uint32_t val) { hw_.Write32(CbOffset() + reg, val); }
  void WriteCb64(uint64_t reg, uint64_t val) { hw_.Write64(CbOffset() + reg, val); }

  static uint64_t* Table(paddr_t table_pa) {
    return reinterpret_cast<uint64_t*>(paddr_to_physmap(table_pa));
  }

  void ZeroAndClean(paddr_t table_pa) {
    memset(Table(table_pa), 0, kPageSize);
    Clean(table_pa);
  }

  SimSmmuHardwareModel hw_;
  std::vector<vm_page_t*> pages_;
};

// Links root -> l1 -> l2 -> l3 for IOVA 0 with one valid leaf at IOVA 0, all
// visible in PoC DRAM, and enables the context.
std::array<paddr_t, 4> BuildOneMapping(DetectorFixture& f, paddr_t leaf_pa = 0x8000'0000,
                                       uint32_t perms = IOMMU_FLAG_PERM_READ) {
  std::array<paddr_t, 4> t = {f.AllocTable(), f.AllocTable(), f.AllocTable(), f.AllocTable()};
  f.Write(t[3], 0, vmsav8_64::MakePageEntry(leaf_pa, perms), 3);
  f.Clean(t[3]);
  for (int level = 2; level >= 0; --level) {
    f.Write(t[level], 0, vmsav8_64::MakeTableEntry(t[level + 1]), level);
    f.Clean(t[level]);
  }
  f.EnableContext(t[0]);
  return t;
}

// A table is unlinked and freed with no TLB invalidation while no leaf was
// ever cached under it. Only a walk cache (not a leaf-only TLB) can notice.
TEST(SmmuModelDetectorTest, TableFreedWhileInWalkCache) {
  {
    DetectorFixture f;
    const paddr_t root = f.AllocTable();
    const paddr_t l1 = f.AllocTable();
    f.Write(root, 0, vmsav8_64::MakeTableEntry(l1), 0);
    f.Clean(root);
    f.EnableContext(root);
    EXPECT_TRUE(f.HasNoHazard());

    f.Write(root, 0, 0, 0);
    f.Clean(root);
    f.Free(l1);
    EXPECT_TRUE(f.TakeHazard("Premature page-table free"));

    // The correct sequence is accepted.
    const paddr_t l1b = f.AllocTable();
    f.Write(root, 1, vmsav8_64::MakeTableEntry(l1b), 0);
    f.Clean(root);
    f.Write(root, 1, 0, 0);
    f.Clean(root);
    f.TlbiAsid();
    f.Sync();
    f.Free(l1b);
    EXPECT_TRUE(f.HasNoHazard());
  }
  EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
}

// A table descriptor that is valid in PoC DRAM points at memory which is not
// an allocated page.
TEST(SmmuModelDetectorTest, WalkReachesUnallocatedTable) {
  DetectorFixture f;
  const paddr_t root = f.AllocTable();
  f.EnableContext(root);
  f.Write(root, 0, vmsav8_64::MakeTableEntry(0x0000'7000'dead'0000), 0);
  f.Clean(root);
  EXPECT_TRUE(f.TakeHazard("unallocated table page"));
}

// A table descriptor is replaced through an invalid state, but with no TLB
// invalidation in between, so the old table is still in the walk cache when
// the new one becomes loadable.
TEST(SmmuModelDetectorTest, TableReplacedWithoutTlbi) {
  DetectorFixture f;
  const paddr_t root = f.AllocTable();
  const paddr_t l1a = f.AllocTable();
  const paddr_t l1b = f.AllocTable();
  f.Write(root, 0, vmsav8_64::MakeTableEntry(l1a), 0);
  f.Clean(root);
  f.EnableContext(root);
  f.Write(root, 0, 0, 0);
  f.Clean(root);
  EXPECT_TRUE(f.HasNoHazard());
  f.Write(root, 0, vmsav8_64::MakeTableEntry(l1b), 0);
  EXPECT_TRUE(f.TakeHazard("un-invalidated cached"));
}

// A leaf is unmapped and cleaned but never invalidated; a DMA access still
// hits the cached leaf.
TEST(SmmuModelDetectorTest, StaleLeafHit) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  EXPECT_TRUE(f.HasNoHazard());
  f.Write(t[3], 0, 0, 3);
  f.Clean(t[3]);
  EXPECT_EQ(DmaFaultType::kStaleTlbHit, f.Translate(0x40).fault);
  EXPECT_TRUE(f.TakeHazard("Stale PWC/IOTLB hit"));

  // After invalidate + sync the same access is an ordinary translation fault.
  f.TlbiVa(0);
  f.Sync();
  EXPECT_EQ(DmaFaultType::kTranslation, f.Translate(0x40).fault);
  EXPECT_TRUE(f.HasNoHazard());
}

// A leaf is written and then cleared again before it was ever cleaned to the
// PoC, and the clear is not invalidated. The CPU cache may have written the
// valid value back in between, so hardware may have cached it.
TEST(SmmuModelDetectorTest, WrittenThenClearedWithoutTlbi) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 1, vmsav8_64::MakePageEntry(0x9000'0000, IOMMU_FLAG_PERM_READ), 3);
  f.Write(t[3], 1, 0, 3);
  f.Clean(t[3]);
  EXPECT_TRUE(f.HasNoHazard());
  EXPECT_EQ(DmaFaultType::kStaleTlbHit, f.Translate(0x1040).fault);
  EXPECT_TRUE(f.TakeHazard("Stale PWC/IOTLB hit"));

  f.TlbiVa(0x1000);
  f.Sync();
  EXPECT_EQ(DmaFaultType::kTranslation, f.Translate(0x1040).fault);
  EXPECT_TRUE(f.HasNoHazard());
}

// The stream's privilege override decides which permission rules apply. An
// execute-only mapping (AP[2:1] = 0b10) is unreadable by an unprivileged access
// but readable by a privileged one.
TEST(SmmuModelDetectorTest, PrivilegeOverrideIsModelled) {
  DetectorFixture f;
  BuildOneMapping(f, 0x8000'0000, IOMMU_FLAG_PERM_EXECUTE);

  // Forced unprivileged (what the driver programs): read is denied, execute is allowed.
  EXPECT_EQ(DmaFaultType::kPermission, f.Translate(0x40, IOMMU_FLAG_PERM_READ).fault);
  EXPECT_TRUE(f.Translate(0x40, IOMMU_FLAG_PERM_EXECUTE).is_ok());
  // Left "as presented": the device may be privileged, so the read is not reliably denied.
  f.RouteStream(0);
  EXPECT_TRUE(f.Translate(0x40, IOMMU_FLAG_PERM_READ).is_ok());
  // Forced privileged: read is allowed.
  f.RouteStream(3);
  EXPECT_TRUE(f.Translate(0x40, IOMMU_FLAG_PERM_READ).is_ok());
  EXPECT_TRUE(f.HasNoHazard());
}

// A stream is routed to a context bank whose CBAR.TYPE is not a stage 1 type.
TEST(SmmuModelDetectorTest, ContextNotStage1) {
  DetectorFixture f;
  BuildOneMapping(f);
  EXPECT_TRUE(f.Translate(0x40).is_ok());
  f.WriteCbar(0);  // CBAR.TYPE = stage 2 context
  EXPECT_FALSE(f.Translate(0x40).is_ok());
  EXPECT_TRUE(f.TakeHazard("not a stage 1 context"));
}

// A context is translating with EPD1 == 0, with hardware table updates enabled,
// or with table descriptors that carry hierarchical attributes.
TEST(SmmuModelDetectorTest, UnsupportedContextConfig) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  EXPECT_TRUE(f.hw().VerifyQuiescent());

  f.WriteTcr(16);  // T0SZ = 16, EPD1 = 0
  EXPECT_FALSE(f.hw().VerifyQuiescent());
  EXPECT_TRUE(f.TakeHazard("EPD1 == 0"));
  f.WriteTcr((1u << 23) | 16u);

  f.WriteTcr2(1u << 10);  // HA
  f.hw().ClearHazards();
  EXPECT_FALSE(f.hw().VerifyQuiescent());
  EXPECT_TRUE(f.TakeHazard("hardware translation table updates enabled"));
  f.WriteTcr2(0);

  const paddr_t l1 = f.AllocTable();
  f.Write(t[0], 1, vmsav8_64::MakeTableEntry(l1) | (1ULL << 61), 0);
  EXPECT_TRUE(f.TakeHazard("hierarchical attribute"));
  f.Write(t[0], 1, 0, 0);
  f.Clean(t[0]);
  f.TlbiAsid();
  f.Sync();
  f.hw().ClearHazards();
}

// With output address size enforcement switched on, a page whose output
// address does not fit the configured size is an address size fault.
TEST(SmmuModelDetectorTest, OutputAddressSize) {
  SimSmmuHardwareModel::Config cfg;
  cfg.enforce_tcr2_fields = true;
  DetectorFixture f(cfg);
  BuildOneMapping(f, 0x1'0000'0000);
  f.WriteTcr2(0);  // PASize = 32 bits
  EXPECT_EQ(DmaFaultType::kAddressSize, f.Translate(0x40).fault);
  f.WriteTcr2(5);  // PASize = 48 bits
  EXPECT_TRUE(f.Translate(0x40).is_ok());
}

// TCR.A1 selects the ASID from TTBR1. A driver which leaves it set while
// giving every context the same TTBR1 ASID makes all contexts share one ASID,
// and its invalidations (which name the TTBR0 ASID) remove nothing.
TEST(SmmuModelDetectorTest, AsidSelectedFromTtbr1) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  // Move the context onto the TTBR1 ASID. Nothing was invalidated, so the
  // entries loaded under the old ASID are now stale.
  f.WriteTtbr1(uint64_t{0xffff} << 48);
  f.WriteTcr((1u << 23) | (1u << 22) | 16u);  // EPD1, A1, T0SZ
  EXPECT_FALSE(f.hw().VerifyQuiescent());
  EXPECT_TRUE(f.TakeHazard("Stale cached descriptor"));

  // An unmap which invalidates by the TTBR0 ASID does not reach the entry
  // tagged 0xffff.
  f.Write(t[3], 0, 0, 3);
  f.Clean(t[3]);
  f.TlbiVa(0);
  f.Sync();
  EXPECT_EQ(DmaFaultType::kStaleTlbHit, f.Translate(0x40).fault);
  EXPECT_TRUE(f.TakeHazard("Stale PWC/IOTLB hit"));
}

// With TCR2 enforcement on and TCR2.AS == 0, ASIDs are 8 bits wide: an
// invalidation which names a 16-bit ASID is malformed.
TEST(SmmuModelDetectorTest, AsidWidth) {
  SimSmmuHardwareModel::Config cfg;
  cfg.enforce_tcr2_fields = true;
  DetectorFixture f(cfg);
  const paddr_t root = f.AllocTable();
  f.EnableContext(root);
  f.WriteTcr2(5);  // PASize = 48 bits, AS = 0
  f.TlbiAsid();
  f.Sync();
  EXPECT_TRUE(f.TakeHazard("upper 8 bits must be written as 0"));
  f.WriteTcr2((1u << 4) | 5u);  // AS = 1
  f.TlbiAsid();
  f.Sync();
  EXPECT_TRUE(f.HasNoHazard());
}

// Routing states which exist only between two register writes, or which let a
// stream bypass translation in enforced mode.
TEST(SmmuModelDetectorTest, TransientMisrouting) {
  DetectorFixture f;
  BuildOneMapping(f);
  EXPECT_TRUE(f.HasNoHazard());

  // A second match register is made valid while its S2CR still holds a bypass type.
  f.WriteGr0(0xc04, 1u << 16);           // S2CR1: TYPE = bypass (stale)
  f.WriteGr0(0x804, (1u << 31) | 0x20);  // SMR1: valid, stream 0x20
  EXPECT_TRUE(f.TakeHazard("bypass-type S2CR"));
  f.WriteGr0(0x804, 0);
  f.hw().ClearHazards();

  // Two valid match registers match the same stream.
  f.WriteGr0(0xc04, (2u << 24) | DetectorFixture::kCb);
  f.WriteGr0(0x804, (1u << 31) | DetectorFixture::kStreamId);
  EXPECT_TRUE(f.TakeHazard("match a common stream ID"));
  f.WriteGr0(0x804, 0);
  f.hw().ClearHazards();

  // The context's MMU is switched off while a stream is still routed to it.
  f.WriteSctlr(0);
  EXPECT_TRUE(f.TakeHazard("SCTLR.M is 0"));
  f.WriteSctlr(1);
  f.hw().ClearHazards();

  // Unmatched streams are allowed to bypass.
  f.WriteGr0(0x000, 0);
  EXPECT_TRUE(f.TakeHazard("USFCFG == 0"));
  f.WriteGr0(0x000, 1u << 10);
  f.hw().ClearHazards();
}

// The driver writes a register the model does not implement, or writes half of
// a 64-bit register.
TEST(SmmuModelDetectorTest, UnmodelledRegisterAccess) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.WriteCbRaw32(0x008, 1);  // RESUME: not modelled
  EXPECT_TRUE(f.TakeHazard("unmodelled context bank register"));
  f.WriteCbRaw32(0x020, static_cast<uint32_t>(t[0]));  // low half of TTBR0
  EXPECT_TRUE(f.TakeHazard("32-bit write to 64-bit"));
  f.WriteGr0(0x064, 0);  // TLBIVMID: not modelled
  EXPECT_TRUE(f.TakeHazard("unmodelled GR0 register"));
  f.WriteGr0(0x800 + 4 * 100, 1u << 31);  // SMR100, beyond NUMSMRG
  EXPECT_TRUE(f.TakeHazard("beyond NUMSMRG"));
}

// A table is unlinked after a "last level only" invalidation. That discards
// the leaf entry but not the walk-cache entry which points at the table, so
// freeing the table is premature.
TEST(SmmuModelDetectorTest, LeafOnlyInvalidationKeepsWalkCache) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 0, 0, 3);
  f.Clean(t[3]);
  f.Write(t[2], 0, 0, 2);
  f.Clean(t[2]);
  f.TlbiVaLeaf(0);
  f.Sync();
  // The walk still goes through the cached level 2 descriptor.
  EXPECT_EQ(DmaFaultType::kStaleTlbHit, f.Translate(0x40).fault);
  EXPECT_TRUE(f.TakeHazard("Stale PWC/IOTLB hit"));
  f.Free(t[3]);
  EXPECT_TRUE(f.TakeHazard("Premature page-table free"));
}

// The root table of a translating context is returned to the PMM.
TEST(SmmuModelDetectorTest, FreeOfActiveRoot) {
  DetectorFixture f;
  const paddr_t root = f.AllocTable();
  f.EnableContext(root);
  f.Free(root);
  EXPECT_TRUE(f.TakeHazard("still set as active TTBR0"));
}

// A stream is routed to a context bank which still has a posted, unsynced
// invalidation.
TEST(SmmuModelDetectorTest, StreamEnabledWithUnsyncedTlbi) {
  DetectorFixture f;
  BuildOneMapping(f);
  f.TlbiAsid();
  f.WriteGr0(0xc04, (2u << 24) | DetectorFixture::kCb);  // S2CR1: translation, same context
  f.WriteGr0(0x804, (1u << 31) | 0x20);                  // SMR1: valid, stream 0x20
  EXPECT_TRUE(f.TakeHazard("unsynchronized TLBI"));
  f.WriteGr0(0x804, 0);
  f.Sync();
}

// Descriptors the driver must never write: a hardware-writable leaf (DBM), a
// non-zero invalid descriptor, a block at an intermediate level, and a level 3
// descriptor without the page bit.
TEST(SmmuModelDetectorTest, MalformedDescriptors) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 1, vmsav8_64::MakePageEntry(0x9000'0000, IOMMU_FLAG_PERM_READ) | (1ULL << 51), 3);
  EXPECT_TRUE(f.TakeHazard("Hardware-writable L3 PTE hazard"));
  f.Write(t[3], 1, 0, 3);
  f.hw().ClearHazards();

  f.Write(t[3], 2, 0x9000'0000, 3);
  EXPECT_TRUE(f.TakeHazard("non-zero invalid descriptor"));
  f.Write(t[3], 2, 0, 3);

  f.Write(t[1], 1, 0x4000'0000 | (1u << 10) | 1u, 1);  // valid, bit 1 clear: a block
  EXPECT_TRUE(f.TakeHazard("non-table valid descriptor"));
  f.Write(t[1], 1, 0, 1);
  f.hw().ClearHazards();

  f.Write(t[3], 3, 0x9000'0000 | (1u << 10) | 1u, 3);  // valid, bit 1 clear at level 3
  EXPECT_TRUE(f.TakeHazard("invalid L3 page descriptor"));
  f.Write(t[3], 3, 0, 3);
  f.Clean(t[1]);
  f.Clean(t[3]);
  f.TlbiAsid();
  f.Sync();
  f.hw().ClearHazards();
}

// A register access beyond the end of the implemented context banks.
TEST(SmmuModelDetectorTest, OutOfRangeMmio) {
  DetectorFixture f;
  const uint64_t beyond = uint64_t{4096} * (16 + 64);  // context bank 64; only 8 are implemented
  f.hw().Write32(beyond, 1);
  EXPECT_TRUE(f.TakeHazard("out-of-range MMIO"));
  (void)f.hw().Read32(beyond);
  EXPECT_TRUE(f.TakeHazard("out-of-range MMIO"));
}

// A context fault is delivered only when SCTLR.CFIE is set and the vector is
// unmasked, it is delivered later if it was recorded while disabled, and a
// handler which leaves it asserted is reported.
TEST(SmmuModelDetectorTest, InterruptDelivery) {
  static int handler_runs = 0;
  handler_runs = 0;
  InterruptMock::Get().Reset();
  DetectorFixture f;
  BuildOneMapping(f);
  const uint32_t vector = f.hw().config().base_context_irq_num + DetectorFixture::kCb;
  // A fault handler which does nothing about the fault.
  ASSERT_OK(register_int_handler(vector, interrupt_handler_t(+[](void*) { ++handler_runs; })));

  // A translation fault with the interrupt disabled and masked: recorded, not delivered.
  SimSmmuHardwareModel::ServicedInterrupts serviced;
  EXPECT_EQ(DmaFaultType::kTranslation, f.hw()
                                            .DeviceAccess(DetectorFixture::kStreamId, 0x10'0000,
                                                          IOMMU_FLAG_PERM_READ, &serviced)
                                            .fault);
  EXPECT_TRUE(serviced.contexts.empty());
  EXPECT_EQ(1u << 1, f.hw().cb_fsr(DetectorFixture::kCb));
  EXPECT_EQ(0, handler_runs);

  // Enabled but still masked: not delivered.
  f.WriteSctlr((1u << 6) | 1u);
  EXPECT_TRUE(f.hw().ServicePendingInterrupts().contexts.empty());

  // Unmasked: the fault recorded earlier is delivered now. The handler does
  // not clear it, so it would fire again at once.
  ASSERT_OK(unmask_interrupt(vector));
  EXPECT_TRUE(f.hw().ServicePendingInterrupts().ran_context(DetectorFixture::kCb));
  EXPECT_EQ(1, handler_runs);
  EXPECT_TRUE(f.TakeHazard("still asserted"));

  // A second fault while one is recorded sets MULTI and keeps the first syndrome.
  f.hw().DeviceAccess(DetectorFixture::kStreamId, 0x20'0000, IOMMU_FLAG_PERM_READ);
  EXPECT_EQ((1u << 31) | (1u << 1), f.hw().cb_fsr(DetectorFixture::kCb));
  f.hw().ClearHazards();

  // Clearing the status (write one to clear) deasserts it.
  f.WriteCbRaw32(0x058, ~0u);
  EXPECT_TRUE(f.hw().ServicePendingInterrupts().contexts.empty());
  ASSERT_OK(mask_interrupt(vector));
  EXPECT_TRUE(f.HasNoHazard());
  InterruptMock::Get().Reset();
}

// The abstract specification and the hardware model disagree about a mapping.
TEST(SmmuModelDetectorTest, SpecMismatchIsDetected) {
  DetectorFixture f;
  BuildOneMapping(f);
  AbstractIommuSpec spec(ArmSmmuMode::kEnforced);
  spec.AddBti(DetectorFixture::kStreamId);
  spec.AddMapping(DetectorFixture::kStreamId, 0, {0x8000'0000}, IOMMU_FLAG_PERM_READ);
  EXPECT_TRUE(spec.VerifyAllMappings(f.hw()));

  // The specification expects a second page which the hardware does not map.
  spec.AddMapping(DetectorFixture::kStreamId, 0x1000, {0x9000'0000}, IOMMU_FLAG_PERM_READ);
  EXPECT_FALSE(spec.VerifyAllMappings(f.hw()));
  spec.RemoveMapping(DetectorFixture::kStreamId, 0x1000, 1);

  // The specification expects different memory behind the mapped page.
  spec.RemoveMapping(DetectorFixture::kStreamId, 0, 1);
  spec.AddMapping(DetectorFixture::kStreamId, 0, {0xa000'0000}, IOMMU_FLAG_PERM_READ);
  EXPECT_FALSE(spec.VerifyAllMappings(f.hw()));
}

// A table is zero-filled in bulk rather than descriptor by descriptor. The
// model sees one clear for each descriptor which was not already zero.
TEST(SmmuModelDetectorTest, BulkZeroIsObserved) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 7, vmsav8_64::MakePageEntry(0x9000'0000, IOMMU_FLAG_PERM_READ), 3);
  f.Write(t[3], 9, vmsav8_64::MakePageEntry(0xa000'0000, IOMMU_FLAG_PERM_READ), 3);
  const size_t before = f.hw().descriptor_write_count();
  vmsav8_64::ZeroTablePage(paddr_to_physmap(t[3]));
  // Three descriptors were valid: index 0 (from the fixture), 7 and 9.
  EXPECT_EQ(before + 3, f.hw().descriptor_write_count());
  EXPECT_TRUE(f.HasNoHazard());
  f.Clean(t[3]);
  f.TlbiAsid();
  f.Sync();
  EXPECT_TRUE(f.hw().VerifyQuiescent());
}

// The driver passes the wrong level for the table it is writing. The model
// derives the level from how the table is linked and reports the disagreement.
TEST(SmmuModelDetectorTest, WrongLevelClaimed) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 5, vmsav8_64::MakePageEntry(0x9000'0000, IOMMU_FLAG_PERM_READ), 2);
  EXPECT_TRUE(f.TakeHazard("claims level 2"));
  f.Write(t[3], 5, 0, 3);
  f.Clean(t[3]);
  f.TlbiVa(0x5000);
  f.Sync();
  EXPECT_TRUE(f.hw().VerifyQuiescent());
}

// An invalidation is posted in one context bank and synced in a different
// one. A context bank sync covers only its own bank (IHI 0062D.c section
// 5.4.2), so the entry must survive. A global sync covers every bank.
TEST(SmmuModelDetectorTest, SyncOnWrongContextBank) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 0, 0, 3);
  f.Clean(t[3]);
  f.TlbiVa(0);
  f.SyncOn(DetectorFixture::kCb + 1);
  EXPECT_EQ(DmaFaultType::kStaleTlbHit, f.Translate(0x40).fault);
  EXPECT_TRUE(f.TakeHazard("Stale PWC/IOTLB hit"));

  f.GlobalSync();
  EXPECT_EQ(DmaFaultType::kTranslation, f.Translate(0x40).fault);
  EXPECT_TRUE(f.hw().VerifyQuiescent());
}

// A sync is requested but its completion is never awaited before the
// operation continues.
TEST(SmmuModelDetectorTest, SyncNotAwaited) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  const paddr_t spare = f.AllocTable();
  f.Write(t[3], 0, 0, 3);
  f.Clean(t[3]);
  f.TlbiVa(0);
  f.SyncWithoutWaiting();

  // The entry is still cached, a free is reported, and the operation is not quiescent.
  EXPECT_EQ(DmaFaultType::kStaleTlbHit, f.Translate(0x40).fault);
  EXPECT_TRUE(f.TakeHazard("Stale PWC/IOTLB hit"));
  f.Free(spare);
  EXPECT_TRUE(f.TakeHazard("sync is still in progress"));
  EXPECT_FALSE(f.hw().VerifyQuiescent());
  EXPECT_TRUE(f.TakeHazard("not synced to completion"));

  // Polling the status register to idle completes it.
  f.PollSync();
  EXPECT_TRUE(f.hw().VerifyQuiescent());
}

// A leaf is unmapped and cleaned, and the operation "returns" with the
// invalidation missing, or posted but not synced. Nothing ever touches that
// address again.
TEST(SmmuModelDetectorTest, StaleEntryAtQuiescence) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  EXPECT_TRUE(f.hw().VerifyQuiescent());
  f.Write(t[3], 0, 0, 3);
  f.Clean(t[3]);
  EXPECT_FALSE(f.hw().VerifyQuiescent());
  EXPECT_TRUE(f.TakeHazard("Stale cached descriptor"));

  f.TlbiVa(0);
  EXPECT_FALSE(f.hw().VerifyQuiescent());
  EXPECT_TRUE(f.TakeHazard("not synced to completion"));

  f.Sync();
  EXPECT_TRUE(f.hw().VerifyQuiescent());
}

// A leaf is cleared in the CPU view and invalidated, but the clear is never
// cleaned to the PoC, so the sync reloads the old descriptor.
TEST(SmmuModelDetectorTest, MissingCleanBeforeSync) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 0, 0, 3);
  // No clean of t[3] here.
  f.TlbiVa(0);
  f.Sync();
  EXPECT_TRUE(f.Translate(0x40).is_ok());
  // The driver believes the page is unmapped; hardware still translates it.
  // Remapping the slot exposes the conflict.
  f.Write(t[3], 0, vmsav8_64::MakePageEntry(0x9000'0000, IOMMU_FLAG_PERM_READ), 3);
  EXPECT_TRUE(f.TakeHazard("Make-Before-Break"));
}

}  // namespace
}  // namespace testing
}  // namespace arm_smmu
