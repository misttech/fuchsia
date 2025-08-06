// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <align.h>
#include <assert.h>
#include <inttypes.h>
#include <lib/counters.h>
#include <lib/fit/defer.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <fbl/alloc_checker.h>
#include <ktl/algorithm.h>
#include <ktl/iterator.h>
#include <ktl/utility.h>
#include <vm/fault.h>
#include <vm/physmap.h>
#include <vm/vm.h>
#include <vm/vm_address_region.h>
#include <vm/vm_aspace.h>
#include <vm/vm_object.h>
#include <vm/vm_object_paged.h>
#include <vm/vm_object_physical.h>

#include "vm/vm_address_region.h"
#include "vm_priv.h"

#include <ktl/enforce.h>

#define LOCAL_TRACE VM_GLOBAL_TRACE(0)

namespace {

KCOUNTER(vm_mapping_attribution_queries, "vm.attributed_memory.mapping.queries")
KCOUNTER(vm_mappings_merged, "vm.aspace.mapping.merged_neighbors")
KCOUNTER(vm_mappings_protect_no_write, "vm.aspace.mapping.protect_without_write")

}  // namespace

// Helper class for managing the logic of skipping certain unmap operations for in progress faults.
// This is expected to be stack allocated under the object lock and the object lock must not be
// dropped over its lifetime.
// Creating this object creates a contract where the caller will either update the mapping for this
// location and call Success, or this object will automatically unmap the location if necessary.
class VmMapping::CurrentlyFaulting {
 public:
  CurrentlyFaulting(VmMapping* mapping, uint64_t object_offset, uint64_t len)
      TA_REQ(mapping->object_->lock())
      : mapping_(mapping), object_offset_(object_offset), len_(len) {
    DEBUG_ASSERT(mapping->currently_faulting_ == nullptr);
    mapping->currently_faulting_ = this;
  }
  ~CurrentlyFaulting() {
    // If the caller did not call Success, and an unmap was skipped, then we must unmap the range
    // ourselves. We only do the unmap here if a prior unmap was skipped to avoid needless unmaps
    // due to transient errors such as needing to wait on a page request.
    if (state_ == State::UnmapSkipped) {
      vaddr_t base;
      size_t new_len;
      bool valid_range = mapping_->ObjectRangeToVaddrRange(object_offset_, len_, &base, &new_len);
      ASSERT(valid_range);
      ASSERT(new_len == len_);
      zx_status_t status = mapping_->aspace_->arch_aspace().Unmap(
          base, new_len / PAGE_SIZE, mapping_->aspace_->EnlargeArchUnmap());
      ASSERT(status == ZX_OK);
    }
    mapping_->currently_faulting_ = nullptr;
  }

  // Called to say that the given range needs to be unmapped. This returns true if updating the
  // range will be handled by the faulting thread and that the unmap can therefore be skipped.
  // Returns false if the caller should unmap themselves.
  bool UnmapRange(uint64_t object_offset, uint64_t len) {
    DEBUG_ASSERT(state_ != State::Completed);
    if (Intersects(object_offset, len, object_offset_, len_)) {
      state_ = State::UnmapSkipped;
      return true;
    }
    return false;
  }

  // Called to indicate that the mapping for the fault location has been updated successfully. This
  // acts to cancel the unmap that would otherwise happen when this object goes out of scope.
  void MappingUpdated() { state_ = State::Completed; }

  DISALLOW_COPY_ASSIGN_AND_MOVE(CurrentlyFaulting);

 private:
  // Reference back to the original mapping.
  VmMapping* mapping_;
  // The offset, in object space, of the page fault.
  uint64_t object_offset_;
  uint64_t len_;
  enum class State {
    NoUnmapNeeded,
    UnmapSkipped,
    Completed,
  };
  State state_ = State::NoUnmapNeeded;
};

VmMapping::VmMapping(VmAddressRegion& parent, vaddr_t base, size_t size, uint32_t vmar_flags,
                     fbl::RefPtr<VmObject> vmo, uint64_t vmo_offset,
                     MappingProtectionRanges&& ranges, Mergeable mergeable)
    : VmAddressRegionOrMapping(base, size, vmar_flags, parent.aspace_.get(), &parent, true),
      mergeable_(mergeable),
      object_(ktl::move(vmo)),
      object_offset_(vmo_offset),
      protection_ranges_(ktl::move(ranges)) {
  LTRACEF("%p aspace %p base %#" PRIxPTR " size %#zx offset %#" PRIx64 "\n", this, aspace_.get(),
          base_, size_, vmo_offset);
}

VmMapping::VmMapping(VmAddressRegion& parent, vaddr_t base, size_t size, uint32_t vmar_flags,
                     fbl::RefPtr<VmObject> vmo, uint64_t vmo_offset, uint arch_mmu_flags,
                     Mergeable mergeable)
    : VmMapping(parent, base, size, vmar_flags, vmo, vmo_offset,
                MappingProtectionRanges(arch_mmu_flags), mergeable) {}

VmMapping::~VmMapping() {
  canary_.Assert();
  LTRACEF("%p aspace %p base %#" PRIxPTR " size %#zx\n", this, aspace_.get(), base_, size_);
}

fbl::RefPtr<VmObject> VmMapping::vmo() const {
  Guard<CriticalMutex> guard{lock()};
  return vmo_locked();
}

VmMapping::AttributionCounts VmMapping::GetAttributedMemoryLocked() {
  canary_.Assert();

  if (state_ != LifeCycleState::ALIVE) {
    return AttributionCounts{};
  }

  vm_mapping_attribution_queries.Add(1);

  return object_->GetAttributedMemoryInRange(object_offset_locked(), size_);
}

void VmMapping::DumpLocked(uint depth, bool verbose) const {
  canary_.Assert();
  for (uint i = 0; i < depth; ++i) {
    printf("  ");
  }
  char vmo_name[32];
  object_->get_name(vmo_name, sizeof(vmo_name));
  printf("map %p [%#" PRIxPTR " %#" PRIxPTR "] sz %#zx state %d mergeable %s\n", this, base_,
         base_ + size_ - 1, size_, (int)state_, mergeable_ == Mergeable::YES ? "true" : "false");
  EnumerateProtectionRangesLocked(base_, size_, [depth](vaddr_t base, size_t len, uint mmu_flags) {
    for (uint i = 0; i < depth + 1; ++i) {
      printf("  ");
    }
    printf(" [%#" PRIxPTR " %#" PRIxPTR "] mmufl %#x\n", base, base + len - 1, mmu_flags);
    return ZX_ERR_NEXT;
  });
  for (uint i = 0; i < depth + 1; ++i) {
    printf("  ");
  }
  AttributionCounts counts = object_->GetAttributedMemoryInRange(object_offset_locked(), size_);
  printf("vmo %p/k%" PRIu64 " off %#" PRIx64 " bytes (%zu/%zu) ref %d '%s'\n", object_.get(),
         object_->user_id(), object_offset_locked(), counts.uncompressed_bytes,
         counts.compressed_bytes, ref_count_debug(), vmo_name);
  if (verbose) {
    object_->Dump(depth + 1, false);
  }
}

using ArchUnmapOptions = ArchVmAspaceInterface::ArchUnmapOptions;

// static
zx_status_t VmMapping::ProtectOrUnmap(const fbl::RefPtr<VmAspace>& aspace, vaddr_t base,
                                      size_t size, uint new_arch_mmu_flags) {
  // This can never be used to set a WRITE permission since it does not ask the underlying VMO to
  // perform the copy-on-write step. The underlying VMO might also support dirty tracking, which
  // requires write permission faults in order to track pages as dirty when written.
  ASSERT(!(new_arch_mmu_flags & ARCH_MMU_FLAG_PERM_WRITE));
  // If not removing all permissions do the protect, otherwise skip straight to unmapping the entire
  // region.
  if ((new_arch_mmu_flags & ARCH_MMU_FLAG_PERM_RWX_MASK) != 0) {
    zx_status_t status = aspace->arch_aspace().Protect(
        base, size / PAGE_SIZE, new_arch_mmu_flags,
        aspace->can_enlarge_arch_unmap() ? ArchUnmapOptions::Enlarge : ArchUnmapOptions::None);
    // If the unmap failed and we are allowed to unmap extra portions of the aspace then fall
    // through and unmap, otherwise return with whatever the status is.
    if (likely(status == ZX_OK) || !aspace->can_enlarge_arch_unmap()) {
      return status;
    }
  }

  return aspace->arch_aspace().Unmap(base, size / PAGE_SIZE, aspace->EnlargeArchUnmap());
}

