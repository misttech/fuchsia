// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_ASPACE_CONSTANTS_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_ASPACE_CONSTANTS_H_

#include <stddef.h>
#include <stdint.h>

#include <arch/arm64/mmu.h>

// Size and alignment of the Rust `ArmArchVmAspace` state that C++ stores inline
// in an `OpaqueStorage`. The size is an upper bound verified at compile time on
// the Rust side via `zr::static_assert_size_and_align!`.
//
// The Rust struct contains a `ksync::KMutex<ksync::RawCriticalMutex>`, whose raw
// lock grows when lockdep, lock name tracing, or spinlock tracing are enabled.
#if WITH_LOCK_DEP
constexpr size_t kArm64ArchVmAspaceStateSize = 184;
#else
constexpr size_t kArm64ArchVmAspaceStateSize = 168;
#endif
constexpr size_t kArm64ArchVmAspaceStateAlign = 8;

constexpr size_t kArm64VmICacheConsistencyManagerStateSize = 3;
constexpr size_t kArm64VmICacheConsistencyManagerStateAlign = 1;

// Computed shifts, TCR flags, and PTE descriptor/attribute masks evaluated by the C preprocessor
// in `<arch/arm64/mmu.h>`.
constexpr uint32_t kMmuKernelSizeShift = MMU_KERNEL_SIZE_SHIFT;
constexpr uint32_t kMmuKernelTopShift = MMU_KERNEL_TOP_SHIFT;
constexpr uint32_t kMmuUserTopShift = MMU_USER_TOP_SHIFT;
constexpr uint32_t kMmuGuestTopShift = MMU_GUEST_TOP_SHIFT;

constexpr uint64_t kMmuTcrTbi0 = MMU_TCR_TBI0;
constexpr uint64_t kMmuTcrFlagsKernel = MMU_TCR_FLAGS_KERNEL;
constexpr uint64_t kMmuTcrFlagsUser = MMU_TCR_FLAGS_USER;
constexpr uint64_t kMmuTcrFlagsUserRestricted = MMU_TCR_FLAGS_USER_RESTRICTED;

constexpr uint64_t kMmuPteValid = MMU_PTE_VALID;
constexpr uint64_t kMmuPteDescriptorInvalid = MMU_PTE_DESCRIPTOR_INVALID;
constexpr uint64_t kMmuPteDescriptorMask = MMU_PTE_DESCRIPTOR_MASK;
constexpr uint64_t kMmuPteL012DescriptorBlock = MMU_PTE_L012_DESCRIPTOR_BLOCK;
constexpr uint64_t kMmuPteL012DescriptorTable = MMU_PTE_L012_DESCRIPTOR_TABLE;
constexpr uint64_t kMmuPteL3DescriptorPage = MMU_PTE_L3_DESCRIPTOR_PAGE;
constexpr uint64_t kMmuPteOutputAddrMask = MMU_PTE_OUTPUT_ADDR_MASK;

constexpr uint64_t kMmuPteAttrResSoftware = MMU_PTE_ATTR_RES_SOFTWARE;
constexpr uint64_t kMmuPteAttrUxn = MMU_PTE_ATTR_UXN;
constexpr uint64_t kMmuPteAttrPxn = MMU_PTE_ATTR_PXN;
constexpr uint64_t kMmuPteAttrNonGlobal = MMU_PTE_ATTR_NON_GLOBAL;
constexpr uint64_t kMmuPteAttrAf = MMU_PTE_ATTR_AF;
constexpr uint64_t kMmuPteAttrShInnerShareable = MMU_PTE_ATTR_SH_INNER_SHAREABLE;
constexpr uint64_t kMmuPteAttrApPRwUNa = MMU_PTE_ATTR_AP_P_RW_U_NA;
constexpr uint64_t kMmuPteAttrApPRwURw = MMU_PTE_ATTR_AP_P_RW_U_RW;
constexpr uint64_t kMmuPteAttrApPRoUNa = MMU_PTE_ATTR_AP_P_RO_U_NA;
constexpr uint64_t kMmuPteAttrApPRoURo = MMU_PTE_ATTR_AP_P_RO_U_RO;
constexpr uint64_t kMmuPteAttrApMask = MMU_PTE_ATTR_AP_MASK;
constexpr uint64_t kMmuPteAttrNonSecure = MMU_PTE_ATTR_NON_SECURE;
constexpr uint64_t kMmuPteAttrAttrIndexMask = MMU_PTE_ATTR_ATTR_INDEX_MASK;
constexpr uint64_t kMmuPtePermissionMask = MMU_PTE_PERMISSION_MASK;

constexpr uint64_t kMmuPteAttrStronglyOrdered = MMU_PTE_ATTR_STRONGLY_ORDERED;
constexpr uint64_t kMmuPteAttrDevice = MMU_PTE_ATTR_DEVICE;
constexpr uint64_t kMmuPteAttrNormalMemory = MMU_PTE_ATTR_NORMAL_MEMORY;
constexpr uint64_t kMmuPteAttrNormalUncached = MMU_PTE_ATTR_NORMAL_UNCACHED;

constexpr uint64_t kMmuS2PteAttrXn = MMU_S2_PTE_ATTR_XN;
constexpr uint64_t kMmuS2PteAttrS2ApRo = MMU_S2_PTE_ATTR_S2AP_RO;
constexpr uint64_t kMmuS2PteAttrS2ApRw = MMU_S2_PTE_ATTR_S2AP_RW;
constexpr uint64_t kMmuS2PteAttrAttrIndexMask = MMU_S2_PTE_ATTR_ATTR_INDEX_MASK;
constexpr uint64_t kMmuS2PteAttrNormalMemory = MMU_S2_PTE_ATTR_NORMAL_MEMORY;
constexpr uint64_t kMmuS2PteAttrNormalUncached = MMU_S2_PTE_ATTR_NORMAL_UNCACHED;
constexpr uint64_t kMmuS2PteAttrStronglyOrdered = MMU_S2_PTE_ATTR_STRONGLY_ORDERED;
constexpr uint64_t kMmuS2PteAttrDevice = MMU_S2_PTE_ATTR_DEVICE;

constexpr uint64_t kMmuTlbiVaddrMask = TLBI_VADDR_MASK;

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_ASPACE_CONSTANTS_H_
