// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <inttypes.h>
#include <lib/syscalls/forward.h>
#include <zircon/errors.h>

#include <object/mbo_dispatcher.h>

// zx_status_t zx_mbo_create
zx_status_t sys_mbo_create(uint32_t options, user_out_handle* out) {
  if (options != 0u)
    return ZX_ERR_INVALID_ARGS;

  KernelHandle<MBODispatcher> handle;
  zx_rights_t rights;

  zx_status_t result = MBODispatcher::Create(&handle, &rights);
  if (result != ZX_OK)
    return result;
  return out->make(ktl::move(handle), rights);
}
