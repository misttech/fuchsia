// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

pub mod apic;
#[cfg(console_enabled)]
pub mod arch;
#[cfg(any(console_enabled, ktest))]
mod arch_ffi;
pub mod bootstrap16;
pub mod cache;
pub mod cpuid;
pub mod fake_msr_access;
pub mod feature;
#[cfg(console_enabled)]
pub mod hwp;
#[cfg(any(console_enabled, ktest))]
mod hwp_ffi;
pub mod interrupts;
pub mod ioapic;
pub mod lapic;
#[cfg(console_enabled)]
pub mod mmu_mem_types;
#[cfg(console_enabled)]
mod mmu_mem_types_ffi;
pub mod mp;
mod mp_ffi;
#[cfg(console_enabled)]
pub mod msr_bench;
#[cfg(console_enabled)]
mod msr_bench_ffi;
pub mod platform_access;
pub mod pv;
pub mod registers;
pub mod system_topology;
pub mod vm;
pub mod x86;

/// Architecture-specific saved normal mode state for x86_64.
///
/// Saves the normal mode `fs_base` and `gs_base` MSR values across restricted mode entry.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArchSavedNormalState {
    pub normal_fs_base: u64,
    pub normal_gs_base: u64,
}

zr::static_assert!(core::mem::size_of::<ArchSavedNormalState>() == 16);
zr::static_assert!(core::mem::align_of::<ArchSavedNormalState>() == 8);

use debug::ltracef;
use zx_status::Status;
use zx_types::{zx_restricted_state_t, zx_status_t, zx_thread_state_general_regs_t};

use arch_x86_aspace_bindings as aspace_bindings;

const LOCAL_TRACE: u32 = 0;

unsafe extern "C" {
    fn cpp_x86_get_fsgsbase_ints_disabled(fsbase: *mut u64, gsbase: *mut u64);
    fn cpp_x86_get_fsgsbase_ints_enabled(fsbase: *mut u64, gsbase: *mut u64);
    fn cpp_x86_set_fsgsbase(fsbase: u64, gsbase: u64);
    fn cpp_x86_enter_uspace(iframe: *const Iframe) -> !;
    fn cpp_x86_get_general_regs(regs: *mut zx_thread_state_general_regs_t) -> i32;
    fn cpp_x86_set_general_regs(regs: *const zx_thread_state_general_regs_t) -> i32;

    fn cpp_io_bitmap_get_current() -> *mut IoBitmap;
    fn cpp_io_bitmap_set_io_bitmap(
        bitmap: *mut IoBitmap,
        port: u32,
        len: u32,
        enable: bool,
    ) -> zx_status_t;
}

/// Opaque Rust facade for the C++ `IoBitmap` class (`arch/x86/ioport.h`).
#[repr(C)]
pub struct IoBitmap {
    _facade: zr::OpaqueFacade,
}

impl IoBitmap {
    /// Invokes `f` with a pinned reference to the [`IoBitmap`] associated with the current
    /// thread's active address space, or returns `None` if the thread has no associated
    /// [`IoBitmap`] (such as idle threads).
    #[inline]
    pub fn with_current<R>(f: impl FnOnce(core::pin::Pin<&IoBitmap>) -> R) -> Option<R> {
        // SAFETY: `cpp_io_bitmap_get_current` returns the current thread's `IoBitmap*` (pinned
        // inside its active `VmAspace`) or null if the thread has no active user address space.
        let ptr = core::ptr::NonNull::new(unsafe { cpp_io_bitmap_get_current() })?;
        // SAFETY: `ptr` points to the current thread's active `VmAspace`'s `IoBitmap`, which
        // remains pinned, alive, and attached for the duration of `f` (detaching or switching
        // the current thread's active `VmAspace` is `unsafe`).
        Some(f(unsafe { core::pin::Pin::new_unchecked(ptr.as_ref()) }))
    }

