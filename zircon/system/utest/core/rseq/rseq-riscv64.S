// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/arch/asm.h>

.text

// int SpinUntilMigratedOrAborted(zx_rseq_t* rseq);
//
// a0 = rseq

.function SpinUntilMigratedOrAborted, global
    // Set up the sequence.
    //
    // Store the start_ip.
    la t0, .Lstart
    sd t0, 8(a0)
    // Compute and store the post_commit_offset.
    lla t0, .Lend
    lla t1, .Lstart
    sub t0, t0, t1
    sd t0, 16(a0)
    // Store the abort_ip.
    la t0, .Labort
    sd t0, 24(a0)

.Lstart:
    // Load the cpu_id from within the sequence.
    lw t1, 0(a0)

.Lloop:
    // Check if we've been migrated without being aborted.
    lw t2, 0(a0)
    bne t1, t2, .Lmigration
    pause
    j .Lloop
.Lend:
    ebreak

.Lmigration:
    // We observed a change in the cpu_id.  Return 0.
    sd zero, 8(a0)
    li a0, 0
    ret

.Labort:
    // We were aborted.  Return 1.
    sd zero, 8(a0)
    li a0, 1
    ret
.end_function
