// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::visitor::{ProgramCounter, Register};
use linux_uapi::{E2BIG, EACCES, EINVAL, bpf_map_type};

/// An error reported by the eBPF verifier, bundling the error kind with the
/// instruction PC where it occurred (or `None` for program-wide errors).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct VerifierError {
    pub kind: VerifierErrorKind,
    pub pc: Option<ProgramCounter>,
}

impl VerifierError {
    /// The Linux errno corresponding to this error.
    pub fn errno(&self) -> u32 {
        self.kind.errno()
    }
}

impl std::fmt::Display for VerifierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.pc {
            Some(pc) => write!(f, "at PC {pc}: {}", self.kind),
            None => self.kind.fmt(f),
        }
    }
}

impl std::error::Error for VerifierError {}

/// Specific error kinds reported by the eBPF verifier.
///
/// Each variant maps to an errno that matches the one returned by the Linux
/// verifier for the equivalent failure, see [`VerifierErrorKind::errno`].
#[derive(thiserror::Error, Debug, PartialEq, Eq, Clone, Copy)]
pub enum VerifierErrorKind {
    // Program-level limits.
    #[error("ebpf program too long")]
    ProgramTooLarge,

    #[error("verification complexity limit exceeded")]
    ComplexityLimitExceeded,

    // Malformed instructions.
    #[error("invalid op code {0:x}")]
    InvalidOpcode(u8),

    #[error("invalid byte swap width: {0}")]
    InvalidByteSwapWidth(i32),

    #[error("invalid shift value: {0}")]
    InvalidShift(i32),

    #[error("division by zero")]
    DivByZero,

    #[error("invalid atomic operation {0:x}")]
    InvalidAtomicOp(u8),

    #[error("unsupported call type {0}")]
    UnsupportedCallType(u8),

    #[error("lddw is missing its second instruction")]
    IncompleteLddw,

    #[error("second instruction of lddw has non-zero fields")]
    MalformedLddw,

    #[error("unsupported lddw source {0}")]
    UnsupportedLddwSource(u8),

    #[error("R{0} is invalid")]
    InvalidRegister(Register),

    // Control flow.
    #[error("jump outside of program")]
    JumpOutOfBounds,

    #[error("execution falls off the end of the program")]
    FellOffProgramEnd,

    #[error("unknown helper {0}")]
    UnknownHelper(u32),

    // Map references in lddw.
    #[error("lddw with invalid map index: {0}")]
    InvalidMapIndex(u32),

    #[error("lddw map value requires an array map, map at index {0} has type {1}")]
    LddwMapValueNotArray(u32, bpf_map_type),

    #[error("array has no entry")]
    EmptyArrayMap,

    // Helper arguments.
    #[error("buffer size is not a constant")]
    NonConstantBufferSize,

    #[error("helper expects program argument {0}")]
    ExpectedProgramArgument(u8),

    #[error("incorrect type for helper argument {0}")]
    HelperArgumentTypeMismatch(usize),

    #[error("helper argument {0} is not a map")]
    HelperArgumentNotMap(u8),

    #[error("map type {0} not allowed in helper")]
    InvalidMapTypeForHelper(bpf_map_type),

    // Resources.
    #[error("access to released resource")]
    UseAfterRelease,

    #[error("resource already released")]
    DoubleRelease,

    #[error("some resources have not been released at exit time")]
    ResourceLeak,

    // Memory access.
    #[error("write into map forbidden")]
    WriteToReadOnlyMap,

    #[error("write into read-only memory forbidden")]
    WriteToReadOnlyMemory,

    #[error("out of bounds memory access")]
    OutOfBoundsMemoryAccess,

    #[error("pointer offset overflow")]
    PointerOffsetOverflow,

    #[error("pointer arithmetic with uninitialized scalar")]
    UninitializedPointerArithmetic,

    #[error("store through a non-pointer value")]
    StoreToNonPointer,

    #[error("load through a non-pointer value")]
    LoadFromNonPointer,

    #[error("cannot store a pointer or an uninitialized value to memory")]
    InvalidStoredValue,

    // Struct access.
    #[error("no struct field at this offset")]
    InvalidStructFieldAccess,

    #[error("store to a read-only field")]
    WriteToReadOnlyField,

    #[error("inconsistent struct field access")]
    InconsistentStructFieldAccess,

