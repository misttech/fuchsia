// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_FUTEX_CONTEXT_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_FUTEX_CONTEXT_H_

#include <lib/object-constants.h>
#include <lib/user_copy/user_ptr.h>
#include <zircon/types.h>

#include <arch/vm.h>
#include <object/opaque_storage.h>

class FutexId {
 public:
  // Shift for a better ID for hashing since the low bits are zero.
  FutexId(user_in_ptr<const zx_futex_t> value_ptr) : id_(GetFutexID(value_ptr.get()) >> 2) {}
  explicit constexpr FutexId(uintptr_t id) : id_(id) {}
  bool operator==(const FutexId& other) const { return id_ == other.id_; }
  bool operator!=(const FutexId& other) const { return id_ != other.id_; }
  uintptr_t get() const { return id_; }

  static inline constexpr FutexId Null() {
    // Since user address spaces always start above 0, 0 is never a legal
    // address and we are free to use it as a sentinel value.
    static_assert(USER_ASPACE_BASE > 0);
    return FutexId{};
  }
  static size_t GetHash(FutexId key) { return key.id_; }

 private:
  constexpr FutexId() : id_(0) {}

  static inline uintptr_t GetFutexID(const zx_futex_t* value_ptr) {
    uintptr_t futex_id = reinterpret_cast<uintptr_t>(value_ptr);
#if defined(__aarch64__)
    // The futex ID is the pointer to the futex itself. This pointer may be tagged.
    // As per [RFC 143]
    // (https://fuchsia.dev/fuchsia-src/contribute/governance/rfcs/0143_userspace_top_byte_ignore),
    // tagged user pointers are treated the same as their untagged equivalents.
    futex_id = arch_detag_ptr(futex_id);
#endif
    return futex_id;
  }

  uintptr_t id_;
};

extern "C" {

void rust_futex_context_init_storage(void* storage);
void rust_futex_context_drop_storage(void* storage);
zx_status_t rust_futex_context_init(void* storage, uint32_t process_type);
zx_status_t rust_futex_context_grow_pool(const void* storage);
void rust_futex_context_shrink_pool(const void* storage);

}  // extern "C"

// FutexContext
//
// A FutexContext is the object which manages the state of all of the active
// futexes for a user-mode process.  Each ProcessDispatcher in the system will
// have a single FutexContext contained within it, and the objects should exist
// no-where else in the system.
class FutexContext {
 public:
  enum class OwnerAction : uint32_t {
    RELEASE = 0,
    ASSIGN_WOKEN = 1,
  };

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

  zx_status_t GrowFutexStatePool() { return rust_futex_context_grow_pool(&storage_); }
  void ShrinkFutexStatePool() { rust_futex_context_shrink_pool(&storage_); }

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
