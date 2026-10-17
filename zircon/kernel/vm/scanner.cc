// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <vm/scanner.h>

extern "C" {
void rust_scanner_push_disable_count();
void rust_scanner_pop_disable_count();
void rust_scanner_debug_dump_state_before_panic();
}

void scanner_push_disable_count() { rust_scanner_push_disable_count(); }

void scanner_pop_disable_count() { rust_scanner_pop_disable_count(); }

void scanner_debug_dump_state_before_panic() { rust_scanner_debug_dump_state_before_panic(); }
