// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use extended_pstate::{ExtendedPstatePointer, ExtendedPstateState};
use starnix_registers::{HeapRegs, RegisterState, RegisterStorage, RegisterStorageEnum};
use starnix_syscalls::SyscallResult;
use starnix_types::arch::ArchWidth;
use starnix_uapi::errors::{Errno, ErrnoCode};
use starnix_uapi::user_address::ArchSpecific;

#[derive(Clone)]
pub enum ArchExtendedPstateStorage {
    // Storage for 64 bit restricted mode.
    State64(Box<ExtendedPstateState>),
    #[cfg(target_arch = "aarch64")]
    // Storage for 32 bit arm restricted mode.
    State32(Box<extended_pstate::ExtendedAarch32PstateState>),
}

impl ArchExtendedPstateStorage {
    /// Returns a type-erased pointer to the underlying storage currently in use.
    pub fn as_ptr(&mut self) -> ExtendedPstatePointer {
        match self {
            ArchExtendedPstateStorage::State64(state) => {
                ExtendedPstatePointer { extended_pstate: state.as_mut() as *mut _ }
            }
            #[cfg(target_arch = "aarch64")]
            ArchExtendedPstateStorage::State32(state) => {
                ExtendedPstatePointer { extended_aarch32_pstate: state.as_mut() as *mut _ }
            }
        }
    }

    pub fn reset(&mut self) {
        match self {
            ArchExtendedPstateStorage::State64(state) => state.reset(),
            #[cfg(target_arch = "aarch64")]
            ArchExtendedPstateStorage::State32(state) => state.reset(),
        }
    }

    pub fn reset_with_arch(&mut self, arch_width: ArchWidth) {
        match (self, arch_width) {
            #[cfg(target_arch = "aarch64")]
            (ArchExtendedPstateStorage::State32(state), ArchWidth::Arch32) => state.reset(),
            #[cfg(target_arch = "aarch64")]
            (this @ ArchExtendedPstateStorage::State64(_), ArchWidth::Arch32)
            | (this @ ArchExtendedPstateStorage::State32(_), ArchWidth::Arch64) => {
                *this = Self::with_arch(arch_width);
            }
            (ArchExtendedPstateStorage::State64(state), _) => state.reset(),
        }
    }

    #[cfg(target_arch = "aarch64")]
    pub fn has_sve(&self) -> bool {
        match self {
            ArchExtendedPstateStorage::State64(state) => state.has_arm64_sve(),
            ArchExtendedPstateStorage::State32(_) => false,
        }
    }

    /// Whether the SVE vector length is inherited across `execve` (`PR_SVE_VL_INHERIT`).
    #[cfg(target_arch = "aarch64")]
    pub fn sve_vl_inherit(&self) -> bool {
        match self {
            ArchExtendedPstateStorage::State64(state) => state.get_arm64_sve_vl_inherit(),
            ArchExtendedPstateStorage::State32(_) => false,
        }
    }

    #[cfg(target_arch = "aarch64")]
    pub fn set_sve_vl_inherit(&mut self, inherit: bool) {
        if let ArchExtendedPstateStorage::State64(state) = self {
            state.set_arm64_sve_vl_inherit(inherit);
        }
    }

    fn with_arch(arch_width: ArchWidth) -> Self {
        #[cfg(target_arch = "aarch64")]
        if arch_width == ArchWidth::Arch32 {
            return ArchExtendedPstateStorage::State32(Box::new(
                extended_pstate::ExtendedAarch32PstateState::default(),
            ));
        }
        let _ = arch_width;
        ArchExtendedPstateStorage::State64(Box::new(ExtendedPstateState::default()))
    }
}

/// The thread related information of a `CurrentTask`. The information should never be used outside
/// of the thread owning the `CurrentTask`.
pub struct ThreadState<T: RegisterStorage> {
    /// A copy of the registers associated with the Zircon thread. Up-to-date values can be read
    /// from `self.handle.read_state_general_regs()`. To write these values back to the thread, call
    /// `self.handle.write_state_general_regs(self.thread_state.registers.into())`.
    pub registers: RegisterState<T>,

    /// Copy of the current extended processor state including floating point and vector registers.
    pub extended_pstate: ArchExtendedPstateStorage,

