// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_FAKE_BTI_RUST_FFI_H_
#define LIB_DRIVER_FAKE_BTI_RUST_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

// LINT.IfChange

__BEGIN_CDECLS

zx_status_t fake_bti_create(zx_handle_t* out);

zx_status_t fake_bti_set_paddrs(zx_handle_t bti, const zx_paddr_t* paddrs, size_t count);

__END_CDECLS

// LINT.ThenChange(//sdk/lib/driver/fake-bti/rust/src/ffi.rs)

#endif  // LIB_DRIVER_FAKE_BTI_RUST_FFI_H_
