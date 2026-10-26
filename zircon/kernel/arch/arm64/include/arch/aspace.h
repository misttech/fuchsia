// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2015-2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ASPACE_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ASPACE_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <arch/arm64/aspace-constants.h>
#include <arch/arm64/mmu.h>
#include <kernel/ffi.h>
#include <vm/arch_vm_aspace.h>

enum class ArmAspaceType : uint8_t {
  kUser,    // Userspace address space.
  kKernel,  // Kernel address space.
  kGuest,   // Second-stage address space.
};

enum class ArmAspaceRole : uint8_t {
  kIndependent,
  kRestricted,
  kShared,
  kUnified,
};

using ArmAspaceState = OpaqueStorage<kArm64ArchVmAspaceStateSize, kArm64ArchVmAspaceStateAlign>;
using ArmICacheState = OpaqueStorage<kArm64VmICacheConsistencyManagerStateSize,
                                     kArm64VmICacheConsistencyManagerStateAlign>;

extern "C" {

void rust_arm64_aspace_construct(ArmAspaceState* aspace, vaddr_t base, size_t size,
                                 ArmAspaceType type, ArchVmAspaceInterface::page_alloc_fn_t paf);
void rust_arm64_aspace_construct_flags(ArmAspaceState* aspace, vaddr_t base, size_t size,
                                       arch_mmu_flags_t mmu_flags,
                                       ArchVmAspaceInterface::page_alloc_fn_t paf);
void rust_arm64_aspace_destruct(ArmAspaceState* aspace);
zx_status_t rust_arm64_aspace_init(const ArmAspaceState& aspace);
zx_status_t rust_arm64_aspace_init_shared(const ArmAspaceState& aspace);
zx_status_t rust_arm64_aspace_init_restricted(const ArmAspaceState& aspace);
zx_status_t rust_arm64_aspace_init_unified(const ArmAspaceState& aspace,
                                           const ArmAspaceState& shared,
                                           const ArmAspaceState& restricted);
void rust_arm64_aspace_disable_updates(const ArmAspaceState& aspace);
zx_status_t rust_arm64_aspace_destroy(const ArmAspaceState& aspace);
zx_status_t rust_arm64_aspace_map(const ArmAspaceState& aspace, vaddr_t vaddr, const paddr_t* phys,
                                  size_t count, arch_mmu_flags_t mmu_flags,
                                  ArchVmAspaceInterface::ExistingEntryAction existing_action);
zx_status_t rust_arm64_aspace_map_contiguous(const ArmAspaceState& aspace, vaddr_t vaddr,
                                             paddr_t paddr, size_t count,
                                             arch_mmu_flags_t mmu_flags);
zx_status_t rust_arm64_aspace_unmap(const ArmAspaceState& aspace, vaddr_t vaddr, size_t count,
                                    uint8_t enlarge);
zx_status_t rust_arm64_aspace_protect(const ArmAspaceState& aspace, vaddr_t vaddr, size_t count,
                                      arch_mmu_flags_t mmu_flags, uint8_t enlarge);
zx_status_t rust_arm64_aspace_query(const ArmAspaceState& aspace, vaddr_t vaddr, paddr_t* paddr,
                                    arch_mmu_flags_t* mmu_flags);
vaddr_t rust_arm64_aspace_pick_spot(const ArmAspaceState& aspace, vaddr_t base, vaddr_t end,
                                    vaddr_t align, size_t size, arch_mmu_flags_t mmu_flags);
zx_status_t rust_arm64_aspace_mark_accessed(const ArmAspaceState& aspace, vaddr_t vaddr,
                                            size_t count);
zx_status_t rust_arm64_aspace_harvest_accessed(
    const ArmAspaceState& aspace, vaddr_t vaddr, size_t count,
    ArchVmAspaceInterface::NonTerminalAction non_terminal_action,
    ArchVmAspaceInterface::TerminalAction terminal_action);
bool rust_arm64_aspace_accessed_since_last_check(const ArmAspaceState& aspace, bool clear);
paddr_t rust_arm64_aspace_arch_table_phys(const ArmAspaceState& aspace);
uint16_t rust_arm64_aspace_arch_asid(const ArmAspaceState& aspace);
void rust_arm64_aspace_arch_set_asid(const ArmAspaceState& aspace, uint16_t asid);
void rust_arm64_aspace_context_switch(const ArmAspaceState* from, const ArmAspaceState* to);
void rust_arm64_aspace_handoff_page_tables_from_physboot(VmPageDoublyLinkedList* mmu_pages);

void rust_arm64_icache_force_clean_to_poc(ArmICacheState& state);
void rust_arm64_icache_sync_addr(ArmICacheState& state, vaddr_t start, size_t len);
void rust_arm64_icache_finish(ArmICacheState& state);

}  // extern "C"

