// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::signals::{SignalDetail, SignalInfo, SignalState};
use crate::task::{ArchExtendedPstateStorage, CurrentTask, Task};
use starnix_logging::{log_debug, track_stub};
use starnix_registers::{RegisterState, RegisterStorageEnum};
use starnix_types::arch::ArchWidth;
use starnix_uapi::errors::Errno;
use starnix_uapi::math::round_down_to_increment;
use starnix_uapi::signals::SigSet;
use starnix_uapi::user_address::{ArchSpecific, UserAddress};
use starnix_uapi::{
    _aarch64_ctx, ESR_MAGIC, EXTRA_MAGIC, FPSIMD_MAGIC, SVE_MAGIC, SVE_SIG_FLAG_SM, SVE_VL_MIN,
    errno, error, esr_context, fpsimd_context, sigaction_t, sigaltstack, sve_context, uapi,
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, transmute_ref};

/// The size of the red zone.
pub const RED_ZONE_SIZE: u64 = 0;

const fn max_const(i1: usize, i2: usize) -> usize {
    if i2 > i1 { i2 } else { i1 }
}

/// Maximum size between the siginfo struct for 32 and 64 bits.
const SIGINFO_RESERVED_DATA_SIZE: usize = max_const(
    std::mem::size_of::<uapi::siginfo_t>(),
    std::mem::size_of::<uapi::arch32::siginfo_t>(),
);

/// Maximum size between the context struct for 32 and 64 bits.
const UCONTEXT_RESERVED_DATA_SIZE: usize =
    max_const(std::mem::size_of::<uapi::ucontext>(), std::mem::size_of::<uapi::arch32::ucontext>());

/// A `SignalStackFrame` contains all the state that is stored on the stack prior to executing a
/// signal handler. The exact layout of this structure is part of the platform's ABI.
#[repr(C, align(16))]
#[derive(Clone, IntoBytes, FromBytes, KnownLayout, Immutable)]
pub struct SignalStackFrame {
    /// Bytes for the siginfo struct relevant to the correct architecture padded with 0.
    pub siginfo_bytes: [u8; SIGINFO_RESERVED_DATA_SIZE],
    /// Bytes for the context struct relevant to the correct architecture padded with 0.
    pub context: [u8; UCONTEXT_RESERVED_DATA_SIZE],
}

/// The size, in bytes, of the signal stack frame.
pub const SIG_STACK_SIZE: usize = std::mem::size_of::<SignalStackFrame>();

impl SignalStackFrame {
    pub fn new(
        task: &Task,
        arch_width: ArchWidth,
        registers: &mut RegisterState<RegisterStorageEnum>,
        extended_pstate: &ArchExtendedPstateStorage,
        signal_state: &SignalState,
        siginfo: &SignalInfo,
        _action: sigaction_t,
        _stack_pointer: UserAddress,
    ) -> Result<SignalStackFrame, Errno> {
        let fault_address = match siginfo.detail {
            SignalDetail::SigFault { addr } => addr,
            _ => 0,
        };
        let uc_stack = signal_state
            .alt_stack
            .map(|stack| sigaltstack {
                ss_sp: stack.ss_sp.into(),
                ss_flags: stack.ss_flags as i32,
                ss_size: stack.ss_size as u64,
                ..Default::default()
            })
            .unwrap_or_default();
        let mut context = [0_u8; UCONTEXT_RESERVED_DATA_SIZE];
        if arch_width.is_arch32() {
            let mut regs = [0_u32; 21];
            // 0, 1, 2 is trap_no, error_code, oldmask, keep at 0.
            for i in 0..16 {
                regs[i + 3] = registers.r[i] as u32;
            }
            regs[19] = registers.cpsr as u32;
            regs[20] = fault_address as u32;
            let context32 = uapi::arch32::ucontext {
                uc_flags: 0,
                uc_link: Default::default(),
                uc_stack: uc_stack.try_into().map_err(|_| errno!(EINVAL))?,
                uc_sigmask64: uapi::sigset_t::from(signal_state.mask()).into(),
                uc_mcontext: zerocopy::transmute!(regs),
                extended_pstate: get_pstate_extended_data(extended_pstate)?, // TODO(https://fxbug.dev/486225910): This isn't right for aarch32.
                ..Default::default()
            };
            context32.write_to_prefix(&mut context).unwrap();
        } else {
            let regs = registers.r.to_vec();
            let context64 = uapi::ucontext {
                uc_flags: 0,
                uc_link: Default::default(),
                uc_stack,
                uc_sigmask: signal_state.mask().into(),
                uc_mcontext: uapi::sigcontext {
                    regs: regs.try_into().unwrap(),
                    sp: registers.sp,
                    pc: registers.pc,
                    pstate: registers.cpsr as u64,
                    fault_address,
                    __reserved: get_pstate_extended_data(extended_pstate)?,
                    ..Default::default()
                },
                ..Default::default()
            };
            context64.write_to_prefix(&mut context).unwrap();
        }

        let vdso_sigreturn_offset = if arch_width.is_arch32() {
            task.kernel().vdso_arch32.as_ref().unwrap().sigreturn_offset
        } else {
            task.kernel().vdso.sigreturn_offset
        };
        let sigreturn_addr = task.mm()?.state.read().vdso_base.ptr() as u64 + vdso_sigreturn_offset;
        registers.r[30] = sigreturn_addr;
        if arch_width.is_arch32() {
            registers.r[14] = registers.r[30];
        }

        Ok(SignalStackFrame { context, siginfo_bytes: siginfo.as_siginfo_bytes(arch_width)? })
    }

    pub fn as_bytes(&self) -> &[u8; SIG_STACK_SIZE] {
        zerocopy::transmute_ref!(self)
    }

    pub fn from_bytes(bytes: [u8; SIG_STACK_SIZE]) -> SignalStackFrame {
        zerocopy::transmute!(bytes)
    }

    pub fn get_signal_mask(&self, is_arch32: bool) -> SigSet {
        if is_arch32 {
            uapi::sigset_t::from(self.get_ucontext32().uc_sigmask64).into()
        } else {
            self.get_ucontext64().uc_sigmask.into()
        }
    }

    fn get_ucontext64(&self) -> &uapi::ucontext {
        uapi::ucontext::ref_from_prefix(&self.context).unwrap().0
    }

    fn get_ucontext32(&self) -> &uapi::arch32::ucontext {
        uapi::arch32::ucontext::ref_from_prefix(&self.context).unwrap().0
    }
}

pub fn restore_registers(
    current_task: &mut CurrentTask,
    signal_stack_frame: &SignalStackFrame,
    _stack_pointer: UserAddress,
) -> Result<(), Errno> {
    if current_task.is_arch32() {
        restore_registers_32(current_task, signal_stack_frame)
    } else {
        restore_registers_64(current_task, signal_stack_frame)
    }
}

fn restore_registers_32(
    current_task: &mut CurrentTask,
    signal_stack_frame: &SignalStackFrame,
) -> Result<(), Errno> {
    let uctx = &signal_stack_frame.get_ucontext32().uc_mcontext;
    let regs: &[u32; 21] = transmute_ref!(uctx);
    const NUM_REGS: usize = 31;
    let mut registers = [0u64; NUM_REGS];
    for i in 0..16 {
        registers[i] = regs[i + 3].into();
    }

    let mut restored_regs = zx::sys::zx_restricted_state_t::default();
    restored_regs.r = registers;
    restored_regs.sp = registers[13];
    restored_regs.pc = registers[15];
    restored_regs.cpsr = regs[19] | (zx::sys::ZX_REG_CPSR_ARCH_32_MASK as u32);
    restored_regs.tpidr_el0 = current_task.thread_state.registers.tpidr_el0;
    current_task.thread_state.registers.load(restored_regs);

    parse_pstate_extended_data(
        &signal_stack_frame.get_ucontext32().extended_pstate,
        &mut current_task.thread_state.extended_pstate,
    )
}

