// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/pmm_node_ffi.h"

#include <zircon/types.h>

#include <kernel/ffi.h>
#include <vm/compression.h>

#include "vm/pmm.h"
#include "vm/pmm_node.h"
#include "vm/vm_cow_pages.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE void cpp_pmm_node_destroy(PmmNode* node) { node->~PmmNode(); }

FFI_ALWAYS_INLINE zx_status_t cpp_pmm_node_alloc_page(PmmNode* node, uint32_t alloc_flags,
                                                      vm_page_t** out_page) {
  zx::result<vm_page_t*> res = node->AllocPage(alloc_flags);
  if (res.is_error()) {
    return res.status_value();
  }
  *out_page = *res;
  return ZX_OK;
}

FFI_ALWAYS_INLINE zx_status_t cpp_pmm_node_alloc_pages(PmmNode* node, size_t count,
                                                       uint32_t alloc_flags,
                                                       VmPageDoublyLinkedList* list) {
  return node->AllocPages(count, alloc_flags, list);
}

FFI_ALWAYS_INLINE void cpp_pmm_node_free_page(PmmNode* node, vm_page_t* page,
                                              PmmOptDelayReuse delay_reuse) {
  node->FreePage(page, delay_reuse);
}

FFI_ALWAYS_INLINE void cpp_pmm_node_free_list(PmmNode* node, VmPageDoublyLinkedList* list,
                                              PmmOptDelayReuse delay_reuse) {
  node->FreeList(list, delay_reuse);
}

FFI_ALWAYS_INLINE uint64_t cpp_pmm_node_count_free_pages(const PmmNode* node) {
  return node->CountFreePages();
}

FFI_ALWAYS_INLINE uint64_t cpp_pmm_node_count_loaned_free_pages(const PmmNode* node) {
  return node->CountLoanedFreePages();
}

FFI_ALWAYS_INLINE uint64_t cpp_pmm_node_count_loan_cancelled_pages(const PmmNode* node) {
  return node->CountLoanCancelledPages();
}

FFI_ALWAYS_INLINE uint64_t cpp_pmm_node_count_loaned_not_free_pages(const PmmNode* node) {
  return node->CountLoanedNotFreePages();
}

FFI_ALWAYS_INLINE uint64_t cpp_pmm_node_count_loaned_pages(const PmmNode* node) {
  return node->CountLoanedPages();
}

FFI_ALWAYS_INLINE uint64_t cpp_pmm_node_count_total_bytes(const PmmNode* node) {
  return node->CountTotalBytes();
}

FFI_ALWAYS_INLINE bool cpp_pmm_node_enable_free_page_filling(PmmNode* node, size_t fill_size,
                                                             uint8_t action) {
  return node->EnableFreePageFilling(fill_size, static_cast<CheckFailAction>(action));
}

FFI_ALWAYS_INLINE void cpp_pmm_node_fill_free_pages_and_arm(PmmNode* node) {
  node->FillFreePagesAndArm();
}

FFI_ALWAYS_INLINE bool cpp_pmm_node_set_free_memory_signal(PmmNode* node, uint64_t lower_bound,
                                                           uint64_t upper_bound,
                                                           uint64_t delay_allocations_pages,
                                                           Event* event) {
  return node->SetFreeMemorySignal(lower_bound, upper_bound, delay_allocations_pages, event);
}

FFI_ALWAYS_INLINE zx_status_t cpp_pmm_node_wait_for_single_page_allocation(
    PmmNode* node, zx_instant_mono_t deadline, bool suspendable, vm_page_t** out_page) {
  zx::result<vm_page_t*> res =
      node->WaitForSinglePageAllocation(Deadline::no_slack(deadline), suspendable);
  if (res.is_error()) {
    return res.status_value();
  }
  *out_page = *res;
  return ZX_OK;
}

FFI_ALWAYS_INLINE void cpp_pmm_node_stop_returning_should_wait(PmmNode* node) {
  node->StopReturningShouldWait();
}

FFI_ALWAYS_INLINE bool cpp_pmm_node_has_alloc_failed_no_mem(const PmmNode* node) {
  return node->has_alloc_failed_no_mem();
}

FFI_ALWAYS_INLINE void cpp_pmm_node_get_first_alloc_failure(PmmNode* node,
                                                            PmmNode::AllocFailure* out_failure) {
  *out_failure = node->GetFirstAllocFailure();
}

FFI_ALWAYS_INLINE void cpp_pmm_node_report_alloc_failure(PmmNode* node,
                                                         const PmmNode::AllocFailure* failure) {
  node->ReportAllocFailure(*failure);
}

FFI_ALWAYS_INLINE void cpp_pmm_node_begin_loan(PmmNode* node, VmPageDoublyLinkedList* list,
                                               PmmOptDelayReuse delay_reuse) {
  node->BeginLoan(list, delay_reuse);
}

FFI_ALWAYS_INLINE void cpp_pmm_node_cancel_loan(PmmNode* node, vm_page_t* page) {
  node->CancelLoan(page);
}

FFI_ALWAYS_INLINE void cpp_pmm_node_end_loan(PmmNode* node, vm_page_t* page) {
  node->EndLoan(page);
}

FFI_ALWAYS_INLINE zx_status_t cpp_pmm_node_alloc_loaned_page(PmmNode* node,
                                                             void (*allocated)(vm_page_t*,
                                                                               void* cookie),
                                                             void* cookie, vm_page_t** out_page) {
  zx::result<vm_page_t*> res =
      node->AllocLoanedPage([allocated, cookie](vm_page_t* page) { allocated(page, cookie); });
  if (res.is_error()) {
    return res.status_value();
  }
  *out_page = *res;
  return ZX_OK;
}

FFI_ALWAYS_INLINE void cpp_pmm_node_begin_free_loaned_page(
    PmmNode* node, vm_page_t* page, void (*release_page)(vm_page_t*, void* cookie), void* cookie,
    FreeLoanedPagesHolder* flph) {
  node->BeginFreeLoanedPage(
      page, [release_page, cookie](vm_page_t* p) { release_page(p, cookie); }, *flph);
}

FFI_ALWAYS_INLINE void cpp_pmm_node_finish_free_loaned_pages(PmmNode* node,
                                                             FreeLoanedPagesHolder* flph) {
  node->FinishFreeLoanedPages(*flph);
}

}  // extern "C"