    // Stack access.
    #[error("variable stack offset")]
    VariableStackOffset,

    #[error("stack access out of bounds")]
    StackAccessOutOfBounds,

    #[error("misaligned stack access")]
    MisalignedStackAccess,

    #[error("misaligned atomic access")]
    MisalignedAtomicAccess,

    #[error("atomic access is allowed only to the stack and map values")]
    AtomicAccessNotAllowed,

    #[error("reading unwritten value from the stack")]
    UninitializedStackRead,

    #[error("reading non scalar value from the stack")]
    NonScalarStackRead,

    #[error("cannot store part of a non scalar value on the stack")]
    PartialNonScalarStackStore,

    #[error("cannot load part of a non scalar value from the stack")]
    PartialNonScalarStackLoad,

    // Registers.
    #[error("R{0} is read-only")]
    WriteToReadOnlyRegister(Register),

    #[error("non permitted comparison")]
    InvalidComparison,

    #[error("register 0 is incorrect at exit time")]
    InvalidReturnValue,

    #[error("byte swap requires an initialized scalar register")]
    InvalidByteSwapTarget,

    // Packet access.
    #[error("R{0} is not a packet")]
    RegisterNotPacket(Register),

    #[error("access to unwritten offset")]
    UninitializedPacketOffset,
}