    /// Sets or clears I/O port permissions `[port, port + len)` in this `IoBitmap`.
    #[inline]
    pub fn set_io_bitmap(
        self: core::pin::Pin<&Self>,
        port: u32,
        len: u32,
        enable: bool,
    ) -> Result<(), Status> {
        // SAFETY: `self` is a valid `IoBitmap` facade reference; `SetIoBitmap` synchronizes via
        // its internal spinlock.
        Status::ok(unsafe {
            cpp_io_bitmap_set_io_bitmap(
                core::ptr::from_ref(self.get_ref()).cast_mut(),
                port,
                len,
                enable,
            )
        })
    }
}

pub const MAX_CACHE_LINE: usize = 64;
#[repr(align(64))]
pub struct CpuAlignMarker;

/// The mask of all RFLAGS bits that user code is permitted to set or modify:
/// * CF (Carry Flag): bit 0 (`0x1`)
/// * PF (Parity Flag): bit 2 (`0x4`)
/// * AF (Auxiliary Carry Flag): bit 4 (`0x10`)
/// * ZF (Zero Flag): bit 6 (`0x40`)
/// * SF (Sign Flag): bit 7 (`0x80`)
/// * TF (Trap Flag): bit 8 (`0x100`)
/// * DF (Direction Flag): bit 10 (`0x400`)
/// * OF (Overflow Flag): bit 11 (`0x800`)
/// * NT (Nested Task Flag): bit 14 (`0x4000`)
/// * AC (Alignment Check Flag): bit 18 (`0x40000`)
/// * ID (Identification Flag): bit 21 (`0x200000`)
///
/// Combining these status, control, and system flags yields:
/// `0x1 | 0x4 | 0x10 | 0x40 | 0x80 | 0x100 | 0x400 | 0x800 | 0x4000 | 0x40000 | 0x200000 = 0x244dd5`.
///
/// See [intel/vol1]: Figure 3-8. EFLAGS Register, and
/// [amd/vol1]: Figure 3-7. RFLAGS Register.
const X86_FLAGS_USER: u64 = 0x244dd5;

/// RFLAGS Interrupt Enable Flag (IF), bit 9 (`1 << 9`).
///
/// See [intel/vol1]: Figure 3-8. EFLAGS Register, and
/// [amd/vol1]: Figure 3-7. RFLAGS Register.
const X86_FLAGS_IF: u64 = 1 << 9;

/// User 64-bit code segment selector (GDT index 6 with Requested Privilege Level 3: `(6 << 3) | 3 = 0x33`).
///
/// See [intel/vol3]: 3.4.2 Segment Selectors, and
/// [amd/vol2]: 4.5.1 Segment Selectors.
const USER_CODE_64_SELECTOR: u64 = 0x30 | 3;

/// User data segment selector (GDT index 5 with Requested Privilege Level 3: `(5 << 3) | 3 = 0x2b`).
///
/// See [intel/vol3]: 3.4.2 Segment Selectors, and
/// [amd/vol2]: 4.5.1 Segment Selectors.
const USER_DATA_SELECTOR: u64 = 0x28 | 3;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Iframe {
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rax: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub vector: u64,
    pub err_code: u64,
    pub ip: u64,
    pub cs: u64,
    pub flags: u64,
    pub user_sp: u64,
    pub user_ss: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct SyscallRegs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
    pub rsp: u64,
}

zr::static_assert!(core::mem::size_of::<Iframe>() == 176);
zr::static_assert!(core::mem::align_of::<Iframe>() == 8);
zr::static_assert!(core::mem::offset_of!(Iframe, rdi) == 0);
zr::static_assert!(core::mem::offset_of!(Iframe, rsi) == 8);
zr::static_assert!(core::mem::offset_of!(Iframe, rbp) == 16);
zr::static_assert!(core::mem::offset_of!(Iframe, rbx) == 24);
zr::static_assert!(core::mem::offset_of!(Iframe, rdx) == 32);
zr::static_assert!(core::mem::offset_of!(Iframe, rcx) == 40);
zr::static_assert!(core::mem::offset_of!(Iframe, rax) == 48);
zr::static_assert!(core::mem::offset_of!(Iframe, r8) == 56);
zr::static_assert!(core::mem::offset_of!(Iframe, r9) == 64);
zr::static_assert!(core::mem::offset_of!(Iframe, r10) == 72);
zr::static_assert!(core::mem::offset_of!(Iframe, r11) == 80);
zr::static_assert!(core::mem::offset_of!(Iframe, r12) == 88);
zr::static_assert!(core::mem::offset_of!(Iframe, r13) == 96);
zr::static_assert!(core::mem::offset_of!(Iframe, r14) == 104);
zr::static_assert!(core::mem::offset_of!(Iframe, r15) == 112);
zr::static_assert!(core::mem::offset_of!(Iframe, vector) == 120);
zr::static_assert!(core::mem::offset_of!(Iframe, err_code) == 128);
zr::static_assert!(core::mem::offset_of!(Iframe, ip) == 136);
zr::static_assert!(core::mem::offset_of!(Iframe, cs) == 144);
zr::static_assert!(core::mem::offset_of!(Iframe, flags) == 152);
zr::static_assert!(core::mem::offset_of!(Iframe, user_sp) == 160);
zr::static_assert!(core::mem::offset_of!(Iframe, user_ss) == 168);

