// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_CHECKER_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_CHECKER_FFI_H_

#include <zircon/compiler.h>

#include <kernel/ffi.h>
#include <vm/pmm_checker.h>

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
__BEGIN_CDECLS

FFI_ALWAYS_INLINE void cpp_pmm_checker_init(ffi::Uninitialized<PmmChecker>* checker);
FFI_ALWAYS_INLINE void cpp_pmm_checker_destroy(PmmChecker* checker);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_CHECKER_FFI_H_