zx_status_t VmMapping::ProtectLocked(vaddr_t base, size_t size, uint new_arch_mmu_flags) {
  // Assert a few things that should already have been checked by the caller.
  DEBUG_ASSERT(size != 0 && IS_PAGE_ROUNDED(base) && IS_PAGE_ROUNDED(size));
  DEBUG_ASSERT(!(new_arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK));
  DEBUG_ASSERT(is_valid_mapping_flags(new_arch_mmu_flags));

  DEBUG_ASSERT(object_);
  // grab the lock for the vmo
  Guard<CriticalMutex> guard{object_->lock()};

  // Persist our current caching mode. Every protect region will have the same caching mode so we
  // can acquire this from any region.
  new_arch_mmu_flags |= (protection_ranges_.FirstRegionMmuFlags() & ARCH_MMU_FLAG_CACHE_MASK);

  // This will get called by UpdateProtectionRange below for every existing unique protection range
  // that gets changed and allows us to fine tune the protect action based on the previous flags.
  auto protect_callback = [new_arch_mmu_flags, this](vaddr_t base, size_t size,
                                                     uint old_arch_mmu_flags) {
    // Perform an early return if the new and old flags are the same, as there's nothing to be done.
    if (new_arch_mmu_flags == old_arch_mmu_flags) {
      return;
    }

    uint flags = new_arch_mmu_flags;
    // Check if the new flags have the write permission. This is problematic as we cannot just
    // change any existing hardware mappings to have the write permission, as any individual mapping
    // may be the result of a read fault and still need to have a copy-on-write step performed. This
    // could also map a dirty tracked VMO which requires write permission faults to track pages as
    // dirty when written.
    if (new_arch_mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) {
      // Whatever happens, we're not going to be protecting the arch aspace to have write mappings,
      // so this has to be a user aspace so that we can lazily take write faults in the future.
      ASSERT(aspace_->is_user() || aspace_->is_guest_physical());
      flags &= ~ARCH_MMU_FLAG_PERM_WRITE;
      vm_mappings_protect_no_write.Add(1);
      // If the new flags without write permission are the same as the old flags, then skip the
      // protect step since it will be a no-op.
      if (flags == old_arch_mmu_flags) {
        return;
      }
    }

    zx_status_t status = ProtectOrUnmap(aspace_, base, size, flags);
    // If the protect failed then we do not have sufficient information left to rollback in order to
    // return an error, nor can we claim success, so require the protect to have succeeded to
    // continue.
    ASSERT(status == ZX_OK);
  };

  zx_status_t status = protection_ranges_.UpdateProtectionRange(
      base_, size_, base, size, new_arch_mmu_flags, protect_callback);
  ASSERT(status == ZX_OK || status == ZX_ERR_NO_MEMORY);
  return status;
}

zx_status_t VmMapping::UnmapLocked(vaddr_t base, size_t size) {
  canary_.Assert();
  DEBUG_ASSERT(size != 0 && IS_PAGE_ROUNDED(size) && IS_PAGE_ROUNDED(base));
  DEBUG_ASSERT(base >= base_ && base - base_ < size_);
  DEBUG_ASSERT(size_ - (base - base_) >= size);
  DEBUG_ASSERT(parent_);

  if (state_ != LifeCycleState::ALIVE) {
    return ZX_ERR_BAD_STATE;
  }

  AssertHeld(parent_->lock_ref());

  // Should never be unmapping everything, otherwise should destroy.
  DEBUG_ASSERT(base != base_ || size != size_);

  LTRACEF("%p\n", this);

  // First create any new mapping. One or two might be required depending on whether unmapping from
  // an end or the middle.
  fbl::RefPtr<VmMapping> left, right;
  if (base_ != base) {
    fbl::AllocChecker ac;
    left = fbl::AdoptRef(new (&ac) VmMapping(*parent_, base_, base - base_, flags_, object_,
                                             object_offset_locked(), MappingProtectionRanges(0),
                                             Mergeable::YES));
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }
  }
  if (base + size != base_ + size_) {
    fbl::AllocChecker ac;
    const vaddr_t offset = base + size - base_;
    right = fbl::AdoptRef(new (&ac) VmMapping(*parent_, base_ + offset, size_ - offset, flags_,
                                              object_, object_offset_locked() + offset,
                                              MappingProtectionRanges(0), Mergeable::YES));
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }
  }

  // Grab the lock for the vmo. This is acquired here so that it is held continuously over both the
  // architectural unmap and removing the current mapping from the VMO.
  DEBUG_ASSERT(object_);
  Guard<CriticalMutex> guard{object_->lock()};

  zx_status_t status =
      aspace_->arch_aspace().Unmap(base, size / PAGE_SIZE, aspace_->EnlargeArchUnmap());
  ASSERT(status == ZX_OK);

  // Split the protection_ranges_ from this mapping into the new mapping(s). This has be done after
  // the mapping construction as this step is destructive and hard to rollback.
  if (right) {
    AssertHeld(right->lock_ref());
    AssertHeld(right->object_lock_ref());
    MappingProtectionRanges right_prot = protection_ranges_.SplitAt(base + size);
    right->protection_ranges_ = ktl::move(right_prot);
  }
  if (left) {
    AssertHeld(left->lock_ref());
    AssertHeld(left->object_lock_ref());
    protection_ranges_.DiscardAbove(base);
    left->protection_ranges_ = ktl::move(protection_ranges_);
  }

  // Now finish destroying this mapping, but remember any memory_priority_ to apply to the new
  // mappings.
  const MemoryPriority old_priority = memory_priority_;
  status = DestroyLockedObject(false);
  ASSERT(status == ZX_OK);

  // Install the new mappings and set their memory priorities.
  auto finish_mapping = [old_priority](fbl::RefPtr<VmMapping>& mapping) {
    if (mapping) {
      AssertHeld(mapping->lock_ref());
      AssertHeld(mapping->object_lock_ref());
      mapping->ActivateLocked();
      zx_status_t status = mapping->SetMemoryPriorityLockedObject(old_priority);
      ASSERT(status == ZX_OK);
    }
  };
  finish_mapping(left);
  finish_mapping(right);
  return ZX_OK;
}

bool VmMapping::ObjectRangeToVaddrRange(uint64_t offset, uint64_t len, vaddr_t* base,
                                        uint64_t* virtual_len) const {
  DEBUG_ASSERT(IS_PAGE_ROUNDED(offset));
  DEBUG_ASSERT(IS_PAGE_ROUNDED(len));
  DEBUG_ASSERT(base);
  DEBUG_ASSERT(virtual_len);

  // Zero sized ranges are considered to have no overlap.
  if (len == 0) {
    *base = 0;
    *virtual_len = 0;
    return false;
  }

  // compute the intersection of the passed in vmo range and our mapping
  uint64_t offset_new;
  if (!GetIntersect(object_offset_locked_object(), static_cast<uint64_t>(size_locked_object()),
                    offset, len, &offset_new, virtual_len)) {
    return false;
  }

  DEBUG_ASSERT(*virtual_len > 0 && *virtual_len <= SIZE_MAX);
  DEBUG_ASSERT(offset_new >= object_offset_locked_object());

  LTRACEF("intersection offset %#" PRIx64 ", len %#" PRIx64 "\n", offset_new, *virtual_len);

  // make sure the base + offset is within our address space
  // should be, according to the range stored in base_ + size_
  bool overflowed =
      add_overflow(base_locked_object(), offset_new - object_offset_locked_object(), base);
  ASSERT(!overflowed);

  // make sure we're only operating within our window
  ASSERT(*base >= base_locked_object());
  ASSERT((*base + *virtual_len - 1) <= (base_locked_object() + size_locked_object() - 1));

  return true;
}

void VmMapping::AspaceUnmapLockedObject(uint64_t offset, uint64_t len, UnmapOptions options) const {
  canary_.Assert();

  // NOTE: must be acquired with the vmo lock held, but doesn't need to take
  // the address space lock, since it will not manipulate its location in the
  // vmar tree. However, it must be held in the ALIVE state across this call.
  //
  // Avoids a race with DestroyLocked() since it removes ourself from the VMO's
  // mapping list with the VMO lock held before dropping this state to DEAD. The
  // VMO cant call back to us once we're out of their list.
  DEBUG_ASSERT(get_state_locked_object() == LifeCycleState::ALIVE);

  // |object_| itself is not accessed in this method, and we do not hold the correct lock for it,
  // but we know the object_->lock() is held and so therefore object_ is valid and will not be
  // modified. Therefore it's correct to read object_ here for the purposes of an assert, but cannot
  // be expressed nicely with regular annotations.
  [&]() TA_NO_THREAD_SAFETY_ANALYSIS { DEBUG_ASSERT(object_); }();

  // In the case of unmapping known instances of the zero page check if this range intersects with
  // an in progress fault. If it does we can skip the unmap with the knowledge that the mapping will
  // be updated later. This is safe since the zero page is, by definition, only mapped read only,
  // and is never modified so delaying the update of the mapping cannot cause either any users to
  // see incorrect data, or users to be able to modify an old mapping.
  if ((options & UnmapOptions::OnlyHasZeroPages) && currently_faulting_ &&
      currently_faulting_->UnmapRange(offset, len)) {
    return;
  }

  LTRACEF("region %p obj_offset %#" PRIx64 " size %zu, offset %#" PRIx64 " len %#" PRIx64 "\n",
          this, object_offset_locked_object(), size_, offset, len);

  // See if there's an intersect.
  vaddr_t base;
  uint64_t new_len;
  if (!ObjectRangeToVaddrRange(offset, len, &base, &new_len)) {
    return;
  }

  // If this is a kernel mapping then we should not be removing mappings out of the arch aspace,
  // unless this mapping has explicitly opted out of this check.
  DEBUG_ASSERT(aspace_->is_user() || aspace_->is_guest_physical() ||
               flags_ & VMAR_FLAG_DEBUG_DYNAMIC_KERNEL_MAPPING);

  auto aspace_op = aspace_->EnlargeArchUnmap();
  if (options & UnmapOptions::Harvest) {
    aspace_op |= ArchUnmapOptions::Harvest;
  }

  zx_status_t status = aspace_->arch_aspace().Unmap(base, new_len / PAGE_SIZE, aspace_op);
  ASSERT(status == ZX_OK);
}

