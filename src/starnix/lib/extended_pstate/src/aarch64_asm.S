// Copyright 2026 The Fuchsia Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <starnix_asm_utils/asm.h>

// LINT.IfChange(aarch64_state_offsets)
#define FPCR_OFFSET 512
#define FPSR_OFFSET 516
#define P_OFFSET 520
#define FFR_OFFSET 552
#define HAS_SVE_OFFSET 554
// LINT.ThenChange(aarch64.rs:aarch64_state_offsets)

// The SVE register state is saved and restored assuming a 128-bit vector length (VL = 16 bytes,
// PL = 2 bytes), which is the only vector length Zircon configures (`ZCR_EL1.LEN = 0` in
// zircon/kernel/arch/arm64/arch.cc) and which `HAS_SVE` in aarch64.rs verifies in debug builds.
// At this vector length:
// - Z0-Z31 are exactly the 128-bit Q0-Q31, which are saved and restored by the `stp`/`ldp`
//   sequences below, so no additional Z state is needed.
// - Each `[x4, #n, mul vl]` predicate slot is PL = 2 bytes, so P0-P15 occupy the 32 bytes at
//   P_OFFSET and the FFR slot (`#16, mul vl`) lands exactly at FFR_OFFSET.
// A larger vector length would overrun the `State` struct and drop the upper bits of Z0-Z31.

// LINT.IfChange(aarch32_state_offsets)
#define FPCR_OFFSET_32 256
#define FPSR_OFFSET_32 260
// LINT.ThenChange(aarch64.rs:aarch32_state_offsets)

