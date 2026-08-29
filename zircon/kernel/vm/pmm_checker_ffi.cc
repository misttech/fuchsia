// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/pmm_checker_ffi.h"

#include <kernel/ffi.h>
#include <ktl/memory.h>

#include "vm/pmm_checker.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE void cpp_pmm_checker_init(ffi::Uninitialized<PmmChecker>* checker) {
  checker->Initialize();
}
FFI_ALWAYS_INLINE void cpp_pmm_checker_destroy(PmmChecker* checker) { ktl::destroy_at(checker); }

}  // extern "C"
