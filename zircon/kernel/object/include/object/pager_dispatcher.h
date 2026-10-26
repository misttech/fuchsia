// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PAGER_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PAGER_DISPATCHER_H_

#include <lib/object-constants.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <object/opaque_storage.h>
#include <object/pager_proxy.h>

class PagerDispatcher;
class PortDispatcher;

extern "C" {
zx_status_t cpp_pager_dispatcher_create(
    ffi::Uninitialized<KernelHandle<PagerDispatcher>>* handle_out);

void cpp_pager_proxy_free(PagerProxy* proxy);
void* cpp_pager_proxy_get_ref_counted(PagerProxy* proxy);
const void* cpp_pager_proxy_get_dll_node(const PagerProxy* proxy);
zx_status_t cpp_pager_proxy_create(PagerDispatcher* dispatcher, PortDispatcher* port, uint64_t key,
                                   uint32_t options, PagerProxy** out_proxy);
zx_status_t cpp_pager_proxy_create_page_source(PagerProxy* proxy, PageSource** out_src);

void rust_pager_dispatcher_state_init(void* state, const PagerDispatcher* disp);
void rust_pager_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_pager_dispatcher_state_get_lock(const void* state);
void rust_pager_dispatcher_on_zero_handles(const PagerDispatcher* disp);
PagerProxy* rust_pager_dispatcher_release_proxy(const PagerDispatcher* disp, PagerProxy* proxy);
void rust_pager_dispatcher_get_debug_name(const PagerDispatcher* disp, char* name, size_t len);
}

class PagerDispatcher final : public Dispatcher {
 public:
  ~PagerDispatcher() final;

  // Drop and return this object's reference to |proxy|. Must be called under
  // |proxy|'s lock to prevent races with dispatcher teardown.
  fbl::RefPtr<PagerProxy> ReleaseProxy(PagerProxy* proxy) const TA_REQ(proxy->mtx_) {
    return fbl::ImportFromRawPtr(rust_pager_dispatcher_release_proxy(this, proxy));
  }

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_PAGER; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final { return false; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

  void on_zero_handles() final { rust_pager_dispatcher_on_zero_handles(this); }

  void get_debug_name(char* name, size_t len) const {
    rust_pager_dispatcher_get_debug_name(this, name, len);
  }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_pager_dispatcher_create(
      ffi::Uninitialized<KernelHandle<PagerDispatcher>>* handle_out);

  explicit PagerDispatcher();
  DISALLOW_COPY_ASSIGN_AND_MOVE(PagerDispatcher);

  OpaqueStorage<kPagerDispatcherStateSize, kPagerDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PAGER_DISPATCHER_H_
