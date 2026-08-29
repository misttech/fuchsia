// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/compression_ffi.h"

#include <kernel/ffi.h>
#include <ktl/memory.h>

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE void cpp_vmcompression_destroy(VmCompression* compression) {
  ktl::destroy_at(compression);
}

FFI_ALWAYS_INLINE void cpp_vmcompression_free(VmCompression* compression) { delete compression; }

FFI_ALWAYS_INLINE fbl::RefCounted<VmCompression>* cpp_vmcompression_get_ref_counted(
    VmCompression* compression) {
  return const_cast<fbl::RefCounted<VmCompression>*>(
      static_cast<const fbl::RefCounted<VmCompression>*>(compression));
}

}  // extern "C"
