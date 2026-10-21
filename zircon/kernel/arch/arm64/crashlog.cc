// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stdio.h>

#include <arch/crashlog.h>

extern "C" void rust_arch_render_crashlog_registers(FILE* target, const crashlog_regs_t* regs);

void arch_render_crashlog_registers(FILE& target, const crashlog_regs_t& regs) {
  rust_arch_render_crashlog_registers(&target, &regs);
}
