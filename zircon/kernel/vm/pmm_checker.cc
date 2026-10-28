// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/pmm_checker.h"

#include <vm/pmm_checker_ffi.h>

// PmmChecker is implemented in Rust (pmm_checker.rs); these methods forward to it.

// static
bool PmmChecker::IsValidFillSize(size_t fill_size) {
  return rust_pmm_checker_is_valid_fill_size(fill_size);
}

bool PmmChecker::IsArmed() const { return rust_pmm_checker_is_armed(this); }

void PmmChecker::PrintStatus(FILE* f) const { rust_pmm_checker_print_status(this, f); }
