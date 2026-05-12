// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/syscalls/forward.h>
#include <zircon/types.h>

#include <kernel/mp.h>

namespace {

void DataIpiTask(void* context) { arch::ThreadMemoryBarrier(); }

void InstructionIpiTask(void* context) {
  // All architectures require a memory barrier before serializing the instruction stream.
  arch::ThreadMemoryBarrier();
  // The intrinsics for serializing the instruction stream vary by architecture.
  // TODO(https://fxbug.dev/42126965): Rationalize these.
#if defined(__aarch64__)
  __isb(ARM_MB_SY);
#elif defined(__x86_64__)
  arch::SerializeInstructions();
#elif defined(__riscv)
  __asm__ volatile("fence.i");
#else
#error Unknown architecture.
#endif
}

}  // namespace

void sys_membarrier_sync_process_data() {
  // The membarrier operations are defined to operate on at least all running threads in
  // the calling process (specifically the calling thread's futex context). For now, just
  // issue a barrier to all running CPUs.
  mp_sync_exec(mp_ipi_target::ALL, 0u, DataIpiTask, nullptr);
}

void sys_membarrier_sync_process_insn() {
  // The membarrier operations are defined to operate on at least all running threads in
  // the calling process (specifically the calling thread's futex context). For now, just
  // issue a barrier to all running CPUs.
  mp_sync_exec(mp_ipi_target::ALL, 0u, InstructionIpiTask, nullptr);
}