zr::static_assert!(core::mem::size_of::<SyscallRegs>() == 144);
zr::static_assert!(core::mem::align_of::<SyscallRegs>() == 8);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rax) == 0);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rbx) == 8);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rcx) == 16);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rdx) == 24);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rsi) == 32);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rdi) == 40);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rbp) == 48);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r8) == 56);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r9) == 64);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r10) == 72);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r11) == 80);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r12) == 88);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r13) == 96);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r14) == 104);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r15) == 112);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rip) == 120);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rflags) == 128);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rsp) == 136);

/// Entry state for a thread.
#[allow(unused_imports)]
pub use arch_types_bindings::{GeneralRegsSource, UserEntryState};

/// Virtual address where the kernel address space begins.
/// Below this is the user address space.
pub const KERNEL_ASPACE_BASE: usize = 0xffffff8000000000; // -512GB
zr::static_assert!(KERNEL_ASPACE_BASE == aspace_bindings::KERNEL_ASPACE_BASE as usize);

/// Virtual address where the kernel address space begins.
/// Below this is the user address space.
pub const KERNEL_ASPACE_SIZE: usize = 0x0000008000000000;
zr::static_assert!(KERNEL_ASPACE_SIZE == aspace_bindings::KERNEL_ASPACE_SIZE as usize);

/// Virtual address where the user-accessible address space begins.
/// Below this is wholly inaccessible.
pub const USER_ASPACE_BASE: usize = 0x0000000000200000; // 2MB
zr::static_assert!(USER_ASPACE_BASE == aspace_bindings::USER_ASPACE_BASE as usize);

/// We set the top of user address space to be (1 << 47) - 4k.
/// See //docs/concepts/kernel/sysret_problem.md for why we subtract 4k here.
pub const USER_ASPACE_SIZE: usize = (1usize << 47) - 4096 - USER_ASPACE_BASE;
zr::static_assert!(USER_ASPACE_SIZE == aspace_bindings::USER_ASPACE_SIZE as usize);

/// Size of the restricted mode address space in unified address spaces.
/// We set the top of the restricted aspace to exactly halfway through the PML4.
pub const USER_RESTRICTED_ASPACE_SIZE: usize = (1usize << 46) - USER_ASPACE_BASE;
zr::static_assert!(
    USER_RESTRICTED_ASPACE_SIZE == aspace_bindings::USER_RESTRICTED_ASPACE_SIZE as usize
);

pub const MMU_GUEST_SIZE_SHIFT: usize = aspace_bindings::MMU_GUEST_SIZE_SHIFT as usize;

/// Validates the x86_64 register state before entering restricted mode.
pub fn validate_state_pre_restricted_entry(state: &zx_restricted_state_t) -> Result<(), Status> {
    // validate that RIP is within user space
    if !vm::is_user_accessible(state.ip as usize) {
        ltracef!("fail due to bad ip {:#x}\n", state.ip);
        return Err(Status::BAD_STATE);
    }

    // validate that the rflags saved only contain user settable flags
    if (state.flags & !X86_FLAGS_USER) != 0 {
        ltracef!("fail due to flags outside of X86_FLAGS_USER set ({:#x})\n", state.flags);
        return Err(Status::BAD_STATE);
    }

    // fs and gs base must be canonical
    if !vm::is_vaddr_canonical(state.fs_base as usize) {
        ltracef!("fail due to bad fs base {:#x}\n", state.fs_base);
        return Err(Status::BAD_STATE);
    }
    if !vm::is_vaddr_canonical(state.gs_base as usize) {
        ltracef!("fail due to bad gs base {:#x}\n", state.gs_base);
        return Err(Status::BAD_STATE);
    }

    // everything else can be whatever value it wants to be. worst case it immediately faults
    // in restricted mode and that's okay.
    Ok(())
}

