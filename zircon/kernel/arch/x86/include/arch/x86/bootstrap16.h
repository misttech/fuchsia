// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_BOOTSTRAP16_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_BOOTSTRAP16_H_

// LINT.IfChange
#define BCD_PHYS_BOOTSTRAP_PML4_OFFSET 0
#define BCD_PHYS_KERNEL_PML4_OFFSET 4
#define BCD_PHYS_GDTR_OFFSET 10
#define BCD_PHYS_LM_ENTRY_OFFSET 16
#define BCD_LM_CS_OFFSET 20
#define BCD_VIRT_LM_HIGH_ENTRY_OFFSET 24
#define BCD_CPU_COUNTER_OFFSET 32
#define BCD_CPU_WAITING_OFFSET 36
#define BCD_PER_CPU_BASE_OFFSET 44
// LINT.ThenChange(//zircon/kernel/arch/x86/src/bootstrap16.rs)

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_BOOTSTRAP16_H_
