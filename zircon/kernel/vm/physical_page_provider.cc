// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "include/vm/physical_page_provider.h"

extern "C" {
void rust_physical_page_provider_init(void* state, uint64_t size);
void rust_physical_page_provider_destroy(void* state);
void rust_physical_page_provider_init_ptrs(void* state, VmCowPages* cow_pages,
                                           PageSource* page_source, paddr_t phys_base);
PageSourceProperties rust_physical_page_provider_properties(const void* state);
void rust_physical_page_provider_send_async_request(void* state, PageRequest* request);
void rust_physical_page_provider_clear_async_request(void* state, PageRequest* request);
void rust_physical_page_provider_swap_async_request(void* state, PageRequest* old_req,
                                                    PageRequest* new_req);
void rust_physical_page_provider_free_pages(void* state, VmPageDoublyLinkedList* pages);
bool rust_physical_page_provider_debug_is_page_ok(const void* state, vm_page_t* page,
                                                  uint64_t offset);
void rust_physical_page_provider_on_detach(void* state);
void rust_physical_page_provider_on_close(void* state);
zx_status_t rust_physical_page_provider_wait_on_event(void* state, Event* event, bool suspendable);
void rust_physical_page_provider_dump(const void* state, const void* provider_ptr, uint32_t depth,
                                      uint32_t max_items);
}

PhysicalPageProvider::PhysicalPageProvider(uint64_t size) {
  rust_physical_page_provider_init(state_ptr(), size);
}

PhysicalPageProvider::~PhysicalPageProvider() { rust_physical_page_provider_destroy(state_ptr()); }

void PhysicalPageProvider::Init(VmCowPages* cow_pages, PageSource* page_source, paddr_t phys_base) {
  rust_physical_page_provider_init_ptrs(state_ptr(), cow_pages, page_source, phys_base);
}

PageSourceProperties PhysicalPageProvider::properties() const {
  return rust_physical_page_provider_properties(state_ptr());
}

// Called under lock of contiguous VMO that needs the pages.  The request is later processed at the
// start of WaitOnEvent.
void PhysicalPageProvider::SendAsyncRequest(PageRequest* request) {
  rust_physical_page_provider_send_async_request(state_ptr(), request);
}

void PhysicalPageProvider::ClearAsyncRequest(PageRequest* request) {
  rust_physical_page_provider_clear_async_request(state_ptr(), request);
}

void PhysicalPageProvider::SwapAsyncRequest(PageRequest* old, PageRequest* new_req) {
  rust_physical_page_provider_swap_async_request(state_ptr(), old, new_req);
}

void PhysicalPageProvider::FreePages(VmPageDoublyLinkedList* pages) {
  rust_physical_page_provider_free_pages(state_ptr(), pages);
}

bool PhysicalPageProvider::DebugIsPageOk(vm_page_t* page, uint64_t offset) {
  return rust_physical_page_provider_debug_is_page_ok(state_ptr(), page, offset);
}

void PhysicalPageProvider::OnDetach() { rust_physical_page_provider_on_detach(state_ptr()); }

void PhysicalPageProvider::OnClose() { rust_physical_page_provider_on_close(state_ptr()); }

// TODO(https://fxbug.dev/42084841): Reason about the use of |suspendable|, ignored for now.
zx_status_t PhysicalPageProvider::WaitOnEvent(Event* event, bool suspendable) {
  return rust_physical_page_provider_wait_on_event(state_ptr(), event, suspendable);
}

void PhysicalPageProvider::Dump(uint depth, uint32_t max_items) {
  rust_physical_page_provider_dump(state_ptr(), this, depth, max_items);
}
