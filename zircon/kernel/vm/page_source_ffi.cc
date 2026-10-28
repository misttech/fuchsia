// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/page_source_ffi.h"

#include <new>

#include <kernel/ffi.h>
#include <ktl/memory.h>
#include <ktl/type_traits.h>
#include <ktl/utility.h>

#include "vm/page_source.h"

namespace {
// `PageProviderHelper` gives local access to the static protected methods of `PageProvider`.
struct PageProviderHelper : public PageProvider {
  using PageProvider::GetRequestLen;
  using PageProvider::GetRequestOffset;
  using PageProvider::GetRequestType;
  using PageProvider::GetRequestVmoId;
};
}  // namespace

// Rust views a `PageProviderTag` node state as an `fbl::DoublyLinkedListNode`, which is a
// `#[repr(C)]` pair of `next` and `prev` pointers.
using PageProviderNodeState = fbl::DoublyLinkedListNodeState<PageRequest*>;
static_assert(
    ktl::is_same_v<PageProviderNodeState&,
                   decltype(fbl::DefaultDoublyLinkedListTraits<PageRequest*, PageProviderTag>::
                                node_state(ktl::declval<PageRequest&>()))>);
static_assert(ktl::is_standard_layout_v<PageProviderNodeState>);
static_assert(sizeof(PageProviderNodeState) == 2 * sizeof(void*));
static_assert(alignof(PageProviderNodeState) == alignof(void*));

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.

FFI_ALWAYS_INLINE void cpp_multi_page_request_construct(MultiPageRequest* req) {
  // Intentional default initialization
  new (req) MultiPageRequest;
}

FFI_ALWAYS_INLINE void cpp_multi_page_request_destroy(MultiPageRequest* req) {
  ktl::destroy_at(req);
}

FFI_ALWAYS_INLINE void cpp_multi_page_request_cancel_requests(MultiPageRequest* req) {
  req->CancelRequests();
}

FFI_ALWAYS_INLINE zx_status_t cpp_multi_page_request_wait(MultiPageRequest* req, bool suspendable) {
  return req->Wait(suspendable);
}

// PageRequest accessors
FFI_ALWAYS_INLINE uint32_t cpp_page_request_get_type(const PageRequest* request) {
  return static_cast<uint32_t>(PageProviderHelper::GetRequestType(request));
}

FFI_ALWAYS_INLINE uint64_t cpp_page_request_get_offset(const PageRequest* request) {
  return PageProviderHelper::GetRequestOffset(request);
}

FFI_ALWAYS_INLINE uint64_t cpp_page_request_get_len(const PageRequest* request) {
  return PageProviderHelper::GetRequestLen(request);
}

FFI_ALWAYS_INLINE void* cpp_page_request_provider_node(PageRequest* request) {
  return &fbl::DefaultDoublyLinkedListTraits<PageRequest*, PageProviderTag>::node_state(*request);
}

// PageSource failure & helper shims
FFI_ALWAYS_INLINE void cpp_page_source_free(PageSource* src) { delete src; }

FFI_ALWAYS_INLINE void* cpp_page_source_get_ref_counted(PageSource* src) {
  return static_cast<fbl::RefCounted<PageRequestInterface>*>(src);
}

FFI_ALWAYS_INLINE void cpp_page_source_on_pages_failed(PageSource* page_source, uint64_t offset,
                                                       uint64_t len, zx_status_t error_status) {
  page_source->OnPagesFailed(offset, len, error_status);
}

FFI_ALWAYS_INLINE bool cpp_page_source_is_valid_external_failure_code(zx_status_t error_status) {
  return PageSource::IsValidExternalFailureCode(error_status);
}

FFI_ALWAYS_INLINE bool cpp_page_source_is_valid_internal_failure_code(zx_status_t status) {
  return PageSource::IsValidInternalFailureCode(status);
}

FFI_ALWAYS_INLINE bool cpp_page_source_paged_vmo_lock_is_held(PageSource* page_source) {
  return page_source->paged_vmo_lock()->lock().IsHeld();
}

void cpp_page_source_dump(const PageSource* page_source, uint32_t depth, uint32_t max_items) {
  page_source->Dump(depth, max_items);
}

}  // extern "C"
