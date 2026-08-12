// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bitrs::{bitfield_repr, layout};
use regio::riscv64::{Csr, encoding};
use zerocopy::{IntoBytes, TryFromBytes};

/// [riscv/priv]: 3.1.6  Supervisor Status Register (sstatus)
pub const SSTATUS: Csr<encoding::sstatus, SupervisorStatus> = Csr::new();

#[bitfield_repr(u8)]
#[derive(Clone, Copy)]
pub enum Xlen {
    Xlen32 = 1,
    Xlen64 = 2,
    Xlen128 = 3,
}

#[bitfield_repr(u8)]
#[derive(Clone, Copy)]
pub enum ExtensionStatus {
    Off = 0,     // Off (FS, VS); All off (XS)
    Initial = 1, // Initial (FS, VS); None dirty or clean, some on (XS)
    Clean = 2,   // Clean (FS, VS); None dirty, some clean (XS)
    Dirty = 3,   // Dirty (FS, VS); Some dirty (XS)
}

layout!({
    /// The layout of [`SSTATUS`].
    pub struct SupervisorStatus(u64);
    {
        let sd @ 63; // State Dirty
        let __ @ 62..34;
        let uxl @ 33..32: Xlen; // UXLEN
        let __ @ 31..20;
        let mxr @ 19; // Make eXecutable Readable
        let sum @ 18; // Supervisor User Memory
        let xs @ 16..15: ExtensionStatus; // other eXtension State
        let fs @ 14..13: ExtensionStatus; // F extension State
        let __ @ 12..11;
        let vs @ 10..9: ExtensionStatus; // V extension State
        let spp @ 8; // Supervisor Previous Privilege
        let __ @ 7;
        let ube @ 6; // User Big-Endian
        let spie @ 5; // Supervisor Previous Interrupt Enable
        let __ @ 4..2;
        let sie @ 1; // Supervisor Interrupt Enable
        let __ @ 0;
    }
});

/// [riscv/priv]: 3.1.7  Supervisor Trap Vector Base Address Register (stvec)
pub const STVEC: Csr<encoding::stvec, SupervisorTrapVector> = Csr::new();

#[bitfield_repr(u8)]
#[derive(Clone, Copy)]
pub enum TrapVectorMode {
    Direct = 0,
    Vectored = 1,
}

layout!({
    /// The layout of [`STVEC`].
    pub struct SupervisorTrapVector(u64);
    {
        #[unshifted]
        let base @ 63..2;
        let mode @ 1..0: TrapVectorMode;
    }
});

/// [riscv/priv]: 3.1.15  Supervisor Cause Register (scause)
pub const SCAUSE: Csr<encoding::scause, SupervisorCause> = Csr::new();

/// exception_code values when interrupt is set.
#[bitfield_repr(u64)]
#[derive(Clone, Copy)]
pub enum InterruptCode {
    SoftwareInterrupt = 1,
    TimerInterrupt = 5,
    ExternalInterrupt = 9,
}

/// exception_code values when interrupt is clear.
#[bitfield_repr(u64)]
#[derive(Clone, Copy)]
pub enum ExceptionCode {
    InstructionAddressMisaligned = 0,
    InstructionAccessFault = 1,
    IllegalInstruction = 2,
    Breakpoint = 3,
    LoadAddressMisaligned = 4,
    LoadAccessFault = 5,
    StoreAddressMisaligned = 6,
    StoreAccessFault = 7,
    EcallUmode = 8,
    EcallSmode = 9,
    InstructionPageFault = 12,
    LoadPageFault = 13,
    StorePageFault = 15,
}

layout!({
    /// The layout of [`SCAUSE`].
    pub struct SupervisorCause(u64);
    {
        let interrupt @ 63;
        let code @ 62..0;
    }
});

impl SupervisorCause {
    #[inline]
    pub fn exception_code(self) -> Option<ExceptionCode> {
        if self.interrupt() {
            return None;
        }
        ExceptionCode::try_read_from_bytes(self.code().as_bytes()).ok()
    }

    #[inline]
    pub fn interrupt_code(self) -> Option<InterruptCode> {
        if !self.interrupt() {
            return None;
        }
        InterruptCode::try_read_from_bytes(self.code().as_bytes()).ok()
    }
}

/// [riscv/priv]: 3.1.16  Supervisor Trap Value Register (stval)
pub const STVAL: Csr<encoding::stval, u64> = Csr::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scause() {
        let mut scause = SupervisorCause::new();
        scause.set_interrupt(false).set_code(ExceptionCode::LoadPageFault as u64);
        assert_eq!(scause.exception_code(), Some(ExceptionCode::LoadPageFault));
        assert_eq!(scause.interrupt_code(), None);

        scause.set_interrupt(true).set_code(InterruptCode::TimerInterrupt as u64);
        assert_eq!(scause.exception_code(), None);
        assert_eq!(scause.interrupt_code(), Some(InterruptCode::TimerInterrupt));
    }
}
