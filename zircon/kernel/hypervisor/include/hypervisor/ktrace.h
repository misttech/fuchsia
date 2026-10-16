// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_KTRACE_H_
#define ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_KTRACE_H_

#include <stdint.h>

enum VcpuMeta : uint8_t {
  // Waits.
  VCPU_INTERRUPT,
  VCPU_PORT,

  // Do not use.
  VCPU_META_COUNT,
};
static_assert(sizeof(VcpuMeta) == sizeof(uint8_t));

enum VcpuExit : uint8_t {
#if ARCH_ARM64
  VCPU_UNDERFLOW_MAINTENANCE_INTERRUPT,
  VCPU_PHYSICAL_INTERRUPT,
  VCPU_WFI_INSTRUCTION,
  VCPU_WFE_INSTRUCTION,
  VCPU_SMC_INSTRUCTION,
  VCPU_SYSTEM_INSTRUCTION,
  VCPU_INSTRUCTION_ABORT,
  VCPU_DATA_ABORT,
  VCPU_SERROR_INTERRUPT,
#elif ARCH_X86
  VCPU_EXCEPTION_OR_NMI,
  VCPU_EXTERNAL_INTERRUPT,
  VCPU_INTERRUPT_WINDOW,
  VCPU_CPUID,
  VCPU_HLT,
  VCPU_CONTROL_REGISTER_ACCESS,
  VCPU_IO_INSTRUCTION,
  VCPU_RDMSR,
  VCPU_WRMSR,
  VCPU_VM_ENTRY_FAILURE,
  VCPU_EPT_VIOLATION,
  VCPU_XSETBV,
  VCPU_PAUSE,
  VCPU_VMCALL,
#endif
  VCPU_NOT_SUPPORTED,
  VCPU_FAILURE,

  // Do not use.
  VCPU_EXIT_COUNT,
};
static_assert(sizeof(VcpuExit) == sizeof(uint8_t));

enum VcpuBlockOp : uint8_t {
  VCPU_BLOCK,
  VCPU_UNBLOCK,
};
static_assert(sizeof(VcpuBlockOp) == sizeof(uint8_t));

extern "C" {
void rust_ktrace_vcpu(VcpuBlockOp block_op, VcpuMeta meta);
void rust_ktrace_vcpu_exit(VcpuExit exit, uint64_t exit_address);
}

inline void ktrace_vcpu(VcpuBlockOp block_op, VcpuMeta meta) { rust_ktrace_vcpu(block_op, meta); }

inline void ktrace_vcpu_exit(VcpuExit exit, uint64_t exit_address) {
  rust_ktrace_vcpu_exit(exit, exit_address);
}

#endif  // ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_KTRACE_H_