void VmMapping::AspaceRemoveWriteLockedObject(uint64_t offset, uint64_t len) const {
  LTRACEF("region %p obj_offset %#" PRIx64 " size %zu, offset %#" PRIx64 " len %#" PRIx64 "\n",
          this, object_offset_, size_, offset, len);

  canary_.Assert();

  // NOTE: must be acquired with the vmo lock held, but doesn't need to take
  // the address space lock, since it will not manipulate its location in the
  // vmar tree. However, it must be held in the ALIVE state across this call.
  //
  // Avoids a race with DestroyLocked() since it removes ourself from the VMO's
  // mapping list with the VMO lock held before dropping this state to DEAD. The
  // VMO cant call back to us once we're out of their list.
  DEBUG_ASSERT(get_state_locked_object() == LifeCycleState::ALIVE);

  // |object_| itself is not accessed in this method, and we do not hold the correct lock for it,
  // but we know the object_->lock() is held and so therefore object_ is valid and will not be
  // modified. Therefore it's correct to read object_ here for the purposes of an assert, but cannot
  // be expressed nicely with regular annotations.
  [&]() TA_NO_THREAD_SAFETY_ANALYSIS { DEBUG_ASSERT(object_); }();

  // If this doesn't support writing then nothing to be done, as we know we have no write mappings.
  if (!(flags_ & VMAR_FLAG_CAN_MAP_WRITE)) {
    return;
  }

  // See if there's an intersect.
  vaddr_t base;
  uint64_t new_len;
  if (!ObjectRangeToVaddrRange(offset, len, &base, &new_len)) {
    return;
  }

  // If this is a kernel mapping then we should not be modify mappings in the arch aspace,
  // unless this mapping has explicitly opted out of this check.
  DEBUG_ASSERT_MSG(aspace_->is_user() || aspace_->is_guest_physical() ||
                       flags_ & VMAR_FLAG_DEBUG_DYNAMIC_KERNEL_MAPPING,
                   "region %p obj_offset %#" PRIx64 " size %zu, offset %#" PRIx64 " len %#" PRIx64
                   "\n",
                   this, object_offset_locked_object(), size_locked_object(), offset, len);

  zx_status_t status = ProtectRangesLockedObject().EnumerateProtectionRanges(
      base_locked_object(), size_locked_object(), base, new_len,
      [this](vaddr_t region_base, size_t region_len, uint mmu_flags) {
        // If this range doesn't currently support being writable then we can skip.
        if (!(mmu_flags & ARCH_MMU_FLAG_PERM_WRITE)) {
          return ZX_ERR_NEXT;
        }

        // Build new mmu flags without writing.
        mmu_flags &= ~(ARCH_MMU_FLAG_PERM_WRITE);

        zx_status_t result = ProtectOrUnmap(aspace_, region_base, region_len, mmu_flags);
        if (result == ZX_OK) {
          return ZX_ERR_NEXT;
        }
        return result;
      });
  ASSERT(status == ZX_OK);
}

void VmMapping::AspaceDebugUnpinLockedObject(uint64_t offset, uint64_t len) const {
  LTRACEF("region %p obj_offset %#" PRIx64 " size %zu, offset %#" PRIx64 " len %#" PRIx64 "\n",
          this, object_offset_, size_, offset, len);

  canary_.Assert();

  // NOTE: must be acquired with the vmo lock held, but doesn't need to take
  // the address space lock, since it will not manipulate its location in the
  // vmar tree. However, it must be held in the ALIVE state across this call.
  //
  // Avoids a race with DestroyLocked() since it removes ourself from the VMO's
  // mapping list with the VMO lock held before dropping this state to DEAD. The
  // VMO cant call back to us once we're out of their list.
  DEBUG_ASSERT(get_state_locked_object() == LifeCycleState::ALIVE);

  // See if there's an intersect.
  vaddr_t base;
  uint64_t new_len;
  if (!ObjectRangeToVaddrRange(offset, len, &base, &new_len)) {
    return;
  }

  // This unpin is not allowed for kernel mappings, unless the mapping has specifically opted out of
  // this debug check due to it performing its own dynamic management.
  DEBUG_ASSERT(aspace_->is_user() || aspace_->is_guest_physical() ||
               flags_ & VMAR_FLAG_DEBUG_DYNAMIC_KERNEL_MAPPING);
}

namespace {

// Helper class for batching installing mappings into the arch aspace. The mappings aspace and
// object lock must be held over the entirety of the lifetime of this object, without ever being
// released.
template <size_t NumPages>
class VmMappingCoalescer {
 public:
  VmMappingCoalescer(VmMapping* mapping, vaddr_t base, uint mmu_flags,
                     ArchVmAspace::ExistingEntryAction existing_entry_action)
      TA_REQ(mapping->lock()) TA_REQ(mapping->object_lock());
  ~VmMappingCoalescer();

  // Add a page to the mapping run.
  zx_status_t Append(vaddr_t vaddr, paddr_t paddr) {
    // If this isn't the expected vaddr, flush the run we have first.
    if (!can_append(vaddr)) {
      zx_status_t status = Flush();
      if (status != ZX_OK) {
        return status;
      }
      base_ = vaddr;
    }
    phys_[count_] = paddr;
    ++count_;
    return ZX_OK;
  }

  zx_status_t AppendOrAdjustMapping(vaddr_t vaddr, paddr_t paddr, uint mmu_flags) {
    // If this isn't the expected vaddr or mmu_flags have changed, flush the run we have first.
    if (!can_append(vaddr) || mmu_flags != mmu_flags_) {
      zx_status_t status = Flush();
      if (status != ZX_OK) {
        return status;
      }
      base_ = vaddr;
      mmu_flags_ = mmu_flags;
    }

    phys_[count_] = paddr;
    ++count_;
    return ZX_OK;
  }

  // How much space remains in the phys_ array, starting from vaddr, that can be used to
  // opportunistically map additional pages.
  size_t ExtraPageCapacityFrom(vaddr_t vaddr) {
    // vaddr must be appendable & the coalescer can't be empty.
    return (can_append(vaddr) && count_ != 0) ? NumPages - count_ : 0;
  }

  // Functions for the user to manually manage the pages array. It is up to the user to manage the
  // page count and ensure the coalescer doesn't overflow, maintains the correct page count and that
  // the pages are contiguous.
  paddr_t* GetNextPageSlot() { return &phys_[count_]; }

  uint GetMmuFlags() { return mmu_flags_; }

  void IncrementCount(size_t i) { count_ += i; }

  // Submit any outstanding mappings to the MMU.
  zx_status_t Flush();

  size_t TotalMapped() { return total_mapped_; }

  // Drop the current outstanding mappings without sending them to the MMU.
  void Drop() { count_ = 0; }

 private:
  // Vaddr can be appended if it's the next free slot and the coalescer isn't full.
  bool can_append(vaddr_t vaddr) {
    return count_ < ktl::size(phys_) && vaddr == base_ + count_ * PAGE_SIZE;
  }

  DISALLOW_COPY_ASSIGN_AND_MOVE(VmMappingCoalescer);

  VmMapping* mapping_;
  vaddr_t base_;
  paddr_t phys_[NumPages];
  size_t count_;
  size_t total_mapped_ = 0;
  uint mmu_flags_;
  const ArchVmAspace::ExistingEntryAction existing_entry_action_;
};

template <size_t NumPages>
VmMappingCoalescer<NumPages>::VmMappingCoalescer(
    VmMapping* mapping, vaddr_t base, uint mmu_flags,
    ArchVmAspace::ExistingEntryAction existing_entry_action)
    : mapping_(mapping),
      base_(base),
      count_(0),
      mmu_flags_(mmu_flags),
      existing_entry_action_(existing_entry_action) {
  // Mapping is only valid if there is at least some access in the flags.
  DEBUG_ASSERT(mmu_flags & ARCH_MMU_FLAG_PERM_RWX_MASK);
}

template <size_t NumPages>
VmMappingCoalescer<NumPages>::~VmMappingCoalescer() {
  // Make sure no outstanding mappings.
  DEBUG_ASSERT(count_ == 0);
}

template <size_t NumPages>
zx_status_t VmMappingCoalescer<NumPages>::Flush() {
  if (count_ == 0) {
    return ZX_OK;
  }

  VM_KTRACE_DURATION(2, "map_page", ("va", ktrace::Pointer{base_}), ("count", count_),
                     ("mmu_flags", mmu_flags_));

  // Assert that we're not accidentally mapping the zero page writable. Unless called from a kernel
  // aspace, as the zero page can be mapped writeable from the kernel aspace in mexec.
  DEBUG_ASSERT(
      !(mmu_flags_ & ARCH_MMU_FLAG_PERM_WRITE) ||
      ktl::all_of(phys_, &phys_[count_], [](paddr_t p) { return p != vm_get_zero_page_paddr(); }) ||
      !mapping_->aspace()->is_user());

  zx_status_t ret = mapping_->aspace()->arch_aspace().Map(base_, phys_, count_, mmu_flags_,
                                                          existing_entry_action_);
  if (ret != ZX_OK) {
    TRACEF("error %d mapping %zu pages starting at va %#" PRIxPTR "\n", ret, count_, base_);
  }
  base_ += count_ * PAGE_SIZE;
  total_mapped_ += count_;
  count_ = 0;
  return ret;
}

}  // namespace

