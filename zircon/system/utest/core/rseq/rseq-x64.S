// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/arch/asm.h>

.text

// int SpinUntilMigratedOrAborted(zx_rseq_t* rseq);
//
// rdi = rseq

.function SpinUntilMigratedOrAborted, global
    // Set up the sequence.
    //
    // Store the start_ip.
    lea .Lstart(%rip), %rax
    mov %rax, 8(%rdi)
    // Compute and store the post_commit_offset.
    movq $(.Lend - .Lstart), 16(%rdi)
    // Store the abort_ip.
    lea .Labort(%rip), %rax
    mov %rax, 24(%rdi)

.Lstart:
    // Load the cpu_id from within the sequence.
    mov 0(%rdi), %esi

.Lloop:
    // Check if we've been migrated without being aborted.
    cmp %esi, 0(%rdi)
    jne .Lmigration
    pause
    jmp .Lloop
.Lend:
    ud2

.Lmigration:
    // We observed a change in the cpu_id.  Return 0.
    movq $0, 8(%rdi)
    xor %rax, %rax
    ret

.Labort:
    // We were aborted.  Return 1.
    movq $0, 8(%rdi)
    mov $1, %rax
    ret
.end_function
