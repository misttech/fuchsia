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
void rust_pmm_checker_set_fill_size(PmmChecker* checker, size_t fill_size);
bool rust_pmm_checker_is_armed(const PmmChecker* checker);
void rust_pmm_checker_arm(const PmmChecker* checker);
void rust_pmm_checker_print_status(const PmmChecker* checker, FILE* f);
void rust_pmm_checker_fill_pattern(const PmmChecker* checker, vm_page_t* page);
bool rust_pmm_checker_validate_pattern(const PmmChecker* checker, vm_page_t* page);
void rust_pmm_checker_assert_pattern(const PmmChecker* checker, vm_page_t* page);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_CHECKER_FFI_H_
