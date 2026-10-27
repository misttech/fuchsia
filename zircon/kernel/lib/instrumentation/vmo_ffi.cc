// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>

#include <kernel/ffi.h>

#include "private.h"

#include <ktl/enforce.h>

namespace {

constexpr InstrumentationDataVmoFfi ToFfi(const InstrumentationDataVmo& vmo) {
  return {
      .announce = vmo.announce.data(),
      .announce_len = vmo.announce.size(),
      .sink_name = vmo.sink_name.data(),
      .sink_name_len = vmo.sink_name.size(),
      .units = vmo.units.data(),
      .units_len = vmo.units.size(),
      .scale = vmo.scale,
      .handle = vmo.handle,
  };
}

}  // namespace

__BEGIN_CDECLS

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_llvm_profdata_vmo(InstrumentationDataVmoFfi* out) {
  *out = ToFfi(LlvmProfdataVmo());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_sancov_get_pc_vmo(InstrumentationDataVmoFfi* out) {
  *out = ToFfi(SancovGetPcVmo());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_sancov_get_counts_vmo(InstrumentationDataVmoFfi* out) {
  *out = ToFfi(SancovGetCountsVmo());
}

__END_CDECLS