fn restore_registers_64(
    current_task: &mut CurrentTask,
    signal_stack_frame: &SignalStackFrame,
) -> Result<(), Errno> {
    let uctx = &signal_stack_frame.get_ucontext64().uc_mcontext;
    const NUM_REGS: usize = 31;
    let mut registers = [0; NUM_REGS];
    registers.copy_from_slice(&uctx.regs[..NUM_REGS]);

    // Restore the register state from before executing the signal handler.
    let mut restored_regs = zx::sys::zx_restricted_state_t::default();
    restored_regs.r = registers;
    restored_regs.sp = uctx.sp;
    restored_regs.pc = uctx.pc;
    restored_regs.cpsr = (uctx.pstate as u32) & !(zx::sys::ZX_REG_CPSR_ARCH_32_MASK as u32);
    restored_regs.tpidr_el0 = current_task.thread_state.registers.tpidr_el0;
    current_task.thread_state.registers.load(restored_regs);

    parse_pstate_extended_data(&uctx.__reserved, &mut current_task.thread_state.extended_pstate)
}

pub fn align_stack_pointer(pointer: u64) -> u64 {
    // When the stack grows down, alignment must be to the next lowest value.
    // arm64/starnix does not support upward growth.
    round_down_to_increment(pointer, 16).expect("Failed to round up stack pointer")
}

// Size of `sigcontext::__reserved`.
const SIGCONTEXT_EXTENDED_PSTATE_DATA_SIZE: usize = 4096;

// Linux arm64 SVE signal frame constants for 128-bit vector length (vq = 1, vl = 16 bytes),
// per `arch/arm64/include/uapi/asm/sigcontext.h`:
// - Header (`sve_context`): 16 bytes
// - Z0-Z31: 32 * 16 = 512 bytes at offset 16
// - P0-P15: 16 * 2 = 32 bytes at offset 528
// - FFR: 2 bytes at offset 560
// - Total unaligned size (`SVE_SIG_CONTEXT_SIZE(1)`): 562 bytes
// - 16-byte aligned section size in `head.size`: 576 bytes
const SVE_SIG_REGS_OFFSET: usize = std::mem::size_of::<sve_context>();
const SVE_SIG_ZREGS_OFFSET: usize = SVE_SIG_REGS_OFFSET;
const SVE_SIG_ZREGS_SIZE: usize = 32 * 16;
const SVE_SIG_PREGS_OFFSET: usize = SVE_SIG_ZREGS_OFFSET + SVE_SIG_ZREGS_SIZE;
const SVE_SIG_PREGS_SIZE: usize = 16 * 2;
const SVE_SIG_FFR_OFFSET: usize = SVE_SIG_PREGS_OFFSET + SVE_SIG_PREGS_SIZE;
const SVE_SIG_FFR_SIZE: usize = 2;
const SVE_SIG_CONTEXT_SIZE: usize = SVE_SIG_FFR_OFFSET + SVE_SIG_FFR_SIZE;
const SVE_SIG_CONTEXT_ALIGNED_SIZE: usize = (SVE_SIG_CONTEXT_SIZE + 15) & !15;

// Returns the array to be saved in `sigcontext.__reserved`. It contains a sequence of sections
// each identified with a `_aarch64_ctx` header. The end is indicated with both fields in the
// header set to 0.
fn get_pstate_extended_data(
    extended_pstate: &ArchExtendedPstateStorage,
) -> Result<[u8; SIGCONTEXT_EXTENDED_PSTATE_DATA_SIZE], Errno> {
    match extended_pstate {
        ArchExtendedPstateStorage::State64(extended_pstate) => {
            let mut result = [0u8; SIGCONTEXT_EXTENDED_PSTATE_DATA_SIZE];

            let fpsimd = fpsimd_context {
                head: _aarch64_ctx {
                    magic: FPSIMD_MAGIC,
                    size: std::mem::size_of::<fpsimd_context>() as u32,
                },
                fpsr: extended_pstate.get_arm64_fpsr(),
                fpcr: extended_pstate.get_arm64_fpcr(),
                vregs: *extended_pstate.get_arm64_qregs(),
            };
            fpsimd.write_to_prefix(&mut result).map_err(|_| errno!(EINVAL))?;

            if extended_pstate.has_arm64_sve() {
                let offset = std::mem::size_of::<fpsimd_context>();
                let sve = sve_context {
                    head: _aarch64_ctx {
                        magic: SVE_MAGIC,
                        size: SVE_SIG_CONTEXT_ALIGNED_SIZE as u32,
                    },
                    vl: SVE_VL_MIN as u16,
                    flags: 0,
                    __reserved: [0; 2],
                };
                let sve_slice = result
                    .get_mut(offset..offset + SVE_SIG_CONTEXT_ALIGNED_SIZE)
                    .ok_or_else(|| errno!(EINVAL))?;
                sve.write_to_prefix(sve_slice).map_err(|_| errno!(EINVAL))?;
                extended_pstate
                    .get_arm64_qregs()
                    .write_to_prefix(&mut sve_slice[SVE_SIG_ZREGS_OFFSET..])
                    .map_err(|_| errno!(EINVAL))?;
                extended_pstate
                    .get_arm64_p_regs()
                    .write_to_prefix(&mut sve_slice[SVE_SIG_PREGS_OFFSET..])
                    .map_err(|_| errno!(EINVAL))?;
                extended_pstate
                    .get_arm64_ffr()
                    .write_to_prefix(&mut sve_slice[SVE_SIG_FFR_OFFSET..])
                    .map_err(|_| errno!(EINVAL))?;
            }

            // TODO(b/313465152): Save ESR with `esr_context` and `ESR_MAGIC`. The register is read-only,
            // but the signal handler may still need to read it from `sigcontext`.
            Ok(result)
        }
        ArchExtendedPstateStorage::State32(extended_aarch32_pstate) => {
            let mut result = [0u8; SIGCONTEXT_EXTENDED_PSTATE_DATA_SIZE];
            let mut vregs = [0u128; 32];
            vregs[..16].copy_from_slice(extended_aarch32_pstate.get_arm32_qregs());

            let fpsimd = fpsimd_context {
                head: _aarch64_ctx {
                    magic: FPSIMD_MAGIC,
                    size: std::mem::size_of::<fpsimd_context>() as u32,
                },
                fpsr: extended_aarch32_pstate.get_arm32_fpsr(),
                fpcr: extended_aarch32_pstate.get_arm32_fpcr(),
                vregs,
            };
            fpsimd.write_to_prefix(&mut result).map_err(|_| errno!(EINVAL))?;

            // TODO(https://fxbug.dev/486225910): Use an aarch32-specific ucontext layout.
            Ok(result)
        }
    }
}

