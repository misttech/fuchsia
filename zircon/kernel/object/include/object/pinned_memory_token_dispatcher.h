// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PINNED_MEMORY_TOKEN_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PINNED_MEMORY_TOKEN_DISPATCHER_H_

#include <sys/types.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <dev/iommu/iommu.h>
#include <dev/iommu/pmt.h>
#include <fbl/array.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/ref_ptr.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <vm/pinned_vm_object.h>

class BusTransactionInitiatorDispatcher;
class VmObject;

// The tag for the list type used by the containing BTI to hold a list of all
// its PMTs, including those which are quarantined.
struct PmtListTag {};

// The tag for the list type used by the containing BTI to hold a list of all
// its quarantined PMTs.
struct PmtQuarantineListTag {};

class PinnedMemoryTokenDispatcher final
    : public SoloDispatcher<PinnedMemoryTokenDispatcher, ZX_DEFAULT_PMT_RIGHTS>,
      public fbl::ContainableBaseClasses<
          fbl::TaggedDoublyLinkedListable<PinnedMemoryTokenDispatcher*, PmtListTag>,
          fbl::TaggedDoublyLinkedListable<fbl::RefPtr<PinnedMemoryTokenDispatcher>,
                                          PmtQuarantineListTag>> {
 public:
  ~PinnedMemoryTokenDispatcher();

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_PMT; }
  void on_zero_handles() final TA_EXCL(get_lock());

  // Unpin and unmap the memory which was managed by this PMT
  void Unpin() TA_EXCL(get_lock()) {
    Guard<CriticalMutex> guard{get_lock()};
    pmt_->ReleasePinnedMemory();
  }

  // Query the pinned and mapped VMO for a region specified by offset/size.
  zx::result<iommu::QueryAddressResult> QueryAddress(uint64_t offset, uint64_t size)
      TA_EXCL(get_lock()) {
    Guard<CriticalMutex> guard{get_lock()};
    return pmt_->QueryAddress(offset, size);
  }

  // Returns the number of bytes pinned by the PMT.
  uint64_t size() const TA_EXCL(get_lock()) {
    Guard<CriticalMutex> guard{get_lock()};
    return pmt_->pinned_vmo().size();
  }

 protected:
  friend BusTransactionInitiatorDispatcher;
  // Set the permissions of |pinned_vmo|'s pinned range to |perms| on
  // behalf of |bti|. |perms| should be flags suitable for the Iommu::Map()
  // interface.  Must be created under the BTI dispatcher's lock.
  static zx_status_t Create(fbl::RefPtr<BusTransactionInitiatorDispatcher> bti,
                            PinnedVmObject pinned_vmo, uint32_t perms,
                            KernelHandle<PinnedMemoryTokenDispatcher>* handle, zx_rights_t* rights);

 private:
  PinnedMemoryTokenDispatcher(fbl::RefPtr<BusTransactionInitiatorDispatcher> bti);
  DISALLOW_COPY_ASSIGN_AND_MOVE(PinnedMemoryTokenDispatcher);

  TA_GUARDED(get_lock()) fbl::RefPtr<iommu::Pmt> pmt_;
  const fbl::RefPtr<BusTransactionInitiatorDispatcher> bti_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PINNED_MEMORY_TOKEN_DISPATCHER_H_
