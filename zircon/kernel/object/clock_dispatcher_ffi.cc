// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/errors.h>

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/utility.h>
#include <object/clock_dispatcher.h>
#include <object/handle.h>
#include <vm/vm_object_paged.h>

#include <ktl/enforce.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t
cpp_clock_dispatcher_create(uint64_t options, zx_time_t backstop_time, VmObjectPaged* vmo,
                            ffi::Uninitialized<KernelHandle<ClockDispatcher>>* handle_out) {
  // Take ownership of the caller's reference before allocating so that it is released if the
  // allocation fails (since a failed operator new does not evaluate the constructor's arguments).
  fbl::RefPtr<VmObjectPaged> vmo_ref = fbl::ImportFromRawPtr(vmo);
  fbl::AllocChecker ac;
  auto disp = fbl::AdoptRef(new (&ac) ClockDispatcher(options, backstop_time, ktl::move(vmo_ref)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  handle_out->Initialize(ktl::move(disp));
  return ZX_OK;
}

}  // extern "C"