pub fn dump(state: &zx_restricted_state_t) {
    use core::fmt::Write;
    use debug::ltrace::KernelConsoleWriter;
    let mut w = KernelConsoleWriter;
    let _ = write!(
        w,
        " RIP: {:#18x}  FL: {:#18x}\n RAX: {:#18x} RBX: {:#18x} RCX: {:#18x} RDX: {:#18x}\n RSI: {:#18x} RDI: {:#18x} RBP: {:#18x} RSP: {:#18x}\n  R8: {:#18x}  R9: {:#18x} R10: {:#18x} R11: {:#18x}\n R12: {:#18x} R13: {:#18x} R14: {:#18x} R15: {:#18x}\nfs base {:#18x} gs base {:#18x}\n",
        state.ip,
        state.flags,
        state.rax,
        state.rbx,
        state.rcx,
        state.rdx,
        state.rsi,
        state.rdi,
        state.rbp,
        state.rsp,
        state.r8,
        state.r9,
        state.r10,
        state.r11,
        state.r12,
        state.r13,
        state.r14,
        state.r15,
        state.fs_base,
        state.gs_base,
    );
}

pub fn save_state_pre_restricted_entry(state: &mut ArchSavedNormalState) {
    // SAFETY: Reads fs/gs base registers for the current thread.
    unsafe {
        cpp_x86_get_fsgsbase_ints_disabled(&mut state.normal_fs_base, &mut state.normal_gs_base);
    }
}

pub fn enter_restricted(state: &zx_restricted_state_t) -> ! {
    // SAFETY: Sets fs/gs base registers for the current thread.
    unsafe {
        cpp_x86_set_fsgsbase(state.fs_base, state.gs_base);
    }

    let iframe = Iframe {
        rdi: state.rdi,
        rsi: state.rsi,
        rbp: state.rbp,
        rbx: state.rbx,
        rdx: state.rdx,
        rcx: state.rcx,
        rax: state.rax,
        r8: state.r8,
        r9: state.r9,
        r10: state.r10,
        r11: state.r11,
        r12: state.r12,
        r13: state.r13,
        r14: state.r14,
        r15: state.r15,
        vector: 0,
        err_code: 0,
        ip: state.ip,
        cs: USER_CODE_64_SELECTOR,
        flags: state.flags | X86_FLAGS_IF,
        user_sp: state.rsp,
        user_ss: USER_DATA_SELECTOR,
    };

    // SAFETY: Enters user space using the constructed iframe. Does not return.
    unsafe {
        cpp_x86_enter_uspace(&iframe);
    }
}

pub fn save_restricted_syscall_state(state: &mut zx_restricted_state_t, regs: &SyscallRegs) {
    state.rdi = regs.rdi;
    state.rsi = regs.rsi;
    state.rbp = regs.rbp;
    state.rbx = regs.rbx;
    state.rdx = regs.rdx;
    state.rcx = regs.rcx;
    state.rax = regs.rax;
    state.rsp = regs.rsp;
    state.r8 = regs.r8;
    state.r9 = regs.r9;
    state.r10 = regs.r10;
    state.r11 = regs.r11;
    state.r12 = regs.r12;
    state.r13 = regs.r13;
    state.r14 = regs.r14;
    state.r15 = regs.r15;
    state.ip = regs.rip;
    state.flags = regs.rflags & X86_FLAGS_USER;

    // SAFETY: Reads fs/gs base registers for the current thread.
    unsafe {
        cpp_x86_get_fsgsbase_ints_disabled(&mut state.fs_base, &mut state.gs_base);
    }
}

