// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/wake-vector.h>

#include <kernel/ffi.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_wake_vector_get_diagnostics(
    const wake_vector::WakeVector* wake_vector,
    wake_vector::WakeVector::Diagnostics* diagnostics_out) {
  wake_vector->GetDiagnostics(*diagnostics_out);
}

}  // extern "C"
