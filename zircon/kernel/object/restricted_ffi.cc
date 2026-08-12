// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/user_copy/user_ptr.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <kernel/restricted_state.h>
#include <kernel/thread.h>
#include <object/process_dispatcher.h>
#include <object/thread_dispatcher.h>
#include <object/vm_object_dispatcher.h>

extern "C" {

zx_status_t cpp_restricted_bind_state(zx_exception_report_t* out_exception_ptr,
                                      zx_handle_t* out_handle);
zx_status_t cpp_restricted_unbind_state();

zx_status_t cpp_restricted_bind_state(zx_exception_report_t* out_exception_ptr,
                                      zx_handle_t* out_handle) {
  // Are we allowed to create a VMO?
  auto up = ProcessDispatcher::GetCurrent();
  zx_status_t status = up->EnforceBasicPolicy(ZX_POL_NEW_VMO);
  if (status != ZX_OK) {
    return status;
  }

  // Create it.
  user_out_ptr<zx_exception_report_t> uptr = make_user_out_ptr(out_exception_ptr);
  zx::result<ktl::unique_ptr<RestrictedState>> result = RestrictedState::Create(uptr);
  if (result.is_error()) {
    return result.error_value();
  }

  // Now wrap the VMO in a VmObjectDispatcher so we can give a handle back to the user.
  ktl::unique_ptr<RestrictedState> rs = ktl::move(result.value());
  fbl::RefPtr<VmObjectPaged> vmo = rs->vmo();
  KernelHandle<VmObjectDispatcher> kernel_handle;
  zx_rights_t rights;
  const uint64_t size = vmo->size();
  status = VmObjectDispatcher::Create(ktl::move(vmo), size,
                                      VmObjectDispatcher::InitialMutability::kMutable,
                                      &kernel_handle, &rights);
  if (status != ZX_OK) {
    return status;
  }

  // Wrap the VmObjectDispatcher in a Handle.
  status = up->MakeAndAddHandle(ktl::move(kernel_handle), rights, out_handle);
  if (status != ZX_OK) {
    return status;
  }

  // Finally, set this thread's restricted state. Note, it's possible the copy-out of the new
  // handle will fail, but that's OK. If that happens a ZX_EXCP_POLICY_CODE_HANDLE_LEAK will
  // be generated, at which point the caller will either be terminated or will need to handle
  // the exception (likely by retrying the operation with a valid out buffer).
  Thread::Current::Get()->set_restricted_state(ktl::move(rs));

  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_restricted_unbind_state() {
  Thread::Current::Get()->set_restricted_state(nullptr);
  return ZX_OK;
}

}  // extern "C"
