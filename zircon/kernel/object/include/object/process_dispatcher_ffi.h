// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PROCESS_DISPATCHER_FFI_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PROCESS_DISPATCHER_FFI_H_

#include <zircon/compiler.h>

class ProcessDispatcher;
class VmAspace;

__BEGIN_CDECLS

VmAspace* cpp_process_dispatcher_normal_aspace(ProcessDispatcher* process);
VmAspace* cpp_process_dispatcher_restricted_aspace(const ProcessDispatcher* process);
bool cpp_process_dispatcher_critical_to_root_job(const ProcessDispatcher* process);

__END_CDECLS

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PROCESS_DISPATCHER_FFI_H_