zx_status_t VmMapping::MapRange(size_t offset, size_t len, bool commit, bool ignore_existing) {
  Guard<CriticalMutex> aspace_guard{lock()};
  canary_.Assert();

  len = ROUNDUP_PAGE_SIZE(len);
  if (len == 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (state_ != LifeCycleState::ALIVE) {
    return ZX_ERR_BAD_STATE;
  }

  LTRACEF("region %p, offset %#zx, size %#zx, commit %d\n", this, offset, len, commit);

  DEBUG_ASSERT(object_);
  if (!IS_PAGE_ROUNDED(offset) || !is_in_range_locked(base_ + offset, len)) {
    return ZX_ERR_INVALID_ARGS;
  }

  // If this is a kernel mapping then validate that all pages being mapped are currently pinned,
  // ensuring that they cannot be taken away for any reason, unless the mapping has specifically
  // opted out of this debug check due to it performing its own dynamic management.
  DEBUG_ASSERT(aspace_->is_user() || aspace_->is_guest_physical() ||
               (flags_ & VMAR_FLAG_DEBUG_DYNAMIC_KERNEL_MAPPING) ||
               object_->DebugIsRangePinned(object_offset_locked() + offset, len));

  // Cache whether the object is dirty tracked, we need to know this when computing mmu flags later.
  const bool dirty_tracked = object_->is_dirty_tracked();

  // The region to map could have multiple different current arch mmu flags, so we need to iterate
  // over them to ensure we install mappings with the correct permissions.
  return EnumerateProtectionRangesLocked(
      base_ + offset, len,
      [this, commit, dirty_tracked, ignore_existing](vaddr_t base, size_t len, uint mmu_flags) {
        AssertHeld(lock_ref());

        // Remove the write permission if this maps a vmo that supports dirty tracking, in order to
        // trigger write permission faults when writes occur, enabling us to track when pages are
        // dirtied.
        if (dirty_tracked) {
          mmu_flags &= ~ARCH_MMU_FLAG_PERM_WRITE;
        }

        // If there are no access permissions on this region then mapping has no effect, so skip.
        if (!(mmu_flags & ARCH_MMU_FLAG_PERM_RWX_MASK)) {
          return ZX_ERR_NEXT;
        }

        // In the scenario where we are committing, and calling RequireOwnedPage, we are supposed to
        // pass in a non-null LazyPageRequest. Technically we could get away with not passing in a
        // PageRequest since:
        //  * Only internal kernel VMOs will have the 'commit' flag passed in for their mappings
        //  * Only pager backed VMOs or VMOs that support delayed memory allocations need to fill
        //    out a PageRequest
        //  * Internal kernel VMOs are never pager backed or have the delayed memory allocation flag
        //    set.
        // However, should these assumptions ever get violated it's better to catch this gracefully
        // than have RequireOwnedPage error/crash internally, and it costs nothing to create and
        // pass in.
        __UNINITIALIZED MultiPageRequest page_request;

        const uint64_t map_offset = base - base_;
        const uint64_t vmo_offset = object_offset_locked() + map_offset;
        if (VmObjectPaged* paged = DownCastVmObject<VmObjectPaged>(object_.get()); likely(paged)) {
          // grab the lock for the vmo
          __UNINITIALIZED VmCowPages::DeferredOps deferred(paged->MakeDeferredOps());
          Guard<CriticalMutex> guard{AssertOrderedAliasedLock, paged->lock(), object_->lock(),
                                     paged->lock_order()};

          // Trim our range to the current VMO size. Our mapping might exceed the VMO in the case
          // where the VMO is resizable, and this should not be considered an error.
          len = TrimmedObjectRangeLocked(map_offset, len);
          if (len == 0) {
            return ZX_ERR_STOP;
          }

          VmMappingCoalescer<16> coalescer(this, base, mmu_flags,
                                           ignore_existing
                                               ? ArchVmAspace::ExistingEntryAction::Skip
                                               : ArchVmAspace::ExistingEntryAction::Error);

          const bool writing = mmu_flags & ARCH_MMU_FLAG_PERM_WRITE;
          __UNINITIALIZED auto cursor = paged->GetLookupCursorLocked(vmo_offset, len);
          if (cursor.is_error()) {
            return cursor.error_value();
          }
          // Do not consider pages touched when mapping in, if they are actually touched they will
          // get an accessed bit set in the hardware.
          cursor->DisableMarkAccessed();
          AssertHeld(cursor->lock_ref());
          for (uint64_t off = 0; off < len; off += PAGE_SIZE) {
            vm_page_t* page = nullptr;
            if (commit) {
              __UNINITIALIZED zx::result<VmCowPages::LookupCursor::RequireResult> result =
                  cursor->RequireOwnedPage(writing, 1, deferred, &page_request);
              if (result.is_error()) {
                zx_status_t status = result.error_value();
                // As per the comment above page_request definition, there should never be commit
                // + pager backed VMO and so we should never end up with a PageRequest needing to be
                // waited on.
                ASSERT(status != ZX_ERR_SHOULD_WAIT);
                // fail when we can't commit every requested page
                coalescer.Drop();
                return status;
              }
              page = result->page;
            } else {
              // Not committing so get a page if one exists. This increments the cursor, returning
              // nullptr if no page.
              page = cursor->MaybePage(writing);
              // This page was not present and if we are in a run of absent pages we would like to
              // efficiently skip them, instead of querying each virtual address individually. Due
              // to the assumptions of the cursor, we cannot call SkipMissingPages if we had just
              // requested the last page in the range of the cursor.
              if (!page && off + PAGE_SIZE < len) {
                // Increment |off| for the any pages we skip and let the original page from
                // MaybePage get incremented on the way around the loop before the range gets
                // checked.
                off += cursor->SkipMissingPages() * PAGE_SIZE;
              }
            }
            if (page) {
              zx_status_t status = coalescer.Append(base + off, page->paddr());
              if (status != ZX_OK) {
                return status;
              }
            }
          }
          zx_status_t status = coalescer.Flush();
          return status == ZX_OK ? ZX_ERR_NEXT : status;
        } else if (VmObjectPhysical* phys = DownCastVmObject<VmObjectPhysical>(object_.get());
                   phys) {
          // grab the lock for the vmo
          Guard<CriticalMutex> object_guard{AliasedLock, phys->lock(), object_->lock()};
          // Physical VMOs are never resizable, so do not need to worry about trimming the range.
          DEBUG_ASSERT(!phys->is_resizable());
          VmMappingCoalescer<16> coalescer(this, base, mmu_flags,
                                           ignore_existing
                                               ? ArchVmAspace::ExistingEntryAction::Skip
                                               : ArchVmAspace::ExistingEntryAction::Error);

          // Physical VMOs are always allocated and contiguous, just need to get the paddr.
          paddr_t phys_base = 0;
          zx_status_t status = phys->LookupContiguousLocked(vmo_offset, len, &phys_base);
          ASSERT(status == ZX_OK);

          for (size_t offset = 0; offset < len; offset += PAGE_SIZE) {
            status = coalescer.Append(base + offset, phys_base + offset);
            if (status != ZX_OK) {
              return status;
            }
          }
          status = coalescer.Flush();
          return status == ZX_OK ? ZX_ERR_NEXT : status;
        } else {
          panic("VmObject should be paged or physical");
          return ZX_ERR_INTERNAL;
        }
      });
}

zx_status_t VmMapping::DecommitRange(size_t offset, size_t len) {
  canary_.Assert();
  LTRACEF("%p [%#zx+%#zx], offset %#zx, len %#zx\n", this, base_, size_, offset, len);

  Guard<CriticalMutex> guard{lock()};
  if (state_ != LifeCycleState::ALIVE) {
    return ZX_ERR_BAD_STATE;
  }
  if (offset + len < offset || offset + len > size_) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  // VmObject::DecommitRange will typically call back into our instance's
  // VmMapping::AspaceUnmapLockedObject.
  return object_->DecommitRange(object_offset_locked() + offset, len);
}

zx_status_t VmMapping::DestroyLocked() {
  canary_.Assert();
  // Keep a refptr to the object_ so we know our lock remains valid.
  fbl::RefPtr<VmObject> object(object_);
  Guard<CriticalMutex> guard{object_->lock()};
  return DestroyLockedObject(true);
}

zx_status_t VmMapping::DestroyLockedObject(bool unmap) {
  // Take a reference to ourself, so that we do not get destructed after
  // dropping our last reference in this method (e.g. when calling
  // subregions_.erase below).
  fbl::RefPtr<VmMapping> self(this);

  // If this is the last_fault_ then clear it before removing from the VMAR tree. Even if this
  // destroy fails, it's always safe to clear last_fault_, so we preference doing it upfront for
  // clarity.
  if (aspace_->last_fault_ == this) {
    aspace_->last_fault_ = nullptr;
  }

  // The vDSO code mapping can never be unmapped, not even
  // by VMAR destruction (except for process exit, of course).
  // TODO(mcgrathr): Turn this into a policy-driven process-fatal case
  // at some point.  teisenbe@ wants to eventually make zx_vmar_destroy
  // never fail.
  if (aspace_->vdso_code_mapping_ == self) {
    return ZX_ERR_ACCESS_DENIED;
  }

  // Remove any priority.
  zx_status_t status = SetMemoryPriorityLockedObject(MemoryPriority::DEFAULT);
  DEBUG_ASSERT(status == ZX_OK);

  if (unmap) {
    status = aspace_->arch_aspace().Unmap(base_, size_ / PAGE_SIZE, aspace_->EnlargeArchUnmap());
    if (status != ZX_OK) {
      return status;
    }
  }
  protection_ranges_.clear();
  object_->RemoveMappingLocked(this);

  // Detach the region from the parent.
  if (parent_) {
    AssertHeld(parent_->lock_ref());
    DEBUG_ASSERT(this->in_subregion_tree());
    parent_->subregions_.RemoveRegion(this);
  }

  // The size may only be set to zero when not in the subregion tree.
  set_size_locked(0);

  // detach from any object we have mapped. Note that we are holding the aspace_->lock() so we
  // will not race with other threads calling vmo()
  object_.reset();

  // mark ourself as dead
  parent_ = nullptr;
  state_ = LifeCycleState::DEAD;
  return ZX_OK;
}

ktl::pair<zx_status_t, uint32_t> VmMapping::PageFaultLocked(vaddr_t va, const uint pf_flags,
                                                            const size_t additional_pages,
                                                            MultiPageRequest* page_request) {
  VM_KTRACE_DURATION(
      2, "VmMapping::PageFault",
      ("user_id", KTRACE_ANNOTATED_VALUE(AssertHeld(lock_ref()), object_->user_id())),
      ("va", ktrace::Pointer{va}));
  canary_.Assert();

  DEBUG_ASSERT(IS_PAGE_ROUNDED(va));

  // Fault batch size when num_pages > 1.
  static constexpr uint64_t kBatchPages = 16;

  const uint64_t vmo_offset = va - base_ + object_offset_locked();

  [[maybe_unused]] char pf_string[5];
  LTRACEF("%p va %#" PRIxPTR " vmo_offset %#" PRIx64 ", pf_flags %#x (%s)\n", this, va, vmo_offset,
          pf_flags, vmm_pf_flags_to_string(pf_flags, pf_string));

  // Need to look up the mmu flags for this virtual address, as well as how large a region those
  // flags are for so we can cap the extra mappings we create.
  const MappingProtectionRanges::FlagsRange range =
      ProtectRangesLocked().FlagsRangeAtAddr(base_, size_, va);

  // Build the mmu flags we need to have based on the page fault. This strategy of building the
  // flags and then comparing all at once allows the compiler to provide much better code gen.
  uint needed_mmu_flags = 0;
  if (pf_flags & VMM_PF_FLAG_USER) {
    needed_mmu_flags |= ARCH_MMU_FLAG_PERM_USER;
  }
  const bool write = pf_flags & VMM_PF_FLAG_WRITE;
  if (write) {
    needed_mmu_flags |= ARCH_MMU_FLAG_PERM_WRITE;
  } else {
    needed_mmu_flags |= ARCH_MMU_FLAG_PERM_READ;
  }
  if (pf_flags & VMM_PF_FLAG_INSTRUCTION) {
    needed_mmu_flags |= ARCH_MMU_FLAG_PERM_EXECUTE;
  }
  // Check that all the needed flags are present.
  if (unlikely((range.mmu_flags & needed_mmu_flags) != needed_mmu_flags)) {
    if ((pf_flags & VMM_PF_FLAG_USER) && !(range.mmu_flags & ARCH_MMU_FLAG_PERM_USER)) {
      // user page fault on non user mapped region
      LTRACEF("permission failure: user fault on non user region\n");
    }
    if ((pf_flags & VMM_PF_FLAG_WRITE) && !(range.mmu_flags & ARCH_MMU_FLAG_PERM_WRITE)) {
      // write to a non-writeable region
      LTRACEF("permission failure: write fault on non-writable region\n");
    }
    if (!(pf_flags & VMM_PF_FLAG_WRITE) && !(range.mmu_flags & ARCH_MMU_FLAG_PERM_READ)) {
      // read to a non-readable region
      LTRACEF("permission failure: read fault on non-readable region\n");
    }
    if ((pf_flags & VMM_PF_FLAG_INSTRUCTION) && !(range.mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE)) {
      // instruction fetch from a no execute region
      LTRACEF("permission failure: execute fault on no execute region\n");
    }
    return {ZX_ERR_ACCESS_DENIED, 0};
  }

  // Calculate the number of pages from va until the end of the protection range.
  const size_t num_protection_range_pages = (range.region_top - va) / PAGE_SIZE;

  // Helper lambda that calculates two values:
  //  * Number of pages we're aiming to fault. If a range > 1 page is supplied, it is assumed the
  //    user knows the appropriate range, so opportunistic pages will not be added.
  //  * Number of requested pages, trimmed to protection range & VMO.
  // Requires the vmo_size to be passed in, which cannot be known until after the lock is acquired
  // in each of the branches.
  auto calculate_pages = [&](size_t vmo_size) -> ktl::optional<ktl::pair<size_t, size_t>> {
    if (vmo_offset >= vmo_size) {
      return ktl::nullopt;
    }
    const size_t num_vmo_pages = (vmo_size - vmo_offset) / PAGE_SIZE;
    if (additional_pages == 0) {
      // Calculate the number of pages from va until the end of the page table, so we don't make
      // extra page table allocations for opportunistic pages.
      const uint64_t next_pt_base = ArchVmAspace::NextUserPageTableOffset(va);
      const size_t num_pt_pages = (next_pt_base - va) / PAGE_SIZE;
      // Number of opportunistic pages we can fault, including the required page.
      const size_t num_fault_pages = ktl::min(
          {kPageFaultMaxOptimisticPages, num_pt_pages, num_protection_range_pages, num_vmo_pages});
      return ktl::optional<ktl::pair<size_t, size_t>>({1, num_fault_pages});
    } else {
      // Cap by requested pages.
      const size_t num_pages =
          ktl::min({num_protection_range_pages, num_vmo_pages, additional_pages + 1});
      DEBUG_ASSERT(num_pages > 0);
      return ktl::optional<ktl::pair<size_t, size_t>>({num_pages, num_pages});
    }
  };

  static constexpr uint64_t coalescer_size = ktl::max(kPageFaultMaxOptimisticPages, kBatchPages);

  if (VmObjectPaged* paged = DownCastVmObject<VmObjectPaged>(object_.get()); paged) {
    __UNINITIALIZED VmCowPages::DeferredOps deferred(paged->MakeDeferredOps());
    Guard<CriticalMutex> guard{AssertOrderedAliasedLock, paged->lock(), object_->lock(),
                               paged->lock_order()};

    // If fault-beyond-stream-size is set, throw exception on memory accesses past the page
    // containing the user defined stream size.
    const uint64_t vmo_size = (flags_ & VMAR_FLAG_FAULT_BEYOND_STREAM_SIZE)
                                  ? *paged->saturating_stream_size_locked()
                                  : paged->size_locked();
    auto pages = calculate_pages(vmo_size);
    if (!pages) {
      return {ZX_ERR_OUT_OF_RANGE, 0};
    }
    auto [num_required_pages, num_fault_pages] = *pages;

    // Opportunistic pages are not considered in currently_faulting optimisation, as it is not
    // guaranteed the mappings will be updated.
    CurrentlyFaulting currently_faulting(this, vmo_offset, num_required_pages * PAGE_SIZE);

    __UNINITIALIZED VmMappingCoalescer<coalescer_size> coalescer(
        this, va, range.mmu_flags, ArchVmAspace::ExistingEntryAction::Upgrade);

    // fault in or grab existing pages.
    const size_t cursor_size = num_fault_pages * PAGE_SIZE;
    __UNINITIALIZED auto cursor = paged->GetLookupCursorLocked(vmo_offset, cursor_size);
    if (cursor.is_error()) {
      return {cursor.error_value(), coalescer.TotalMapped()};
    }
    // Do not consider pages touched when mapping in, if they are actually touched they will
    // get an accessed bit set in the hardware.
    cursor->DisableMarkAccessed();
    AssertHeld(cursor->lock_ref());

    // Fault requested pages.
    uint64_t offset = 0;
    for (; offset < (num_required_pages * PAGE_SIZE); offset += PAGE_SIZE) {
      uint curr_mmu_flags = range.mmu_flags;

      uint num_curr_pages = static_cast<uint>(num_required_pages - (offset / PAGE_SIZE));
      __UNINITIALIZED zx::result<VmCowPages::LookupCursor::RequireResult> result =
          cursor->RequirePage(write, num_curr_pages, deferred, page_request);
      if (result.is_error()) {
        coalescer.Flush();
        return {result.error_value(), coalescer.TotalMapped()};
      }

      DEBUG_ASSERT(!write || result->writable);

      // We looked up in order to write. Mark as modified. Only need to do this once.
      if (write && offset == 0) {
        object_->mark_modified_locked();
      }

      // If we read faulted, and lookup didn't say that this is always writable, then we map or
      // modify the page without any write permissions. This ensures we will fault again if a write
      // is attempted so we can potentially replace this page with a copy or a new one, or update
      // the page's dirty state.
      if (!write && !result->writable) {
        // we read faulted, so only map with read permissions
        curr_mmu_flags &= ~ARCH_MMU_FLAG_PERM_WRITE;
      }

      zx_status_t status =
          coalescer.AppendOrAdjustMapping(va + offset, result->page->paddr(), curr_mmu_flags);
      if (status != ZX_OK) {
        // Flush any existing pages in the coalescer.
        coalescer.Flush();
        return {status, coalescer.TotalMapped()};
      }
    }

    // Fault opportunistic pages. If a range is supplied, it is assumed the user knows the
    // appropriate range, so opportunistic pages will not be fault.
    if (additional_pages == 0) {
      DEBUG_ASSERT(num_fault_pages > 0);
      // Check how much space the coalescer has for faulting additional pages.
      size_t extra_pages = coalescer.ExtraPageCapacityFrom(va + PAGE_SIZE);
      extra_pages = ktl::min(extra_pages, num_fault_pages - 1);

      // Acquire any additional pages, but only if they already exist as the user has not attempted
      // to use these pages yet.
      if (extra_pages > 0) {
        bool writeable = (coalescer.GetMmuFlags() & ARCH_MMU_FLAG_PERM_WRITE);
        size_t num_extra_pages = cursor->IfExistPages(writeable, static_cast<uint>(extra_pages),
                                                      coalescer.GetNextPageSlot());
        coalescer.IncrementCount(num_extra_pages);
      }
    }
    zx_status_t status = coalescer.Flush();
    if (status == ZX_OK) {
      // Mapping has been successfully updated by us. Inform the faulting helper so that it knows
      // not to unmap the range instead.
      currently_faulting.MappingUpdated();
    }
    return {status, coalescer.TotalMapped()};
  } else if (VmObjectPhysical* phys = DownCastVmObject<VmObjectPhysical>(object_.get()); phys) {
    Guard<CriticalMutex> guard{AliasedLock, phys->lock(), object_->lock()};

    auto pages = calculate_pages(phys->size_locked());
    if (!pages) {
      return {ZX_ERR_OUT_OF_RANGE, 0};
    }
    auto [num_required_pages, num_fault_pages] = *pages;

    // Opportunistic pages are not considered in currently_faulting optimisation, as it is not
    // guaranteed the mappings will be updated.
    CurrentlyFaulting currently_faulting(this, vmo_offset, num_required_pages * PAGE_SIZE);

    __UNINITIALIZED VmMappingCoalescer<coalescer_size> coalescer(
        this, va, range.mmu_flags, ArchVmAspace::ExistingEntryAction::Upgrade);

    // Already validated the size, and since physical VMOs are always allocated, and not
    // resizable, we know we can always retrieve the maximum number of pages without failure.
    uint64_t phys_len = num_fault_pages * PAGE_SIZE;
    paddr_t phys_base = 0;
    zx_status_t status = phys->LookupContiguousLocked(vmo_offset, phys_len, &phys_base);

    ASSERT(status == ZX_OK);

    status = coalescer.AppendOrAdjustMapping(va, phys_base, range.mmu_flags);
    if (status != ZX_OK) {
      return {status, coalescer.TotalMapped()};
    }

    // Extrapolate the pages from the base address.
    for (size_t offset = PAGE_SIZE; offset < phys_len; offset += PAGE_SIZE) {
      status = coalescer.Append(va + offset, phys_base + offset);
      if (status != ZX_OK) {
        return {status, coalescer.TotalMapped()};
      }
    }

    status = coalescer.Flush();
    if (status == ZX_OK) {
      // Mapping has been successfully updated by us. Inform the faulting helper so that it knows
      // not to unmap the range instead.
      currently_faulting.MappingUpdated();
    }
    return {status, coalescer.TotalMapped()};
  }
  panic("Unknown VMO type");
  return {ZX_ERR_INTERNAL, 0};
}

void VmMapping::ActivateLocked() {
  DEBUG_ASSERT(state_ == LifeCycleState::NOT_READY);
  DEBUG_ASSERT(parent_);

  state_ = LifeCycleState::ALIVE;
  object_->AddMappingLocked(this);

  // Now that we have added a mapping to the VMO it's cache policy becomes fixed, and we can read it
  // and augment our arch_mmu_flags.
  uint32_t cache_policy = object_->GetMappingCachePolicyLocked();
  uint arch_mmu_flags = protection_ranges_.FirstRegionMmuFlags();
  if ((arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) != cache_policy) {
    // Warn in the event that we somehow receive a VMO that has a cache
    // policy set while also holding cache policy flags within the arch
    // flags. The only path that should be able to achieve this is if
    // something in the kernel maps into their aspace incorrectly.
    if ((arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) != 0) {
      TRACEF(
          "warning: mapping has conflicting cache policies: vmo %#02x "
          "arch_mmu_flags %#02x.\n",
          cache_policy, arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK);
      // Clear the existing cache policy and use the new one.
      arch_mmu_flags &= ~ARCH_MMU_FLAG_CACHE_MASK;
    }
    // If we are changing the cache policy then this can only happen if this is a new mapping region
    // and not a new mapping occurring as a result of an unmap split. In the case of a new mapping
    // region we know there cannot yet be any protection ranges.
    DEBUG_ASSERT(protection_ranges_.IsSingleRegion());
    arch_mmu_flags |= cache_policy;
    protection_ranges_.SetFirstRegionMmuFlags(arch_mmu_flags);
  }

  AssertHeld(parent_->lock_ref());
  parent_->subregions_.InsertRegion(fbl::RefPtr<VmAddressRegionOrMapping>(this));
}

void VmMapping::Activate() {
  Guard<CriticalMutex> guard{object_->lock()};
  ActivateLocked();
}

void VmMapping::TryMergeRightNeighborLocked(VmMapping* right_candidate) {
  AssertHeld(right_candidate->lock_ref());

  // This code is tolerant of many 'miss calls' if mappings aren't mergeable or are not neighbours
  // etc, but the caller should not be attempting to merge if these mappings are not actually from
  // the same vmar parent. Doing so indicates something structurally wrong with the hierarchy.
  DEBUG_ASSERT(parent_ == right_candidate->parent_);

  // Should not be able to have the same parent yet have gotten a different memory priority.
  DEBUG_ASSERT(memory_priority_ == right_candidate->memory_priority_);

  // These tests are intended to be ordered such that we fail as fast as possible. As such testing
  // for mergeability, which we commonly expect to succeed and not fail, is done last.

  // Need to refer to the same object.
  if (object_.get() != right_candidate->object_.get()) {
    return;
  }
  // Aspace and VMO ranges need to be contiguous. Validate that the right candidate is actually to
  // the right in addition to checking that base+size lines up for single scenario where base_+size_
  // can overflow and becomes zero.
  if (base_ + size_ != right_candidate->base_ || right_candidate->base_ < base_) {
    return;
  }
  if (object_offset_locked() + size_ != right_candidate->object_offset_locked()) {
    return;
  }
  // All flags need to be consistent.
  if (flags_ != right_candidate->flags_) {
    return;
  }
  // Although we can combine the protect_region_list_rest_ of the two mappings, we require that they
  // be of the same cacheability, as this is an assumption that mapping has a single cacheability
  // type. Since all protection regions have the same cacheability we can check any arbitrary one in
  // each of the mappings. Note that this check is technically redundant, since a VMO can only have
  // one kind of cacheability and we already know this is the same VMO, but some extra paranoia here
  // does not hurt.
  if ((ProtectRangesLocked().FirstRegionMmuFlags() & ARCH_MMU_FLAG_CACHE_MASK) !=
      (right_candidate->ProtectRangesLocked().FirstRegionMmuFlags() & ARCH_MMU_FLAG_CACHE_MASK)) {
    return;
  }

  // Only merge live mappings.
  if (state_ != LifeCycleState::ALIVE || right_candidate->state_ != LifeCycleState::ALIVE) {
    return;
  }
  // Both need to be mergeable.
  if (mergeable_ == Mergeable::NO || right_candidate->mergeable_ == Mergeable::NO) {
    return;
  }

  {
    // Although it was safe to read size_ without holding the object lock, we need to acquire it to
    // perform changes.
    Guard<CriticalMutex> guard{AliasedLock, object_->lock(), right_candidate->object_->lock()};

    // Attempt to merge the protection region lists first. This is done first as a node allocation
    // might be needed, which could fail. If it fails we can still abort now without needing to roll
    // back any changes.
    zx_status_t status = protection_ranges_.MergeRightNeighbor(right_candidate->protection_ranges_,
                                                               right_candidate->base_);
    if (status != ZX_OK) {
      ASSERT(status == ZX_ERR_NO_MEMORY);
      return;
    }

    const size_t new_size = size_ + right_candidate->size_;

    status = right_candidate->DestroyLockedObject(false);
    ASSERT(status == ZX_OK);

    // The size of this mapping must be updated after removing the right candidate from the region
    // tree to ensure correct re-validation of the subtree invariants. Failure to do so may trigger
    // a consistency check, depending on the structure of related WAVLTree nodes.
    set_size_locked(new_size);
  }

  vm_mappings_merged.Add(1);
}

void VmMapping::TryMergeNeighborsLocked() {
  canary_.Assert();

  // Check that this mapping is mergeable and is currently in the correct lifecycle state.
  if (mergeable_ == Mergeable::NO || state_ != LifeCycleState::ALIVE) {
    return;
  }
  // As a VmMapping if we we are alive we by definition have a parent.
  DEBUG_ASSERT(parent_);

  // We expect there to be a RefPtr to us held beyond the one for the wavl tree ensuring that we
  // cannot trigger our own destructor should we remove ourselves from the hierarchy.
  DEBUG_ASSERT(ref_count_debug() > 1);

  // First consider merging any mapping on our right, into |this|.
  AssertHeld(parent_->lock_ref());
  auto right_candidate = parent_->subregions_.RightOf(this);
  if (right_candidate.IsValid()) {
    // Request mapping as a refptr as we need to hold a refptr across the try merge.
    if (fbl::RefPtr<VmMapping> mapping = right_candidate->as_vm_mapping()) {
      TryMergeRightNeighborLocked(mapping.get());
    }
  }

  // Now attempt to merge |this| with any left neighbor.
  AssertHeld(parent_->lock_ref());
  auto left_candidate = parent_->subregions_.LeftOf(this);
  if (!left_candidate.IsValid()) {
    return;
  }
  if (auto mapping = left_candidate->as_vm_mapping()) {
    // Attempt actual merge. If this succeeds then |this| is in the dead state, but that's fine as
    // we are finished anyway.
    AssertHeld(mapping->lock_ref());
    mapping->TryMergeRightNeighborLocked(this);
  }
}

void VmMapping::MarkMergeable(fbl::RefPtr<VmMapping> mapping) {
  Guard<CriticalMutex> guard{mapping->lock()};
  // Now that we have the lock check this mapping is still alive and we haven't raced with some
  // kind of destruction.
  if (mapping->state_ != LifeCycleState::ALIVE) {
    return;
  }
  // Skip marking any vdso segments mergeable. Although there is currently only one vdso segment and
  // so it would never actually get merged, marking it mergeable is technically incorrect.
  if (mapping->aspace_->vdso_code_mapping_ == mapping) {
    return;
  }
  mapping->mergeable_ = Mergeable::YES;
  mapping->TryMergeNeighborsLocked();
}

zx_status_t VmMapping::SetMemoryPriorityLocked(VmAddressRegion::MemoryPriority priority) {
  DEBUG_ASSERT(state_ == LifeCycleState::ALIVE);
  if (priority == memory_priority_) {
    return ZX_OK;
  }
  Guard<CriticalMutex> guard{object_->lock()};
  return SetMemoryPriorityLockedObject(priority);
}

zx_status_t VmMapping::SetMemoryPriorityLockedObject(VmAddressRegion::MemoryPriority priority) {
  DEBUG_ASSERT(state_ == LifeCycleState::ALIVE);
  if (priority == memory_priority_) {
    return ZX_OK;
  }
  memory_priority_ = priority;
  const bool is_high = priority == VmAddressRegion::MemoryPriority::HIGH;
  aspace_->ChangeHighPriorityCountLocked(is_high ? 1 : -1);
  object_->ChangeHighPriorityCountLocked(is_high ? 1 : -1);
  return ZX_OK;
}

void VmMapping::CommitHighMemoryPriority() {
  fbl::RefPtr<VmObject> vmo;
  uint64_t offset;
  uint64_t len;
  {
    Guard<CriticalMutex> guard{lock()};
    if (state_ != LifeCycleState::ALIVE || memory_priority_ != MemoryPriority::HIGH) {
      return;
    }
    vmo = object_;
    offset = object_offset_locked();
    len = size_locked();
  }
  DEBUG_ASSERT(vmo);
  vmo->CommitHighPriorityPages(offset, len);
  // Ignore the return result of MapRange as this is just best effort.
  MapRange(offset, len, false, true);
}

zx_status_t VmMapping::ForceWritableLocked() {
  canary_.Assert();
  if (state_ != LifeCycleState::ALIVE) {
    return ZX_ERR_BAD_STATE;
  }
  DEBUG_ASSERT(object_);
  // If we have already re-directed to a private clone then there is no need to do so again.
  if (private_clone_) {
    return ZX_OK;
  }
  // If the mapping is already possible to write to (even if disabled by current protections), then
  // writing is already safe.
  if (is_valid_mapping_flags(ARCH_MMU_FLAG_PERM_WRITE)) {
    return ZX_OK;
  }
  // A physical VMO cannot be cloned and so we cannot make this safe, just allow the write.
  if (!object_->is_paged()) {
    return ZX_OK;
  }
  // Create a clone of our VMO that covers the size of our mapping.
  fbl::RefPtr<VmObject> clone;
  zx_status_t status = object_->CreateClone(Resizability::NonResizable, SnapshotType::OnWrite,
                                            object_offset_locked(), size_locked(), true, &clone);
  if (status != ZX_OK) {
    return status;
  }
  {
    Guard<CriticalMutex> guard{object_->lock()};
    // Clear out all mappings from the previous object, Must be done the object lock to prevent
    // mappings being modified in between.
    status = aspace_->arch_aspace().Unmap(base_, size_ / PAGE_SIZE, aspace_->EnlargeArchUnmap());
    if (status != ZX_OK) {
      return status;
    }
    // Finally unlink from the object_.
    object_->RemoveMappingLocked(this);
    // We created the clone started at object_offset_ in the old object, so that makes the
    // equivalent object_offset_ start at 0 in the clone.
    object_offset_ = 0;
  }
  // Reset object_ outside its lock in case we trigger its destructor.
  object_.reset();
  // Take the lock for the clone so we can install it.
  Guard<CriticalMutex> guard{clone->lock()};
  clone->AddMappingLocked(this);
  object_ = ktl::move(clone);
  // Set private_clone_ so that we do not repeatedly create clones of clones for no reason.
  private_clone_ = true;
  return ZX_OK;
}

uint64_t VmMapping::TrimmedObjectRangeLocked(uint64_t offset, uint64_t len) const TA_REQ(lock())
    TA_REQ(object_->lock()) {
  const uint64_t vmo_offset = object_offset_locked() + offset;
  const uint64_t vmo_size = object_->size_locked();
  if (vmo_offset >= vmo_size) {
    return 0;
  }

  uint64_t trim_len = vmo_size - vmo_offset;

  if (flags_ & VMAR_FLAG_FAULT_BEYOND_STREAM_SIZE) {
    VmObjectPaged* paged = DownCastVmObject<VmObjectPaged>(object_.get());
    DEBUG_ASSERT(paged);
    AssertHeld(paged->lock_ref());
    auto stream_size_res = paged->saturating_stream_size_locked();
    // Creating a fault-beyond-stream-size mapping should have allocated a CSM.
    DEBUG_ASSERT(stream_size_res);
    size_t stream_size = stream_size_res.value();
    DEBUG_ASSERT(stream_size <= vmo_size);
    trim_len = stream_size - vmo_offset;
  }

  return ktl::min(trim_len, len);
}

template <typename F>
zx_status_t MappingProtectionRanges::UpdateProtectionRange(vaddr_t mapping_base,
                                                           size_t mapping_size, vaddr_t base,
                                                           size_t size, uint new_arch_mmu_flags,
                                                           F callback) {
  // If we're changing the whole mapping, just make the change.
  if (mapping_base == base && mapping_size == size) {
    protect_region_list_rest_.clear();
    callback(base, size, first_region_arch_mmu_flags_);
    first_region_arch_mmu_flags_ = new_arch_mmu_flags;
    return ZX_OK;
  }

  // Find the range of nodes that will need deleting.
  auto first = protect_region_list_rest_.lower_bound(base);
  auto last = protect_region_list_rest_.upper_bound(base + (size - 1));

  // Work the flags in the regions before the first/last nodes. We need to cache these flags so that
  // once we are inserting the new protection nodes, we do not insert nodes such that we would cause
  // two regions to have the same flags (which would be redundant).
  const uint start_carry_flags = FlagsForPreviousRegion(first);
  const uint end_carry_flags = FlagsForPreviousRegion(last);

  // Determine how many new nodes we are going to need so we can allocate up front. This ensures
  // that after we have deleted nodes from the tree (and destroyed information) we do not have to
  // do an allocation that might fail and leave us in an unrecoverable state. However, we would
  // like to avoid actually performing allocations as far as possible, so do the following
  // 1. Count how many nodes will be needed to represent the new protection range (after the nodes
  //    between first,last have been deleted. As a protection range has two points, a start and an
  //    end, the most nodes we can ever possibly need is two.
  // 2. Of these new nodes we will need, work out how many we can reuse from deletion.
  // 3. Allocate the remainder.
  ktl::optional<ktl::unique_ptr<ProtectNode>> protect_nodes[2];
  const uint total_nodes_needed = NodeAllocationsForRange(mapping_base, mapping_size, base, size,
                                                          first, last, new_arch_mmu_flags);
  uint nodes_needed = total_nodes_needed;
  // First see how many of the nodes we will be able to get by erasing and can reuse.
  for (auto it = first; nodes_needed > 0 && it != last; it++) {
    nodes_needed--;
  }
  // If there are any nodes_needed still, allocate them so that they are available.
  uint nodes_available = 0;
  // Allocate any remaining nodes_needed that we will not fulfill from deletions.
  while (nodes_available < nodes_needed) {
    fbl::AllocChecker ac;
    ktl::unique_ptr<ProtectNode> new_node(ktl::make_unique<ProtectNode>(&ac));
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }
    protect_nodes[nodes_available++].emplace(ktl::move(new_node));
  }

  // Now that we have done all memory allocations and know that we cannot fail start the destructive
  // part and erase any nodes in the the range as well as call the provided callback with the old
  // data.
  {
    vaddr_t old_start = base;
    uint old_flags = start_carry_flags;
    while (first != last) {
      // On the first iteration if the range is aligned to a node then we skip, since we do not want
      // to do the callback for a zero sized range.
      if (old_start != first->region_start) {
        callback(old_start, first->region_start - old_start, old_flags);
      }
      old_start = first->region_start;
      old_flags = first->arch_mmu_flags;
      auto node = protect_region_list_rest_.erase(first++);
      if (nodes_available < total_nodes_needed) {
        protect_nodes[nodes_available++].emplace(ktl::move(node));
      }
    }
    // If the range was not aligned to a node then process any remainder.
    if (old_start <= base + (size - 1)) {
      callback(old_start, base + size - old_start, old_flags);
    }
  }

  // At this point we should now have all the nodes.
  DEBUG_ASSERT(total_nodes_needed == nodes_available);

  // Check if we are updating the implicit first node, which just involves changing
  // first_region_arch_mmu_flags_, or if there's a protection change that requires a node insertion.
  if (base == mapping_base) {
    first_region_arch_mmu_flags_ = new_arch_mmu_flags;
  } else if (start_carry_flags != new_arch_mmu_flags) {
    ASSERT(nodes_available > 0);
    auto node = ktl::move(protect_nodes[--nodes_available].value());
    node->region_start = base;
    node->arch_mmu_flags = new_arch_mmu_flags;
    protect_region_list_rest_.insert(ktl::move(node));
  }

  // To create the end of the region we first check if there is a gap between the end of this region
  // and the start of the next region. Additionally this needs to handle the case where there is no
  // next node in the tree, and so we have to check against mapping limit of mapping_base +
  // mapping_size.
  const uint64_t next_region_start =
      last.IsValid() ? last->region_start : (mapping_base + mapping_size);
  if (next_region_start != base + size) {
    // There is a gap to the next node so we need to make sure it keeps its old protection value,
    // end_carry_flags. However, it could have ended up that these flags are what we are protecting
    // to, in which case a new node isn't needed as we can just effectively merge the gap into this
    // protection range.
    if (end_carry_flags != new_arch_mmu_flags) {
      ASSERT(nodes_available > 0);
      auto node = ktl::move(protect_nodes[--nodes_available].value());
      node->region_start = base + size;
      node->arch_mmu_flags = end_carry_flags;
      protect_region_list_rest_.insert(ktl::move(node));
      // Since we are essentially moving forward a node that we previously deleted, to essentially
      // shrink the previous protection range, we know that there is no merging needed with the next
      // node.
      DEBUG_ASSERT(!last.IsValid() || last->arch_mmu_flags != end_carry_flags);
    }
  } else if (last.IsValid() && last->arch_mmu_flags == new_arch_mmu_flags) {
    // From the previous `if` block we know that if last.IsValid is true, then the end of the region
    // being protected is last->region_start. If this next region happens to have the same flags as
    // what we just protected, then we need to drop this node.
    protect_region_list_rest_.erase(last);
  }

  // We should not have allocated more nodes than we needed, this indicates a bug in the calculation
  // logic.
  DEBUG_ASSERT(nodes_available == 0);
  return ZX_OK;
}