pub fn save_restricted_iframe_state(state: &mut zx_restricted_state_t, frame: &Iframe) {
    state.rdi = frame.rdi;
    state.rsi = frame.rsi;
    state.rbp = frame.rbp;
    state.rbx = frame.rbx;
    state.rdx = frame.rdx;
    state.rcx = frame.rcx;
    state.rax = frame.rax;
    state.r8 = frame.r8;
    state.r9 = frame.r9;
    state.r10 = frame.r10;
    state.r11 = frame.r11;
    state.r12 = frame.r12;
    state.r13 = frame.r13;
    state.r14 = frame.r14;
    state.r15 = frame.r15;
    state.ip = frame.ip;
    state.flags = frame.flags & X86_FLAGS_USER;
    state.rsp = frame.user_sp;

    // SAFETY: Reads fs/gs base registers for the current thread.
    unsafe {
        cpp_x86_get_fsgsbase_ints_disabled(&mut state.fs_base, &mut state.gs_base);
    }
}

pub fn save_restricted_exception_state(state: &mut zx_restricted_state_t) {
    let mut regs = zx_thread_state_general_regs_t::default();
    // SAFETY: Gets general registers of the current thread.
    let status = unsafe { cpp_x86_get_general_regs(&mut regs) };
    assert_eq!(Status::ok(status), Ok(()));

    state.rdi = regs.rdi;
    state.rsi = regs.rsi;
    state.rbp = regs.rbp;
    state.rbx = regs.rbx;
    state.rdx = regs.rdx;
    state.rcx = regs.rcx;
    state.rax = regs.rax;
    state.rsp = regs.rsp;
    state.r8 = regs.r8;
    state.r9 = regs.r9;
    state.r10 = regs.r10;
    state.r11 = regs.r11;
    state.r12 = regs.r12;
    state.r13 = regs.r13;
    state.r14 = regs.r14;
    state.r15 = regs.r15;
    state.ip = regs.rip;
    state.flags = regs.rflags & X86_FLAGS_USER;

    // SAFETY: Reads fs/gs base registers for the current thread.
    unsafe {
        cpp_x86_get_fsgsbase_ints_enabled(&mut state.fs_base, &mut state.gs_base);
    }
}

pub fn redirect_restricted_exception_to_normal(
    arch_state: &ArchSavedNormalState,
    vector_table: usize,
    context: usize,
    reason: u64,
) {
    let regs = zx_thread_state_general_regs_t {
        rax: 0,
        rbx: 0,
        rcx: 0,
        rdx: 0,
        rsi: reason,
        rdi: context as u64,
        rbp: 0,
        rsp: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        rip: vector_table as u64,
        rflags: X86_FLAGS_IF,
        fs_base: arch_state.normal_fs_base,
        gs_base: arch_state.normal_gs_base,
    };
    // SAFETY: Sets general registers on current thread.
    let status = unsafe { cpp_x86_set_general_regs(&regs) };
    assert_eq!(Status::ok(status), Ok(()));
}

pub fn enter_full(
    arch_state: &ArchSavedNormalState,
    vector_table: usize,
    context: usize,
    code: u64,
) -> ! {
    // SAFETY: Restores user fs/gs base from normal mode.
    unsafe {
        cpp_x86_set_fsgsbase(arch_state.normal_fs_base, arch_state.normal_gs_base);
    }

    let iframe = Iframe {
        rdi: context as u64,
        rsi: code,
        rbp: 0,
        rbx: 0,
        rdx: 0,
        rcx: 0,
        rax: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        vector: 0,
        err_code: 0,
        ip: vector_table as u64,
        cs: USER_CODE_64_SELECTOR,
        flags: X86_FLAGS_IF,
        user_sp: 0,
        user_ss: USER_DATA_SELECTOR,
    };

    // SAFETY: Enters user space using constructed iframe. Does not return.
    unsafe {
        cpp_x86_enter_uspace(&iframe);
    }
}

#[cfg(ktest)]
/// Architecture unit tests for x86.
#[unittest::suite(name = "x86")]
mod x86_tests {
    use super::vm::{is_kernel_address, is_user_accessible, is_valid_user_pc};
    use unittest::{assert_err, assert_false, assert_ok, assert_true};

