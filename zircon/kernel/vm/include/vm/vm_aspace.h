// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ASPACE_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ASPACE_H_

#include <assert.h>
#include <lib/crypto/prng.h>
#include <zircon/types.h>

#include <arch/aspace.h>
#include <fbl/canary.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/intrusive_wavl_tree.h>
#include <fbl/macros.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <kernel/lockdep.h>
#include <kernel/mutex.h>
#include <vm/arch_vm_aspace.h>
#include <vm/attribution.h>
#include <vm/vm.h>

class VmAddressRegion;
class VmMapping;
class VmAddressRegionOrMapping;

namespace hypervisor {
class GuestPhysicalAspace;
}  // namespace hypervisor

class VmObject;

class VmAspace : public fbl::DoublyLinkedListable<VmAspace*>, public fbl::RefCounted<VmAspace> {
 public:
  enum class Type {
    User = 0,
    Kernel,
    // You probably do not want to use LOW_KERNEL. It is primarily used for SMP bootstrap or mexec
    // to allow mappings of very low memory using the standard VMM subsystem.
    LowKernel,
    // Used to construct an address space representing hypervisor guest memory.
    GuestPhysical,
  };

  // Create an address space of the type specified in |type| with name |name|.
  //
  // Although reference counted, the returned VmAspace must be explicitly destroyed via Destroy.
  //
  // Returns null on failure (e.g. due to resource starvation).
  static fbl::RefPtr<VmAspace> Create(Type type, const char* name);

  // Create an address space of the type specified in |type| with name |name|.
  //
  // The returned aspace will start at |base| and span |size|.
  //
  // If |share_opt| is ShareOpt::Shared, we're creating a shared address space, and the underlying
  // ArchVmAspace will be initialized using the `InitShared` method instead of the normal
  // `Init` method.
  //
  // If |share_opt| is ShareOpt::Restricted, we're creating a restricted address space, and the
  // underlying ArchVmAspace will be initialized using the `InitRestricted` method.
  //
  // Although reference counted, the returned VmAspace must be explicitly destroyed via Destroy.
  //
  // Returns null on failure (e.g. due to resource starvation).
  enum class ShareOpt {
    None,
    Restricted,
    Shared,
  };
  static fbl::RefPtr<VmAspace> Create(vaddr_t base, size_t size, Type type, const char* name,
                                      ShareOpt share_opt);

  // Create a unified address space that consists of the given constituent address spaces.
  //
  // The passed in address spaces must meet the following criteria:
  // 1. They must manage non-overlapping regions.
  // 2. The shared VmAspace must have been created with the shared argument set to true.
  //
  // Although reference counted, the returned VmAspace must be explicitly destroyed via Destroy.
  // Note that it must be Destroy()'d before the shared and restricted VmAspaces; Destroy()'ing the
  // constituent VmAspaces before Destroy()'ing this one will trigger asserts.
  //
  // Returns null on failure (e.g. due to resource starvation).
  static fbl::RefPtr<VmAspace> CreateUnified(VmAspace* shared, VmAspace* restricted,
                                             const char* name);

  // Destroy this address space.
  //
  // Destroy does not free this object, but rather allows it to be freed when the last retaining
  // RefPtr is destroyed.
  zx_status_t Destroy();

  void Rename(const char* name);

  // simple accessors
  vaddr_t base() const { return base_; }
  size_t size() const { return size_; }
  const char* name() const { return name_; }
  ArchVmAspace& arch_aspace() { return arch_aspace_; }
  bool is_user() const { return type_ == Type::User; }
  bool is_aslr_enabled() const { return aslr_config_.enabled; }

  // Get the root VMAR (briefly acquires the aspace lock)
  // May return nullptr if the aspace has been destroyed or is not yet initialized.
  fbl::RefPtr<VmAddressRegion> RootVmar();

  // Returns true if the address space has been destroyed.
  bool is_destroyed() const;

  // accessor for singleton kernel address space
  static VmAspace* kernel_aspace() { return kernel_aspace_; }

  // set the per thread aspace pointer to this
  void AttachToThread(Thread* t);

  void Dump(bool verbose) const;
  void DumpLocked(bool verbose) const TA_REQ(lock_);

  static void DropAllUserPageTables();
  void DropUserPageTables();

  static void DumpAllAspaces(bool verbose);

  // Harvests all accessed information across all user mappings and updates any page age
  // information for terminal mappings, and potentially harvests page tables depending on the
  // passed in action. This requires holding the aspaces_list_lock_ over the entire duration and
  // whilst not a commonly used lock this function should still only be called infrequently to
  // avoid monopolizing the lock.
  using NonTerminalAction = ArchVmAspace::NonTerminalAction;
  using TerminalAction = ArchVmAspace::TerminalAction;
  static void HarvestAllUserAccessedBits(NonTerminalAction non_terminal_action,
                                         TerminalAction terminal_action);

  // A collection of memory usage counts.
  struct vm_usage_t {
    // A count of bytes covered by VmMapping ranges.
    size_t mapped_bytes = 0;