uint MappingProtectionRanges::MmuFlagsForWavlRegion(vaddr_t vaddr) const {
  DEBUG_ASSERT(!protect_region_list_rest_.is_empty());
  auto it = --protect_region_list_rest_.upper_bound(vaddr);
  if (it.IsValid()) {
    DEBUG_ASSERT(it->region_start <= vaddr);
    return it->arch_mmu_flags;
  } else {
    DEBUG_ASSERT(protect_region_list_rest_.begin()->region_start > vaddr);
    return first_region_arch_mmu_flags_;
  }
}

// Counts how many nodes would need to be allocated for a protection range. This calculation is
// based of whether there are actually changes in the protection type that require a node to be
// added.
uint MappingProtectionRanges::NodeAllocationsForRange(vaddr_t mapping_base, size_t mapping_size,
                                                      vaddr_t base, size_t size,
                                                      RegionList::iterator removal_start,
                                                      RegionList::iterator removal_end,
                                                      uint new_mmu_flags) const {
  uint nodes_needed = 0;
  // Check if we will need a node at the start. if base==base_ then we will just be changing the
  // first_region_arch_mmu_flags_, otherwise we need a node if we're actually causing a protection
  // change.
  if (base != mapping_base && FlagsForPreviousRegion(removal_start) != new_mmu_flags) {
    nodes_needed++;
  }
  // The node for the end of the region is needed under two conditions
  // 1. There will be a non-zero gap between the end of our new region and the start of the next
  //    existing region.
  // 2. This non-zero sized gap is of a different protection type.
  const uint64_t next_region_start =
      removal_end.IsValid() ? removal_end->region_start : (mapping_base + mapping_size);
  if (next_region_start != base + size && FlagsForPreviousRegion(removal_end) != new_mmu_flags) {
    nodes_needed++;
  }
  return nodes_needed;
}

