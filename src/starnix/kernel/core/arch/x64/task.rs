// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::signals::SignalInfo;
use crate::task::{CurrentTask, ExceptionResult, PageFaultExceptionReport};
use starnix_sync::{Locked, Unlocked};
use starnix_uapi::signals::{SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGTRAP};

pub fn handle_hardware_exception(
    locked: &mut Locked<Unlocked>,
    current_task: &CurrentTask,
    report: &zx::ExceptionReport,
) -> Option<ExceptionResult> {
    match report.ty {
        // See Intel® 64 and IA-32 Architectures Software Developer's Manual, Volume 3, Chapter 6
        // (Interrupt and exception handling).
        zx::ExceptionType::General => match report.arch.vector {
            // 0: Division by 0.
            // 16: FPU exception.
            // 19: SSE exception.
            0 | 16 | 19 => Some(ExceptionResult::Signal(SignalInfo::kernel(SIGFPE))),

            // 13: General Protection Fault, e.g. `hlt` instruction.
            13 => Some(ExceptionResult::Signal(SignalInfo::kernel(SIGSEGV))),

            _ => None,
        },
        zx::ExceptionType::FatalPageFault { status } => {
            let decoded = decode_page_fault_exception_report(&report.arch);
            Some(current_task.handle_page_fault(locked, decoded, status))
        }
        zx::ExceptionType::UndefinedInstruction => {
            Some(ExceptionResult::Signal(SignalInfo::kernel(SIGILL)))
        }
        zx::ExceptionType::UnalignedAccess => {
            Some(ExceptionResult::Signal(SignalInfo::kernel(SIGBUS)))
        }
        zx::ExceptionType::SoftwareBreakpoint | zx::ExceptionType::HardwareBreakpoint => {
            Some(ExceptionResult::Signal(SignalInfo::kernel(SIGTRAP)))
        }
        _ => None,
    }
}

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
