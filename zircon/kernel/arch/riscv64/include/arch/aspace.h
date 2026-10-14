// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_RISCV64_INCLUDE_ARCH_ASPACE_H_
#define ZIRCON_KERNEL_ARCH_RISCV64_INCLUDE_ARCH_ASPACE_H_

// RISC-V arch-specific declarations for VmAspace implementation.

#include <debug.h>
#include <lib/page/size.h>
#include <lib/zx/result.h>

#include <arch/riscv64/aspace-constants.h>
#include <arch/riscv64/mmu.h>
#include <kernel/ffi.h>
#include <vm/arch_vm_aspace.h>

enum class Riscv64AspaceType : uint8_t {
  kUser,    // Userspace address space.
  kKernel,  // Kernel address space.
  kGuest,   // Second-stage address space.
};

enum class Riscv64AspaceRole : uint8_t {
  kIndependent,
  kRestricted,
  kShared,
  kUnified,
};

class Riscv64ArchVmAspace final : public ArchVmAspaceInterface {
 public:
  Riscv64ArchVmAspace(vaddr_t base, size_t size, Riscv64AspaceType type,
                      page_alloc_fn_t test_paf = nullptr);
  Riscv64ArchVmAspace(vaddr_t base, size_t size, arch_mmu_flags_t mmu_flags,
                      page_alloc_fn_t test_paf = nullptr);
  virtual ~Riscv64ArchVmAspace();

  using ArchVmAspaceInterface::page_alloc_fn_t;

  // See comments on `ArchVmAspaceInterface` where these methods are declared.
  zx_status_t Init() override;
  zx_status_t InitShared() override;
  zx_status_t InitRestricted() override;
  zx_status_t InitUnified(ArchVmAspaceInterface& shared,
                          ArchVmAspaceInterface& restricted) override;

  void DisableUpdates() override;

  zx_status_t Destroy() override;

  // main methods
  zx_status_t Map(vaddr_t vaddr, paddr_t* phys, size_t count, arch_mmu_flags_t mmu_flags,
                  ExistingEntryAction existing_action) override;
  zx_status_t MapContiguous(vaddr_t vaddr, paddr_t paddr, size_t count,
                            arch_mmu_flags_t mmu_flags) override;

  zx_status_t Unmap(vaddr_t vaddr, size_t count, ArchUnmapOptions enlarge) override;
  // riscv does not document any restrictions on manipulating page tables such that duplicate TLB
  // entries could exist, as long as sfence.vma gets called, so unmap is safe to split large pages
  // without enlarging.
  bool UnmapOnlyEnlargeOnOom() const override { return true; }

  zx_status_t Protect(vaddr_t vaddr, size_t count, arch_mmu_flags_t mmu_flags,
                      ArchUnmapOptions enlarge) override;

  zx_status_t Query(vaddr_t vaddr, paddr_t* paddr, arch_mmu_flags_t* mmu_flags) override;

  vaddr_t PickSpot(vaddr_t base, vaddr_t end, vaddr_t align, size_t size,
                   arch_mmu_flags_t mmu_flags) override;

  zx_status_t MarkAccessed(vaddr_t vaddr, size_t count) override;

  zx_status_t HarvestAccessed(vaddr_t vaddr, size_t count, NonTerminalAction non_terminal_action,
                              TerminalAction terminal_action) override;

  bool AccessedSinceLastCheck(bool clear) override;

  paddr_t arch_table_phys() const override;
  uint16_t asid() const;

  static void ContextSwitch(Riscv64ArchVmAspace* from, Riscv64ArchVmAspace* to);

  static void HandoffPageTablesFromPhysboot(VmPageDoublyLinkedList* mmu_pages);

  static constexpr bool HasNonTerminalAccessedFlag() { return false; }

  static constexpr vaddr_t NextUserPageTableOffset(vaddr_t va) {
    // Work out the virtual address the next page table would start at by first masking the va down
    // to determine its index, then adding 1 and turning it back into a virtual address.
    const uint pt_bits = (kPageShift - 3);
    const uint page_pt_shift = kPageShift + pt_bits;
    return ((va >> page_pt_shift) + 1) << page_pt_shift;
  }

 private:
  // The Rust `Riscv64ArchVmAspace` that backs this object, constructed in place by
  // the constructor and dropped by the destructor.  Opaque to C++: every operation
  // goes through the `rust_riscv64_aspace_*` entry points in mmu.cc.
  OpaqueStorage<kRiscv64ArchVmAspaceStateSize, kRiscv64ArchVmAspaceStateAlign> rust_aspace_;
};

class Riscv64VmICacheConsistencyManager final : public ArchVmICacheConsistencyManagerInterface {
 public:
  Riscv64VmICacheConsistencyManager() = default;
  ~Riscv64VmICacheConsistencyManager() override { Finish(); }
  void SyncAddr(vaddr_t start, size_t len) override;
  void Finish() override;

 private:
  bool need_invalidate_ = false;
};

using ArchVmAspace = Riscv64ArchVmAspace;
using ArchVmICacheConsistencyManager = Riscv64VmICacheConsistencyManager;
#endif  // ZIRCON_KERNEL_ARCH_RISCV64_INCLUDE_ARCH_ASPACE_H_
