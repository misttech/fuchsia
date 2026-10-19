// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_HANDLE_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_HANDLE_H_

#include <stdint.h>
#include <zircon/types.h>

#include <fbl/gparena.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/macros.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/atomic.h>
#include <ktl/utility.h>

class Dispatcher;
class Handle;

constexpr uint32_t kHandleReservedBits = 2;

template <typename T>
class KernelHandle;

extern "C" {
zx_status_t cpp_handle_arena_init(const char* name, size_t max_count);
void* cpp_handle_arena_alloc();
void cpp_handle_arena_free(void* ptr);
bool cpp_handle_arena_committed(const void* ptr);
uintptr_t cpp_handle_arena_confine(uintptr_t addr);
void* cpp_handle_arena_base();
size_t cpp_handle_arena_diagnostic_count();
void cpp_handle_arena_dump();

zx_status_t rust_handle_init();
Handle* rust_handle_from_u32(uint32_t value);
Handle* rust_handle_make(Dispatcher* dispatcher, zx_rights_t rights);
Handle* rust_handle_dup(const Handle* source, zx_rights_t rights);
void rust_handle_destroy(Handle* handle);
size_t rust_handle_diagnostic_count();
void rust_handle_dump_table_info();
int64_t rust_handle_get_alloc_failed_count();
}

// Callable object for destroying uniquely owned handles.
struct HandleDestroyer {
  void operator()(Handle*);
};

// HandleOwner wraps a Handle in a unique_ptr that has single
// ownership of the Handle and deletes it whenever it falls out of scope.
using HandleOwner = ktl::unique_ptr<Handle, HandleDestroyer>;

class HandleTableArena;

// A Handle is how a specific process refers to a specific Dispatcher.
class Handle final {
 public:
  // Returns the Dispatcher to which this instance points.
  const fbl::RefPtr<Dispatcher>& dispatcher() const { return dispatcher_; }

  // Returns the handle table that owns this instance. Used to guarantee
  // that a process may only access handles in its own handle table.
  zx_koid_t handle_table_id() const { return handle_table_id_.load(ktl::memory_order_relaxed); }

  // Sets the value returned by handle_table_id().
  void set_handle_table_id(zx_koid_t pid);

  // Returns the |rights| parameter that was provided when this instance
  // was created.
  uint32_t rights() const { return rights_; }

  // Returns true if this handle has all of the desired rights bits set.
  bool HasRights(zx_rights_t desired) const { return (rights_ & desired) == desired; }

  // Returns a value that can be decoded by Handle::FromU32() to derive a
  // pointer to this instance.  ProcessDispatcher will XOR this with its
  // |handle_rand_| to create the zx_handle_t value that user space sees.
  uint32_t base_value() const { return base_value_; }

  // To be called once during bring up.
  static void Init();

  // Maps an integer obtained by Handle::base_value() back to a Handle.
  static Handle* FromU32(uint32_t value);

  // Get the number of outstanding handles for a given dispatcher.
  static uint32_t Count(const Dispatcher&);

  // Should only be called by diagnostics.cpp.
  struct diagnostics {
    // Dumps internal details of the handle table using printf().
    static void DumpTableInfo();

    // Returns the number of outstanding handles.
    static size_t OutstandingHandles();
  };

  // Handle should never be created by anything other than Make or Dup.
  static HandleOwner Make(fbl::RefPtr<Dispatcher> dispatcher, zx_rights_t rights);
  static HandleOwner Make(KernelHandle<Dispatcher> kernel_handle, zx_rights_t rights);
  static HandleOwner Dup(const Handle& source, zx_rights_t rights);

  // Use a manually declared linked list node state instead of inheriting so that the early
  // memory of the class can be used by the members we want to preserve, and our NodeState can
  // be placed later.
  using NodeState = fbl::DoublyLinkedListNodeState<Handle*>;
  struct NodeListTraits {
    static NodeState& node_state(Handle& h) { return h.node_state_; }
  };

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(Handle);

  // Called only by Make.
  Handle(fbl::RefPtr<Dispatcher> dispatcher, zx_rights_t rights, uint32_t base_value);
  // Called only by Dup.
  Handle(const Handle& rhs, zx_rights_t rights, uint32_t base_value);

