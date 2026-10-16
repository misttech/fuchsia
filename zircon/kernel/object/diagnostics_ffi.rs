// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{CStr, c_char};
use zx_types::{zx_koid_t, zx_obj_type_t};

unsafe extern "C" {
    fn cpp_dump_process_list();
    fn cpp_dump_job_list();
    fn cpp_dump_job_policies(id: zx_koid_t);
    fn cpp_dump_process_handles(id: zx_koid_t);
    fn cpp_dump_peer_dispatchers_by_koid(obj_type: zx_obj_type_t, id: zx_koid_t);
    fn cpp_dump_all_peer_dispatchers(obj_type: zx_obj_type_t);
    fn cpp_dump_all_vm_objects(hidden_only: bool, format_unit: c_char);
    fn cpp_dump_orphaned_slices(format_unit: c_char);
    fn cpp_dump_orphaned_mapped_vmos(format_unit: c_char);
    fn cpp_dump_process_vm_objects(id: zx_koid_t, format_unit: c_char);
    fn cpp_dump_vm_object_cow_tree(id: zx_koid_t);
    fn cpp_kill_process(id: zx_koid_t);
    fn cpp_dump_process_address_space(id: zx_koid_t);
    fn cpp_dump_process_memory_usage(prefix: *const c_char, min_bytes: usize);
    fn cpp_dump_handles_for_koid(id: zx_koid_t);
    fn cpp_handle_diagnostics_outstanding_handles() -> usize;
    fn cpp_handle_diagnostics_dump_table_info();
}

/// Dumps all processes in the system to the kernel console.
pub(super) fn dump_process_list() {
    // SAFETY: Takes no pointers and synchronizes access to the job/process tree internally.
    unsafe { cpp_dump_process_list() };
}

/// Dumps all jobs in the system to the kernel console.
pub(super) fn dump_job_list() {
    // SAFETY: Takes no pointers and synchronizes access to the job tree internally.
    unsafe { cpp_dump_job_list() };
}

/// Dumps policies for the job with koid `id` to the kernel console.
pub(super) fn dump_job_policies(id: zx_koid_t) {
    // SAFETY: Takes only a scalar koid and synchronizes access to the job tree internally.
    unsafe { cpp_dump_job_policies(id) };
}

/// Dumps handles for the process with koid `id` to the kernel console.
pub(super) fn dump_process_handles(id: zx_koid_t) {
    // SAFETY: Takes only a scalar koid and synchronizes process/handle-table access internally.
    unsafe { cpp_dump_process_handles(id) };
}

/// Dumps peer dispatchers of `obj_type` matching process or object koid `id`.
pub(super) fn dump_peer_dispatchers_by_koid(obj_type: zx_obj_type_t, id: zx_koid_t) {
    // SAFETY: Takes only scalar arguments and synchronizes process/handle-table access internally.
    unsafe { cpp_dump_peer_dispatchers_by_koid(obj_type, id) };
}

/// Dumps all peer dispatchers of `obj_type` across all processes.
pub(super) fn dump_all_peer_dispatchers(obj_type: zx_obj_type_t) {
    // SAFETY: Takes only a scalar object type and synchronizes process/handle-table access
    // internally.
    unsafe { cpp_dump_all_peer_dispatchers(obj_type) };
}

/// Dumps all (or hidden-only) VMOs in the system using `format_unit`.
pub(super) fn dump_all_vm_objects(hidden_only: bool, format_unit: c_char) {
    // SAFETY: Takes only scalar arguments and synchronizes VMO list access internally.
    unsafe { cpp_dump_all_vm_objects(hidden_only, format_unit) };
}

/// Dumps summary statistics for orphaned slice VMOs using `format_unit`.
pub(super) fn dump_orphaned_slices(format_unit: c_char) {
    // SAFETY: Takes only a scalar format unit and synchronizes VMO list access internally.
    unsafe { cpp_dump_orphaned_slices(format_unit) };
}

/// Dumps summary statistics for orphaned mapped VMOs using `format_unit`.
pub(super) fn dump_orphaned_mapped_vmos(format_unit: c_char) {
    // SAFETY: Takes only a scalar format unit and synchronizes VMO list access internally.
    unsafe { cpp_dump_orphaned_mapped_vmos(format_unit) };
}

/// Dumps VMOs held or mapped by the process with koid `id` using `format_unit`.
pub(super) fn dump_process_vm_objects(id: zx_koid_t, format_unit: c_char) {
    // SAFETY: Takes only scalar arguments and synchronizes process/VMO access internally.
    unsafe { cpp_dump_process_vm_objects(id, format_unit) };
}

/// Dumps the copy-on-write tree for the VMO with koid `id`.
pub(super) fn dump_vm_object_cow_tree(id: zx_koid_t) {
    // SAFETY: Takes only a scalar koid and synchronizes VMO/COW-tree access internally.
    unsafe { cpp_dump_vm_object_cow_tree(id) };
}

/// Sends a kill signal to the process with koid `id` if found.
pub(super) fn kill_process(id: zx_koid_t) {
    // SAFETY: Takes only a scalar koid and synchronizes process lookup internally.
    unsafe { cpp_kill_process(id) };
}

/// Dumps the address space of the process with koid `id`.
pub(super) fn dump_process_address_space(id: zx_koid_t) {
    // SAFETY: Takes only a scalar koid and synchronizes process/aspace access internally.
    unsafe { cpp_dump_process_address_space(id) };
}

/// Dumps processes whose uncompressed memory attribution is at least `min_bytes`.
pub(super) fn dump_process_memory_usage(prefix: &CStr, min_bytes: usize) {
    // SAFETY: `prefix.as_ptr()` points to a valid, NUL-terminated C string for the duration of the
    // call.
    unsafe { cpp_dump_process_memory_usage(prefix.as_ptr(), min_bytes) };
}

/// Dumps all handles and owning processes for `id`.
pub(super) fn dump_handles_for_koid(id: zx_koid_t) {
    // SAFETY: Takes only a scalar koid and synchronizes process/handle-table access internally.
    unsafe { cpp_dump_handles_for_koid(id) };
}

/// Returns the number of outstanding handles in the global handle table arena.
pub(super) fn handle_diagnostics_outstanding_handles() -> usize {
    // SAFETY: Takes no pointers and queries atomic/locked arena diagnostic state internally.
    unsafe { cpp_handle_diagnostics_outstanding_handles() }
}

/// Dumps diagnostic information about the global handle table arena.
pub(super) fn handle_diagnostics_dump_table_info() {
    // SAFETY: Takes no pointers and synchronizes arena diagnostic access internally.
    unsafe { cpp_handle_diagnostics_dump_table_info() };
}