zx_status_t MappingProtectionRanges::MergeRightNeighbor(MappingProtectionRanges& right,
                                                        vaddr_t merge_addr) {
  // We need to insert a node if the protection type of the end of the left mapping is not the
  // same as the protection type of the start of the right mapping.
  if (FlagsForPreviousRegion(protect_region_list_rest_.end()) !=
      right.first_region_arch_mmu_flags_) {
    fbl::AllocChecker ac;
    ktl::unique_ptr<ProtectNode> region =
        ktl::make_unique<ProtectNode>(&ac, merge_addr, right.first_region_arch_mmu_flags_);
    if (!ac.check()) {
      // No state has changed yet, so even though we do not forward up an error it is safe to just
      // not merge.
      TRACEF("Aborted region merge due to out of memory\n");
      return ZX_ERR_NO_MEMORY;
    }
    protect_region_list_rest_.insert(ktl::move(region));
  }
  // Carry over any remaining regions.
  while (!right.protect_region_list_rest_.is_empty()) {
    protect_region_list_rest_.insert(right.protect_region_list_rest_.pop_front());
  }
  return ZX_OK;
}

MappingProtectionRanges MappingProtectionRanges::SplitAt(vaddr_t split) {
  // Determine the mmu flags the right most mapping would start at.
  auto right_nodes = protect_region_list_rest_.upper_bound(split);
  const uint right_mmu_flags = FlagsForPreviousRegion(right_nodes);

  MappingProtectionRanges ranges(right_mmu_flags);

  // Move any protect regions into the right half.
  while (right_nodes != protect_region_list_rest_.end()) {
    ranges.protect_region_list_rest_.insert(protect_region_list_rest_.erase(right_nodes++));
  }
  return ranges;
}

void MappingProtectionRanges::DiscardBelow(vaddr_t addr) {
  auto last = protect_region_list_rest_.upper_bound(addr);
  while (protect_region_list_rest_.begin() != last) {
    first_region_arch_mmu_flags_ = protect_region_list_rest_.pop_front()->arch_mmu_flags;
  }
}

void MappingProtectionRanges::DiscardAbove(vaddr_t addr) {
  for (auto it = protect_region_list_rest_.lower_bound(addr);
       it != protect_region_list_rest_.end();) {
    protect_region_list_rest_.erase(it++);
  }
}

bool MappingProtectionRanges::DebugNodesWithinRange(vaddr_t mapping_base, size_t mapping_size) {
  if (protect_region_list_rest_.is_empty()) {
    return true;
  }
  if (protect_region_list_rest_.begin()->region_start < mapping_base) {
    return false;
  }
  if ((--protect_region_list_rest_.end())->region_start >= mapping_base + mapping_size) {
    return false;
  }
  return true;
}