  // Handle should never be destroyed by anything other than rust_handle_destroy.
  ~Handle() = default;

  // handle_table_id_ is atomic because threads from different processes can
  // access it concurrently, while holding different instances of
  // handle_table_lock_.
  ktl::atomic<zx_koid_t> handle_table_id_;
  fbl::RefPtr<Dispatcher> dispatcher_;
  const zx_rights_t rights_;
  const uint32_t base_value_;

  // Up to here the members need to be preserved when handles are free'd to the arena. The
  // PreserveSize is an 'approximation' of how large all the previous members are; HandleLayoutCheck
  // in handle.cc statically validates that the chosen PreserveSize is correct. Any incorrect size
  // will result in a compilation error!
  static constexpr size_t PreserveSize = 24;
  friend HandleTableArena;

  // Validates in handle.cc that this C++ view of a Handle matches the Rust definition that
  // actually constructs them (see handle.rs), and that the preserved members fit within
  // PreserveSize.
  friend struct HandleLayoutCheck;

  NodeState node_state_;
};

// Owns the GPArena backing all Handles. Allocation, construction and destruction of Handles is
// done by Rust (see handle.rs), which reaches the arena through the cpp_handle_arena_* shims.
class HandleTableArena {
 public:
  static int64_t get_alloc_failed_count();

  using HandleGPArena = fbl::GPArena<Handle::PreserveSize, sizeof(Handle)>;
  HandleGPArena& arena() { return arena_; }

 private:
  HandleGPArena arena_;
};

extern HandleTableArena gHandleTableArena;

// This can't be defined directly in the HandleDestroyer struct definition
// because Handle is an incomplete type at that point.
inline void HandleDestroyer::operator()(Handle* handle) {
  // ktl::unique_ptr only calls its deleter when the pointer is non-null.
  // Still, we double check.
  DEBUG_ASSERT(handle != nullptr);
  rust_handle_destroy(handle);
}

// A minimal wrapper around a Dispatcher which is owned by the kernel.
//
// Intended usage when creating a new Dispatcher object is:
//   1. Create a KernelHandle on the stack (cannot fail)
//   2. Move the RefPtr<Dispatcher> into the KernelHandle (cannot fail)
//   3. When ready to give the handle to a process, upgrade the KernelHandle
//      to a full HandleOwner via Handle::Make() (can fail)
//
// This sequence ensures that the Dispatcher's on_zero_handles() method is
// called even if errors occur during or before HandleOwner creation, which
// is necessary to break circular references for some Dispatcher types.
//
// This class is thread-unsafe and must be externally synchronized if used
// across multiple threads.
template <typename T>
class KernelHandle {
 public:
  KernelHandle() = default;

  // |dispatcher|'s handle count must be 0.
  explicit KernelHandle(fbl::RefPtr<T> dispatcher) : dispatcher_(ktl::move(dispatcher)) {
    DEBUG_ASSERT(!dispatcher_ || dispatcher_->current_handle_count() == 0);
  }

  ~KernelHandle() { reset(); }

  // Movable but not copyable since we own the underlying Dispatcher.
  KernelHandle(const KernelHandle&) = delete;
  KernelHandle& operator=(const KernelHandle&) = delete;

  template <typename U>
  KernelHandle(KernelHandle<U>&& other) : dispatcher_(ktl::move(other.dispatcher_)) {}

  template <typename U>
  KernelHandle& operator=(KernelHandle<U>&& other) {
    reset(ktl::move(other.dispatcher_));
    return *this;
  }

  void reset() { reset(fbl::RefPtr<T>()); }

  template <typename U>
  void reset(fbl::RefPtr<U> dispatcher) {
    if (dispatcher_) {
      dispatcher_->on_zero_handles();
    }
    dispatcher_ = ktl::move(dispatcher);
  }

  const fbl::RefPtr<T>& dispatcher() const { return dispatcher_; }

  fbl::RefPtr<T> release() { return ktl::move(dispatcher_); }

 private:
  template <typename U>
  friend class KernelHandle;

  fbl::RefPtr<T> dispatcher_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_HANDLE_H_
