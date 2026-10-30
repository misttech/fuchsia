// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// Temporary Phase 2–3 cross-test FFI header (`SMMUv2_rust_port_plan.md` §3) declaring the
// `cpp_*` and `rust_*` C ABI shims and dual-backend wrapper classes used to run the host unit
// and model test suites across both C++ and Rust implementations.

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_CROSS_TEST_FFI_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_CROSS_TEST_FFI_H_

#include <lib/zbi-format/driver-config.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <zircon/syscalls/iommu.h>
#include <zircon/types.h>

#include <chrono>
#include <random>

#include <dev/arm_smmu/device_aspace.h>
#include <dev/arm_smmu/page_cache.h>
#include <dev/arm_smmu/smmu.h>
#include <dev/arm_smmu/smmu_bti.h>
#include <dev/arm_smmu/smmu_mode.h>
#include <dev/arm_smmu/smmu_pmt.h>
#include <dev/iommu/bti.h>
#include <dev/iommu/iommu.h>
#include <dev/iommu/pmt.h>
#include <fbl/ref_ptr.h>
#include <region-alloc/region-alloc.h>
#include <vm/pinned_vm_object.h>

struct RustSmmu;
struct RustSmmuBti;
struct RustSmmuPmt;
struct RustPageCache;
struct RustDeviceAspace;
struct RustDeviceAspaceAlloc;

static_assert(sizeof(zbi_dcfg_arm_smmu_irq_t) == 8);
static_assert(alignof(zbi_dcfg_arm_smmu_irq_t) == 4);
static_assert(sizeof(zbi_dcfg_arm_smmu_driver_t) == 1120);
static_assert(alignof(zbi_dcfg_arm_smmu_driver_t) == 8);
static_assert(sizeof(zx_iommu_desc_arm_smmu_t) == 8);
static_assert(alignof(zx_iommu_desc_arm_smmu_t) == 8);

namespace arm_smmu::testing {

// Returns the seed for a randomized model sequence which should differ from
// run to run: the value of the ARM_SMMU_MODEL_SEED environment variable if it
// is set (to replay a run), otherwise a fresh value. Callers print the seed.
inline uint64_t FreshModelSeed() {
  if (const char* env = getenv("ARM_SMMU_MODEL_SEED"); env != nullptr && env[0] != '\0') {
    return strtoull(env, nullptr, 0);
  }
  std::random_device entropy;
  const uint64_t now =
      static_cast<uint64_t>(std::chrono::steady_clock::now().time_since_epoch().count());
  return (uint64_t{entropy()} << 32) ^ uint64_t { entropy() } ^ now;
}

}  // namespace arm_smmu::testing

namespace arm_smmu {

enum class BackendKind {
  kCpp,
  kRust,
};

inline const char* BackendTag(BackendKind kind) {
  switch (kind) {
    case BackendKind::kCpp:
      return "C++->C++";
    case BackendKind::kRust:
      return "C++->Rust";
  }
  return "Unknown";
}

uint64_t GetCppDeviceAspaceRootTtPaddr(const DeviceAspace* aspace);
zx_status_t TestCppDeviceAspaceRegionContainedBy(const DeviceAspace* aspace, uint64_t base,
                                                 uint64_t size, bool is_allocated_set,
                                                 bool* out_contained);

// Friend helper to cleanly tear down an Smmu instance between host tests
// (removing it from the static Smmu::instances_ list while verifying destructor
// invariants) and bridge C++ Smmu/SmmuBti/SmmuPmt/PageCache/DeviceAspace
// operations to the outer cross-test FFI boundary.
class SmmuTestAccess {
 public:
  static void DestroySmmu(fbl::RefPtr<iommu::Iommu>&& iommu);
  static bool IsRegisteredSmmu(const iommu::Iommu* iommu);

  static zx_status_t CreateCppSmmu(const zbi_dcfg_arm_smmu_driver_t* config,
                                   iommu::Iommu** out_smmu);
  static zx_status_t FetchCppSmmu(const uint8_t* desc, size_t desc_len, iommu::Iommu** out_smmu);
  static void DestroyCppSmmu(iommu::Iommu* smmu);
  static void ReleaseCppSmmu(iommu::Iommu* smmu);