    /// The errno code (if any) that indicated this task should restart a syscall.
    pub restart_code: Option<ErrnoCode>,

    /// A custom function to resume a syscall that has been interrupted by SIGSTOP.
    /// To use, call set_syscall_restart_func and return ERESTART_RESTARTBLOCK. sys_restart_syscall
    /// will eventually call it.
    pub syscall_restart_func: Option<Box<SyscallRestartFunc>>,
}

impl<T: RegisterStorage + Default> Default for ThreadState<T> {
    // TODO(https://fxbug.dev/407084069): Implementing default doesn't make much
    // sense - we should only initialize thread state when we know the target
    // architecture and we should initialize for that target specifically.
    fn default() -> Self {
        let registers = RegisterState::<T>::default();
        let extended_pstate = ArchExtendedPstateStorage::with_arch(ArchWidth::Arch64);

        Self { registers, extended_pstate, restart_code: None, syscall_restart_func: None }
    }
}

impl<T: RegisterStorage> ThreadState<T> {
    pub fn arch_width(&self) -> ArchWidth {
        #[cfg(target_arch = "aarch64")]
        {
            return if self.registers.is_arch32() { ArchWidth::Arch32 } else { ArchWidth::Arch64 };
        }
        #[cfg(not(target_arch = "aarch64"))]
        ArchWidth::Arch64
    }

    /// Returns a new `ThreadState` with the same `registers` as this one.
    pub fn snapshot<R: RegisterStorage>(&self) -> ThreadState<R>
    where
        RegisterState<R>: From<RegisterState<T>>,
    {
        ThreadState::<R> {
            registers: self.registers.clone().into(),
            extended_pstate: self.extended_pstate.clone(),
            restart_code: self.restart_code,
            syscall_restart_func: None,
        }
    }

    pub fn extended_snapshot<R: RegisterStorage>(&self) -> ThreadState<R>
    where
        RegisterState<R>: From<RegisterState<T>>,
    {
        ThreadState::<R> {
            registers: self.registers.clone().into(),
            extended_pstate: self.extended_pstate.clone(),
            restart_code: self.restart_code,
            syscall_restart_func: None,
        }
    }

    pub fn replace_registers<O: RegisterStorage>(&mut self, other: &ThreadState<O>) {
        let self_arch = self.arch_width();
        let other_arch = other.arch_width();
        self.registers.copy_from(&other.registers);
        // If we're switching between 32 and 64 bit mode, re-initialize the extended processor state.
        self.extended_pstate = if self_arch == other_arch {
            other.extended_pstate.clone()
        } else {
            ArchExtendedPstateStorage::with_arch(other_arch)
        };
    }

    pub fn get_user_register(&mut self, offset: usize) -> Result<usize, Errno> {
        let mut result: usize = 0;
        self.registers.apply_user_register(offset, &mut |register| result = *register as usize)?;
        Ok(result)
    }

    pub fn set_user_register(&mut self, offset: usize, value: usize) -> Result<(), Errno> {
        let self_arch = self.arch_width();
        let result =
            self.registers.apply_user_register(offset, &mut |register| *register = value as u64);
        // If setting the CPSR register to switch between 32 and 64 bit mode, re-initialize the extended processor state.
        if self_arch != self.arch_width() {
            self.extended_pstate = ArchExtendedPstateStorage::with_arch(self.arch_width());
        }
        result
    }
}

impl From<ThreadState<HeapRegs>> for ThreadState<RegisterStorageEnum> {
    fn from(value: ThreadState<HeapRegs>) -> Self {
        ThreadState {
            registers: value.registers.into(),
            extended_pstate: value.extended_pstate,
            restart_code: value.restart_code,
            syscall_restart_func: value.syscall_restart_func,
        }
    }
}

impl<T: RegisterStorage> ArchSpecific for ThreadState<T> {
    fn is_arch32(&self) -> bool {
        #[cfg(target_arch = "aarch64")]
        return self.registers.is_arch32();
        #[cfg(not(target_arch = "aarch64"))]
        false
    }
}

