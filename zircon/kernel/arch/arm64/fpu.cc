// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2015 Google Inc. All rights reserved
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <bits.h>
#include <trace.h>

#include <arch/arm64.h>
#include <arch/arm64/feature.h>
#include <kernel/thread.h>

#define LOCAL_TRACE 0

[[gnu::target("+fp")]]
void arm64_fpu_restore_state(const Thread* t) {
  const struct fpstate* fpstate = &t->arch().fpstate;

  LTRACEF("cpu %u, thread %s, load fpstate %p\n", arch_curr_cpu_num(), t->name(), fpstate);

  static_assert(sizeof(fpstate->regs) == static_cast<size_t>(16 * 32));
  __asm__ volatile(
      "ldp     q0, q1, [%[regs], #(0 * 32)]\n"
      "ldp     q2, q3, [%[regs], #(1 * 32)]\n"
      "ldp     q4, q5, [%[regs], #(2 * 32)]\n"
      "ldp     q6, q7, [%[regs], #(3 * 32)]\n"
      "ldp     q8, q9, [%[regs], #(4 * 32)]\n"
      "ldp     q10, q11, [%[regs], #(5 * 32)]\n"
      "ldp     q12, q13, [%[regs], #(6 * 32)]\n"
      "ldp     q14, q15, [%[regs], #(7 * 32)]\n"
      "ldp     q16, q17, [%[regs], #(8 * 32)]\n"
      "ldp     q18, q19, [%[regs], #(9 * 32)]\n"
      "ldp     q20, q21, [%[regs], #(10 * 32)]\n"
      "ldp     q22, q23, [%[regs], #(11 * 32)]\n"
      "ldp     q24, q25, [%[regs], #(12 * 32)]\n"
      "ldp     q26, q27, [%[regs], #(13 * 32)]\n"
      "ldp     q28, q29, [%[regs], #(14 * 32)]\n"
      "ldp     q30, q31, [%[regs], #(15 * 32)]\n"
      :
      : [regs] "r"(fpstate->regs), "m"(fpstate->regs));

  if (arm64_feature_test(ZX_ARM64_FEATURE_ISA_SVE)) {
    static_assert(sizeof(fpstate->p_regs) == static_cast<size_t>(16 * 2));
    __asm__ volatile(
        ".arch_extension sve\n"
        "ldr     p0, [%[ffr]]\n"
        "wrffr   p0.b\n"
        "ldr     p0, [%[p_regs], #0, mul vl]\n"
        "ldr     p1, [%[p_regs], #1, mul vl]\n"
        "ldr     p2, [%[p_regs], #2, mul vl]\n"
        "ldr     p3, [%[p_regs], #3, mul vl]\n"
        "ldr     p4, [%[p_regs], #4, mul vl]\n"
        "ldr     p5, [%[p_regs], #5, mul vl]\n"
        "ldr     p6, [%[p_regs], #6, mul vl]\n"
        "ldr     p7, [%[p_regs], #7, mul vl]\n"
        "ldr     p8, [%[p_regs], #8, mul vl]\n"
        "ldr     p9, [%[p_regs], #9, mul vl]\n"
        "ldr     p10, [%[p_regs], #10, mul vl]\n"
        "ldr     p11, [%[p_regs], #11, mul vl]\n"
        "ldr     p12, [%[p_regs], #12, mul vl]\n"
        "ldr     p13, [%[p_regs], #13, mul vl]\n"
        "ldr     p14, [%[p_regs], #14, mul vl]\n"
        "ldr     p15, [%[p_regs], #15, mul vl]\n"
        ".arch_extension nosve\n"
        :
        : [p_regs] "r"(fpstate->p_regs), "m"(fpstate->p_regs), [ffr] "r"(&fpstate->ffr),
          "m"(fpstate->ffr));
  }

  __arm_wsr64("fpcr", fpstate->fpcr);
  __arm_wsr64("fpsr", fpstate->fpsr);
}