// The Rust side mirrors these as `#[repr(u8)]` enums and they are passed across the
// FFI boundary unconverted, so their widths are part of the ABI. A scoped enum with
// no fixed underlying type would be `int`.
static_assert(sizeof(ArmAspaceType) == 1);
static_assert(sizeof(ArchVmAspaceInterface::ExistingEntryAction) == 1);
static_assert(sizeof(ArchVmAspaceInterface::NonTerminalAction) == 1);
static_assert(sizeof(ArchVmAspaceInterface::TerminalAction) == 1);

class ArmArchVmAspace final : public ArchVmAspaceInterface {
 public:
  ArmArchVmAspace(vaddr_t base, size_t size, ArmAspaceType type, page_alloc_fn_t paf = nullptr) {
    rust_arm64_aspace_construct(&rust_aspace_, base, size, type, paf);
  }
  ArmArchVmAspace(vaddr_t base, size_t size, arch_mmu_flags_t mmu_flags,
                  page_alloc_fn_t paf = nullptr) {
    rust_arm64_aspace_construct_flags(&rust_aspace_, base, size, mmu_flags, paf);
  }
  virtual ~ArmArchVmAspace() { rust_arm64_aspace_destruct(&rust_aspace_); }

  // See comments on `ArchVmAspaceInterface` where these methods are declared.
  zx_status_t Init() override { return rust_arm64_aspace_init(rust_aspace_); }
  zx_status_t InitShared() override { return rust_arm64_aspace_init_shared(rust_aspace_); }
  zx_status_t InitRestricted() override { return rust_arm64_aspace_init_restricted(rust_aspace_); }
  zx_status_t InitUnified(ArchVmAspaceInterface& shared,
                          ArchVmAspaceInterface& restricted) override {
    return rust_arm64_aspace_init_unified(rust_aspace_,
                                          static_cast<ArmArchVmAspace&>(shared).rust_aspace_,
                                          static_cast<ArmArchVmAspace&>(restricted).rust_aspace_);
  }

  void DisableUpdates() override { rust_arm64_aspace_disable_updates(rust_aspace_); }

  zx_status_t Destroy() override { return rust_arm64_aspace_destroy(rust_aspace_); }

  // main methods
  zx_status_t Map(vaddr_t vaddr, paddr_t* phys, size_t count, arch_mmu_flags_t mmu_flags,
                  ExistingEntryAction existing_action) override {
    return rust_arm64_aspace_map(rust_aspace_, vaddr, phys, count, mmu_flags, existing_action);
  }
  zx_status_t MapContiguous(vaddr_t vaddr, paddr_t paddr, size_t count,
                            arch_mmu_flags_t mmu_flags) override {
    return rust_arm64_aspace_map_contiguous(rust_aspace_, vaddr, paddr, count, mmu_flags);
  }

  using ArchUnmapOptions = ArchVmAspaceInterface::ArchUnmapOptions;

  zx_status_t Unmap(vaddr_t vaddr, size_t count, ArchUnmapOptions enlarge) override {
    return rust_arm64_aspace_unmap(rust_aspace_, vaddr, count, static_cast<uint8_t>(enlarge));
  }

  // ARM states that we must perform a break-before-make when changing the block size used by the
  // translation system (unless ARMv8.4-TTRem is supported) and so an unmap that needs to split a
  // large page must break-before-make, counting as an enlargement.
  bool UnmapOnlyEnlargeOnOom() const override { return false; }

  zx_status_t Protect(vaddr_t vaddr, size_t count, arch_mmu_flags_t mmu_flags,
                      ArchUnmapOptions enlarge) override {
    return rust_arm64_aspace_protect(rust_aspace_, vaddr, count, mmu_flags,
                                     static_cast<uint8_t>(enlarge));
  }