fn parse_pstate_extended_data(
    data: &[u8; SIGCONTEXT_EXTENDED_PSTATE_DATA_SIZE],
    extended_pstate: &mut ArchExtendedPstateStorage,
) -> Result<(), Errno> {
    const FPSIMD_CONTEXT_SIZE: u32 = std::mem::size_of::<fpsimd_context>() as u32;
    const ESR_CONTEXT_SIZE: u32 = std::mem::size_of::<esr_context>() as u32;
    const SVE_CONTEXT_HEADER_SIZE: u32 = std::mem::size_of::<sve_context>() as u32;

    let has_sve = match extended_pstate {
        ArchExtendedPstateStorage::State64(state) => state.has_arm64_sve(),
        ArchExtendedPstateStorage::State32(_) => false,
    };

    let mut parsed_fpsimd: Option<fpsimd_context> = None;
    let mut parsed_sve: Option<Option<([u16; 16], u16)>> = None;
    let mut offset: usize = 0;
    loop {
        match _aarch64_ctx::read_from_prefix(&data[offset..]) {
            Ok((_aarch64_ctx { magic: 0, size: 0 }, _)) => break,

            Ok((_aarch64_ctx { magic: FPSIMD_MAGIC, size: FPSIMD_CONTEXT_SIZE }, _))
                if parsed_fpsimd.is_some() =>
            {
                log_debug!("Found duplicate `fpsimd_context` in `sigcontext`");
                return error!(EINVAL);
            }

            Ok((_aarch64_ctx { magic: FPSIMD_MAGIC, size: FPSIMD_CONTEXT_SIZE }, _)) => {
                let (fpsimd, _) = fpsimd_context::read_from_prefix(&data[offset..])
                    .map_err(|_| errno!(EINVAL))?;
                parsed_fpsimd = Some(fpsimd);
                offset += FPSIMD_CONTEXT_SIZE as usize;
            }

            Ok((_aarch64_ctx { magic: FPSIMD_MAGIC, size }, _)) => {
                log_debug!("Invalid size for `fpsimd_context` in `sigcontext`: {}", size);
                return error!(EINVAL);
            }

            Ok((_aarch64_ctx { magic: SVE_MAGIC, size }, _)) => {
                if !has_sve || parsed_sve.is_some() {
                    return error!(EINVAL);
                }
                if size < SVE_CONTEXT_HEADER_SIZE
                    || size % 16 != 0
                    || offset + (size as usize) > data.len()
                {
                    log_debug!("Invalid size for `sve_context` in `sigcontext`: {}", size);
                    return error!(EINVAL);
                }

                let sve_slice = &data[offset..offset + (size as usize)];
                let (sve, _) =
                    sve_context::read_from_prefix(sve_slice).map_err(|_| errno!(EINVAL))?;
                // Like Linux's `restore_sve_fpsimd_context`, only validate `vl` and reject
                // streaming mode (`SVE_SIG_FLAG_SM`, which requires SME). Other `flags`
                // bits and `__reserved` are ignored for forward compatibility.
                if u32::from(sve.vl) != SVE_VL_MIN || (u32::from(sve.flags) & SVE_SIG_FLAG_SM) != 0
                {
                    return error!(EINVAL);
                }

                if size == SVE_CONTEXT_HEADER_SIZE {
                    // A header-only `sve_context` means no SVE register state was live.
                    parsed_sve = Some(None);
                } else if (size as usize) >= SVE_SIG_CONTEXT_SIZE {
                    // As on Linux, any section large enough to hold the register dump
                    // (`SVE_SIG_CONTEXT_SIZE(vq)`) is accepted; trailing bytes are ignored.
                    // Per the Linux ARM64 SVE signal ABI (`restore_sve_fpsimd_context` /
                    // `fpsimd_to_sve`), `fpsimd_context.vregs` is authoritative for bits
                    // `[127:0]` of `Z0..Z31` while `sve_context` only supplies bits
                    // `[VL-1:128]`; at 128-bit VL (`VL == 128`), `Z0..Z31` has no bits
                    // above 127 beyond `fpsimd_context.vregs`.
                    let (p_regs, _) =
                        <[u16; 16]>::read_from_prefix(&sve_slice[SVE_SIG_PREGS_OFFSET..])
                            .map_err(|_| errno!(EINVAL))?;
                    let (ffr, _) = u16::read_from_prefix(&sve_slice[SVE_SIG_FFR_OFFSET..])
                        .map_err(|_| errno!(EINVAL))?;
                    parsed_sve = Some(Some((p_regs, ffr)));
                } else {
                    log_debug!("Invalid size for `sve_context` in `sigcontext`: {}", size);
                    return error!(EINVAL);
                }

                offset += size as usize;
            }

            Ok((_aarch64_ctx { magic: ESR_MAGIC, size: ESR_CONTEXT_SIZE }, _)) => {
                // ESR register is read-only so we can skip it.
                offset += ESR_CONTEXT_SIZE as usize;
            }

            Ok((_aarch64_ctx { magic: ESR_MAGIC, size }, _)) => {
                log_debug!("Invalid size for `esr_context` in `sigcontext`: {}", size);
                return error!(EINVAL);
            }

            Ok((_aarch64_ctx { magic: EXTRA_MAGIC, size }, _)) => {
                if size as usize <= std::mem::size_of::<_aarch64_ctx>() {
                    log_debug!("Invalid size for `EXTRA_MAGIC` section in `sigcontext`");
                    return error!(EINVAL);
                }

                track_stub!(TODO("https://fxbug.dev/322873793"), "sigcontext EXTRA_MAGIC");
                offset += ESR_CONTEXT_SIZE as usize;
            }

            Ok((_aarch64_ctx { magic, size }, _)) => {
                log_debug!(
                    "Unrecognized sectionin `sigcontext` (magic: 0x{:x}. size: {})",
                    magic,
                    size
                );
                return error!(EINVAL);
            }

            Err(_) => return error!(EINVAL),
        };
    }

    let Some(fpsimd) = parsed_fpsimd else {
        log_debug!("Couldn't find `fpsimd_context` in `sigcontext`");
        return error!(EINVAL);
    };

    match extended_pstate {
        ArchExtendedPstateStorage::State64(extended_pstate) => {
            extended_pstate.set_arm64_state(&fpsimd.vregs, fpsimd.fpsr, fpsimd.fpcr);
            if extended_pstate.has_arm64_sve() {
                match parsed_sve {
                    Some(Some((p_regs, ffr))) => extended_pstate.set_arm64_sve_state(&p_regs, ffr),
                    Some(None) | None => extended_pstate.reset_arm64_sve_state(),
                }
            }
        }
        ArchExtendedPstateStorage::State32(extended_aarch32_pstate) => {
            // TODO(https://fxbug.dev/486225910): Use an aarch32-specific ucontext layout.
            extended_aarch32_pstate.set_arm32_state(
                fpsimd.vregs[..16].try_into().unwrap(),
                fpsimd.fpsr,
                fpsimd.fpcr,
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::spawn_kernel_and_run;
    use extended_pstate::ExtendedPstateState;
    use starnix_uapi::signals::SIGUSR1;

    fn sample_qregs() -> [u128; 32] {
        let mut qregs = [0u128; 32];
        for i in 0..32 {
            qregs[i] = (i as u128 + 1) * 0x0102030405060708_u128;
        }
        qregs
    }

    fn sample_pregs() -> [u16; 16] {
        let mut pregs = [0u16; 16];
        for i in 0..16 {
            pregs[i] = 0x5a00 | (i as u16 + 1);
        }
        pregs
    }

    /// Returns the offsets of every `_aarch64_ctx` record with the given `magic` in
    /// `sigcontext.__reserved`, walking the records up to the `{0, 0}` terminator.
    fn find_records(data: &[u8; SIGCONTEXT_EXTENDED_PSTATE_DATA_SIZE], magic: u32) -> Vec<usize> {
        let mut offsets = Vec::new();
        let mut offset = 0;
        loop {
            let (header, _) = _aarch64_ctx::read_from_prefix(&data[offset..]).unwrap();
            if header.magic == 0 && header.size == 0 {
                return offsets;
            }
            if header.magic == magic {
                offsets.push(offset);
            }
            assert!(header.size >= std::mem::size_of::<_aarch64_ctx>() as u32);
            offset += header.size as usize;
        }
    }

    /// Byte offset of `sigcontext.__reserved` within `SignalStackFrame::context` for a 64-bit
    /// frame.
    fn reserved_offset_in_context() -> usize {
        std::mem::offset_of!(uapi::ucontext, uc_mcontext)
            + std::mem::offset_of!(uapi::sigcontext, __reserved)
    }

    fn assert_state64(
        storage: &ArchExtendedPstateStorage,
        has_sve: bool,
        qregs: &[u128; 32],
        fpsr: u32,
        fpcr: u32,
        pregs: &[u16; 16],
        ffr: u16,
        context: &str,
    ) {
        let ArchExtendedPstateStorage::State64(state) = storage else { unreachable!() };
        assert_eq!(state.has_arm64_sve(), has_sve, "{context}");
        assert_eq!(state.get_arm64_qregs(), qregs, "{context}");
        assert_eq!(state.get_arm64_fpsr(), fpsr, "{context}");
        assert_eq!(state.get_arm64_fpcr(), fpcr, "{context}");
        assert_eq!(state.get_arm64_p_regs(), pregs, "{context}");
        assert_eq!(state.get_arm64_ffr(), ffr, "{context}");
    }

    #[::fuchsia::test]
    fn test_pstate_extended_data_without_sve() {
        let mut pstate = ExtendedPstateState::with_sve(false);
        let qregs = sample_qregs();
        pstate.set_arm64_state(&qregs, 0x10, 0x20);

        let storage = ArchExtendedPstateStorage::State64(Box::new(pstate));
        let data = get_pstate_extended_data(&storage).expect("get_pstate_extended_data");

        let fpsimd_size = std::mem::size_of::<fpsimd_context>();
        let (term, _) = _aarch64_ctx::read_from_prefix(&data[fpsimd_size..]).unwrap();
        assert_eq!(term.magic, 0);
        assert_eq!(term.size, 0);

        // Restoring into a non-SVE state restores FPSIMD registers.
        let mut restored_non_sve =
            ArchExtendedPstateStorage::State64(Box::new(ExtendedPstateState::with_sve(false)));
        parse_pstate_extended_data(&data, &mut restored_non_sve).expect("parse should succeed");
        let ArchExtendedPstateStorage::State64(restored) = &restored_non_sve else {
            unreachable!()
        };
        assert_eq!(restored.get_arm64_qregs(), &qregs);
        assert_eq!(restored.get_arm64_fpsr(), 0x10);
        assert_eq!(restored.get_arm64_fpcr(), 0x20);

        // Restoring a frame without SVE_MAGIC into an SVE-enabled state resets P0-P15 and FFR.
        let mut sve_pstate = ExtendedPstateState::with_sve(true);
        sve_pstate.set_arm64_sve_state(&sample_pregs(), 0x00ff);
        let mut restored_sve = ArchExtendedPstateStorage::State64(Box::new(sve_pstate));
        parse_pstate_extended_data(&data, &mut restored_sve).expect("parse should succeed");
        let ArchExtendedPstateStorage::State64(restored) = &restored_sve else { unreachable!() };
        assert_eq!(restored.get_arm64_qregs(), &qregs);
        assert_eq!(restored.get_arm64_p_regs(), &[0u16; 16]);
        assert_eq!(restored.get_arm64_ffr(), 0);
    }

    #[::fuchsia::test]
    fn test_pstate_extended_data_with_sve_roundtrip() {
        let mut pstate = ExtendedPstateState::with_sve(true);
        let qregs = sample_qregs();
        let pregs = sample_pregs();
        pstate.set_arm64_state(&qregs, 0x10, 0x20);
        pstate.set_arm64_sve_state(&pregs, 0x00ff);

        let storage = ArchExtendedPstateStorage::State64(Box::new(pstate));
        let data = get_pstate_extended_data(&storage).expect("get_pstate_extended_data");

        // Verify the serialized `sve_context` section layout.
        let fpsimd_size = std::mem::size_of::<fpsimd_context>();
        let sve_slice = &data[fpsimd_size..fpsimd_size + SVE_SIG_CONTEXT_ALIGNED_SIZE];
        let (sve_hdr, _) = sve_context::read_from_prefix(sve_slice).unwrap();
        assert_eq!(sve_hdr.head.magic, SVE_MAGIC);
        assert_eq!(sve_hdr.head.size, SVE_SIG_CONTEXT_ALIGNED_SIZE as u32);
        assert_eq!(u32::from(sve_hdr.vl), SVE_VL_MIN);
        assert_eq!(sve_hdr.flags, 0);
        assert_eq!(sve_hdr.__reserved, [0; 2]);
        let (saved_z, _) =
            <[u128; 32]>::read_from_prefix(&sve_slice[SVE_SIG_ZREGS_OFFSET..]).unwrap();
        let (saved_p, _) =
            <[u16; 16]>::read_from_prefix(&sve_slice[SVE_SIG_PREGS_OFFSET..]).unwrap();
        let (saved_ffr, _) = u16::read_from_prefix(&sve_slice[SVE_SIG_FFR_OFFSET..]).unwrap();
        assert_eq!(saved_z, qregs);
        assert_eq!(saved_p, pregs);
        assert_eq!(saved_ffr, 0x00ff);

        let (term, _) =
            _aarch64_ctx::read_from_prefix(&data[fpsimd_size + SVE_SIG_CONTEXT_ALIGNED_SIZE..])
                .unwrap();
        assert_eq!(term.magic, 0);
        assert_eq!(term.size, 0);

        // Parse into a fresh SVE-enabled state.
        let mut restored_storage =
            ArchExtendedPstateStorage::State64(Box::new(ExtendedPstateState::with_sve(true)));
        parse_pstate_extended_data(&data, &mut restored_storage).expect("parse should succeed");
        let ArchExtendedPstateStorage::State64(restored) = &restored_storage else {
            unreachable!()
        };
        assert_eq!(restored.get_arm64_qregs(), &qregs);
        assert_eq!(restored.get_arm64_fpsr(), 0x10);
        assert_eq!(restored.get_arm64_fpcr(), 0x20);
        assert_eq!(restored.get_arm64_p_regs(), &pregs);
        assert_eq!(restored.get_arm64_ffr(), 0x00ff);
    }

    #[::fuchsia::test]
    fn test_pstate_extended_data_sve_header_only() {
        let mut pstate = ExtendedPstateState::with_sve(true);
        let qregs = sample_qregs();
        let pregs = sample_pregs();
        pstate.set_arm64_state(&qregs, 0x10, 0x20);
        pstate.set_arm64_sve_state(&pregs, 0x00ff);

        let mut storage = ArchExtendedPstateStorage::State64(Box::new(pstate));
        let mut header_only_data =
            get_pstate_extended_data(&storage).expect("get_pstate_extended_data");
        let fpsimd_size = std::mem::size_of::<fpsimd_context>();
        let header_only_sve = sve_context {
            head: _aarch64_ctx {
                magic: SVE_MAGIC,
                size: std::mem::size_of::<sve_context>() as u32,
            },
            vl: SVE_VL_MIN as u16,
            flags: 0,
            __reserved: [0; 2],
        };
        header_only_sve.write_to_prefix(&mut header_only_data[fpsimd_size..]).unwrap();
        header_only_data[fpsimd_size + std::mem::size_of::<sve_context>()..].fill(0);

        parse_pstate_extended_data(&header_only_data, &mut storage)
            .expect("header-only sve_context should succeed");
        let ArchExtendedPstateStorage::State64(restored) = &storage else { unreachable!() };
        assert_eq!(restored.get_arm64_qregs(), &qregs);
        assert_eq!(restored.get_arm64_p_regs(), &[0u16; 16]);
        assert_eq!(restored.get_arm64_ffr(), 0);
    }

    #[::fuchsia::test]
    fn test_pstate_extended_data_sve_validation_errors() {
        fn sve_header(size: u32, flags: u16, reserved: [u16; 2]) -> sve_context {
            sve_context {
                head: _aarch64_ctx { magic: SVE_MAGIC, size },
                vl: SVE_VL_MIN as u16,
                flags,
                __reserved: reserved,
            }
        }

        let mut pstate = ExtendedPstateState::with_sve(true);
        pstate.set_arm64_state(&sample_qregs(), 0x10, 0x20);
        pstate.set_arm64_sve_state(&sample_pregs(), 0x00ff);
        let mut storage = ArchExtendedPstateStorage::State64(Box::new(pstate));
        let valid_data = get_pstate_extended_data(&storage).expect("get_pstate_extended_data");
        let fpsimd_size = std::mem::size_of::<fpsimd_context>();

        // Returns `valid_data` with its `sve_context` header replaced by `sve`.
        let with_sve_header = |sve: sve_context| {
            let mut data = valid_data;
            sve.write_to_prefix(&mut data[fpsimd_size..]).unwrap();
            data
        };
        let full_size = SVE_SIG_CONTEXT_ALIGNED_SIZE as u32;

        // Non-SVE state must reject SVE_MAGIC.
        let mut non_sve_storage =
            ArchExtendedPstateStorage::State64(Box::new(ExtendedPstateState::with_sve(false)));
        assert_eq!(parse_pstate_extended_data(&valid_data, &mut non_sve_storage), error!(EINVAL));

        // Invalid VL must be rejected.
        let mut bad_vl_sve = sve_header(full_size, 0, [0; 2]);
        bad_vl_sve.vl = 32;
        assert_eq!(
            parse_pstate_extended_data(&with_sve_header(bad_vl_sve), &mut storage),
            error!(EINVAL)
        );

        // Streaming mode (SVE_SIG_FLAG_SM) must be rejected, as SME is not supported.
        assert_eq!(
            parse_pstate_extended_data(
                &with_sve_header(sve_header(full_size, SVE_SIG_FLAG_SM as u16, [0; 2])),
                &mut storage
            ),
            error!(EINVAL)
        );

        // Invalid section sizes must be rejected: 0, unaligned sizes, sizes between the header
        // and the full register dump (e.g. 32 or 560), and sizes that don't fit in `__reserved`.
        for bad_size in [0u32, 8, 24, 32, full_size - 16, 4096] {
            assert_eq!(
                parse_pstate_extended_data(
                    &with_sve_header(sve_header(bad_size, 0, [0; 2])),
                    &mut storage
                ),
                error!(EINVAL),
                "bad_size={} should fail",
                bad_size
            );
        }

        // As on Linux, unknown `flags` bits and non-zero `__reserved` fields are ignored, and any
        // 16-byte aligned section at least as large as the register dump (e.g. 592 bytes) is
        // accepted, with the trailing bytes ignored.
        for accepted_sve in [
            sve_header(full_size, 0x8000, [0; 2]),
            sve_header(full_size, 0, [1, 2]),
            sve_header(full_size + 16, 0, [0; 2]),
        ] {
            let mut restored_storage =
                ArchExtendedPstateStorage::State64(Box::new(ExtendedPstateState::with_sve(true)));
            parse_pstate_extended_data(&with_sve_header(accepted_sve), &mut restored_storage)
                .expect("sve_context should be accepted");
            let ArchExtendedPstateStorage::State64(restored) = &restored_storage else {
                unreachable!()
            };
            assert_eq!(restored.get_arm64_qregs(), &sample_qregs());
            assert_eq!(restored.get_arm64_p_regs(), &sample_pregs());
            assert_eq!(restored.get_arm64_ffr(), 0x00ff);
        }

        // Duplicate SVE_MAGIC section must be rejected.
        let hdr_only = sve_header(std::mem::size_of::<sve_context>() as u32, 0, [0; 2]);
        let mut dup_sve_data = valid_data;
        hdr_only
            .write_to_prefix(&mut dup_sve_data[fpsimd_size + SVE_SIG_CONTEXT_ALIGNED_SIZE..])
            .unwrap();
        assert_eq!(parse_pstate_extended_data(&dup_sve_data, &mut storage), error!(EINVAL));

        // Frame with SVE_MAGIC but missing FPSIMD_MAGIC must be rejected.
        let mut no_fpsimd_data = [0u8; SIGCONTEXT_EXTENDED_PSTATE_DATA_SIZE];
        hdr_only.write_to_prefix(&mut no_fpsimd_data).unwrap();
        assert_eq!(parse_pstate_extended_data(&no_fpsimd_data, &mut storage), error!(EINVAL));

        // Any validation error after a valid `fpsimd_context` or `sve_context` must leave
        // `extended_pstate` (both Q/FPSR/FPCR and SVE P/FFR registers) completely untouched.
        let orig_qregs = [0xdeadbeef_cafebabe_u128; 32];
        let orig_pregs = [0x1357u16; 16];
        let mut untouched_pstate = ExtendedPstateState::with_sve(true);
        untouched_pstate.set_arm64_state(&orig_qregs, 0xaa, 0xbb);
        untouched_pstate.set_arm64_sve_state(&orig_pregs, 0x00ff);
        let mut untouched_storage = ArchExtendedPstateStorage::State64(Box::new(untouched_pstate));

        let mut trailing_bad_record = valid_data;
        let bad_ctx = _aarch64_ctx { magic: 0xbad0_0bad, size: 16 };
        bad_ctx
            .write_to_prefix(&mut trailing_bad_record[fpsimd_size + SVE_SIG_CONTEXT_ALIGNED_SIZE..])
            .unwrap();
        for invalid_frame in [&trailing_bad_record, &dup_sve_data, &no_fpsimd_data] {
            assert_eq!(
                parse_pstate_extended_data(invalid_frame, &mut untouched_storage),
                error!(EINVAL)
            );
            let ArchExtendedPstateStorage::State64(state) = &untouched_storage else {
                unreachable!()
            };
            assert_eq!(state.get_arm64_qregs(), &orig_qregs);
            assert_eq!(state.get_arm64_fpsr(), 0xaa);
            assert_eq!(state.get_arm64_fpcr(), 0xbb);
            assert_eq!(state.get_arm64_p_regs(), &orig_pregs);
            assert_eq!(state.get_arm64_ffr(), 0x00ff);
        }
    }

    #[::fuchsia::test]
    fn test_pstate_extended_data_sve_before_fpsimd() {
        let qregs = sample_qregs();
        let pregs = sample_pregs();
        let mut pstate = ExtendedPstateState::with_sve(true);
        pstate.set_arm64_state(&qregs, 0x10, 0x20);
        pstate.set_arm64_sve_state(&pregs, 0x00ff);
        let storage = ArchExtendedPstateStorage::State64(Box::new(pstate));
        let canonical_data = get_pstate_extended_data(&storage).expect("get_pstate_extended_data");

        let fpsimd_size = std::mem::size_of::<fpsimd_context>();
        let sve_size = SVE_SIG_CONTEXT_ALIGNED_SIZE;

        // Build a signal frame where `SVE_MAGIC` precedes `FPSIMD_MAGIC` in `__reserved`, and
        // overwrite the Z-register area in `sve_context` with distinct bytes to verify that
        // `fpsimd_context.vregs` remains authoritative for `Q0..Q31` regardless of record order.
        let mut sve_first_data = [0u8; SIGCONTEXT_EXTENDED_PSTATE_DATA_SIZE];
        sve_first_data[..sve_size]
            .copy_from_slice(&canonical_data[fpsimd_size..fpsimd_size + sve_size]);
        sve_first_data[SVE_SIG_ZREGS_OFFSET..SVE_SIG_PREGS_OFFSET].fill(0xee);
        sve_first_data[sve_size..sve_size + fpsimd_size]
            .copy_from_slice(&canonical_data[..fpsimd_size]);

        let mut restored_storage =
            ArchExtendedPstateStorage::State64(Box::new(ExtendedPstateState::with_sve(true)));
        parse_pstate_extended_data(&sve_first_data, &mut restored_storage)
            .expect("SVE_MAGIC before FPSIMD_MAGIC should succeed");
        let ArchExtendedPstateStorage::State64(restored) = &restored_storage else {
            unreachable!()
        };
        assert_eq!(restored.get_arm64_qregs(), &qregs);
        assert_eq!(restored.get_arm64_fpsr(), 0x10);
        assert_eq!(restored.get_arm64_fpcr(), 0x20);
        assert_eq!(restored.get_arm64_p_regs(), &pregs);
        assert_eq!(restored.get_arm64_ffr(), 0x00ff);

        // Header-only `SVE_MAGIC` before valid `FPSIMD_MAGIC` must restore `Q0..Q31` from
        // `fpsimd_context` and zero `P0..P15` and `FFR`.
        let sve_hdr_size = std::mem::size_of::<sve_context>();
        let mut hdr_sve_first_data = [0u8; SIGCONTEXT_EXTENDED_PSTATE_DATA_SIZE];
        let header_only_sve = sve_context {
            head: _aarch64_ctx { magic: SVE_MAGIC, size: sve_hdr_size as u32 },
            vl: SVE_VL_MIN as u16,
            flags: 0,
            __reserved: [0; 2],
        };
        header_only_sve.write_to_prefix(&mut hdr_sve_first_data).unwrap();
        hdr_sve_first_data[sve_hdr_size..sve_hdr_size + fpsimd_size]
            .copy_from_slice(&canonical_data[..fpsimd_size]);

        parse_pstate_extended_data(&hdr_sve_first_data, &mut restored_storage)
            .expect("header-only SVE_MAGIC before FPSIMD_MAGIC should succeed");
        let ArchExtendedPstateStorage::State64(restored) = &restored_storage else {
            unreachable!()
        };
        assert_eq!(restored.get_arm64_qregs(), &qregs);
        assert_eq!(restored.get_arm64_fpsr(), 0x10);
        assert_eq!(restored.get_arm64_fpcr(), 0x20);
        assert_eq!(restored.get_arm64_p_regs(), &[0u16; 16]);
        assert_eq!(restored.get_arm64_ffr(), 0);

        // When a valid `SVE_MAGIC` comes first and is followed by a malformed `FPSIMD_MAGIC`
        // (e.g. invalid size) or duplicate `FPSIMD_MAGIC`, `parse_pstate_extended_data` must
        // return `EINVAL` without mutating either `P0..P15`/`FFR` or `Q0..Q31`/`FPSR`/`FPCR`.
        let orig_qregs = [0xdeadbeef_cafebabe_u128; 32];
        let orig_pregs = [0x1357u16; 16];
        let mut untouched_pstate = ExtendedPstateState::with_sve(true);
        untouched_pstate.set_arm64_state(&orig_qregs, 0xaa, 0xbb);
        untouched_pstate.set_arm64_sve_state(&orig_pregs, 0x00ff);
        let mut untouched_storage = ArchExtendedPstateStorage::State64(Box::new(untouched_pstate));

        let mut sve_first_bad_fpsimd = sve_first_data;
        let bad_fpsimd_hdr = _aarch64_ctx { magic: FPSIMD_MAGIC, size: 16 };
        bad_fpsimd_hdr.write_to_prefix(&mut sve_first_bad_fpsimd[sve_size..]).unwrap();

        let mut sve_first_dup_fpsimd = sve_first_data;
        sve_first_dup_fpsimd[sve_size + fpsimd_size..sve_size + 2 * fpsimd_size]
            .copy_from_slice(&canonical_data[..fpsimd_size]);

        for invalid_frame in [&sve_first_bad_fpsimd, &sve_first_dup_fpsimd] {
            assert_eq!(
                parse_pstate_extended_data(invalid_frame, &mut untouched_storage),
                error!(EINVAL)
            );
            let ArchExtendedPstateStorage::State64(state) = &untouched_storage else {
                unreachable!()
            };
            assert_eq!(state.get_arm64_qregs(), &orig_qregs);
            assert_eq!(state.get_arm64_fpsr(), 0xaa);
            assert_eq!(state.get_arm64_fpcr(), 0xbb);
            assert_eq!(state.get_arm64_p_regs(), &orig_pregs);
            assert_eq!(state.get_arm64_ffr(), 0x00ff);
        }
    }

    #[::fuchsia::test]
    async fn test_signal_stack_frame_sve_roundtrip() {
        spawn_kernel_and_run(async |current_task| {
            let qregs = sample_qregs();
            let pregs = sample_pregs();
            let mut pstate = ExtendedPstateState::with_sve(true);
            pstate.set_arm64_state(&qregs, 0x10, 0x20);
            pstate.set_arm64_sve_state(&pregs, 0x00ff);
            current_task.thread_state.extended_pstate =
                ArchExtendedPstateStorage::State64(Box::new(pstate));

            let signal_state = SignalState::default();
            let siginfo = SignalInfo::kernel(SIGUSR1);
            let frame = SignalStackFrame::new(
                &current_task.task,
                ArchWidth::Arch64,
                &mut current_task.thread_state.registers,
                &current_task.thread_state.extended_pstate,
                &signal_state,
                &siginfo,
                sigaction_t::default(),
                UserAddress::default(),
            )
            .expect("SignalStackFrame::new should succeed");

            // Clobber the task's extended_pstate before restoring from the signal stack frame.
            current_task.thread_state.extended_pstate.reset();
            restore_registers(current_task, &frame, UserAddress::default())
                .expect("restore_registers should succeed");

            let ArchExtendedPstateStorage::State64(restored) =
                &current_task.thread_state.extended_pstate
            else {
                unreachable!()
            };
            assert_eq!(restored.get_arm64_qregs(), &qregs);
            assert_eq!(restored.get_arm64_fpsr(), 0x10);
            assert_eq!(restored.get_arm64_fpcr(), 0x20);
            assert_eq!(restored.get_arm64_p_regs(), &pregs);
            assert_eq!(restored.get_arm64_ffr(), 0x00ff);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_signal_stack_frame_without_sve_roundtrip() {
        spawn_kernel_and_run(async |current_task| {
            // A 64-bit task without SVE (independently of the host CPU's capabilities).
            let qregs = sample_qregs();
            let mut pstate = ExtendedPstateState::with_sve(false);
            pstate.set_arm64_state(&qregs, 0x10, 0x20);
            current_task.thread_state.extended_pstate =
                ArchExtendedPstateStorage::State64(Box::new(pstate));

            let signal_state = SignalState::default();
            let siginfo = SignalInfo::kernel(SIGUSR1);
            let frame = SignalStackFrame::new(
                &current_task.task,
                ArchWidth::Arch64,
                &mut current_task.thread_state.registers,
                &current_task.thread_state.extended_pstate,
                &signal_state,
                &siginfo,
                sigaction_t::default(),
                UserAddress::default(),
            )
            .expect("SignalStackFrame::new should succeed");

            // The frame holds a single `fpsimd_context` followed by the terminator, and no
            // `sve_context` at all, as on a Linux system without SVE.
            let reserved = &frame.get_ucontext64().uc_mcontext.__reserved;
            let fpsimd_size = std::mem::size_of::<fpsimd_context>();
            assert_eq!(find_records(reserved, FPSIMD_MAGIC), vec![0]);
            assert_eq!(find_records(reserved, SVE_MAGIC), Vec::<usize>::new());
            let (fpsimd, _) = fpsimd_context::read_from_prefix(&reserved[..]).unwrap();
            assert_eq!(fpsimd.head.size, fpsimd_size as u32);
            assert_eq!(fpsimd.vregs, qregs);
            assert_eq!(fpsimd.fpsr, 0x10);
            assert_eq!(fpsimd.fpcr, 0x20);
            let (term, _) = _aarch64_ctx::read_from_prefix(&reserved[fpsimd_size..]).unwrap();
            assert_eq!((term.magic, term.size), (0, 0));

            // Restoring the frame succeeds, restores Q0-Q31/FPSR/FPCR, and keeps `has_sve` off.
            current_task.thread_state.extended_pstate.reset();
            assert_state64(
                &current_task.thread_state.extended_pstate,
                false,
                &[0u128; 32],
                0,
                0,
                &[0u16; 16],
                0,
                "after reset",
            );
            restore_registers(current_task, &frame, UserAddress::default())
                .expect("restore_registers should succeed");
            assert_state64(
                &current_task.thread_state.extended_pstate,
                false,
                &qregs,
                0x10,
                0x20,
                &[0u16; 16],
                0,
                "after restore",
            );

            // A frame carrying an `sve_context`, header-only or with a full register dump, is
            // rejected with EINVAL on a task without SVE (as on Linux), leaving the task's
            // extended state untouched.
            let other_qregs = [0x5555_aaaa_5555_aaaa_u128; 32];
            let reserved_offset = reserved_offset_in_context();
            for sve_size in [std::mem::size_of::<sve_context>(), SVE_SIG_CONTEXT_ALIGNED_SIZE] {
                let context = format!("sve_context size={sve_size}");
                let mut sve_frame = frame.clone();
                let sve = sve_context {
                    head: _aarch64_ctx { magic: SVE_MAGIC, size: sve_size as u32 },
                    vl: SVE_VL_MIN as u16,
                    flags: 0,
                    __reserved: [0; 2],
                };
                // The bytes after the header (register dump and the new terminator) are zero.
                sve.write_to_prefix(&mut sve_frame.context[reserved_offset + fpsimd_size..])
                    .unwrap();
                assert_eq!(
                    find_records(&sve_frame.get_ucontext64().uc_mcontext.__reserved, SVE_MAGIC),
                    vec![fpsimd_size],
                    "{context}"
                );

                let ArchExtendedPstateStorage::State64(state) =
                    &mut current_task.thread_state.extended_pstate
                else {
                    unreachable!()
                };
                state.set_arm64_state(&other_qregs, 0x30, 0x40);
                assert_eq!(
                    restore_registers(current_task, &sve_frame, UserAddress::default()),
                    error!(EINVAL),
                    "{context}"
                );
                assert_state64(
                    &current_task.thread_state.extended_pstate,
                    false,
                    &other_qregs,
                    0x30,
                    0x40,
                    &[0u16; 16],
                    0,
                    &context,
                );
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_signal_stack_frame_sve_context_sizes() {
        spawn_kernel_and_run(async |current_task| {
            let qregs = sample_qregs();
            let pregs = sample_pregs();
            let mut pstate = ExtendedPstateState::with_sve(true);
            pstate.set_arm64_state(&qregs, 0x10, 0x20);
            pstate.set_arm64_sve_state(&pregs, 0x00ff);
            current_task.thread_state.extended_pstate =
                ArchExtendedPstateStorage::State64(Box::new(pstate));

            let signal_state = SignalState::default();
            let siginfo = SignalInfo::kernel(SIGUSR1);
            let frame = SignalStackFrame::new(
                &current_task.task,
                ArchWidth::Arch64,
                &mut current_task.thread_state.registers,
                &current_task.thread_state.extended_pstate,
                &signal_state,
                &siginfo,
                sigaction_t::default(),
                UserAddress::default(),
            )
            .expect("SignalStackFrame::new should succeed");

            let fpsimd_size = std::mem::size_of::<fpsimd_context>();
            let sve_header_size = std::mem::size_of::<sve_context>();
            let reserved_offset = reserved_offset_in_context();
            let reserved = &frame.get_ucontext64().uc_mcontext.__reserved;
            assert_eq!(find_records(reserved, FPSIMD_MAGIC), vec![0]);
            assert_eq!(find_records(reserved, SVE_MAGIC), vec![fpsimd_size]);
            let (sve, _) = sve_context::read_from_prefix(&reserved[fpsimd_size..]).unwrap();
            assert_eq!(sve.head.size as usize, SVE_SIG_CONTEXT_ALIGNED_SIZE);

            // Returns a copy of `frame` whose `sve_context` header is replaced by `sve`; the
            // register dump following the header is left in place.
            let with_sve_header = |sve: sve_context| {
                let mut sve_frame = frame.clone();
                sve.write_to_prefix(&mut sve_frame.context[reserved_offset + fpsimd_size..])
                    .unwrap();
                sve_frame
            };
            let restore = |current_task: &mut CurrentTask, sve_frame: &SignalStackFrame| {
                // Clobber the task's state so that every register must come from the frame.
                let ArchExtendedPstateStorage::State64(state) =
                    &mut current_task.thread_state.extended_pstate
                else {
                    unreachable!()
                };
                state.set_arm64_state(&[0x77u128; 32], 0x1234, 0x5678);
                state.set_arm64_sve_state(&[0x8001u16; 16], 0x0001);
                restore_registers(current_task, sve_frame, UserAddress::default())
            };

            // A header-only `sve_context` (`size == 16`), as emitted by Linux when no SVE state
            // was live, restores Q0-Q31/FPSR/FPCR from `fpsimd_context` and zeroes P0-P15 and FFR.
            {
                let mut header_only = with_sve_header(sve_context {
                    head: _aarch64_ctx { magic: SVE_MAGIC, size: sve_header_size as u32 },
                    vl: SVE_VL_MIN as u16,
                    flags: 0,
                    __reserved: [0; 2],
                });
                // The old register dump now sits where the next record is expected: terminate
                // the frame there instead.
                header_only.context[reserved_offset + fpsimd_size + sve_header_size..].fill(0);
                assert_eq!(
                    find_records(&header_only.get_ucontext64().uc_mcontext.__reserved, SVE_MAGIC),
                    vec![fpsimd_size]
                );
                restore(current_task, &header_only)
                    .expect("header-only sve_context should succeed");
                assert_state64(
                    &current_task.thread_state.extended_pstate,
                    true,
                    &qregs,
                    0x10,
                    0x20,
                    &[0u16; 16],
                    0,
                    "header-only sve_context",
                );
            }

            // An oversized, 16-byte aligned `sve_context` (`size == 608`) with non-zero
            // `__reserved` and unknown (non-`SVE_SIG_FLAG_SM`) flag bits, as a newer Linux ABI
            // might produce, is accepted: the trailing bytes are ignored and P0-P15/FFR restored.
            {
                let oversized_size = SVE_SIG_CONTEXT_ALIGNED_SIZE + 32;
                assert_eq!(oversized_size, 608);
                let oversized = with_sve_header(sve_context {
                    head: _aarch64_ctx { magic: SVE_MAGIC, size: oversized_size as u32 },
                    vl: SVE_VL_MIN as u16,
                    flags: 0x8000 | 0x0002,
                    __reserved: [0xaaaa, 0xbbbb],
                });
                // The original terminator is now inside the section, and the bytes following
                // the section are zero, so the frame is still well terminated.
                assert_eq!(
                    find_records(&oversized.get_ucontext64().uc_mcontext.__reserved, SVE_MAGIC),
                    vec![fpsimd_size]
                );
                restore(current_task, &oversized).expect("oversized sve_context should succeed");
                assert_state64(
                    &current_task.thread_state.extended_pstate,
                    true,
                    &qregs,
                    0x10,
                    0x20,
                    &pregs,
                    0x00ff,
                    "oversized sve_context",
                );
            }

            // Through the same path, streaming mode (`SVE_SIG_FLAG_SM`), an unaligned oversized
            // section, and a wrong vector length are rejected with EINVAL.
            let rejected = [
                (
                    "SVE_SIG_FLAG_SM",
                    SVE_SIG_CONTEXT_ALIGNED_SIZE as u32,
                    SVE_VL_MIN as u16,
                    (SVE_SIG_FLAG_SM as u16) | 0x8000,
                ),
                (
                    "unaligned size",
                    (SVE_SIG_CONTEXT_ALIGNED_SIZE + 24) as u32,
                    SVE_VL_MIN as u16,
                    0,
                ),
                ("vl != 16", SVE_SIG_CONTEXT_ALIGNED_SIZE as u32, 2 * SVE_VL_MIN as u16, 0),
            ];
            for (context, size, vl, flags) in rejected {
                let sve_frame = with_sve_header(sve_context {
                    head: _aarch64_ctx { magic: SVE_MAGIC, size },
                    vl,
                    flags,
                    __reserved: [0; 2],
                });
                assert_eq!(
                    restore(current_task, &sve_frame),
                    error!(EINVAL),
                    "{context} should be rejected"
                );
                // The clobbered state written by `restore` is left as is.
                assert_state64(
                    &current_task.thread_state.extended_pstate,
                    true,
                    &[0x77u128; 32],
                    0x1234,
                    0x5678,
                    &[0x8001u16; 16],
                    0x0001,
                    context,
                );
            }

            // The unmodified frame still restores everything.
            restore(current_task, &frame).expect("restore_registers should succeed");
            assert_state64(
                &current_task.thread_state.extended_pstate,
                true,
                &qregs,
                0x10,
                0x20,
                &pregs,
                0x00ff,
                "canonical frame",
            );
        })
        .await;
    }

    #[::fuchsia::test]
    fn test_pstate_extended_data_ffr_canonicalization() {
        let qregs = sample_qregs();
        let mut pstate = ExtendedPstateState::with_sve(true);
        pstate.set_arm64_state(&qregs, 0x10, 0x20);
        pstate.set_arm64_sve_state(&sample_pregs(), 0x00ff);
        let storage = ArchExtendedPstateStorage::State64(Box::new(pstate));
        let valid_data = get_pstate_extended_data(&storage).expect("get_pstate_extended_data");
        let fpsimd_size = std::mem::size_of::<fpsimd_context>();

        // A signal handler (or a malicious frame) may leave any bit pattern in the FFR slot of
        // `sve_context`. Architecturally valid values (a contiguous run of 1s from bit 0) are
        // restored as is, while any other value is canonicalized to its valid prefix, so that
        // `wrffr` always has well-defined behavior. Predicates accept any bit pattern verbatim.
        let ffr_cases: [(u16, u16); 10] = [
            (0x0000, 0x0000),
            (0x0001, 0x0001),
            (0b1000_0000, 0),
            (0b1010_0111, 0b0000_0111),
            (0x00ff, 0x00ff),
            (0x7fff, 0x7fff),
            (0x8001, 0x0001),
            (0xfffe, 0),
            (0xbfff, 0x3fff),
            (0xffff, 0xffff),
        ];
        for (raw_ffr, canonical_ffr) in ffr_cases {
            let context = format!("ffr={raw_ffr:#06x}");
            let mut pregs = sample_pregs();
            pregs[..8].copy_from_slice(&[raw_ffr, !raw_ffr, 0x8001, 0xffff, 0, 1, 0x5555, 0xaaaa]);
            let mut data = valid_data;
            let sve_slice = &mut data[fpsimd_size..fpsimd_size + SVE_SIG_CONTEXT_ALIGNED_SIZE];
            pregs.write_to_prefix(&mut sve_slice[SVE_SIG_PREGS_OFFSET..]).unwrap();
            raw_ffr.write_to_prefix(&mut sve_slice[SVE_SIG_FFR_OFFSET..]).unwrap();

            let mut restored_storage =
                ArchExtendedPstateStorage::State64(Box::new(ExtendedPstateState::with_sve(true)));
            parse_pstate_extended_data(&data, &mut restored_storage)
                .unwrap_or_else(|e| panic!("{context}: {e:?}"));
            assert_state64(
                &restored_storage,
                true,
                &qregs,
                0x10,
                0x20,
                &pregs,
                canonical_ffr,
                &context,
            );
        }
    }

    #[::fuchsia::test]
    fn test_pstate_extended_data_state32_roundtrip_and_rejects_sve() {
        let qregs64 = sample_qregs();
        let qregs32: [u128; 16] = qregs64[..16].try_into().unwrap();
        let mut pstate32 = extended_pstate::ExtendedAarch32PstateState::default();
        pstate32.set_arm32_state(&qregs32, 0x11223344, 0x55667788);
        let storage32 = ArchExtendedPstateStorage::State32(Box::new(pstate32));

        let data = get_pstate_extended_data(&storage32).expect("get_pstate_extended_data");
        let (fpsimd, _) = fpsimd_context::read_from_prefix(&data).expect("fpsimd_context");
        assert_eq!(fpsimd.head.magic, FPSIMD_MAGIC);
        assert_eq!(fpsimd.head.size as usize, std::mem::size_of::<fpsimd_context>());
        assert_eq!(&fpsimd.vregs[..16], &qregs32);
        assert_eq!(&fpsimd.vregs[16..], &[0u128; 16]);
        let (terminator, _) =
            _aarch64_ctx::read_from_prefix(&data[std::mem::size_of::<fpsimd_context>()..]).unwrap();
        assert_eq!(terminator.magic, 0);
        assert_eq!(terminator.size, 0);

        let mut restored32 = ArchExtendedPstateStorage::State32(Box::new(
            extended_pstate::ExtendedAarch32PstateState::default(),
        ));
        parse_pstate_extended_data(&data, &mut restored32).expect("parse_pstate_extended_data");
        match &restored32 {
            ArchExtendedPstateStorage::State32(s) => {
                assert_eq!(s.get_arm32_qregs(), &qregs32);
                assert_eq!(s.get_arm32_fpsr(), 0x11223344);
                assert_eq!(s.get_arm32_fpcr(), 0x55667788);
            }
            ArchExtendedPstateStorage::State64(_) => panic!("expected State32"),
        }

        // An `sve_context` record is not valid for AArch32 (`State32`).
        let mut sve_pstate = ExtendedPstateState::with_sve(true);
        sve_pstate.set_arm64_state(&qregs64, 0x10, 0x20);
        sve_pstate.set_arm64_sve_state(&sample_pregs(), 0x00ff);
        let sve_data =
            get_pstate_extended_data(&ArchExtendedPstateStorage::State64(Box::new(sve_pstate)))
                .expect("get_pstate_extended_data");
        assert_eq!(parse_pstate_extended_data(&sve_data, &mut restored32), error!(EINVAL));
    }
}