FUNCTION(save_extended_pstate)
    // state_addr is in x0
    ldr x1, [x0] // state_ptr is in x1
    stp  q0,  q1, [x1, #( 0 * 32)]
    stp  q2,  q3, [x1, #( 1 * 32)]
    stp  q4,  q5, [x1, #( 2 * 32)]
    stp  q6,  q7, [x1, #( 3 * 32)]
    stp  q8,  q9, [x1, #( 4 * 32)]
    stp q10, q11, [x1, #( 5 * 32)]
    stp q12, q13, [x1, #( 6 * 32)]
    stp q14, q15, [x1, #( 7 * 32)]
    stp q16, q17, [x1, #( 8 * 32)]
    stp q18, q19, [x1, #( 9 * 32)]
    stp q20, q21, [x1, #(10 * 32)]
    stp q22, q23, [x1, #(11 * 32)]
    stp q24, q25, [x1, #(12 * 32)]
    stp q26, q27, [x1, #(13 * 32)]
    stp q28, q29, [x1, #(14 * 32)]
    stp q30, q31, [x1, #(15 * 32)]
    mrs x2, fpcr
    mrs x3, fpsr
    str w2, [x1, #(FPCR_OFFSET)]
    str w3, [x1, #(FPSR_OFFSET)]
    ldrb w2, [x1, #(HAS_SVE_OFFSET)]
    cbz w2, .Lskip_sve_save
    // Save P0-P15 and FFR, assuming a 128-bit vector length (see the comment at the top).
    .arch_extension sve
    add x4, x1, #(P_OFFSET)
    str  p0, [x4, #0, mul vl]
    str  p1, [x4, #1, mul vl]
    str  p2, [x4, #2, mul vl]
    str  p3, [x4, #3, mul vl]
    str  p4, [x4, #4, mul vl]
    str  p5, [x4, #5, mul vl]
    str  p6, [x4, #6, mul vl]
    str  p7, [x4, #7, mul vl]
    str  p8, [x4, #8, mul vl]
    str  p9, [x4, #9, mul vl]
    str p10, [x4, #10, mul vl]
    str p11, [x4, #11, mul vl]
    str p12, [x4, #12, mul vl]
    str p13, [x4, #13, mul vl]
    str p14, [x4, #14, mul vl]
    str p15, [x4, #15, mul vl]
    rdffr p0.b
    str  p0, [x4, #16, mul vl]
    .arch_extension nosve
.Lskip_sve_save:
    ret
END_FUNCTION(save_extended_pstate)

FUNCTION(restore_extended_pstate)
    // state_addr is in x0
    ldr x1, [x0] // state_ptr is in x1
    ldp  q0,  q1, [x1, #( 0 * 32)]
    ldp  q2,  q3, [x1, #( 1 * 32)]
    ldp  q4,  q5, [x1, #( 2 * 32)]
    ldp  q6,  q7, [x1, #( 3 * 32)]
    ldp  q8,  q9, [x1, #( 4 * 32)]
    ldp q10, q11, [x1, #( 5 * 32)]
    ldp q12, q13, [x1, #( 6 * 32)]
    ldp q14, q15, [x1, #( 7 * 32)]
    ldp q16, q17, [x1, #( 8 * 32)]
    ldp q18, q19, [x1, #( 9 * 32)]
    ldp q20, q21, [x1, #(10 * 32)]
    ldp q22, q23, [x1, #(11 * 32)]
    ldp q24, q25, [x1, #(12 * 32)]
    ldp q26, q27, [x1, #(13 * 32)]
    ldp q28, q29, [x1, #(14 * 32)]
    ldp q30, q31, [x1, #(15 * 32)]
    ldr w2, [x1, #(FPCR_OFFSET)]
    ldr w3, [x1, #(FPSR_OFFSET)]
    // From the ARM Software Optimization Guide:
    //
    // 4.10 FPCR self-synchronization
    // Programmers and compiler writers should note that writes to the FPCR register are
    // self-synchronizing, i.e. its effect on subsequent instructions can be relied upon without an
    // intervening context synchronizing operation.
    //
    // Writing to fpcr triggers a flush, even if the value hasn't changed, and most programs don't
    // change fpcr. Reading doesn't trigger a flush, so we check first before writing.
    mrs x4, fpcr
    cmp w4, w2
    b.eq .Lskip_fpcr64
    msr fpcr, x2
.Lskip_fpcr64:
    // FPSR holds cumulative exception flags rather than execution control bits and does
    // not trigger an arithmetic pipeline flush.
    msr fpsr, x3
    ldrb w2, [x1, #(HAS_SVE_OFFSET)]
    cbz w2, .Lskip_sve_restore
    // Restore FFR (via p0) and then P0-P15, assuming a 128-bit vector length (see the comment at
    // the top).
    .arch_extension sve
    add x4, x1, #(P_OFFSET)
    ldr  p0, [x4, #16, mul vl]
    wrffr p0.b
    ldr  p0, [x4, #0, mul vl]
    ldr  p1, [x4, #1, mul vl]
    ldr  p2, [x4, #2, mul vl]
    ldr  p3, [x4, #3, mul vl]
    ldr  p4, [x4, #4, mul vl]
    ldr  p5, [x4, #5, mul vl]
    ldr  p6, [x4, #6, mul vl]
    ldr  p7, [x4, #7, mul vl]
    ldr  p8, [x4, #8, mul vl]
    ldr  p9, [x4, #9, mul vl]
    ldr p10, [x4, #10, mul vl]
    ldr p11, [x4, #11, mul vl]
    ldr p12, [x4, #12, mul vl]
    ldr p13, [x4, #13, mul vl]
    ldr p14, [x4, #14, mul vl]
    ldr p15, [x4, #15, mul vl]
    .arch_extension nosve
.Lskip_sve_restore:
    ret
END_FUNCTION(restore_extended_pstate)

FUNCTION(save_extended_aarch32_pstate)
    // state_addr is in x0
    ldr x1, [x0] // state_ptr is in x1
    stp  q0,  q1, [x1, #( 0 * 32)]
    stp  q2,  q3, [x1, #( 1 * 32)]
    stp  q4,  q5, [x1, #( 2 * 32)]
    stp  q6,  q7, [x1, #( 3 * 32)]
    stp  q8,  q9, [x1, #( 4 * 32)]
    stp q10, q11, [x1, #( 5 * 32)]
    stp q12, q13, [x1, #( 6 * 32)]
    stp q14, q15, [x1, #( 7 * 32)]
    mrs x2, fpcr
    mrs x3, fpsr
    str w2, [x1, #(FPCR_OFFSET_32)]
    str w3, [x1, #(FPSR_OFFSET_32)]
    ret
END_FUNCTION(save_extended_aarch32_pstate)

FUNCTION(restore_extended_aarch32_pstate)
    // state_addr is in x0
    ldr x1, [x0] // state_ptr is in x1
    ldp  q0,  q1, [x1, #( 0 * 32)]
    ldp  q2,  q3, [x1, #( 1 * 32)]
    ldp  q4,  q5, [x1, #( 2 * 32)]
    ldp  q6,  q7, [x1, #( 3 * 32)]
    ldp  q8,  q9, [x1, #( 4 * 32)]
    ldp q10, q11, [x1, #( 5 * 32)]
    ldp q12, q13, [x1, #( 6 * 32)]
    ldp q14, q15, [x1, #( 7 * 32)]
    ldr w2, [x1, #(FPCR_OFFSET_32)]
    ldr w3, [x1, #(FPSR_OFFSET_32)]
    // See the comment about fpcr flushing in `restore_extended_pstate`.
    mrs x4, fpcr
    cmp w4, w2
    b.eq .Lskip_fpcr32
    msr fpcr, x2
.Lskip_fpcr32:
    msr fpsr, x3
    ret
END_FUNCTION(restore_extended_aarch32_pstate)
