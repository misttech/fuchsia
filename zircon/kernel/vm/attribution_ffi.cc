// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "include/vm/attribution_ffi.h"

#include <kernel/ffi.h>

extern "C" {

FFI_ALWAYS_INLINE void cpp_attribution_counts_zero(vm::AttributionCounts* out_counts) {
  *out_counts = vm::AttributionCounts{};
}

}  // extern "C"
