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