    /// Tests `is_user_accessible`.
    #[test]
    fn test_is_user_accessible() {
        assert_true!(is_user_accessible(0x00000000_00100000));
        assert_true!(is_user_accessible(0x00007fff_ffffffff));
        assert_false!(is_user_accessible(0x00008000_00000000));
        assert_false!(is_user_accessible(0xffff8000_00000000));
    }

    /// Tests `validate_state_pre_restricted_entry`.
    #[test]
    fn test_validate_state_pre_restricted_entry() {
        let state = zx_restricted_state_t { ip: 0x1000, ..Default::default() };
        assert_ok!(validate_state_pre_restricted_entry(&state));

        let state_bad = zx_restricted_state_t { ip: 0xffff_8000_0000_0000, ..Default::default() };
        assert_err!(validate_state_pre_restricted_entry(&state_bad), Status::BAD_STATE);
    }

    /// Tests `dump`.
    #[test]
    fn test_dump() {
        let state = zx_restricted_state_t::default();
        dump(&state);
    }

    /// Tests `is_kernel_address`.
    #[test]
    fn test_is_kernel_address() {
        assert_true!(is_kernel_address(KERNEL_ASPACE_BASE));
        assert_true!(is_kernel_address(KERNEL_ASPACE_BASE + 0x1000));
        assert_true!(is_kernel_address(usize::MAX));
        assert_false!(is_kernel_address(0));
        assert_false!(is_kernel_address(0x1000));
        assert_false!(is_kernel_address(0x0000_7fff_ffff_ffff));
        assert_false!(is_kernel_address(KERNEL_ASPACE_BASE - 1));
    }

    /// Tests `is_valid_user_pc`.
    #[test]
    fn test_is_valid_user_pc() {
        // Null pointer is valid (used for threads intended to fault).
        assert_true!(is_valid_user_pc(0));
        // Valid userspace addresses.
        assert_true!(is_valid_user_pc(0x1000));
        assert_true!(is_valid_user_pc(0x0000_7fff_ffff_0000));
        // Non-canonical address.
        assert_false!(is_valid_user_pc(0x0000_8000_0000_0000));
        // Kernel address.
        assert_false!(is_valid_user_pc(KERNEL_ASPACE_BASE));
        assert_false!(is_valid_user_pc(0xffff_8000_0000_0000));
    }
}

#[cfg(ktest)]
/// Hardware P-states (HWP) unit tests.
#[unittest::suite(name = "x86_hwp_rust")]
mod x86_hwp_tests {
    use super::hwp_ffi::cpp_x86_intel_hwp_parse_policy;
    use boot_options::IntelHwpPolicy;
    use unittest::{assert_true, expect_eq, expect_false};

    /// TestParsePolicy
    #[test]
    fn test_parse_policy() {
        let mut policy: u8 = 0;

        // Valid parse.
        // SAFETY: `c"bios-specified"` is a valid NUL-terminated string and `policy` is valid.
        assert_true!(unsafe {
            cpp_x86_intel_hwp_parse_policy(c"bios-specified".as_ptr(), &mut policy)
        });
        expect_eq!(policy, IntelHwpPolicy::BiosSpecified as u8);

        // Invalid parses.
        // SAFETY: `cpp_x86_intel_hwp_parse_policy` handles null and NUL-terminated C strings.
        unsafe {
            expect_false!(cpp_x86_intel_hwp_parse_policy(core::ptr::null(), &mut policy));
            expect_false!(cpp_x86_intel_hwp_parse_policy(c"".as_ptr(), &mut policy));
            expect_false!(cpp_x86_intel_hwp_parse_policy(c"invalid".as_ptr(), &mut policy));
            expect_false!(cpp_x86_intel_hwp_parse_policy(c"\n".as_ptr(), &mut policy));
        }
    }
}

