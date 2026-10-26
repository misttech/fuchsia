// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_FUTEX_CONTEXT_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_FUTEX_CONTEXT_H_

#include <lib/object-constants.h>
#include <zircon/types.h>

#include <arch/vm.h>
#include <kernel/ffi.h>
#include <object/opaque_storage.h>

class FutexId {
 public:
  explicit constexpr FutexId(uintptr_t id) : id_(id) {}
  uintptr_t get() const { return id_; }

  static inline constexpr FutexId Null() {
    // Since user address spaces always start above 0, 0 is never a legal
    // address and we are free to use it as a sentinel value.
    static_assert(USER_ASPACE_BASE > 0);
    return FutexId{};
  }

 private:
  constexpr FutexId() : id_(0) {}

  uintptr_t id_;
};

extern "C" {

void rust_futex_context_init_storage(void* storage);
void rust_futex_context_drop_storage(void* storage);
zx_status_t rust_futex_context_init(void* storage, uint32_t process_type);

}  // extern "C"

// FutexContext
//
// A FutexContext is the object which manages the state of all of the active
// futexes for a user-mode process.  Each ProcessDispatcher in the system will
// have a single FutexContext contained within it, and the objects should exist
// no-where else in the system.
class FutexContext {
 public:
  // See |Init|.
  enum class ProcessType : uint32_t {
    Regular = 0,
    Shared = 1,
  };

  FutexContext() { rust_futex_context_init_storage(&storage_); }
  ~FutexContext() { rust_futex_context_drop_storage(&storage_); }

  // Initialize this instance for use in the specified process type.
  //
  // Must be called exactly once.
  zx_status_t Init(ProcessType type) {
    return rust_futex_context_init(&storage_, static_cast<uint32_t>(type));
  }

  void* get() const { return const_cast<void*>(static_cast<const void*>(&storage_)); }

 private:
  // FutexContexts may not be copied, moved, or allocated on the heap.  They
  // are to exist as singleton members of the ProcessDispatcher class and
  // exist nowhere else in the system.
  FutexContext(const FutexContext&) = delete;
  FutexContext& operator=(const FutexContext&) = delete;
  FutexContext(FutexContext&&) = delete;
  FutexContext& operator=(FutexContext&&) = delete;
  static void* operator new(size_t) = delete;
  static void* operator new[](size_t) = delete;

  OpaqueStorage<kFutexContextStorageSize, kFutexContextStorageAlign> storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_FUTEX_CONTEXT_H_
