// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_HANDLE_TABLE_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_HANDLE_TABLE_H_

#include <lib/object-constants.h>

#include <fbl/canary.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/name.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <kernel/brwlock.h>
#include <kernel/event.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>
#include <kernel/task_runtime_stats.h>
#include <kernel/thread.h>
#include <ktl/array.h>
#include <ktl/forward.h>
#include <ktl/type_traits.h>
#include <object/dispatcher.h>
#include <object/exceptionate.h>
#include <object/futex_context.h>
#include <object/handle.h>
#include <object/job_policy.h>
#include <object/opaque_storage.h>
#include <object/thread_dispatcher.h>
#include <vm/vm_aspace.h>

class HandleTable;
class ProcessDispatcher;

extern "C" {
void rust_handle_table_init(void* storage);
void rust_handle_table_destroy(void* storage);
Lock<BrwLockPi>* rust_handle_table_get_lock(const void* storage);
zx_handle_t rust_handle_table_map_handle_to_value(const void* storage, const Handle* handle);
Handle* rust_handle_table_get_handle_locked(const void* storage, ProcessDispatcher* caller,
                                            zx_handle_t handle_value);
zx_koid_t rust_handle_table_get_koid(const void* storage);
uint32_t rust_handle_table_random_value(const void* storage);
uint32_t rust_handle_table_count(const void* storage);
void rust_handle_table_add_handle(void* storage, Handle* handle);
void rust_handle_table_add_handle_locked(void* storage, Handle* handle);
Handle* rust_handle_table_remove_handle_ptr_locked(void* storage, Handle* handle);
Handle* rust_handle_table_remove_handle_val(void* storage, ProcessDispatcher* caller,
                                            zx_handle_t handle_value);
Handle* rust_handle_table_remove_handle_val_locked(void* storage, ProcessDispatcher* caller,
                                                   zx_handle_t handle_value);
zx_status_t rust_handle_table_clean(void* storage);
using HandleTableForEachCallback = zx_status_t (*)(zx_handle_t handle, zx_rights_t rights,
                                                   const Dispatcher* disp, void* ctx);
zx_status_t rust_handle_table_for_each_handle(const void* storage, HandleTableForEachCallback cb,
                                              void* ctx);
zx_status_t rust_handle_table_for_each_handle_batched(const void* storage,
                                                      HandleTableForEachCallback cb, void* ctx);
}

// A HandleTable is the data structure which stores the Handles
// associated with one or more ProcessDispatchers.
//
// Methods that require a ProcessDispatcher& |caller| will use to supplied
// ProcessDispatcher as the target of ZX_POL_BAD_HANDLE job policy.
class HandleTable {
 public:
  HandleTable() { rust_handle_table_init(&storage_); }
  ~HandleTable() { rust_handle_table_destroy(&storage_); }

  HandleTable(const HandleTable&) = delete;
  HandleTable(HandleTable&&) = delete;

  HandleTable& operator=(const HandleTable&) = delete;
  HandleTable& operator=(HandleTable&&) = delete;

  // Maps a |handle| to an integer which can be given to usermode as a
  // handle value. Uses Handle->base_value() plus additional mixing.
  zx_handle_t MapHandleToValue(const Handle* handle) const {
    return rust_handle_table_map_handle_to_value(&storage_, handle);
  }
  zx_handle_t MapHandleToValue(const HandleOwner& handle) const {
    return rust_handle_table_map_handle_to_value(&storage_, handle.get());
  }

  // Maps a handle value into a Handle as long we can verify that
  // it belongs to this handle table.
  Handle* GetHandleLocked(ProcessDispatcher& caller, zx_handle_t handle_value)
      TA_REQ_SHARED(get_lock()) {
    return GetHandleLocked(&caller, handle_value);
  }

  zx_koid_t get_koid() const { return rust_handle_table_get_koid(&storage_); }
  uint32_t random_value() const { return rust_handle_table_random_value(&storage_); }

  // Returns the number of outstanding handles in this handle table.
  uint32_t HandleCount() const { return rust_handle_table_count(&storage_); }

  // Adds |handle| to this handle table.
  void AddHandle(HandleOwner handle) { rust_handle_table_add_handle(&storage_, handle.release()); }
  void AddHandleLocked(HandleOwner handle) TA_REQ(get_lock()) {
    rust_handle_table_add_handle_locked(&storage_, handle.release());
  }

  // Set of overloads that remove the |handle| or |handle_value| from this
  // handle table and returns ownership to the handle.
  HandleOwner RemoveHandleLocked(Handle* handle) TA_REQ(get_lock()) {
    return HandleOwner(rust_handle_table_remove_handle_ptr_locked(&storage_, handle));
  }
  HandleOwner RemoveHandleLocked(ProcessDispatcher& caller, zx_handle_t handle_value)
      TA_REQ(get_lock()) {
    return HandleOwner(
        rust_handle_table_remove_handle_val_locked(&storage_, &caller, handle_value));
  }
  HandleOwner RemoveHandle(ProcessDispatcher& caller, zx_handle_t handle_value) {
    return HandleOwner(rust_handle_table_remove_handle_val(&storage_, &caller, handle_value));
  }