    // For the fields below, a byte is considered committed if a VmMapping covers a range of a
    // VmObject that contains that byte's page, and the page has physical memory allocated to it.

    // A count of committed bytes that are only mapped into this address space and are not shared
    // between VMOs via copy-on-write.
    size_t private_bytes = 0;

    // A count of committed bytes that are mapped into this and at least one other address space, or
    // are shared between VMOs via copy-on-write (even if the VMOs are both mapped into this address
    // space).
    size_t shared_bytes = 0;

    // A number that estimates the fraction of shared_bytes that this address space is responsible
    // for keeping alive.
    //
    // An estimate of:
    //   For each shared, committed page:
    //   share_factor = (number of VMOs sharing this page) *
    //                  (number of address spaces mapping this page)
    //   scaled_shared_bytes += PAGE_SIZE / share_factor
    //
    // This number is strictly smaller than shared_bytes.
    vm::FractionalBytes scaled_shared_bytes{0};
  };

  // Counts memory usage under the VmAspace.
  zx_status_t GetMemoryUsage(vm_usage_t* usage);

  // Generates a soft fault against this aspace. This is similar to a PageFault except:
  //  * This aspace may not currently be active and this does not have to be called from the
  //    hardware exception handler.
  //  * May be invoked spuriously in situations where the hardware mappings would have prevented a
  //    real PageFault from occurring.
  zx_status_t SoftFault(vaddr_t va, uint flags);
  // Similar to SoftFault, but additionally takes a length indicating that the range of [va, va+len)
  // is expected to be accessed with |flags| after resolving this fault. The aspace can take this
  // range as a hint to attempt to preemptively avoid future faults.
  // There are no alignment restrictions on |va| or |len|, although it is assumed that |len| is
  // greater than zero.
  zx_status_t SoftFaultInRange(vaddr_t va, uint flags, size_t len);

  // Generates an accessed flag fault against this aspace. This is a specialized version of
  // SoftFault that will only resolve a potential missing access flag and nothing else.
  zx_status_t AccessedFault(vaddr_t va);

  // Page fault routine. Should only be called by the hypervisor or by Thread::Current::Fault.
  zx_status_t PageFault(vaddr_t va, uint flags);

  // Convenience method for traversing the tree of VMARs to find the deepest
  // VMAR in the tree that includes *va*.
  // Returns nullptr if the aspace has been destroyed or is not yet initialized.
  fbl::RefPtr<VmAddressRegionOrMapping> FindRegion(vaddr_t va);

  // For region creation routines
  static const uint VMM_FLAG_VALLOC_SPECIFIC = (1u << 0);  // allocate at specific address
  static const uint VMM_FLAG_COMMIT = (1u << 1);  // commit memory up front (no demand paging)

  // legacy functions to assist in the transition to VMARs
  // These all assume a flat VMAR structure in which all VMOs are mapped
  // as children of the root.  They will all assert if used on user aspaces
  // TODO(teisenbe): remove uses of these in favor of new VMAR interfaces
  zx_status_t AllocPhysical(const char* name, size_t size, void** ptr, uint8_t align_pow2,
                            paddr_t paddr, uint vmm_flags, uint arch_mmu_flags);
  zx_status_t AllocContiguous(const char* name, size_t size, void** ptr, uint8_t align_pow2,
                              uint vmm_flags, uint arch_mmu_flags);
  zx_status_t Alloc(const char* name, size_t size, void** ptr, uint8_t align_pow2, uint vmm_flags,
                    uint arch_mmu_flags);
  zx_status_t FreeRegion(vaddr_t va);

  // Internal use function for mapping VMOs.  Do not use.  This is exposed in
  // the public API purely for tests.
  zx_status_t MapObjectInternal(fbl::RefPtr<VmObject> vmo, const char* name, uint64_t offset,
                                size_t size, void** ptr, uint8_t align_pow2, uint vmm_flags,
                                uint arch_mmu_flags);

  uintptr_t vdso_base_address() const;
  uintptr_t vdso_code_address() const;

  // Helper function to test for collision with vdso_code_mapping_.
  bool IntersectsVdsoCodeLocked(vaddr_t base, size_t size) const TA_REQ(lock_);

  // Returns whether this aspace is currently set to be a high memory priority.
  bool IsHighMemoryPriority() const;

 protected:
  // Share the aspace lock with VmAddressRegion/VmMapping/GuestPhysicalAspace so they can serialize
  // changes to the aspace.
  friend class VmAddressRegionOrMapping;
  friend class VmAddressRegion;
  friend class VmMapping;
  friend class hypervisor::GuestPhysicalAspace;
  Lock<CriticalMutex>* lock() const TA_RET_CAP(lock_) { return &lock_; }
  Lock<CriticalMutex>& lock_ref() const TA_RET_CAP(lock_) { return lock_; }

  // Expose the PRNG for ASLR to VmAddressRegion
  crypto::Prng& AslrPrngLocked() TA_REQ(lock_) {
    DEBUG_ASSERT(is_aslr_enabled());
    return aslr_prng_;
  }

  uint8_t AslrEntropyBits(bool compact) const {
    return compact ? aslr_config_.compact_entropy_bits : aslr_config_.entropy_bits;
  }

