// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_DIAGNOSTICS_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_DIAGNOSTICS_H_

#include <zircon/types.h>

// Prints all handles and owning processes for the specified koid.
void DumpHandlesForKoid(zx_koid_t id);

extern "C" {
void cpp_dump_job_policies(zx_koid_t id);
void cpp_dump_process_handles(zx_koid_t id);
void cpp_dump_peer_dispatchers_by_koid(zx_obj_type_t type, zx_koid_t id);
void cpp_dump_all_peer_dispatchers(zx_obj_type_t type);
void cpp_dump_all_vm_objects(bool hidden_only, char format_unit);
void cpp_dump_orphaned_slices(char format_unit);
void cpp_dump_orphaned_mapped_vmos(char format_unit);
void cpp_dump_process_vm_objects(zx_koid_t id, char format_unit);
void cpp_dump_vm_object_cow_tree(zx_koid_t id);
void cpp_kill_process(zx_koid_t id);
void cpp_dump_process_address_space(zx_koid_t id);
void cpp_dump_process_memory_usage(const char* prefix, size_t min_bytes);
void cpp_dump_handles_for_koid(zx_koid_t id);
}  // extern "C"

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_DIAGNOSTICS_H_