  // Get the dispatcher corresponding to this handle value, after
  // checking that this handle has the desired rights.
  template <typename T>
  zx_status_t GetDispatcherWithRights(ProcessDispatcher& caller, zx_handle_t handle_value,
                                      zx_rights_t desired_rights, fbl::RefPtr<T>* dispatcher) {
    return GetDispatcherWithRightsImpl(caller, handle_value, desired_rights, dispatcher);
  }

  // Calls the provided
  // |zx_status_t func(zx_handle_t, zx_rights_t, fbl::RefPtr<Dispatcher>)|
  // on every handle owned by the handle table. Stops if |func| returns an error,
  // returning the error value.
  template <typename T>
  zx_status_t ForEachHandle(T func) const {
    auto cb = [](zx_handle_t handle, zx_rights_t rights, const Dispatcher* disp,
                 void* ctx) -> zx_status_t {
      auto& f = *static_cast<T*>(ctx);
      return f(handle, rights, disp);
    };
    return rust_handle_table_for_each_handle(&storage_, cb, &func);
  }

  // Iterates over every handle owned by this handle table and calls |func| on each one.
  //
  // Batch size is bounded to ensure that the lock is not held for an arbitrary amount of time.
  //
  // Calls |zx_status_t func(zx_handle_t, zx_rights_t, const Dispatcher*)| on every handle.
  //
  // |func| may perform operations on the handle table, including adding and removing handles.
  //
  // Stops if |func| returns an error, returning the error value.
  //
  // Note: While this method breaks up its iteration into multiple critical sections, it does
  // not release any resources it acquires until the entire iteration is complete, including at the
  // end of the call.
  //
  // Handles being added or removed concurrent with |ForEachHandleBatched| may or may not be
  // observed by |func|.
  //
  // A Handle observed by |func| may or may not be owned by the handle table at the moment |func| is
  // invoked, however, it is guaranteed it was held at some point between the invocation of this
  // method and |func|.
  template <typename Func>
  zx_status_t ForEachHandleBatched(Func&& func) {
    using CallbackType = ktl::remove_reference_t<Func>;
    auto cb = [](zx_handle_t handle, zx_rights_t rights, const Dispatcher* disp,
                 void* ctx) -> zx_status_t {
      auto& f = *static_cast<CallbackType*>(ctx);
      return f(handle, rights, disp);
    };
    return rust_handle_table_for_each_handle_batched(&storage_, cb, &func);
  }

  // Called when all the containing ProcessDispatchers have transitioned to the Dead state.
  void Clean() { rust_handle_table_clean(&storage_); }

  // accessors
  Lock<BrwLockPi>* get_lock() const { return rust_handle_table_get_lock(&storage_); }

 private:
  // Same as public |GetHandleLocked| overload, except process can be null.
  //
  // When |caller| is null, no policy enforcement happens.
  Handle* GetHandleLocked(ProcessDispatcher* caller, zx_handle_t handle_value)
      TA_REQ_SHARED(get_lock()) {
    return rust_handle_table_get_handle_locked(&storage_, caller, handle_value);
  }

  // Get the dispatcher corresponding to this handle value, after
  // checking that this handle has the desired rights.
  // WRONG_TYPE is returned before ACCESS_DENIED, because if the
  // wrong handle was passed, evaluating its rights does not have
  // much meaning and also this aids in debugging.
  template <typename T>
  zx_status_t GetDispatcherWithRightsImpl(ProcessDispatcher& caller, zx_handle_t handle_value,
                                          zx_rights_t desired_rights,
                                          fbl::RefPtr<T>* out_dispatcher) {
    bool has_desired_rights;
    fbl::RefPtr<Dispatcher> generic_dispatcher;

    {
      // Scope utilized to reduce lock duration.
      Guard<BrwLockPi, BrwLockPi::Reader> guard{get_lock()};
      Handle* handle = GetHandleLocked(caller, handle_value);
      if (!handle)
        return ZX_ERR_BAD_HANDLE;

      has_desired_rights = handle->HasRights(desired_rights);
      generic_dispatcher = handle->dispatcher();
    }

    fbl::RefPtr<T> dispatcher = DownCastDispatcher<T>(&generic_dispatcher);

    // Wrong type takes precedence over access denied.
    if (!dispatcher)
      return ZX_ERR_WRONG_TYPE;

    if (!has_desired_rights)
      return ZX_ERR_ACCESS_DENIED;

    *out_dispatcher = ktl::move(dispatcher);
    return ZX_OK;
  }

  // Backing storage for the Rust `HandleTable` (see handle_table.rs), which owns all of the
  // state; this class is only a thin wrapper around it.  handle_table.cc verifies the size and
  // alignment match.
  OpaqueStorage<kHandleTableStorageSize, kHandleTableStorageAlign> storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_HANDLE_TABLE_H_