 private:
  friend lazy_init::Access;

  // Represents the ALSR configuration for a VmAspace. This is grouped in a struct so it can be
  // conveniently grouped together as it is const over the lifetime of a VmAspace.
  struct AslrConfig {
    bool enabled;
    uint8_t entropy_bits;
    uint8_t compact_entropy_bits;
    // We record the PRNG seed to enable reproducible debugging.
    uint8_t seed[crypto::Prng::kMinEntropy];
  };

  // can only be constructed via factory or LazyInit
  VmAspace(vaddr_t base, size_t size, Type type, AslrConfig aslr_config, const char* name);

  DISALLOW_COPY_ASSIGN_AND_MOVE(VmAspace);

  // private destructor that can only be used from the ref ptr
  ~VmAspace();
  friend fbl::RefPtr<VmAspace>;

  // complete initialization, may fail in OOM cases
  zx_status_t Init(ShareOpt share_opt);

  void InitializeAslr();

  static AslrConfig CreateAslrConfig(Type type);

  // Increments or decrements the priority count of this aspace. The high priority count is used to
  // control active page table reclamation, and applies to the whole aspace. The count is never
  // allowed to go negative and so callers must only subtract what they have already added. Further,
  // callers are required to remove any additions before the aspace is destroyed.
  void ChangeHighPriorityCountLocked(int64_t delta) TA_REQ(lock());

  // Returns whether this aspace is a guest physical address space.
  // TODO(https://fxbug.dev/42054461): Rationalize usage of `is_user` and `is_guest_physical`.
  bool is_guest_physical() const { return type_ == Type::GuestPhysical; }

  bool can_enlarge_arch_unmap() const { return is_user() || is_guest_physical(); }

  using ArchUnmapOptions = ArchVmAspaceInterface::ArchUnmapOptions;

  // Encodes the idea that we can always unmap from user aspaces.
  ArchUnmapOptions EnlargeArchUnmap() const {
    return can_enlarge_arch_unmap() ? ArchUnmapOptions::Enlarge : ArchUnmapOptions::None;
  }

  fbl::RefPtr<VmAddressRegion> RootVmarLocked() TA_REQ(lock_);

  // Internal helper for resolving page faults. Takes an aligned va.
  zx_status_t PageFaultInternal(vaddr_t va, uint flags, size_t additional_pages);

  // magic
  fbl::Canary<fbl::magic("VMAS")> canary_;

  // members
  const vaddr_t base_;
  const size_t size_;
  const Type type_;
  char name_[ZX_MAX_NAME_LEN] TA_GUARDED(lock_);
  bool aspace_destroyed_ TA_GUARDED(lock_) = false;

  // The high priority count is used to determine whether this aspace should perform page table
  // reclamation, with any non-zero count completely disabling reclamation. This is an atomic so
  // that it can be safely read outside the lock, however writes should occur inside the lock.
  ktl::atomic<int64_t> high_priority_count_ = 0;

  mutable DECLARE_CRITICAL_MUTEX(VmAspace) lock_;

  // Keep a cache of the VmMapping of the last PageFault that occurred. On a page fault this can
  // be checked to see if it matches more quickly than walking the full vmar tree. Mappings that
  // are stored here must be in the ALIVE state, implying that they are in the VMAR tree. It is
  // then the responsibility of the VmMapping to remove itself from here should it transition out
  // of ALIVE, and remove itself from the VMAR tree. A raw pointer is stored here since the
  // VmMapping must be alive and in tree anyway and if it were a RefPtr we would not be able to
  // handle being the one to drop the last ref and perform destruction.
  VmMapping* last_fault_ TA_GUARDED(lock_) = nullptr;

  // root of virtual address space
  // Access to this reference is guarded by lock_.
  fbl::RefPtr<VmAddressRegion> root_vmar_ TA_GUARDED(lock_);

  // PRNG used by VMARs for address choices. The PRNG is thread safe and does not need to be guarded
  // by the lock.
  crypto::Prng aslr_prng_;
  const AslrConfig aslr_config_;

  // architecturally specific part of the aspace. This is internally locked and does not need to be
  // guarded by lock_.
  ArchVmAspace arch_aspace_;

  fbl::RefPtr<VmMapping> vdso_code_mapping_ TA_GUARDED(lock_);

  // The number of page table reclamations attempted since last active. This is used since we need
  // to perform pt reclamation twice in a row (once to clear accessed bits, another time to
  // reclaim page tables) before the aspace is at a fixed point and we can actually stop
  // performing the harvests.
  uint32_t pt_harvest_since_active_ TA_GUARDED(AspaceListLock::Get()) = 0;

  DECLARE_SINGLETON_MUTEX(AspaceListLock);
  static fbl::DoublyLinkedList<VmAspace*> aspaces_list_ TA_GUARDED(AspaceListLock::Get());

  // initialization routines need to construct the singleton kernel address space
  // at a particular points in the bootup process
  static void KernelAspaceInit();
  static VmAspace* kernel_aspace_;
  friend void vm_init();
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ASPACE_H_
