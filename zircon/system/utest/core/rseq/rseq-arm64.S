// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/arch/asm.h>

.text

// int SpinUntilMigratedOrAborted(zx_rseq_t* rseq);
//
// x0 = rseq

.function SpinUntilMigratedOrAborted, global
    // Set up the sequence.
    //
    // Store the start_ip.
    adr x2, .Lstart
    str x2, [x0, #8]
    // Compute and store the post_commit_offset.
    mov x2, #(.Lend - .Lstart)
    str x2, [x0, #16]
    // Store the abort_ip.
    adr x2, .Labort
    str x2, [x0, #24]

.Lstart:
    // Load the cpu_id from within the sequence.
    ldr w1, [x0]

.Lloop:
    // Check if we've been migrated without being aborted.
    ldr w2, [x0]
    cmp w1, w2
    b.ne .Lmigration
    yield
    b .Lloop
.Lend:
    brk #0

.Lmigration:
    // We observed a change in the cpu_id.  Return 0.
    mov x2, #0
    str x2, [x0, #8]
    mov x0, #0
    ret

.Labort:
    // We were aborted.  Return 1.
    mov x2, #0
    str x2, [x0, #8]
    mov x0, #1
    ret
.end_function
