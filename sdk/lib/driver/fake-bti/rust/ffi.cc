// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "sdk/lib/driver/fake-bti/rust/ffi.h"

#include <vector>

#include "sdk/lib/driver/fake-bti/cpp/fake-bti.h"

namespace {

__BEGIN_C_DECLS

__EXPORT
zx_status_t fake_bti_create(zx_handle_t* out) {
  zx::result result = fake_bti::CreateFakeBti();
  if (result.is_error()) {
    return result.status_value();
  }
  *out = result->release();
  return ZX_OK;
}

__EXPORT
zx_status_t fake_bti_set_paddrs(zx_handle_t bti, const zx_paddr_t* paddrs, size_t count) {
  std::vector<zx_paddr_t> owned_paddrs(paddrs, paddrs + count);
  zx::result result = fake_bti::SetPaddrs(zx::unowned_bti(bti), std::move(owned_paddrs));
  if (result.is_error()) {
    return result.status_value();
  }
  return ZX_OK;
}

__END_C_DECLS

}  // namespace
