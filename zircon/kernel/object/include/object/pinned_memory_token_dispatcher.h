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

#include <dev/iommu.h>
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
  void on_zero_handles() final;

  // Unpin this PMT. If this is not done before on_zero_handles() runs, then it will get moved to
  // the quarantine.
  void Unpin();

  zx_status_t QueryAddress(uint64_t offset, uint64_t size, dev_vaddr_t* mapped_addr,
                           size_t* mapped_len);

  // Returns the number of bytes pinned by the PMT.
  uint64_t size() const { return pinned_vmo_.size(); }

 protected:
  friend BusTransactionInitiatorDispatcher;
  // Set the permissions of |pinned_vmo|'s pinned range to |perms| on
  // behalf of |bti|. |perms| should be flags suitable for the Iommu::Map()
  // interface.  Must be created under the BTI dispatcher's lock.
  static zx_status_t Create(fbl::RefPtr<BusTransactionInitiatorDispatcher> bti,
                            PinnedVmObject pinned_vmo, uint32_t perms,
                            KernelHandle<PinnedMemoryTokenDispatcher>* handle, zx_rights_t* rights);

 private:
  PinnedMemoryTokenDispatcher(fbl::RefPtr<BusTransactionInitiatorDispatcher> bti,
                              PinnedVmObject pinned_vmo);
  DISALLOW_COPY_ASSIGN_AND_MOVE(PinnedMemoryTokenDispatcher);

  zx_status_t MapIntoIommu(uint32_t perms);
  zx_status_t UnmapFromIommuLocked() TA_REQ(get_lock());

  PinnedVmObject pinned_vmo_;

  // Set to true by Unpin()
  bool explicitly_unpinned_ TA_GUARDED(get_lock()) = false;

  const fbl::RefPtr<BusTransactionInitiatorDispatcher> bti_;
  uint64_t map_token_ TA_GUARDED(get_lock()) = UINT64_MAX;

  // Set to true during Create() once we are fully initialized. Do not call
  // any |bti_| locking methods if this is false, since that indicates we're
  // being called from Create() and already have the |bti_| lock.
  bool initialized_ = false;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PINNED_MEMORY_TOKEN_DISPATCHER_H_
