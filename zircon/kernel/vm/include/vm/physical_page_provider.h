// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PHYSICAL_PAGE_PROVIDER_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PHYSICAL_PAGE_PROVIDER_H_

#include <sys/types.h>
#include <zircon/types.h>

#include <fbl/ref_ptr.h>
#include <vm/page_source.h>
#include <vm/vm_constants.h>

// Page provider implementation that provides requested loaned physical pages.
//
// This is used by contiguous VMOs which have had pages decommitted, when the pages are again
// committed.  The reason we use a PageProvider for this is it lines up well with the pager model in
// the sense that a page request can be processed while not holding the contiguous VMO's lock.
//
// PhysicalPageProvider always operates async (similar to PagerProxy), because we'd like to (in
// typical non-overlapping commit/decommit usage) have one batch that covers the entire commit,
// regardless of the fact that some of the pages may already be free and therefore could be
// immediately obtained.  Quite often at least one page will be presently owned by a different
// VMO, so we may as well always do one big async batch that deals with all the presently
// non-FREE pages.
class PhysicalPageProvider : public PageProvider {
 public:
  explicit PhysicalPageProvider(uint64_t size);
  ~PhysicalPageProvider() override;

  // Called shortly after construction, before any use.
  void Init(VmCowPages* cow_pages, PageSource* page_source, paddr_t phys_base);

 private:
  // PageProvider methods.
  PageSourceProperties properties() const final;
  void SendAsyncRequest(PageRequest* request) final;
  void ClearAsyncRequest(PageRequest* request) final;
  void SwapAsyncRequest(PageRequest* old, PageRequest* new_req) final;
  void FreePages(VmPageDoublyLinkedList* pages) final;
  bool DebugIsPageOk(vm_page_t* page, uint64_t offset) final;
  // This also calls pmm_delete_lender()
  void OnClose() final;
  void OnDetach() final;
  // Before actually waiting on the event, uses the calling thread (which isn't holding any locks)
  // to process all the requests in pending_requests_.
  zx_status_t WaitOnEvent(Event* event, bool suspendable) final;

  void Dump(uint depth, uint32_t max_items) final;

  alignas(kPhysicalPageProviderStateAlign) uint8_t state_[kPhysicalPageProviderStateSize];

  void* state_ptr() { return state_; }
  const void* state_ptr() const { return state_; }
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PHYSICAL_PAGE_PROVIDER_H_
