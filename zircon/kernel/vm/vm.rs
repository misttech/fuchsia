// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::vm_address_region::VmAddressRegion;
use crate::kernel::types::PAddr;
use core::ffi::{CStr, c_char, c_void};
use core::pin::Pin;
use lazy_init::{LazyInit, NoCheck};
use vm_bindings as bindings;

unsafe extern "C" {
    fn cpp_vaddr_to_paddr(va: *const c_void) -> PAddr;
}

// While this method is implemented using FFI clippy cannot observer that the pointer is not
// de-referenced and so for now squash this lint.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
/// Converts a kernel virtual address to a physical address.
pub fn vaddr_to_paddr(va: *const c_void) -> PAddr {
    unsafe { cpp_vaddr_to_paddr(va) }
}

/// Return the physical address of the zero page.
#[inline]
pub fn vm_get_zero_page_paddr() -> PAddr {
    // SAFETY: `zero_page_paddr` is written once during single-threaded early boot in `vm_init`
    // before interrupts or secondary CPUs are enabled, and is never modified afterward.
    PAddr(unsafe { bindings::zero_page_paddr })
}

/// Evaluates whether VM ktracing is enabled at the requested level.
#[macro_export]
macro_rules! vm_ktrace_level_enabled {
    (1) => {
        cfg!(vm_tracing_level_at_least_1)
    };
    (2) => {
        cfg!(vm_tracing_level_at_least_2)
    };
    (3) => {
        cfg!(vm_tracing_level_at_least_3)
    };
}
pub use vm_ktrace_level_enabled;

/// Creates a scoped VM ktrace duration if tracing is enabled at `$level`.
#[macro_export]
macro_rules! vm_ktrace_duration {
    ($level:tt, $label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        let _duration = $crate::ktrace_rs::begin_scope_cond!(
            $crate::vm::vm::vm_ktrace_level_enabled!($level),
            "kernel:vm",
            $label
            $(, $key => $val)*
        );
    };
}
pub use vm_ktrace_duration;

#[macro_export]
macro_rules! vm_ktrace_duration_begin {
    ($level:tt, $label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        if $crate::vm::vm::vm_ktrace_level_enabled!($level) {
            $crate::ktrace_rs::duration_begin!("kernel:vm", $label $(, $key => $val)*);
        }
    };
}
pub use vm_ktrace_duration_begin;

#[macro_export]
macro_rules! vm_ktrace_duration_end {
    ($level:tt, $label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        if $crate::vm::vm::vm_ktrace_level_enabled!($level) {
            $crate::ktrace_rs::duration_end!("kernel:vm", $label $(, $key => $val)*);
        }
    };
}
pub use vm_ktrace_duration_end;

// In builds with verbose VM tracing enabled (VM_TRACING_LEVEL >= 1), we emit these events under the
// "kernel:vm" category at the specified VM tracing level. In standard/production builds
// (VM_TRACING_LEVEL = 0), we fallback to emitting under the lightweight "kernel:oom" category
// instead.
//
// The general expectation is that users will enable "kernel:oom" in cases where "kernel:vm" is
// too expensive. Using a compile-time selection here prevents duplicate overlapping slices in
// the trace visualizer when both categories are enabled at runtime, while still providing a
// single category for developers opting for more verbose VM tracing.
#[cfg(vm_tracing_level_at_least_1)]
pub use vm_ktrace_duration as oom_ktrace_duration;
#[cfg(not(vm_tracing_level_at_least_1))]
#[macro_export]
macro_rules! oom_ktrace_duration {
    ($level:tt, $label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        let _duration = $crate::ktrace_rs::begin_scope_cond!(
            $crate::vm::vm::vm_ktrace_level_enabled!($level),
            "kernel:oom",
            $label
            $(, $key => $val)*
        );
    };
}
#[cfg(not(vm_tracing_level_at_least_1))]
pub use oom_ktrace_duration;

/// Declare storage for the kernel's heap VMAR. Maybe unused if virtual heap is disabled.
static KERNEL_HEAP_VMAR: LazyInit<VmAddressRegion, NoCheck> = LazyInit::uninit();

/// # Safety
///
/// Must be called at most once, before any other use of the kernel heap region. The last
/// reference to the returned region must never be released. `name` points to a null-terminated
/// string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_kernel_heap_init(
    parent: &VmAddressRegion,
    base: usize,
    size: usize,
    vmar_flags: u32,
    name: *const c_char,
) -> &'static VmAddressRegion {
    // SAFETY: `name` points to a null-terminated string.
    let name = unsafe { CStr::from_ptr(name) };
    let init = VmAddressRegion::new_child(parent, base, size, vmar_flags, name);
    // SAFETY: `KERNEL_HEAP_VMAR` is initialized at most once, before any other use.
    let Ok(()) = unsafe { Pin::static_ref(&KERNEL_HEAP_VMAR).init_pin(init) };
    // SAFETY: `KERNEL_HEAP_VMAR` is initialized.
    unsafe { KERNEL_HEAP_VMAR.get_unchecked() }
}