pub type SyscallRestartFunc =
    dyn FnOnce(&mut CurrentTask) -> Result<SyscallResult, Errno> + Send + Sync;

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;

    #[::fuchsia::test]
    fn test_arch_extended_pstate_storage_sve() {
        let mut pstate = ExtendedPstateState::with_sve(true);
        let qregs = [0x112233445566778899aabbccddeeff00u128; 32];
        let pregs = [0x5a5au16; 16];
        pstate.set_arm64_state(&qregs, 0x10, 0x20);
        pstate.set_arm64_sve_state(&pregs, 0x00ff);

        let mut storage = ArchExtendedPstateStorage::State64(Box::new(pstate));
        assert!(storage.has_sve());

        // `PR_SVE_VL_INHERIT` defaults to off and can be toggled without touching registers.
        assert!(!storage.sve_vl_inherit());
        storage.set_sve_vl_inherit(true);
        assert!(storage.sve_vl_inherit());
        let ArchExtendedPstateStorage::State64(state) = &storage else { unreachable!() };
        assert_eq!(state.get_arm64_qregs(), &qregs);
        assert_eq!(state.get_arm64_p_regs(), &pregs);
        assert_eq!(state.get_arm64_ffr(), 0x00ff);

        // `reset` (used on execve) zeroes all vector and SVE register state while preserving
        // `has_sve` and the `PR_SVE_VL_INHERIT` setting, like Linux's `fpsimd_flush_thread`.
        storage.reset();
        assert!(storage.has_sve());
        assert!(storage.sve_vl_inherit());
        let ArchExtendedPstateStorage::State64(state) = &storage else { unreachable!() };
        assert_eq!(state.get_arm64_qregs(), &[0u128; 32]);
        assert_eq!(state.get_arm64_fpsr(), 0);
        assert_eq!(state.get_arm64_fpcr(), 0);
        assert_eq!(state.get_arm64_p_regs(), &[0u16; 16]);
        assert_eq!(state.get_arm64_ffr(), 0);

        storage.set_sve_vl_inherit(false);
        storage.reset();
        assert!(storage.has_sve());
        assert!(!storage.sve_vl_inherit());

        // 32-bit storage never reports SVE support or vector length inheritance.
        let mut storage32 = ArchExtendedPstateStorage::State32(Box::default());
        assert!(!storage32.has_sve());
        storage32.set_sve_vl_inherit(true);
        assert!(!storage32.sve_vl_inherit());
    }

    #[::fuchsia::test]
    fn test_arch_extended_pstate_storage_reset_with_arch() {
        let mut pstate = ExtendedPstateState::with_sve(true);
        let qregs = [0x112233445566778899aabbccddeeff00u128; 32];
        let pregs = [0x5a5au16; 16];
        pstate.set_arm64_state(&qregs, 0x10, 0x20);
        pstate.set_arm64_sve_state(&pregs, 0x00ff);

        let mut storage = ArchExtendedPstateStorage::State64(Box::new(pstate));
        storage.set_sve_vl_inherit(true);

        // Arch64 -> Arch64 execve resets register state in-place while preserving `has_sve`
        // and `PR_SVE_VL_INHERIT`.
        storage.reset_with_arch(ArchWidth::Arch64);
        assert!(storage.has_sve());
        assert!(storage.sve_vl_inherit());
        let ArchExtendedPstateStorage::State64(state) = &storage else { unreachable!() };
        assert_eq!(state.get_arm64_qregs(), &[0u128; 32]);
        assert_eq!(state.get_arm64_p_regs(), &[0u16; 16]);
        assert_eq!(state.get_arm64_ffr(), 0);

        // Arch64 -> Arch32 execve switches storage variant to `State32`.
        storage.reset_with_arch(ArchWidth::Arch32);
        assert!(matches!(storage, ArchExtendedPstateStorage::State32(_)));
        assert!(!storage.has_sve());
        assert!(!storage.sve_vl_inherit());

        // Arch32 -> Arch32 execve resets `State32` in-place.
        storage.reset_with_arch(ArchWidth::Arch32);
        let ArchExtendedPstateStorage::State32(state32) = &storage else { unreachable!() };
        assert_eq!(state32.get_arm32_qregs(), &[0u128; 16]);
        assert_eq!(state32.get_arm32_fpsr(), 0);
        assert_eq!(state32.get_arm32_fpcr(), 0);

        // Arch32 -> Arch64 execve switches storage variant back to a fresh `State64`.
        storage.reset_with_arch(ArchWidth::Arch64);
        assert!(matches!(storage, ArchExtendedPstateStorage::State64(_)));
        assert!(!storage.sve_vl_inherit());
    }

    /// Returns the address of the boxed 64-bit state, to verify that copies are deep copies.
    fn state64_ptr(storage: &ArchExtendedPstateStorage) -> *const ExtendedPstateState {
        let ArchExtendedPstateStorage::State64(state) = storage else { unreachable!() };
        std::ptr::from_ref(state.as_ref())
    }

    fn assert_state64_regs(
        storage: &ArchExtendedPstateStorage,
        qregs: &[u128; 32],
        fpsr: u32,
        fpcr: u32,
        pregs: &[u16; 16],
        ffr: u16,
        context: &str,
    ) {
        let ArchExtendedPstateStorage::State64(state) = storage else { unreachable!() };
        assert_eq!(state.get_arm64_qregs(), qregs, "{context}");
        assert_eq!(state.get_arm64_fpsr(), fpsr, "{context}");
        assert_eq!(state.get_arm64_fpcr(), fpcr, "{context}");
        assert_eq!(state.get_arm64_p_regs(), pregs, "{context}");
        assert_eq!(state.get_arm64_ffr(), ffr, "{context}");
    }

    #[::fuchsia::test]
    fn test_arch_extended_pstate_storage_clone_and_reset_preserve_sve_flags() {
        let qregs = [0x00112233445566778899aabbccddeeffu128; 32];
        let pregs = [0x0f0fu16; 16];
        // Regardless of whether the host CPU supports SVE, `Clone` (fork/clone), `reset` and
        // `reset_with_arch` (execve) must preserve `has_sve` and `sve_vl_inherit` exactly as set,
        // for every combination of the two flags.
        for has_sve in [false, true] {
            for inherit in [false, true] {
                let context = format!("has_sve={has_sve} inherit={inherit}");
                let mut pstate = ExtendedPstateState::with_sve(has_sve);
                pstate.set_arm64_state(&qregs, 0x10, 0x20);
                pstate.set_arm64_sve_state(&pregs, 0x00ff);
                let mut storage = ArchExtendedPstateStorage::State64(Box::new(pstate));
                storage.set_sve_vl_inherit(inherit);
                assert_eq!(storage.has_sve(), has_sve, "{context}");
                assert_eq!(storage.sve_vl_inherit(), inherit, "{context}");

                // `Clone` deep-copies the flags and every register into a separate allocation.
                let mut cloned = storage.clone();
                assert_eq!(cloned.has_sve(), has_sve, "{context}");
                assert_eq!(cloned.sve_vl_inherit(), inherit, "{context}");
                assert_ne!(state64_ptr(&cloned), state64_ptr(&storage), "{context}");
                assert_state64_regs(&cloned, &qregs, 0x10, 0x20, &pregs, 0x00ff, &context);

                // Mutating the clone leaves the original untouched, and vice versa.
                cloned.set_sve_vl_inherit(!inherit);
                cloned.reset();
                assert_eq!(cloned.has_sve(), has_sve, "{context}");
                assert_eq!(cloned.sve_vl_inherit(), !inherit, "{context}");
                assert_state64_regs(&cloned, &[0u128; 32], 0, 0, &[0u16; 16], 0, &context);
                assert_eq!(storage.has_sve(), has_sve, "{context}");
                assert_eq!(storage.sve_vl_inherit(), inherit, "{context}");
                assert_state64_regs(&storage, &qregs, 0x10, 0x20, &pregs, 0x00ff, &context);

                // `reset` zeroes the registers in place and keeps both flags.
                let ptr_before_reset = state64_ptr(&storage);
                storage.reset();
                assert_eq!(storage.has_sve(), has_sve, "{context}");
                assert_eq!(storage.sve_vl_inherit(), inherit, "{context}");
                assert_eq!(state64_ptr(&storage), ptr_before_reset, "{context}");
                assert_state64_regs(&storage, &[0u128; 32], 0, 0, &[0u16; 16], 0, &context);

                // So does `reset_with_arch(Arch64)` on 64-bit storage (the `finish_exec` path).
                let ArchExtendedPstateStorage::State64(state) = &mut storage else {
                    unreachable!()
                };
                state.set_arm64_state(&qregs, 0x30, 0x40);
                state.set_arm64_sve_state(&pregs, 0x0001);
                storage.reset_with_arch(ArchWidth::Arch64);
                assert_eq!(storage.has_sve(), has_sve, "{context}");
                assert_eq!(storage.sve_vl_inherit(), inherit, "{context}");
                assert_eq!(state64_ptr(&storage), ptr_before_reset, "{context}");
                assert_state64_regs(&storage, &[0u128; 32], 0, 0, &[0u16; 16], 0, &context);
            }
        }
    }

    #[::fuchsia::test]
    fn test_thread_state_snapshot_preserves_sve_state() {
        // `ThreadState::snapshot` is how `clone_task` copies the parent's thread state into the
        // child on fork/clone: the child must inherit `has_sve`, `PR_SVE_VL_INHERIT`, and the live
        // Q0-Q31/FPSR/FPCR/P0-P15/FFR registers, in its own storage.
        let qregs = [0x0123456789abcdef0123456789abcdefu128; 32];
        let pregs = [0x8001u16; 16];
        for has_sve in [false, true] {
            for inherit in [false, true] {
                let context = format!("has_sve={has_sve} inherit={inherit}");
                let mut parent = ThreadState::<HeapRegs>::default();
                let mut pstate = ExtendedPstateState::with_sve(has_sve);
                pstate.set_arm64_state(&qregs, 0x10, 0x20);
                pstate.set_arm64_sve_state(&pregs, 0x0007);
                parent.extended_pstate = ArchExtendedPstateStorage::State64(Box::new(pstate));
                parent.extended_pstate.set_sve_vl_inherit(inherit);

                for mut child in
                    [parent.snapshot::<HeapRegs>(), parent.extended_snapshot::<HeapRegs>()]
                {
                    assert_eq!(child.arch_width(), ArchWidth::Arch64, "{context}");
                    assert_eq!(child.extended_pstate.has_sve(), has_sve, "{context}");
                    assert_eq!(child.extended_pstate.sve_vl_inherit(), inherit, "{context}");
                    assert_ne!(
                        state64_ptr(&child.extended_pstate),
                        state64_ptr(&parent.extended_pstate),
                        "{context}"
                    );
                    assert_state64_regs(
                        &child.extended_pstate,
                        &qregs,
                        0x10,
                        0x20,
                        &pregs,
                        0x0007,
                        &context,
                    );

                    // The child's `PR_SVE_SET_VL` and `execve` do not affect the parent.
                    child.extended_pstate.set_sve_vl_inherit(!inherit);
                    child.extended_pstate.reset_with_arch(child.arch_width());
                    assert_eq!(child.extended_pstate.has_sve(), has_sve, "{context}");
                    assert_eq!(child.extended_pstate.sve_vl_inherit(), !inherit, "{context}");
                    assert_state64_regs(
                        &child.extended_pstate,
                        &[0u128; 32],
                        0,
                        0,
                        &[0u16; 16],
                        0,
                        &context,
                    );
                    assert_eq!(parent.extended_pstate.has_sve(), has_sve, "{context}");
                    assert_eq!(parent.extended_pstate.sve_vl_inherit(), inherit, "{context}");
                    assert_state64_regs(
                        &parent.extended_pstate,
                        &qregs,
                        0x10,
                        0x20,
                        &pregs,
                        0x0007,
                        &context,
                    );
                }

                // Nor does the parent's `execve` affect an existing child.
                let child = parent.snapshot::<HeapRegs>();
                parent.extended_pstate.reset_with_arch(parent.arch_width());
                assert_state64_regs(
                    &child.extended_pstate,
                    &qregs,
                    0x10,
                    0x20,
                    &pregs,
                    0x0007,
                    &context,
                );
                assert_eq!(child.extended_pstate.sve_vl_inherit(), inherit, "{context}");
            }
        }

        // A 32-bit parent forks a 32-bit child, which never reports SVE support.
        let mut parent32 = ThreadState::<HeapRegs>::default();
        let mut regs32 = zx::sys::zx_restricted_state_t::default();
        regs32.cpsr = zx::sys::ZX_REG_CPSR_ARCH_32_MASK as u32;
        parent32.registers.load(regs32);
        parent32.extended_pstate.reset_with_arch(parent32.arch_width());
        assert!(matches!(parent32.extended_pstate, ArchExtendedPstateStorage::State32(_)));
        let child32 = parent32.snapshot::<HeapRegs>();
        assert_eq!(child32.arch_width(), ArchWidth::Arch32);
        assert!(matches!(child32.extended_pstate, ArchExtendedPstateStorage::State32(_)));
        assert!(!child32.extended_pstate.has_sve());
        assert!(!child32.extended_pstate.sve_vl_inherit());
    }

    #[::fuchsia::test]
    fn test_thread_state_finish_exec_cross_arch_and_sve_inheritance() {
        // Mirrors the exact sequence in `CurrentTask::finish_exec`:
        //   self.thread_state.registers.load(regs);
        //   self.thread_state.extended_pstate.reset_with_arch(self.thread_state.arch_width());
        fn simulate_finish_exec(
            thread_state: &mut ThreadState<HeapRegs>,
            regs: zx::sys::zx_restricted_state_t,
        ) {
            thread_state.registers.load(regs);
            thread_state.extended_pstate.reset_with_arch(thread_state.arch_width());
        }

        let qregs = [0x112233445566778899aabbccddeeff00u128; 32];
        let pregs = [0x5a5au16; 16];

        let mut thread_state = ThreadState::<HeapRegs>::default();
        assert_eq!(thread_state.arch_width(), ArchWidth::Arch64);
        assert!(!thread_state.is_arch32());

        let mut pstate = ExtendedPstateState::with_sve(true);
        pstate.set_arm64_state(&qregs, 0x10, 0x20);
        pstate.set_arm64_sve_state(&pregs, 0x00ff);
        thread_state.extended_pstate = ArchExtendedPstateStorage::State64(Box::new(pstate));

        // 1. `PR_SVE_SET_VL_ONEXEC` without `PR_SVE_VL_INHERIT`:
        // Updating `sve_vl_inherit` before `execve` leaves live Q/FPSR/FPCR and SVE P/FFR state
        // intact until `finish_exec` runs `reset_with_arch(ArchWidth::Arch64)`.
        thread_state.extended_pstate.set_sve_vl_inherit(false);
        let state64_box_ptr = match &thread_state.extended_pstate {
            ArchExtendedPstateStorage::State64(state) => {
                assert_eq!(state.get_arm64_qregs(), &qregs);
                assert_eq!(state.get_arm64_p_regs(), &pregs);
                assert_eq!(state.get_arm64_ffr(), 0x00ff);
                std::ptr::from_ref(state.as_ref())
            }
            ArchExtendedPstateStorage::State32(_) => unreachable!(),
        };

        let mut regs64 = zx::sys::zx_restricted_state_t::default();
        regs64.cpsr = 0;
        simulate_finish_exec(&mut thread_state, regs64);
        assert_eq!(thread_state.arch_width(), ArchWidth::Arch64);
        assert!(thread_state.extended_pstate.has_sve());
        assert!(!thread_state.extended_pstate.sve_vl_inherit());
        // SAFETY: `thread_state.extended_pstate` is `State64`.
        let raw_ptr64 =
            unsafe { thread_state.extended_pstate.as_ptr().extended_pstate.cast_const() };
        assert_eq!(raw_ptr64, state64_box_ptr);
        let ArchExtendedPstateStorage::State64(state) = &mut thread_state.extended_pstate else {
            unreachable!()
        };
        assert_eq!(state.get_arm64_qregs(), &[0u128; 32]);
        assert_eq!(state.get_arm64_fpsr(), 0);
        assert_eq!(state.get_arm64_fpcr(), 0);
        assert_eq!(state.get_arm64_p_regs(), &[0u16; 16]);
        assert_eq!(state.get_arm64_ffr(), 0);

        // 2. `PR_SVE_VL_INHERIT` across `Arch64 -> Arch64` `execve`:
        // Register state is zeroed on `finish_exec`, while `has_sve` and `sve_vl_inherit` persist.
        state.set_arm64_state(&qregs, 0x33, 0x44);
        state.set_arm64_sve_state(&pregs, 0x00ff);
        thread_state.extended_pstate.set_sve_vl_inherit(true);
        simulate_finish_exec(&mut thread_state, regs64);
        assert_eq!(thread_state.arch_width(), ArchWidth::Arch64);
        assert!(thread_state.extended_pstate.has_sve());
        assert!(thread_state.extended_pstate.sve_vl_inherit());
        let ArchExtendedPstateStorage::State64(state) = &mut thread_state.extended_pstate else {
            unreachable!()
        };
        assert_eq!(state.get_arm64_qregs(), &[0u128; 32]);
        assert_eq!(state.get_arm64_fpsr(), 0);
        assert_eq!(state.get_arm64_fpcr(), 0);
        assert_eq!(state.get_arm64_p_regs(), &[0u16; 16]);
        assert_eq!(state.get_arm64_ffr(), 0);

        // 3. `Arch64 -> Arch32` `execve` via `registers.load(regs32)` + `reset_with_arch`:
        // Reinitializes `extended_pstate` to `State32`, clearing `has_sve` and `sve_vl_inherit`.
        state.set_arm64_state(&qregs, 0x55, 0x66);
        state.set_arm64_sve_state(&pregs, 0x00ff);
        let mut regs32 = zx::sys::zx_restricted_state_t::default();
        regs32.cpsr = zx::sys::ZX_REG_CPSR_ARCH_32_MASK as u32;
        simulate_finish_exec(&mut thread_state, regs32);
        assert_eq!(thread_state.arch_width(), ArchWidth::Arch32);
        assert!(thread_state.is_arch32());
        assert!(!thread_state.extended_pstate.has_sve());
        assert!(!thread_state.extended_pstate.sve_vl_inherit());
        let state32_box_ptr = match &thread_state.extended_pstate {
            ArchExtendedPstateStorage::State32(state32) => {
                assert_eq!(state32.get_arm32_qregs(), &[0u128; 16]);
                assert_eq!(state32.get_arm32_fpsr(), 0);
                assert_eq!(state32.get_arm32_fpcr(), 0);
                std::ptr::from_ref(state32.as_ref())
            }
            ArchExtendedPstateStorage::State64(_) => unreachable!(),
        };
        // SAFETY: `thread_state.extended_pstate` is `State32`.
        let raw_ptr32 =
            unsafe { thread_state.extended_pstate.as_ptr().extended_aarch32_pstate.cast_const() };
        assert_eq!(raw_ptr32, state32_box_ptr);

        // 4. `Arch32 -> Arch64` `execve` via `registers.load(regs64)` + `reset_with_arch`:
        // Reinitializes `extended_pstate` to a fresh `State64` with `sve_vl_inherit == false`.
        simulate_finish_exec(&mut thread_state, regs64);
        assert_eq!(thread_state.arch_width(), ArchWidth::Arch64);
        assert!(!thread_state.is_arch32());
        assert_eq!(
            thread_state.extended_pstate.has_sve(),
            ExtendedPstateState::default().has_arm64_sve()
        );
        assert!(!thread_state.extended_pstate.sve_vl_inherit());
        let new_state64_box_ptr = match &thread_state.extended_pstate {
            ArchExtendedPstateStorage::State64(state64) => {
                assert_eq!(state64.get_arm64_qregs(), &[0u128; 32]);
                assert_eq!(state64.get_arm64_fpsr(), 0);
                assert_eq!(state64.get_arm64_fpcr(), 0);
                assert_eq!(state64.get_arm64_p_regs(), &[0u16; 16]);
                assert_eq!(state64.get_arm64_ffr(), 0);
                std::ptr::from_ref(state64.as_ref())
            }
            ArchExtendedPstateStorage::State32(_) => unreachable!(),
        };
        // SAFETY: `thread_state.extended_pstate` is `State64`.
        let new_raw_ptr64 =
            unsafe { thread_state.extended_pstate.as_ptr().extended_pstate.cast_const() };
        assert_eq!(new_raw_ptr64, new_state64_box_ptr);
    }
}
