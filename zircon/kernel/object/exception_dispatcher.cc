// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/exception_dispatcher.h"

#include <lib/object-constants.h>

#include <fbl/alloc_checker.h>
#include <kernel/ffi.h>
#include <ktl/utility.h>

#include <ktl/enforce.h>

extern "C" ExceptionDispatcher* cpp_exception_dispatcher_create(
    ThreadDispatcher* thread, zx_excp_type_t exception_type, const zx_exception_report_t* report,
    const arch_exception_context_t* arch_context) {
  fbl::RefPtr<ThreadDispatcher> thread_ref = fbl::ImportFromRawPtr(thread);
  fbl::AllocChecker ac;
  fbl::RefPtr<ExceptionDispatcher> exception = fbl::AdoptRef(
      new (&ac) ExceptionDispatcher(ktl::move(thread_ref), exception_type, report, arch_context));
  if (!ac.check()) {
    // ExceptionDispatchers are small so if we get to this point a lot of
    // other things will be failing too, but we could potentially pre-
    // allocate space for an ExceptionDispatcher in each thread if we want
    // to eliminate this case.
    return nullptr;
  }
  return fbl::ExportToRawPtr(&exception);
}

ExceptionDispatcher::ExceptionDispatcher(fbl::RefPtr<ThreadDispatcher> thread,
                                         zx_excp_type_t exception_type,
                                         const zx_exception_report_t* report,
                                         const arch_exception_context_t* arch_context)
    : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(ExceptionDispatcher, kExceptionDispatcherStateOffset);
  rust_exception_dispatcher_state_init(&opaque_storage_, this, fbl::ExportToRawPtr(&thread),
                                       exception_type, report, arch_context);
}

IMPLEMENT_DISPATCHER_RUST_STATE(ExceptionDispatcher, rust_exception_dispatcher_state_get_lock,
                                rust_exception_dispatcher_state_destroy)
