// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_int;
use zx_types::{ZX_OK, zx_status_t};

use super::smp::x86_bringup_aps;

unsafe extern "C" {
    #[link_name = "x86_secondary_cpu_long_mode_entry"]
    pub(super) static X86_SECONDARY_CPU_LONG_MODE_ENTRY: u8;

    /// Allocates all of the necessary structures for all of the APs to run
    /// (`x86_allocate_ap_structures`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `apic_ids` points to a live allocation of `cpu_count` valid
    /// APIC IDs and that this function is called once during single-threaded boot.
    pub(super) fn x86_allocate_ap_structures(apic_ids: *mut u32, cpu_count: u8) -> zx_status_t;

    /// Translates an APIC ID to a logical CPU number, or `-1` if unknown
    /// (`x86_apic_id_to_cpu_num`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that the bootstrap processor and AP structures have been initialized.
    pub(super) fn x86_apic_id_to_cpu_num(apic_id: u32) -> c_int;

    /// Initializes the idle threads for `secondary_cpu_count` secondary CPUs
    /// (`lk_init_secondary_cpus`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called once during single-threaded SMP
    /// initialization before secondary CPUs are brought online.
    pub(super) fn lk_init_secondary_cpus(secondary_cpu_count: u32);
}

/// FFI entry point for [`x86_bringup_aps`].
///
/// # Safety
///
/// The caller must ensure that `apic_ids` points to a live (not freed) allocation of at least
/// `count` valid `u32` elements (when `count > 0`), that `x86_init_smp` has already been called,
/// and that this function is called in a thread context where blocking (`thread::sleep_relative`)
/// is permitted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_bringup_aps(apic_ids: *mut u32, count: u32) -> zx_status_t {
    // SAFETY: Caller upholds the `x86_bringup_aps` preconditions.
    match unsafe { x86_bringup_aps(apic_ids, count) } {
        Ok(()) => ZX_OK,
        Err(status) => status.into_raw(),
    }
}
