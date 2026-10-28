// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_CHECKER_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_CHECKER_FFI_H_

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <zircon/compiler.h>

#include <vm/pmm_checker.h>

__BEGIN_CDECLS

bool rust_pmm_checker_is_valid_fill_size(size_t fill_size);
bool rust_pmm_checker_is_armed(const PmmChecker* checker);
void rust_pmm_checker_print_status(const PmmChecker* checker, FILE* f);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_CHECKER_FFI_H_
