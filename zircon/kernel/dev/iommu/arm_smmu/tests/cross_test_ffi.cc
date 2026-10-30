// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// C++ side of the outer cross-test FFI boundary used during Checkpoints 1–3
// (`SMMUv2_rust_port_plan.md` §3) to drive the C++ SMMUv2 implementation from
// the Rust test harness (`Rust -> C++`).

#include "cross_test_ffi.h"

extern "C" {

zx_status_t cpp_smmu_create(const zbi_dcfg_arm_smmu_driver_t* config, iommu::Iommu** out_smmu) {
  return arm_smmu::SmmuTestAccess::CreateCppSmmu(config, out_smmu);
}

zx_status_t cpp_smmu_fetch(const uint8_t* desc, size_t desc_len, iommu::Iommu** out_smmu) {
  return arm_smmu::SmmuTestAccess::FetchCppSmmu(desc, desc_len, out_smmu);
}

void cpp_smmu_destroy(iommu::Iommu* smmu) { arm_smmu::SmmuTestAccess::DestroyCppSmmu(smmu); }

void cpp_smmu_release(iommu::Iommu* smmu) { arm_smmu::SmmuTestAccess::ReleaseCppSmmu(smmu); }

zx_status_t cpp_smmu_create_bti(iommu::Iommu* smmu, uint64_t bti_id, iommu::Bti** out_bti) {
  return arm_smmu::SmmuTestAccess::CreateCppBti(smmu, bti_id, out_bti);
}

void cpp_smmu_bti_on_zero_handles(iommu::Bti* bti) {
  arm_smmu::SmmuTestAccess::OnZeroHandlesCppBti(bti);
}

void cpp_smmu_bti_release(iommu::Bti* bti) { arm_smmu::SmmuTestAccess::ReleaseCppBti(bti); }

void cpp_smmu_bti_release_quarantine(iommu::Bti* bti) {
  arm_smmu::SmmuTestAccess::ReleaseQuarantineCppBti(bti);
}

bool cpp_smmu_bti_in_fault_state(const iommu::Bti* bti) {
  return arm_smmu::SmmuTestAccess::InFaultStateCppBti(bti);
}

size_t cpp_smmu_bti_quarantine_count(const iommu::Bti* bti) {
  return arm_smmu::SmmuTestAccess::QuarantineCountCppBti(bti);
}

uint64_t cpp_smmu_bti_pmo_count(const iommu::Bti* bti) {
  return arm_smmu::SmmuTestAccess::PmoCountCppBti(bti);
}

uint64_t cpp_smmu_bti_minimum_contiguity(const iommu::Bti* bti) {
  return arm_smmu::SmmuTestAccess::MinimumContiguityCppBti(bti);
}

uint64_t cpp_smmu_bti_aspace_size(const iommu::Bti* bti) {
  return arm_smmu::SmmuTestAccess::AspaceSizeCppBti(bti);
}

uint64_t cpp_smmu_bti_id(const iommu::Bti* bti) {
  return arm_smmu::SmmuTestAccess::BtiIdCppBti(bti);
}

void cpp_smmu_bti_get_name(const iommu::Bti* bti, char* out_name) {
  arm_smmu::SmmuTestAccess::GetNameCppBti(bti, out_name);
}

void cpp_smmu_bti_set_name(iommu::Bti* bti, const char* name, size_t name_len) {
  arm_smmu::SmmuTestAccess::SetNameCppBti(bti, name, name_len);
}

zx_status_t cpp_smmu_bti_map(iommu::Bti* bti, PinnedVmObject* pinned_vmo, uint32_t perms,
                             bool require_contiguous, iommu::Pmt** out_pmt) {
  return arm_smmu::SmmuTestAccess::MapCppBti(bti, pinned_vmo, perms, require_contiguous, out_pmt);
}

zx_status_t cpp_smmu_pmt_query_address(iommu::Pmt* pmt, uint64_t offset, size_t size,
                                       uint64_t* out_device_vaddr, size_t* out_size) {
  return arm_smmu::SmmuTestAccess::QueryAddressCppPmt(pmt, offset, size, out_device_vaddr,
                                                      out_size);
}

void cpp_smmu_pmt_release_pinned_memory(iommu::Pmt* pmt) {
  arm_smmu::SmmuTestAccess::ReleasePinnedMemoryCppPmt(pmt);
}

void cpp_smmu_pmt_on_zero_handles(iommu::Pmt* pmt) {
  arm_smmu::SmmuTestAccess::OnZeroHandlesCppPmt(pmt);
}

void cpp_smmu_pmt_release(iommu::Pmt* pmt) { arm_smmu::SmmuTestAccess::ReleaseCppPmt(pmt); }

bool cpp_smmu_validate_mode_string(const char* mode_str) {
  return arm_smmu::SmmuTestAccess::ValidateCppModeString(mode_str);
}

bool cpp_smmu_get_mode(const char* mode_str, bool has_base_addr, uint64_t base_addr,
                       uint32_t* out_mode) {
  return arm_smmu::SmmuTestAccess::GetCppModeString(mode_str, has_base_addr, base_addr, out_mode);
}

}  // extern "C"