  zx_status_t Query(vaddr_t vaddr, paddr_t* paddr, arch_mmu_flags_t* mmu_flags) override {
    return rust_arm64_aspace_query(rust_aspace_, vaddr, paddr, mmu_flags);
  }

  vaddr_t PickSpot(vaddr_t base, vaddr_t end, vaddr_t align, size_t size,
                   arch_mmu_flags_t mmu_flags) override {
    return rust_arm64_aspace_pick_spot(rust_aspace_, base, end, align, size, mmu_flags);
  }

  zx_status_t MarkAccessed(vaddr_t vaddr, size_t count) override {
    return rust_arm64_aspace_mark_accessed(rust_aspace_, vaddr, count);
  }

  zx_status_t HarvestAccessed(vaddr_t vaddr, size_t count, NonTerminalAction non_terminal_action,
                              TerminalAction terminal_action) override {
    return rust_arm64_aspace_harvest_accessed(rust_aspace_, vaddr, count, non_terminal_action,
                                              terminal_action);
  }

  bool AccessedSinceLastCheck(bool clear) override {
    return rust_arm64_aspace_accessed_since_last_check(rust_aspace_, clear);
  }

  paddr_t arch_table_phys() const override {
    return rust_arm64_aspace_arch_table_phys(rust_aspace_);
  }
  uint16_t arch_asid() const { return rust_arm64_aspace_arch_asid(rust_aspace_); }
  void arch_set_asid(uint16_t asid) { rust_arm64_aspace_arch_set_asid(rust_aspace_, asid); }

  static void ContextSwitch(ArmArchVmAspace* from, ArmArchVmAspace* to) {
    rust_arm64_aspace_context_switch(from ? &from->rust_aspace_ : nullptr,
                                     to ? &to->rust_aspace_ : nullptr);
  }

  static void HandoffPageTablesFromPhysboot(VmPageDoublyLinkedList* mmu_pages) {
    rust_arm64_aspace_handoff_page_tables_from_physboot(mmu_pages);
  }

  // ARM only has accessed flags on terminal page mappings. This means that FreeUnaccessed will
  // only be able to free page tables where terminal accessed flags have been removed using
  // HarvestAccessed.
  static constexpr bool HasNonTerminalAccessedFlag() { return false; }

  static constexpr vaddr_t NextUserPageTableOffset(vaddr_t va) {
    // Work out the virtual address the next page table would start at by first masking the va down
    // to determine its index, then adding 1 and turning it back into a virtual address.
    const uint pt_bits = (MMU_USER_PAGE_SIZE_SHIFT - 3);
    const uint page_pt_shift = MMU_USER_PAGE_SIZE_SHIFT + pt_bits;
    return ((va >> page_pt_shift) + 1) << page_pt_shift;
  }

 private:
  // The Rust `ArmArchVmAspace` that backs this object, constructed in place by
  // the constructor and dropped by the destructor. Opaque to C++: every
  // operation forwards to `rust_arm64_aspace_*`.
  ArmAspaceState rust_aspace_;
};

class ArmVmICacheConsistencyManager final : public ArchVmICacheConsistencyManagerInterface {
 public:
  ArmVmICacheConsistencyManager() = default;
  ~ArmVmICacheConsistencyManager() override { Finish(); }

  void SyncAddr(vaddr_t start, size_t len) override {
    rust_arm64_icache_sync_addr(rust_state_, start, len);
  }
  void Finish() override { rust_arm64_icache_finish(rust_state_); }

  // If a cache operation is going to be performed anyone on the addresses provided, then can avoid
  // doing duplicate operations by insisting SyncAddr cleans to the PoC. If SyncAddr did not need
  // to do any cache operation, due to CTR.IDC[28]=1, then this still forces it to do a clean to
  // PoC. Note that the default clean, when CTR.IDC[28] = 0, is only to PoU.
  void ForceCleanToPoC() { rust_arm64_icache_force_clean_to_poc(rust_state_); }

 private:
  ArmICacheState rust_state_ = {};
};

static inline uint64_t arm64_vttbr(uint16_t vmid, paddr_t baddr) {
  return static_cast<paddr_t>(vmid) << 48 | baddr;
}

using ArchVmAspace = ArmArchVmAspace;
using ArchVmICacheConsistencyManager = ArmVmICacheConsistencyManager;

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ASPACE_H_