  static zx_status_t CreateCppBti(iommu::Iommu* smmu, uint64_t bti_id, iommu::Bti** out_bti);
  static void OnZeroHandlesCppBti(iommu::Bti* bti);
  static void ReleaseCppBti(iommu::Bti* bti);
  static void ReleaseQuarantineCppBti(iommu::Bti* bti);
  static bool InFaultStateCppBti(const iommu::Bti* bti);
  static size_t QuarantineCountCppBti(const iommu::Bti* bti);
  static uint64_t PmoCountCppBti(const iommu::Bti* bti);
  static uint64_t MinimumContiguityCppBti(const iommu::Bti* bti);
  static uint64_t AspaceSizeCppBti(const iommu::Bti* bti);
  static uint64_t BtiIdCppBti(const iommu::Bti* bti);
  static void GetNameCppBti(const iommu::Bti* bti, char* out_name);
  static void SetNameCppBti(iommu::Bti* bti, const char* name, size_t name_len);
  static zx_status_t MapCppBti(iommu::Bti* bti, PinnedVmObject* pinned_vmo, uint32_t perms,
                               bool require_contiguous, iommu::Pmt** out_pmt);

  static zx_status_t QueryAddressCppPmt(iommu::Pmt* pmt, uint64_t offset, size_t size,
                                        uint64_t* out_device_vaddr, size_t* out_size);
  static void ReleasePinnedMemoryCppPmt(iommu::Pmt* pmt);
  static void OnZeroHandlesCppPmt(iommu::Pmt* pmt);
  static void ReleaseCppPmt(iommu::Pmt* pmt);

  static bool ValidateCppModeString(const char* mode_str);
  static bool GetCppModeString(const char* mode_str, bool has_base_addr, uint64_t base_addr,
                               uint32_t* out_mode);
};

}  // namespace arm_smmu

