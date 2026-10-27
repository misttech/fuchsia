// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/crashlog.h>
#include <stddef.h>

// The Rust `CrashlogRegs` storage must match the architecture's `crashlog_regs_t`.
#ifdef __aarch64__
static_assert(sizeof(crashlog_regs_t) == 48);
#elifdef __x86_64__
static_assert(sizeof(crashlog_regs_t) == 40);
#elifdef __riscv
static_assert(sizeof(crashlog_regs_t) == 24);
#else
#error "Unsupported architecture for crashlog_regs_t size"
#endif
static_assert(alignof(crashlog_regs_t) == 8);
static_assert(sizeof(crashlog_t) == sizeof(crashlog_regs_t));