#[cfg(ktest)]
/// x64 platform tests.
#[unittest::suite(name = "x64_platform_tests_rust")]
mod x64_platform_tests {
    use super::apic::ApicInterruptDeliveryMode;
    use super::arch_ffi::{cpp_x86_read_msr_on_cpu, cpp_x86_write_msr_on_cpu};
    use super::feature::{
        X86_FEATURE_RDTSCP, X86HypervisorList, x86_feature_test, x86_get_hypervisor,
    };
    use super::hwp_ffi::{cpp_x86_intel_hwp_init, cpp_x86_intel_hwp_supported};
    use super::interrupts::X86_INT_NMI;
    use super::lapic::apic_send_mask_ipi;
    use super::registers::{X86_MSR_IA32_FMASK, X86_MSR_IA32_LSTAR, X86_MSR_IA32_TSC_AUX};
    use super::x86::{read_msr, write_msr};
    use crate::console::console_run_script_locked;
    use crate::kernel::deadline::{Deadline, TimerSlack};
    use crate::kernel::mp::{MpIpiTarget, is_cpu_online, sync_exec_raw};
    use crate::kernel::scheduler::peek_active_mask;
    use crate::kernel::types::cpu_num_to_mask;
    use crate::platform_rs::timer::{DurationMono, current_mono_time};
    use boot_options::BootOptions;
    use kprint::kprintln;
    use unittest::{expect_eq, expect_ne};
    use zx_status::sys::ZX_OK;

    unsafe extern "C" fn rdtscp_aux(context: *mut core::ffi::c_void) {
        // SAFETY: `context` points to a valid `u32` on the caller's stack.
        let aux = unsafe { &mut *(context as *mut u32) };
        let aux_msr: u32;
        // SAFETY: `rdtscp` reads the timestamp counter and IA32_TSC_AUX into eax, edx, ecx.
        unsafe {
            core::arch::asm!(
                "rdtscp",
                out("eax") _,
                out("edx") _,
                out("ecx") aux_msr,
                options(nostack, preserves_flags),
            );
        }
        *aux = aux_msr;
    }

    /// basic test of read/write MSR variants
    #[test]
    fn test_x64_msrs() {
        let int_state = crate::arch_rs::arch_interrupt_save();
        // Test read_msr for an MSR that is known to always exist on x64.
        // SAFETY: `X86_MSR_IA32_LSTAR` is an architectural MSR that always exists on x86_64.
        let val = unsafe { read_msr(X86_MSR_IA32_LSTAR) };
        expect_ne!(val, 0u64);

        // Test write_msr to write that value back.
        // SAFETY: Writing back the current value of `X86_MSR_IA32_LSTAR` with interrupts disabled.
        unsafe { write_msr(X86_MSR_IA32_LSTAR, val) };
        crate::arch_rs::arch_interrupt_restore(int_state);

        // Test read_msr_safe for an MSR that is known to not exist.
        // If read_msr_safe is busted, then this will #GP (panic).
        // TODO: Enable when the QEMU TCG issue is sorted (TCG never
        // generates a #GP on MSR access).

        // Test read_msr_on_cpu.
        // SAFETY: `X86_MSR_IA32_FMASK` is an architectural MSR that always exists on x86_64.
        let initial_fmask = unsafe { read_msr(X86_MSR_IA32_FMASK) };
        for i in 0..crate::arch_rs::max_num_cpus() {
            if !is_cpu_online(i) {
                continue;
            }
            // SAFETY: CPU `i` is online and `X86_MSR_IA32_FMASK` is a valid MSR.
            let fmask = unsafe { cpp_x86_read_msr_on_cpu(i, X86_MSR_IA32_FMASK) };
            expect_eq!(initial_fmask, fmask);
        }

        // Test write_msr_on_cpu
        for i in 0..crate::arch_rs::max_num_cpus() {
            if !is_cpu_online(i) {
                continue;
            }
            // SAFETY: CPU `i` is online and writing back `initial_fmask` preserves state.
            unsafe { cpp_x86_write_msr_on_cpu(i, X86_MSR_IA32_FMASK, initial_fmask) };
        }

        // If RDTSCP is supported, check that the TSC_AUX MSR is correctly programmed.
        if x86_feature_test(X86_FEATURE_RDTSCP) {
            for i in 0..crate::arch_rs::max_num_cpus() {
                if !is_cpu_online(i) {
                    continue;
                }
                // SAFETY: CPU `i` is online and `X86_MSR_IA32_TSC_AUX` is supported when RDTSCP is
                // present.
                let cpuid = unsafe { cpp_x86_read_msr_on_cpu(i, X86_MSR_IA32_TSC_AUX) };
                expect_eq!(cpuid, i as u64);

                let mut aux: u32 = 0;
                let mut mask = 0;
                mask |= cpu_num_to_mask(i);
                // SAFETY: `rdtscp_aux` is a valid callback and `&raw mut aux` is valid for the
                // duration of the synchronous call.
                unsafe {
                    sync_exec_raw(MpIpiTarget::Mask, mask, rdtscp_aux, (&raw mut aux).cast());
                }
                expect_eq!(cpuid, aux as u64);
            }
        }
    }