impl VerifierErrorKind {
    /// The Linux errno corresponding to this error.
    pub fn errno(&self) -> u32 {
        match self {
            // Program-level limits.
            Self::ProgramTooLarge | Self::ComplexityLimitExceeded => E2BIG,

            // Structural errors: malformed instructions, invalid control flow, and invalid
            // references to maps or helpers.
            Self::InvalidOpcode(_)
            | Self::InvalidByteSwapWidth(_)
            | Self::InvalidShift(_)
            | Self::DivByZero
            | Self::InvalidAtomicOp(_)
            | Self::UnsupportedCallType(_)
            | Self::IncompleteLddw
            | Self::MalformedLddw
            | Self::UnsupportedLddwSource(_)
            | Self::InvalidRegister(_)
            | Self::JumpOutOfBounds
            | Self::FellOffProgramEnd
            | Self::UnknownHelper(_)
            | Self::InvalidMapIndex(_)
            | Self::LddwMapValueNotArray(_, _)
            | Self::EmptyArrayMap
            | Self::InvalidMapTypeForHelper(_)
            | Self::ResourceLeak
            | Self::RegisterNotPacket(_)
            | Self::PointerOffsetOverflow => EINVAL,

            // Type, bounds and permission violations.
            Self::NonConstantBufferSize
            | Self::ExpectedProgramArgument(_)
            | Self::HelperArgumentTypeMismatch(_)
            | Self::HelperArgumentNotMap(_)
            | Self::UseAfterRelease
            | Self::DoubleRelease
            | Self::WriteToReadOnlyMap
            | Self::WriteToReadOnlyMemory
            | Self::OutOfBoundsMemoryAccess
            | Self::UninitializedPointerArithmetic
            | Self::StoreToNonPointer
            | Self::LoadFromNonPointer
            | Self::InvalidStoredValue
            | Self::InvalidStructFieldAccess
            | Self::WriteToReadOnlyField
            | Self::InconsistentStructFieldAccess
            | Self::VariableStackOffset
            | Self::StackAccessOutOfBounds
            | Self::MisalignedStackAccess
            | Self::MisalignedAtomicAccess
            | Self::AtomicAccessNotAllowed
            | Self::UninitializedStackRead
            | Self::NonScalarStackRead
            | Self::PartialNonScalarStackStore
            | Self::PartialNonScalarStackLoad
            | Self::WriteToReadOnlyRegister(_)
            | Self::InvalidComparison
            | Self::InvalidReturnValue
            | Self::InvalidByteSwapTarget
            | Self::UninitializedPacketOffset => EACCES,
        }
    }
}

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum EbpfError {
    #[error("Verification error loading program: {0}")]
    ProgramVerifyError(#[from] VerifierError),

    #[error("Failed to link program: {0}")]
    ProgramLinkError(String),

    #[error("cBPF program too large: {0} instructions")]
    CbpfProgramTooLarge(usize),

    #[error("Invalid cBPF instruction code: 0x{0:x}")]
    InvalidCbpfInstruction(u16),

    #[error("Invalid cBPF scratch memory offset: 0x{0:x}")]
    InvalidCbpfScratchOffset(u32),

    #[error("Invalid cBPF jump offset: 0x{0:x}")]
    InvalidCbpfJumpOffset(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verifier_error_errno() {
        // Program-level limits map to E2BIG.
        assert_eq!(VerifierErrorKind::ProgramTooLarge.errno(), E2BIG);
        assert_eq!(VerifierErrorKind::ComplexityLimitExceeded.errno(), E2BIG);
        assert_eq!(
            VerifierError { kind: VerifierErrorKind::ProgramTooLarge, pc: None }.errno(),
            E2BIG
        );

        // Structural errors map to EINVAL.
        assert_eq!(VerifierErrorKind::InvalidOpcode(0xff).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidByteSwapWidth(8).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidShift(32).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::DivByZero.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidAtomicOp(0xff).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::UnsupportedCallType(1).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::IncompleteLddw.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::MalformedLddw.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::UnsupportedLddwSource(99).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidRegister(11).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::JumpOutOfBounds.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::FellOffProgramEnd.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::UnknownHelper(1).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidMapIndex(0).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::LddwMapValueNotArray(0, 1).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::EmptyArrayMap.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidMapTypeForHelper(1).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::ResourceLeak.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::RegisterNotPacket(6).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::PointerOffsetOverflow.errno(), EINVAL);

        // Type, bounds, and permission violations map to EACCES.
        assert_eq!(VerifierErrorKind::NonConstantBufferSize.errno(), EACCES);
        assert_eq!(VerifierErrorKind::ExpectedProgramArgument(0).errno(), EACCES);
        assert_eq!(VerifierErrorKind::HelperArgumentTypeMismatch(0).errno(), EACCES);
        assert_eq!(VerifierErrorKind::HelperArgumentNotMap(0).errno(), EACCES);
        assert_eq!(VerifierErrorKind::UseAfterRelease.errno(), EACCES);
        assert_eq!(VerifierErrorKind::DoubleRelease.errno(), EACCES);
        assert_eq!(VerifierErrorKind::WriteToReadOnlyMap.errno(), EACCES);
        assert_eq!(VerifierErrorKind::WriteToReadOnlyMemory.errno(), EACCES);
        assert_eq!(VerifierErrorKind::OutOfBoundsMemoryAccess.errno(), EACCES);
        assert_eq!(VerifierErrorKind::UninitializedPointerArithmetic.errno(), EACCES);
        assert_eq!(VerifierErrorKind::StoreToNonPointer.errno(), EACCES);
        assert_eq!(VerifierErrorKind::LoadFromNonPointer.errno(), EACCES);
        assert_eq!(VerifierErrorKind::InvalidStoredValue.errno(), EACCES);
        assert_eq!(VerifierErrorKind::InvalidStructFieldAccess.errno(), EACCES);
        assert_eq!(VerifierErrorKind::WriteToReadOnlyField.errno(), EACCES);
        assert_eq!(VerifierErrorKind::InconsistentStructFieldAccess.errno(), EACCES);
        assert_eq!(VerifierErrorKind::VariableStackOffset.errno(), EACCES);
        assert_eq!(VerifierErrorKind::StackAccessOutOfBounds.errno(), EACCES);
        assert_eq!(VerifierErrorKind::MisalignedStackAccess.errno(), EACCES);
        assert_eq!(VerifierErrorKind::UninitializedStackRead.errno(), EACCES);
        assert_eq!(VerifierErrorKind::NonScalarStackRead.errno(), EACCES);
        assert_eq!(VerifierErrorKind::PartialNonScalarStackStore.errno(), EACCES);
        assert_eq!(VerifierErrorKind::PartialNonScalarStackLoad.errno(), EACCES);
        assert_eq!(VerifierErrorKind::WriteToReadOnlyRegister(10).errno(), EACCES);
        assert_eq!(VerifierErrorKind::InvalidComparison.errno(), EACCES);
        assert_eq!(VerifierErrorKind::InvalidReturnValue.errno(), EACCES);
        assert_eq!(VerifierErrorKind::InvalidByteSwapTarget.errno(), EACCES);
        assert_eq!(VerifierErrorKind::UninitializedPacketOffset.errno(), EACCES);
    }
}
