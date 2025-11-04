// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::PageFaultExceptionReport;
use starnix_uapi::signals::{SIGFPE, SIGSEGV, Signal};

pub fn decode_page_fault_exception_report(
    data: &zx::ExceptionArchData,
) -> PageFaultExceptionReport {
    // [intel/vol3]: 6.15: Interrupt 14--Page-Fault Exception (#PF)
    let faulting_address = data.cr2;
    let not_present = data.err_code & 0x01 == 0; // Low bit means "present"
    let is_write = data.err_code & 0x02 != 0;
    let is_execute = data.err_code & 0xF0 != 0;

    PageFaultExceptionReport { faulting_address, not_present, is_write, is_execute }
}

pub fn get_signal_for_general_exception(data: &zx::ExceptionArchData) -> Option<Signal> {
    // See Intel® 64 and IA-32 Architectures Software Developer's Manual, Volume 3, Chapter 6
    // (Interrupt and exception handling).
    match data.vector {
        // 0: Division by 0.
        // 16: FPU exception.
        // 19: SSE exception.
        0 | 16 | 19 => Some(SIGFPE),

        // General Protection Fault, e.g. `hlt` instruction.
        13 => Some(SIGSEGV),

        _ => None,
    }
}