/// Kernel console `vm` command implementation.
#[cfg(console_enabled)]
mod console {
    use crate::arch_rs::vm::is_kernel_address;
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use crate::kernel::types::PAddr;
    use crate::vm::arch_vm_aspace::{ARCH_UNMAP_OPTION_NONE, ArchMmuFlags};
    use crate::vm::physmap::{is_physmap_phys_addr, paddr_to_physmap};
    use crate::vm::vm_aspace::VmAspace;
    use core::ffi::{c_int, c_void};
    use kprint::kprintln;
    use zx_status::Status;

    /// Console command handler for `k vm`.
    ///
    /// # Safety
    ///
    /// - `argc` must be `>= 1` and `argv` must be non-null and valid for reads of `argc`
    ///   initialized `CmdArgs` entries for the duration of the call.
    /// - For the `map` and `unmap` subcommands, the caller-supplied physical and kernel virtual
    ///   address ranges must be valid to map or unmap in the kernel address space without
    ///   corrupting active kernel mappings.
    unsafe extern "C" fn cmd_vm(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return Status::INTERNAL.into_raw();
        }
        // SAFETY: `argv` is non-null and points to `argc` initialized `CmdArgs` entries from the
        // console dispatcher.
        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let cmd_name = args[0].as_str();
        let usage = || -> c_int {
            kprintln!("usage:");
            kprintln!("{:s} phys2virt <address>", cmd_name);
            kprintln!("{:s} virt2phys <address>", cmd_name);
            kprintln!("{:s} map <phys> <virt> <count> <flags>", cmd_name);
            kprintln!("{:s} unmap <virt> <count>", cmd_name);
            Status::INTERNAL.into_raw()
        };
        let not_enough_args = || -> c_int {
            kprintln!("not enough arguments");
            usage()
        };

        if args.len() < 2 {
            return not_enough_args();
        }

        let subcmd = args[1].as_str();
        if subcmd == "phys2virt" {
            if args.len() < 3 {
                return not_enough_args();
            }

            if !is_physmap_phys_addr(PAddr(args[2].arg_uint as usize)) {
                kprintln!("address isn't in physmap");
                return -1;
            }

            let ptr = core::ptr::with_exposed_provenance::<c_void>(
                paddr_to_physmap(PAddr(args[2].arg_uint as usize)).0,
            );
            kprintln!("paddr_to_physmap returns {:p}", ptr);
        } else if subcmd == "virt2phys" {
            if args.len() < 3 {
                return not_enough_args();
            }

            if !is_kernel_address(args[2].arg_uint as usize) {
                kprintln!("ERROR: outside of kernel address space");
                return -1;
            }

            match VmAspace::kernel_aspace().arch_aspace().query(args[2].arg_uint as usize) {
                Ok((pa, mmu_flags)) => {
                    kprintln!("arch_mmu_query returns {}", zx_status::sys::ZX_OK);
                    kprintln!("\tpa {:#x}, flags {:#x}", pa.0, mmu_flags);
                }
                Err(s) => {
                    kprintln!("arch_mmu_query returns {}", s.into_raw());
                }
            }
        } else if subcmd == "map" {
            if args.len() < 6 {
                return not_enough_args();
            }

            if !is_kernel_address(args[3].arg_uint as usize) {
                kprintln!("ERROR: outside of kernel address space");
                return -1;
            }

            // SAFETY: Console debugging command mapping user-specified physical pages into
            // kernel virtual address space.
            let err = Status::result_into_raw(unsafe {
                VmAspace::kernel_aspace().arch_aspace().map_contiguous(
                    args[3].arg_uint as usize,
                    PAddr(args[2].arg_uint as usize),
                    args[4].arg_uint as u32 as usize,
                    args[5].arg_uint as ArchMmuFlags,
                )
            });
            kprintln!("arch_mmu_map returns {}", err);
        } else if subcmd == "unmap" {
            if args.len() < 4 {
                return not_enough_args();
            }

            if !is_kernel_address(args[2].arg_uint as usize) {
                kprintln!("ERROR: outside of kernel address space");
                return -1;
            }

            // Strictly only attempt to unmap exactly what the user requested, they can deal with
            // any failure that might result.
            // SAFETY: Console debugging command unmapping user-specified kernel virtual
            // address range.
            let err = Status::result_into_raw(unsafe {
                VmAspace::kernel_aspace().arch_aspace().unmap(
                    args[2].arg_uint as usize,
                    args[3].arg_uint as u32 as usize,
                    ARCH_UNMAP_OPTION_NONE,
                )
            });
            kprintln!("arch_mmu_unmap returns {}", err);
        } else {
            kprintln!("unknown command");
            return usage();
        }

        zx_status::sys::ZX_OK
    }

    static_command!(VM, c"vm".as_ptr(), c"vm commands".as_ptr(), cmd_vm, CMD_AVAIL_NORMAL);
}
