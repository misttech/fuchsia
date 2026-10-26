// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_EXCEPTION_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_EXCEPTION_DISPATCHER_H_

#include <lib/object-constants.h>
#include <zircon/rights.h>
#include <zircon/syscalls/exception.h>
#include <zircon/types.h>

#include <arch/exception.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/opaque_storage.h>
#include <object/thread_dispatcher.h>

class ExceptionDispatcher;

extern "C" {
ExceptionDispatcher* cpp_exception_dispatcher_create(ThreadDispatcher* thread,
                                                     zx_excp_type_t exception_type,
                                                     const zx_exception_report_t* report,
                                                     const arch_exception_context_t* arch_context);

void rust_exception_dispatcher_state_init(void* state, void* disp, ThreadDispatcher* thread,
                                          zx_excp_type_t exception_type,
                                          const zx_exception_report_t* report,
                                          const arch_exception_context_t* arch_context);
void rust_exception_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_exception_dispatcher_state_get_lock(const void* state);
void rust_exception_dispatcher_on_zero_handles(ExceptionDispatcher* disp);
}

// Zircon channel-based exception handling uses two primary classes,
// ExceptionDispatcher (this file) and Exceptionate (exceptionate.h).
//
// An ExceptionDispatcher represents a single currently-active exception. This
// will be transmitted to registered exception handlers in userspace and
// provides them with exception state and control functionality.
//
// An Exceptionate wraps a channel endpoint to help with sending exceptions to
// userspace. It is a kernel-internal helper class and not exposed to userspace.

class ExceptionDispatcher final : public Dispatcher {
 public:
  ~ExceptionDispatcher() final;

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_EXCEPTION; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final {
    return (ZX_DEFAULT_EXCEPTION_RIGHTS & ZX_RIGHT_WAIT) != 0;  // NOLINT(bugprone-signed-bitwise)
  }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

  // Marks the current exception handler as done.
  //
  // Once a handle has been created around this object, either
  // wait_for_handle_close() or discard_handle_close() must be called to reset
  // our state for the next handler.
  void on_zero_handles() final { rust_exception_dispatcher_on_zero_handles(this); }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend ExceptionDispatcher* cpp_exception_dispatcher_create(
      ThreadDispatcher* thread, zx_excp_type_t exception_type, const zx_exception_report_t* report,
      const arch_exception_context_t* arch_context);

  ExceptionDispatcher(fbl::RefPtr<ThreadDispatcher> thread, zx_excp_type_t exception_type,
                      const zx_exception_report_t* report,
                      const arch_exception_context_t* arch_context);

  OpaqueStorage<kExceptionDispatcherStateSize, kExceptionDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_EXCEPTION_DISPATCHER_H_
