// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_HANDLE_TABLE_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_HANDLE_TABLE_H_

#include <lib/object-constants.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>

extern "C" {
void rust_handle_table_init(void* storage);
void rust_handle_table_destroy(void* storage);
uint32_t rust_handle_table_count(const void* storage);
void rust_handle_table_add_handle(void* storage, Handle* handle);
zx_status_t rust_handle_table_clean(void* storage);
using HandleTableForEachCallback = zx_status_t (*)(zx_handle_t handle, zx_rights_t rights,
                                                   const Dispatcher* disp, void* ctx);
zx_status_t rust_handle_table_for_each_handle(const void* storage, HandleTableForEachCallback cb,
                                              void* ctx);
}

// A HandleTable is the data structure which stores the Handles
// associated with one or more ProcessDispatchers.
class HandleTable {
 public:
  HandleTable() { rust_handle_table_init(&storage_); }
  ~HandleTable() { rust_handle_table_destroy(&storage_); }

  HandleTable(const HandleTable&) = delete;
  HandleTable(HandleTable&&) = delete;

  HandleTable& operator=(const HandleTable&) = delete;
  HandleTable& operator=(HandleTable&&) = delete;

  // Returns the number of outstanding handles in this handle table.
  uint32_t HandleCount() const { return rust_handle_table_count(&storage_); }

  // Adds |handle| to this handle table.
  void AddHandle(HandleOwner handle) { rust_handle_table_add_handle(&storage_, handle.release()); }

  // Calls the provided
  // |zx_status_t func(zx_handle_t, zx_rights_t, const Dispatcher*)|
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

  // Called when all the containing ProcessDispatchers have transitioned to the Dead state.
  void Clean() { rust_handle_table_clean(&storage_); }

 private:
  // Backing storage for the Rust `HandleTable` (see handle_table.rs), which owns all of the
  // state; this class is only a thin wrapper around it.
  OpaqueStorage<kHandleTableStorageSize, kHandleTableStorageAlign> storage_;
};

static_assert(sizeof(HandleTable) == kHandleTableStorageSize);
static_assert(alignof(HandleTable) == kHandleTableStorageAlign);

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_HANDLE_TABLE_H_