extern "C" {

// Rust SMMUv2 implementation entry points called by C++ cross-tests (`C++ -> Rust`).
zx_status_t rust_smmu_create(const zbi_dcfg_arm_smmu_driver_t* config, const RustSmmu** out_smmu);
zx_status_t rust_smmu_fetch(const uint8_t* desc, size_t desc_len, const RustSmmu** out_smmu);
void rust_smmu_destroy(const RustSmmu* smmu);
void rust_smmu_release(const RustSmmu* smmu);

zx_status_t rust_smmu_create_bti(const RustSmmu* smmu, uint64_t bti_id,
                                 const RustSmmuBti** out_bti);
void rust_smmu_bti_on_zero_handles(const RustSmmuBti* bti);
void rust_smmu_bti_release(const RustSmmuBti* bti);
void rust_smmu_bti_release_quarantine(const RustSmmuBti* bti);
bool rust_smmu_bti_in_fault_state(const RustSmmuBti* bti);
size_t rust_smmu_bti_quarantine_count(const RustSmmuBti* bti);
uint64_t rust_smmu_bti_pmo_count(const RustSmmuBti* bti);
uint64_t rust_smmu_bti_minimum_contiguity(const RustSmmuBti* bti);
uint64_t rust_smmu_bti_aspace_size(const RustSmmuBti* bti);
uint64_t rust_smmu_bti_id(const RustSmmuBti* bti);
void rust_smmu_bti_get_name(const RustSmmuBti* bti, char* out_name);
void rust_smmu_bti_set_name(const RustSmmuBti* bti, const char* name, size_t name_len);
zx_status_t rust_smmu_bti_map(const RustSmmuBti* bti, PinnedVmObject* pinned_vmo, uint32_t perms,
                              bool require_contiguous, const RustSmmuPmt** out_pmt);

zx_status_t rust_smmu_pmt_query_address(const RustSmmuPmt* pmt, uint64_t offset, size_t size,
                                        uint64_t* out_device_vaddr, size_t* out_size);
void rust_smmu_pmt_release_pinned_memory(const RustSmmuPmt* pmt);
void rust_smmu_pmt_on_zero_handles(const RustSmmuPmt* pmt);
void rust_smmu_pmt_release(const RustSmmuPmt* pmt);

bool rust_smmu_validate_mode_string(const char* mode_str);
bool rust_smmu_get_mode(const char* mode_str, bool has_base_addr, uint64_t base_addr,
                        uint32_t* out_mode);

RustPageCache* rust_page_cache_create();
void rust_page_cache_destroy(RustPageCache* cache);
zx_status_t rust_page_cache_get_page(RustPageCache* cache, vm_page_t** out_page);
void rust_page_cache_return_page(RustPageCache* cache, vm_page_t* page);
void rust_page_cache_trim(RustPageCache* cache, size_t max_cached);
size_t rust_page_cache_in_flight_pages(const RustPageCache* cache);
size_t rust_page_cache_cache_entries(const RustPageCache* cache);

// Note: All allocations returned by `rust_device_aspace_map` must be unmapped or dropped via
// `rust_device_aspace_drop_allocation` before `rust_device_aspace_destroy` is called, and
// `tlb_cbk` must not reentrantly access `aspace`.
zx_status_t rust_device_aspace_create(uint64_t aspace_start, uint64_t aspace_len,
                                      uint32_t max_cache_pages, RustDeviceAspace** out_aspace);
void rust_device_aspace_destroy(RustDeviceAspace* aspace);
zx_status_t rust_device_aspace_map(RustDeviceAspace* aspace, const PinnedVmObject* pinned_vmo,
                                   uint32_t perms, void (*tlb_cbk)(void*, uint64_t, uint64_t),
                                   void* tlb_ctx, bool has_location, uint64_t location,
                                   RustDeviceAspaceAlloc** out_alloc, uint64_t* out_base,
                                   uint64_t* out_size);
void rust_device_aspace_unmap(RustDeviceAspace* aspace, RustDeviceAspaceAlloc* alloc,
                              void (*tlb_cbk)(void*, uint64_t, uint64_t), void* tlb_ctx);
void rust_device_aspace_free_translation_tables(RustDeviceAspace* aspace,
                                                void (*tlb_cbk)(void*, uint64_t, uint64_t),
                                                void* tlb_ctx);
void rust_device_aspace_drop_allocation(RustDeviceAspaceAlloc* alloc);
uint64_t rust_device_aspace_aspace_start(const RustDeviceAspace* aspace);
uint64_t rust_device_aspace_aspace_len(const RustDeviceAspace* aspace);
size_t rust_device_aspace_cache_entries(const RustDeviceAspace* aspace);
uint64_t rust_device_aspace_root_tt_paddr(const RustDeviceAspace* aspace);
zx_status_t rust_device_aspace_test_region_contained_by(const RustDeviceAspace* aspace,
                                                        uint64_t base, uint64_t size,
                                                        bool is_allocated_set, bool* out_contained);

// C++ SMMUv2 implementation entry points called by Rust cross-tests (`Rust -> C++`).
zx_status_t cpp_smmu_create(const zbi_dcfg_arm_smmu_driver_t* config, iommu::Iommu** out_smmu);
zx_status_t cpp_smmu_fetch(const uint8_t* desc, size_t desc_len, iommu::Iommu** out_smmu);
void cpp_smmu_destroy(iommu::Iommu* smmu);
void cpp_smmu_release(iommu::Iommu* smmu);

zx_status_t cpp_smmu_create_bti(iommu::Iommu* smmu, uint64_t bti_id, iommu::Bti** out_bti);
void cpp_smmu_bti_on_zero_handles(iommu::Bti* bti);
void cpp_smmu_bti_release(iommu::Bti* bti);
void cpp_smmu_bti_release_quarantine(iommu::Bti* bti);
bool cpp_smmu_bti_in_fault_state(const iommu::Bti* bti);
size_t cpp_smmu_bti_quarantine_count(const iommu::Bti* bti);
uint64_t cpp_smmu_bti_pmo_count(const iommu::Bti* bti);
uint64_t cpp_smmu_bti_minimum_contiguity(const iommu::Bti* bti);
uint64_t cpp_smmu_bti_aspace_size(const iommu::Bti* bti);
uint64_t cpp_smmu_bti_id(const iommu::Bti* bti);
void cpp_smmu_bti_get_name(const iommu::Bti* bti, char* out_name);
void cpp_smmu_bti_set_name(iommu::Bti* bti, const char* name, size_t name_len);
zx_status_t cpp_smmu_bti_map(iommu::Bti* bti, PinnedVmObject* pinned_vmo, uint32_t perms,
                             bool require_contiguous, iommu::Pmt** out_pmt);

zx_status_t cpp_smmu_pmt_query_address(iommu::Pmt* pmt, uint64_t offset, size_t size,
                                       uint64_t* out_device_vaddr, size_t* out_size);
void cpp_smmu_pmt_release_pinned_memory(iommu::Pmt* pmt);
void cpp_smmu_pmt_on_zero_handles(iommu::Pmt* pmt);
void cpp_smmu_pmt_release(iommu::Pmt* pmt);

bool cpp_smmu_validate_mode_string(const char* mode_str);
bool cpp_smmu_get_mode(const char* mode_str, bool has_base_addr, uint64_t base_addr,
                       uint32_t* out_mode);

// Native Rust (`Rust -> Rust`) component unit test entry points.
bool rust_arm_smmu_common_smoke_test();

}  // extern "C"

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_CROSS_TEST_FFI_H_
