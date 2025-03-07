// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/syscalls/debug.h>

#include <hwreg/asm.h>

int main(int argc, char** argv) {
  return hwreg::AsmHeader()  //
      .Line({"#if defined(__aarch64__)"})
      .Macro("REGS_X0", offsetof(zx_arm64_thread_state_general_regs_t, r[0]))
      .Macro("REGS_X(n)", "(REGS_X0 + ((n) * 8))")
      .Macro("REGS_LR", offsetof(zx_arm64_thread_state_general_regs_t, lr))
      .Macro("REGS_SP", offsetof(zx_arm64_thread_state_general_regs_t, sp))
      .Macro("REGS_PC", offsetof(zx_arm64_thread_state_general_regs_t, pc))
      .Macro("REGS_CPSR", offsetof(zx_arm64_thread_state_general_regs_t, cpsr))
      .Macro("REGS_FPCR", offsetof(zx_arm64_thread_state_vector_regs_t, fpcr))
      .Macro("REGS_FPSR", offsetof(zx_arm64_thread_state_vector_regs_t, fpsr))
      .Macro("REGS_Q0", offsetof(zx_arm64_thread_state_vector_regs_t, v))
      .Macro("REGS_Q(n)", "(REGS_Q0 + ((n) * 16))")
      .Line({"#elif defined(__riscv)"})
      .Macro("REGS_PC", offsetof(zx_riscv64_thread_state_general_regs_t, pc))
      .Macro("REGS_RA", offsetof(zx_riscv64_thread_state_general_regs_t, ra))
      .Macro("REGS_SP", offsetof(zx_riscv64_thread_state_general_regs_t, sp))
      .Macro("REGS_GP", offsetof(zx_riscv64_thread_state_general_regs_t, gp))
      .Macro("REGS_TP", offsetof(zx_riscv64_thread_state_general_regs_t, tp))
      .Macro("REGS_T0", offsetof(zx_riscv64_thread_state_general_regs_t, t0))
      .Macro("REGS_T1", offsetof(zx_riscv64_thread_state_general_regs_t, t1))
      .Macro("REGS_T2", offsetof(zx_riscv64_thread_state_general_regs_t, t2))
      .Macro("REGS_S0", offsetof(zx_riscv64_thread_state_general_regs_t, s0))
      .Macro("REGS_S1", offsetof(zx_riscv64_thread_state_general_regs_t, s1))
      .Macro("REGS_A0", offsetof(zx_riscv64_thread_state_general_regs_t, a0))
      .Macro("REGS_A1", offsetof(zx_riscv64_thread_state_general_regs_t, a1))
      .Macro("REGS_A2", offsetof(zx_riscv64_thread_state_general_regs_t, a2))
      .Macro("REGS_A3", offsetof(zx_riscv64_thread_state_general_regs_t, a3))
      .Macro("REGS_A4", offsetof(zx_riscv64_thread_state_general_regs_t, a4))
      .Macro("REGS_A5", offsetof(zx_riscv64_thread_state_general_regs_t, a5))
      .Macro("REGS_A6", offsetof(zx_riscv64_thread_state_general_regs_t, a6))
      .Macro("REGS_A7", offsetof(zx_riscv64_thread_state_general_regs_t, a7))
      .Macro("REGS_S2", offsetof(zx_riscv64_thread_state_general_regs_t, s2))
      .Macro("REGS_S3", offsetof(zx_riscv64_thread_state_general_regs_t, s3))
      .Macro("REGS_S4", offsetof(zx_riscv64_thread_state_general_regs_t, s4))
      .Macro("REGS_S5", offsetof(zx_riscv64_thread_state_general_regs_t, s5))
      .Macro("REGS_S6", offsetof(zx_riscv64_thread_state_general_regs_t, s6))
      .Macro("REGS_S7", offsetof(zx_riscv64_thread_state_general_regs_t, s7))
      .Macro("REGS_S8", offsetof(zx_riscv64_thread_state_general_regs_t, s8))
      .Macro("REGS_S9", offsetof(zx_riscv64_thread_state_general_regs_t, s9))
      .Macro("REGS_S10", offsetof(zx_riscv64_thread_state_general_regs_t, s10))
      .Macro("REGS_S11", offsetof(zx_riscv64_thread_state_general_regs_t, s11))
      .Macro("REGS_T3", offsetof(zx_riscv64_thread_state_general_regs_t, t3))
      .Macro("REGS_T4", offsetof(zx_riscv64_thread_state_general_regs_t, t4))
      .Macro("REGS_T5", offsetof(zx_riscv64_thread_state_general_regs_t, t5))
      .Macro("REGS_T6", offsetof(zx_riscv64_thread_state_general_regs_t, t6))
      .Macro("REGS_F0", offsetof(zx_riscv64_thread_state_fp_regs_t, q))
      .Macro("REGS_F(n)", "(REGS_F0 + ((n) * 16))")
      .Macro("RISCV64_VECTOR_STATE_V", offsetof(zx_riscv64_thread_state_vector_regs_t, v))
      .Macro("RISCV64_VECTOR_STATE_VCSR", offsetof(zx_riscv64_thread_state_vector_regs_t, vcsr))
      .Macro("RISCV64_VECTOR_STATE_VL", offsetof(zx_riscv64_thread_state_vector_regs_t, vl))
      .Macro("RISCV64_VECTOR_STATE_VSTART", offsetof(zx_riscv64_thread_state_vector_regs_t, vstart))
      .Macro("RISCV64_VECTOR_STATE_VTYPE", offsetof(zx_riscv64_thread_state_vector_regs_t, vtype))
      .Line({"#elif defined(__x86_64__)"})
      .Macro("REGS_RAX", offsetof(zx_x86_64_thread_state_general_regs_t, rax))
      .Macro("REGS_RBX", offsetof(zx_x86_64_thread_state_general_regs_t, rbx))
      .Macro("REGS_RCX", offsetof(zx_x86_64_thread_state_general_regs_t, rcx))
      .Macro("REGS_RDX", offsetof(zx_x86_64_thread_state_general_regs_t, rdx))
      .Macro("REGS_RSI", offsetof(zx_x86_64_thread_state_general_regs_t, rsi))
      .Macro("REGS_RDI", offsetof(zx_x86_64_thread_state_general_regs_t, rdi))
      .Macro("REGS_RBP", offsetof(zx_x86_64_thread_state_general_regs_t, rbp))
      .Macro("REGS_RSP", offsetof(zx_x86_64_thread_state_general_regs_t, rsp))
      .Macro("REGS_R8", offsetof(zx_x86_64_thread_state_general_regs_t, r8))
      .Macro("REGS_R9", offsetof(zx_x86_64_thread_state_general_regs_t, r9))
      .Macro("REGS_R10", offsetof(zx_x86_64_thread_state_general_regs_t, r10))
      .Macro("REGS_R11", offsetof(zx_x86_64_thread_state_general_regs_t, r11))
      .Macro("REGS_R12", offsetof(zx_x86_64_thread_state_general_regs_t, r12))
      .Macro("REGS_R13", offsetof(zx_x86_64_thread_state_general_regs_t, r13))
      .Macro("REGS_R14", offsetof(zx_x86_64_thread_state_general_regs_t, r14))
      .Macro("REGS_R15", offsetof(zx_x86_64_thread_state_general_regs_t, r15))
      .Macro("REGS_RIP", offsetof(zx_x86_64_thread_state_general_regs_t, rip))
      .Macro("REGS_RFLAGS", offsetof(zx_x86_64_thread_state_general_regs_t, rflags))
      .Macro("REGS_FS_BASE", offsetof(zx_x86_64_thread_state_general_regs_t, fs_base))
      .Macro("REGS_GS_BASE", offsetof(zx_x86_64_thread_state_general_regs_t, gs_base))
      .Macro("REGS_FCW", offsetof(zx_x86_64_thread_state_fp_regs_t, fcw))
      .Macro("REGS_FSW", offsetof(zx_x86_64_thread_state_fp_regs_t, fsw))
      .Macro("REGS_FTW", offsetof(zx_x86_64_thread_state_fp_regs_t, ftw))
      .Macro("REGS_FOP", offsetof(zx_x86_64_thread_state_fp_regs_t, fop))
      .Macro("REGS_FIP", offsetof(zx_x86_64_thread_state_fp_regs_t, fip))
      .Macro("REGS_FDP", offsetof(zx_x86_64_thread_state_fp_regs_t, fdp))
      .Macro("REGS_ST0", offsetof(zx_x86_64_thread_state_fp_regs_t, st))
      .Macro("REGS_ST(n)", "(REGS_ST0 + ((n) * 16))")
      .Macro("REGS_ZMM0", offsetof(zx_x86_64_thread_state_vector_regs_t, zmm))
      .Macro("REGS_ZMM(n)", "(REGS_ZMM0 + ((n) * 64))")
      .Macro("REGS_MXCSR", offsetof(zx_x86_64_thread_state_vector_regs_t, mxcsr))
      .Macro("REGS_DR0", offsetof(zx_x86_64_thread_state_debug_regs_t, dr))
      .Macro("REGS_DR(n)", "(REGS_DR0 + ((n) * 8))")
      .Macro("REGS_DR6", offsetof(zx_x86_64_thread_state_debug_regs_t, dr6))
      .Macro("REGS_DR7", offsetof(zx_x86_64_thread_state_debug_regs_t, dr7))
      .Line({"#endif"})
      .Main(argc, argv);
}