[[gnu::target("+fp")]]
void arm64_fpu_save_state(Thread* t) {
  struct fpstate* fpstate = &t->arch().fpstate;

  LTRACEF("cpu %u, thread %s, save fpstate %p\n", arch_curr_cpu_num(), t->name(), fpstate);

  static_assert(sizeof(fpstate->regs) == static_cast<size_t>(16 * 32));
  __asm__ volatile(
      "stp     q0, q1, [%[regs], #(0 * 32)]\n"
      "stp     q2, q3, [%[regs], #(1 * 32)]\n"
      "stp     q4, q5, [%[regs], #(2 * 32)]\n"
      "stp     q6, q7, [%[regs], #(3 * 32)]\n"
      "stp     q8, q9, [%[regs], #(4 * 32)]\n"
      "stp     q10, q11, [%[regs], #(5 * 32)]\n"
      "stp     q12, q13, [%[regs], #(6 * 32)]\n"
      "stp     q14, q15, [%[regs], #(7 * 32)]\n"
      "stp     q16, q17, [%[regs], #(8 * 32)]\n"
      "stp     q18, q19, [%[regs], #(9 * 32)]\n"
      "stp     q20, q21, [%[regs], #(10 * 32)]\n"
      "stp     q22, q23, [%[regs], #(11 * 32)]\n"
      "stp     q24, q25, [%[regs], #(12 * 32)]\n"
      "stp     q26, q27, [%[regs], #(13 * 32)]\n"
      "stp     q28, q29, [%[regs], #(14 * 32)]\n"
      "stp     q30, q31, [%[regs], #(15 * 32)]\n"
      : "=m"(fpstate->regs)
      : [regs] "r"(fpstate->regs));

  if (arm64_feature_test(ZX_ARM64_FEATURE_ISA_SVE)) {
    static_assert(sizeof(fpstate->p_regs) == static_cast<size_t>(16 * 2));
    __asm__ volatile(
        ".arch_extension sve\n"
        "str     p0, [%[p_regs], #0, mul vl]\n"
        "str     p1, [%[p_regs], #1, mul vl]\n"
        "str     p2, [%[p_regs], #2, mul vl]\n"
        "str     p3, [%[p_regs], #3, mul vl]\n"
        "str     p4, [%[p_regs], #4, mul vl]\n"
        "str     p5, [%[p_regs], #5, mul vl]\n"
        "str     p6, [%[p_regs], #6, mul vl]\n"
        "str     p7, [%[p_regs], #7, mul vl]\n"
        "str     p8, [%[p_regs], #8, mul vl]\n"
        "str     p9, [%[p_regs], #9, mul vl]\n"
        "str     p10, [%[p_regs], #10, mul vl]\n"
        "str     p11, [%[p_regs], #11, mul vl]\n"
        "str     p12, [%[p_regs], #12, mul vl]\n"
        "str     p13, [%[p_regs], #13, mul vl]\n"
        "str     p14, [%[p_regs], #14, mul vl]\n"
        "str     p15, [%[p_regs], #15, mul vl]\n"
        "rdffr   p0.b\n"
        "str     p0, [%[ffr]]\n"
        "ldr     p0, [%[p_regs], #0, mul vl]\n"
        ".arch_extension nosve\n"
        : "=m"(fpstate->p_regs), "=m"(fpstate->ffr)
        : [p_regs] "r"(fpstate->p_regs), [ffr] "r"(&fpstate->ffr));
  }

  fpstate->fpcr = static_cast<uint32_t>(__arm_rsr64("fpcr"));
  fpstate->fpsr = static_cast<uint32_t>(__arm_rsr64("fpsr"));

  LTRACEF("thread %s, fpcr %x, fpsr %x\n", t->name(), fpstate->fpcr, fpstate->fpsr);
}

void arm64_fpu_context_switch(Thread* oldthread, Thread* newthread) {
  // The kernel itself does not use the FPU outside of managing state for user space threads. Thus
  // the only threads that can have relevant FPU state to save or restore are those that have
  // user space threads. This code only saves the FPU state when switching away from a user space
  // thread and only loads the FPU register state when switching to a user space thread.
  // Note: When running a kernel-only thread, FPU state from the most recent
  // user space thread may be resident in the hardware registers.
  if (oldthread->user_thread()) {
    arm64_fpu_save_state(oldthread);
  }
  if (newthread->user_thread()) {
    arm64_fpu_restore_state(newthread);
  }
}
