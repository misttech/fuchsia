// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_SOURCE_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_SOURCE_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <vm/page_source.h>

__BEGIN_CDECLS

void cpp_multi_page_request_construct(MultiPageRequest* req);
void cpp_multi_page_request_destroy(MultiPageRequest* req);
void cpp_multi_page_request_cancel_requests(MultiPageRequest* req);
zx_status_t cpp_multi_page_request_wait(MultiPageRequest* req, bool suspendable);

// PageRequest helpers
uint32_t cpp_page_request_get_type(const PageRequest* request);
uint64_t cpp_page_request_get_offset(const PageRequest* request);
uint64_t cpp_page_request_get_len(const PageRequest* request);
// Returns a pointer to the `PageProviderTag` list node state of |request|, allowing Rust page
// providers to link requests into a Rust `fbl::DoublyLinkedList`.
void* cpp_page_request_provider_node(PageRequest* request);

// PageSource failure & helper shims
void cpp_page_source_free(PageSource* src);
void* cpp_page_source_get_ref_counted(PageSource* src);
void cpp_page_source_on_pages_failed(PageSource* page_source, uint64_t offset, uint64_t len,
                                     zx_status_t error_status);
bool cpp_page_source_is_valid_external_failure_code(zx_status_t error_status);
bool cpp_page_source_is_valid_internal_failure_code(zx_status_t status);
// Returns whether the calling thread holds |page_source|'s paged VMO lock. Intended for debug
// assertions in callers that are required to hold that lock.
bool cpp_page_source_paged_vmo_lock_is_held(PageSource* page_source);
void cpp_page_source_dump(const PageSource* page_source, uint32_t depth, uint32_t max_items);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_SOURCE_FFI_H_