    /// test k cpu rdmsr commands
    #[test]
    fn test_x64_msrs_k_commands() {
        console_run_script_locked("cpu rdmsr 0 0x10");
    }

    /// test k hwp commands
    #[test]
    fn test_x64_hwp_k_commands() {
        // Don't test at all if HWP disabled on the command line.
        if !BootOptions::get().x86_hwp {
            return true;
        }

        // If we don't support HWP, expect every command to just return "not supported".
        // SAFETY: `cpp_x86_intel_hwp_supported` queries CPUID without side effects.
        if !unsafe { cpp_x86_intel_hwp_supported() } {
            expect_eq!(Status::NOT_SUPPORTED.into_raw(), console_run_script_locked("hwp"));
            return true;
        }

        // Test top-level parsing.
        expect_eq!(Status::INVALID_ARGS.into_raw(), console_run_script_locked("hwp"));
        expect_eq!(Status::INVALID_ARGS.into_raw(), console_run_script_locked("hwp invalid"));
        expect_eq!(Status::INVALID_ARGS.into_raw(), console_run_script_locked("hwp 3"));

        // Set policy.
        expect_eq!(Status::INVALID_ARGS.into_raw(), console_run_script_locked("hwp set-policy"));
        expect_eq!(
            Status::INVALID_ARGS.into_raw(),
            console_run_script_locked("hwp set-policy invalid-policy")
        );
        expect_eq!(Status::INVALID_ARGS.into_raw(), console_run_script_locked("hwp set-policy 3"));
        expect_eq!(
            Status::INVALID_ARGS.into_raw(),
            console_run_script_locked("hwp set-policy performance 42")
        );
        expect_eq!(ZX_OK, console_run_script_locked("hwp set-policy performance"));
        expect_eq!(ZX_OK, console_run_script_locked("hwp set-policy power-save"));

        // Set Freq
        expect_eq!(Status::INVALID_ARGS.into_raw(), console_run_script_locked("hwp set-freq"));
        expect_eq!(Status::INVALID_ARGS.into_raw(), console_run_script_locked("hwp set-freq 0"));
        expect_eq!(Status::INVALID_ARGS.into_raw(), console_run_script_locked("hwp set-freq 256"));
        expect_eq!(
            Status::INVALID_ARGS.into_raw(),
            console_run_script_locked("hwp set-freq 10 10")
        );
        expect_eq!(ZX_OK, console_run_script_locked("hwp set-freq 100"));
        expect_eq!(ZX_OK, console_run_script_locked("hwp set-freq 255"));

        // Restore the policy to default.
        // SAFETY: `cpp_x86_intel_hwp_init` reinitializes HWP policy using the boot option value.
        unsafe {
            cpp_x86_intel_hwp_init(BootOptions::get().x86_hwp_policy as u8);
        }
    }

    // TODO(https://fxbug.dev/42169592): Unconditionally re-enable this test when
    // https://fxbug.dev/42169592 has been resolved.
    //
    // Spam all the CPUs with NMIs, see that nothing bad happens.
    /// test nmi spam
    #[test]
    fn test_nmi_spam() {
        if x86_get_hypervisor() == X86HypervisorList::Kvm {
            kprintln!("skipping test, see https://fxbug.dev/42169592");
            return true;
        }

        const DURATION: DurationMono = DurationMono::from_millis(100);
        let deadline = Deadline::after_mono(DURATION, TimerSlack::none());
        while {
            apic_send_mask_ipi(X86_INT_NMI.0, peek_active_mask(), ApicInterruptDeliveryMode::NMI);
            current_mono_time().0 < deadline.when().0
        } {}
    }
}
