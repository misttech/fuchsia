// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/handle.h"

#include <kernel/ffi.h>
#include <object/dispatcher.h>

HandleTableArena gHandleTableArena;

// Handles are constructed and destroyed by Rust (rust_handle_make, rust_handle_dup,
// rust_handle_destroy), so every Handle the C++ accessors in handle.h touch was laid out by the
// Rust `Handle` in //zircon/kernel/object/handle.rs, which pins its own layout with a matching set
// of assertions.  The assertions below pin the C++ view of the very same bytes, so that the two
// can never drift apart silently.  They live here rather than in the header because offsetof() on
// Handle's private members needs friend access.
struct HandleLayoutCheck {
  static_assert(sizeof(Handle) == 40);
  static_assert(alignof(Handle) == 8);
  static_assert(offsetof(Handle, handle_table_id_) == 0);
  static_assert(offsetof(Handle, dispatcher_) == 8);
  static_assert(offsetof(Handle, rights_) == 16);
  static_assert(offsetof(Handle, base_value_) == 20);
  static_assert(offsetof(Handle, node_state_) == 24);

  // Validate that all the fields we need to preserve fit within the preservation window.
  static_assert(offsetof(Handle, handle_table_id_) + sizeof(Handle::handle_table_id_) <=
                Handle::PreserveSize);
  static_assert(offsetof(Handle, base_value_) + sizeof(Handle::base_value_) <=
                Handle::PreserveSize);
  static_assert(offsetof(Handle, dispatcher_) + sizeof(Handle::dispatcher_) <=
                Handle::PreserveSize);

  // KernelHandle<Dispatcher> is passed by pointer across the FFI boundary and mirrored by the Rust
  // `KernelHandle<Dispatcher>` in handle.rs.
  static_assert(sizeof(KernelHandle<Dispatcher>) == 8);
  static_assert(alignof(KernelHandle<Dispatcher>) == 8);
};

void Handle::Init() { rust_handle_init(); }

HandleOwner Handle::Make(fbl::RefPtr<Dispatcher> dispatcher, zx_rights_t rights) {
  Handle* h = rust_handle_make(dispatcher.get(), rights);
  if (!h) {
    return nullptr;
  }
  Dispatcher* raw = fbl::ExportToRawPtr(&dispatcher);
  (void)raw;
  return HandleOwner(h);
}

HandleOwner Handle::Make(KernelHandle<Dispatcher> kernel_handle, zx_rights_t rights) {
  Handle* h = rust_handle_make(kernel_handle.dispatcher().get(), rights);
  if (!h) {
    return nullptr;
  }
  fbl::RefPtr<Dispatcher> disp = kernel_handle.release();
  Dispatcher* raw = fbl::ExportToRawPtr(&disp);
  (void)raw;
  return HandleOwner(h);
}

uint32_t Handle::Count(const Dispatcher& dispatcher) { return dispatcher.current_handle_count(); }

int64_t HandleTableArena::get_alloc_failed_count() { return rust_handle_get_alloc_failed_count(); }

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_handle_arena_init(const char* name, size_t max_count) {
  return gHandleTableArena.arena().Init(name, max_count);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_handle_arena_alloc() { return gHandleTableArena.arena().Alloc(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_handle_arena_free(void* ptr) { gHandleTableArena.arena().Free(ptr); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_handle_arena_committed(const void* ptr) {
  return gHandleTableArena.arena().Committed(const_cast<void*>(ptr));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uintptr_t cpp_handle_arena_confine(uintptr_t addr) {
  return gHandleTableArena.arena().Confine(addr);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_handle_arena_base() { return gHandleTableArena.arena().Base(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE size_t cpp_handle_arena_diagnostic_count() {
  return gHandleTableArena.arena().DiagnosticCount();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_handle_arena_dump() { gHandleTableArena.arena().Dump(); }

}  // extern "C"
