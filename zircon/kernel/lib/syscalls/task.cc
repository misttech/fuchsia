// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <inttypes.h>
#include <lib/boot-options/boot-options.h>
#include <lib/counters.h>
#include <lib/ktrace.h>
#include <lib/page-map.h>
#include <lib/syscalls/forward.h>
#include <lib/user_copy/user_ptr.h>
#include <lib/userabi/vdso.h>
#include <platform.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/syscalls/debug.h>
#include <zircon/syscalls/exception.h>
#include <zircon/syscalls/policy.h>
#include <zircon/syscalls/rseq.h>
#include <zircon/types.h>

#include <arch/arch_ops.h>
#include <fbl/inline_array.h>
#include <fbl/ref_ptr.h>
#include <ktl/algorithm.h>
#include <object/handle.h>
#include <object/job_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/resource_dispatcher.h>
#include <object/thread_dispatcher.h>
#include <object/vm_address_region_dispatcher.h>

#include <ktl/enforce.h>

#define LOCAL_TRACE 0

namespace {

constexpr size_t kMaxDebugReadBlock = 64 * 1024u * 1024u;
constexpr size_t kMaxDebugWriteBlock = 64 * 1024u * 1024u;

// TODO(https://fxbug.dev/42105890): copy_user_string may truncate the incoming string,
// and may copy extra data past the NUL.
// TODO(dbort): If anyone else needs this, move it into user_ptr.
zx_status_t copy_user_string(const user_in_ptr<const char>& src, size_t src_len, char* buf,
                             size_t buf_len, ktl::string_view* sp) {
  // Disallow 0 buf_len (since we are copying into it), but allow 0 src_len (to allow
  // "", src_len doesn't include '\0'). With the check for buf_len, we won't underflow
  // src_len below. Also, 0 src_len is valid input (for "" src strings).
  if (!src || buf_len == 0 || src_len > buf_len) {
    return ZX_ERR_INVALID_ARGS;
  }
  zx_status_t result = src.copy_array_from_user(buf, src_len);
  if (result != ZX_OK) {
    return ZX_ERR_INVALID_ARGS;
  }

  // ensure zero termination
  size_t str_len = (src_len == buf_len ? src_len - 1 : src_len);
  buf[str_len] = 0;
  *sp = ktl::string_view(buf);

  return ZX_OK;
}

}  // namespace

// zx_status_t zx_process_create
zx_status_t sys_process_create(zx_handle_t job_handle, user_in_ptr<const char> _name,
                               size_t name_len, uint32_t options, zx_handle_t* proc_handle,
                               zx_handle_t* vmar_handle) {
  LTRACEF("job handle %x, options %#x\n", job_handle, options);

  // currently, the only valid option values are 0 or ZX_PROCESS_SHARED
  if (options != ZX_PROCESS_SHARED && options != 0)
    return ZX_ERR_INVALID_ARGS;

  auto up = ProcessDispatcher::GetCurrent();

  // We check the policy against the process calling zx_process_create, which
  // is the operative policy, rather than against |job_handle|. Access to
  // |job_handle| is controlled by the rights associated with the handle.
  zx_status_t result = up->EnforceBasicPolicy(ZX_POL_NEW_PROCESS);
  if (result != ZX_OK)
    return result;

  // copy out the name
  char buf[ZX_MAX_NAME_LEN];
  ktl::string_view sp;
  // Silently truncate the given name.
  if (name_len > sizeof(buf))
    name_len = sizeof(buf);
  result = copy_user_string(_name, name_len, buf, sizeof(buf), &sp);
  if (result != ZX_OK)
    return result;
  LTRACEF("name %s\n", buf);

  fbl::RefPtr<JobDispatcher> job;
  auto status =
      up->handle_table().GetDispatcherWithRights(*up, job_handle, ZX_RIGHT_MANAGE_PROCESS, &job);
  if (status != ZX_OK) {
    return status;
  }

  // create a new process dispatcher
  KernelHandle<ProcessDispatcher> new_process_handle;
  KernelHandle<VmAddressRegionDispatcher> new_vmar_handle;
  zx_rights_t proc_rights, vmar_rights;
  result = ProcessDispatcher::Create(ktl::move(job), sp, options, &new_process_handle, &proc_rights,
                                     &new_vmar_handle, &vmar_rights);
  if (result != ZX_OK)
    return result;

  KTRACE_KERNEL_OBJECT("kernel:meta", new_process_handle.dispatcher()->get_koid(),
                       ZX_OBJ_TYPE_PROCESS, buf,
                       ("job", ktrace::Koid(new_process_handle.dispatcher()->job()->get_koid())));

  result = up->MakeAndAddHandle(ktl::move(new_process_handle), proc_rights, proc_handle);
  if (result == ZX_OK)
    result = up->MakeAndAddHandle(ktl::move(new_vmar_handle), vmar_rights, vmar_handle);
  return result;
}

// zx_status_t zx_process_create_shared
zx_status_t sys_process_create_shared(zx_handle_t shared_proc_handle, uint32_t options,
                                      user_in_ptr<const char> _name, size_t name_len,
                                      zx_handle_t* proc_handle,
                                      zx_handle_t* restricted_vmar_handle) {
  // currently, the only valid option value is 0
  if (options != 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  auto up = ProcessDispatcher::GetCurrent();

  // We check the policy against the process calling zx_process_create, which
  // is the operative policy.
  // TODO(https://fxbug.dev/42181309): Figure out which policy check makes sense here.
  zx_status_t result = up->EnforceBasicPolicy(ZX_POL_NEW_PROCESS);
  if (result != ZX_OK) {
    return result;
  }

  // copy out the name
  char buf[ZX_MAX_NAME_LEN];
  ktl::string_view sp;
  // Silently truncate the given name.
  if (name_len > sizeof(buf)) {
    name_len = sizeof(buf);
  }
  result = copy_user_string(_name, name_len, buf, sizeof(buf), &sp);
  if (result != ZX_OK) {
    return result;
  }
  LTRACEF("name %s\n", buf);

  fbl::RefPtr<ProcessDispatcher> shared_proc;
  result = up->handle_table().GetDispatcherWithRights(
      *up, shared_proc_handle, ZX_RIGHT_MANAGE_PROCESS | ZX_RIGHT_GET_PROPERTY, &shared_proc);
  if (result != ZX_OK) {
    return result;
  }

  // create a new process dispatcher
  KernelHandle<ProcessDispatcher> new_process_handle;
  KernelHandle<VmAddressRegionDispatcher> new_restricted_vmar_handle;
  zx_rights_t proc_rights, restricted_vmar_rights;
  result =
      ProcessDispatcher::CreateShared(shared_proc, sp, options, &new_process_handle, &proc_rights,
                                      &new_restricted_vmar_handle, &restricted_vmar_rights);
  if (result != ZX_OK) {
    return result;
  }

  KTRACE_KERNEL_OBJECT("kernel:meta", new_process_handle.dispatcher()->get_koid(),
                       ZX_OBJ_TYPE_PROCESS, buf,
                       ("job", ktrace::Koid(new_process_handle.dispatcher()->job()->get_koid())));

  result = up->MakeAndAddHandle(ktl::move(new_process_handle), proc_rights, proc_handle);

  if (result == ZX_OK) {
    result = up->MakeAndAddHandle(ktl::move(new_restricted_vmar_handle), restricted_vmar_rights,
                                  restricted_vmar_handle);
  }

  return result;
}

// Note: This is used to start the main thread (as opposed to using
// sys_thread_start for that) for a few reasons:
// - less easily exploitable
//   We want to make sure we can't generically transfer handles to a process.
//   This has the nice property of restricting the evil (transferring handle
//   to new process) to exactly one spot, and can be called exactly once per
//   process, since it also pushes it into a new state.
// - maintains the state machine invariant that 'started' processes have one
//   thread running

// zx_status_t zx_process_start
zx_status_t sys_process_start(zx_handle_t process_handle, zx_handle_t thread_handle, zx_vaddr_t pc,
                              zx_vaddr_t sp, zx_handle_t arg_handle_value, uintptr_t arg2) {
  LTRACEF("phandle %x, thandle %x, pc %#" PRIxPTR ", sp %#" PRIxPTR
          ", arg_handle %x, arg2 %#" PRIxPTR "\n",
          process_handle, thread_handle, pc, sp, arg_handle_value, arg2);
  auto up = ProcessDispatcher::GetCurrent();

  // get process dispatcher
  fbl::RefPtr<ProcessDispatcher> process;
  zx_status_t status =
      up->handle_table().GetDispatcherWithRights(*up, process_handle, ZX_RIGHT_WRITE, &process);
  if (status != ZX_OK) {
    if (arg_handle_value != ZX_HANDLE_INVALID) {
      up->handle_table().RemoveHandle(*up, arg_handle_value);
    }
    return status;
  }

  // get thread_dispatcher
  fbl::RefPtr<ThreadDispatcher> thread;
  status = up->handle_table().GetDispatcherWithRights(*up, thread_handle, ZX_RIGHT_WRITE, &thread);
  if (status != ZX_OK) {
    if (arg_handle_value != ZX_HANDLE_INVALID) {
      up->handle_table().RemoveHandle(*up, arg_handle_value);
    }
    return status;
  }

  HandleOwner arg_handle;
  if (arg_handle_value != ZX_HANDLE_INVALID) {
    arg_handle = up->handle_table().RemoveHandle(*up, arg_handle_value);
  }

  return process->Start(ktl::move(thread), pc, sp, ktl::move(arg_handle), arg2);
}

void sys_process_exit(int64_t retcode) {
  LTRACEF("retcode %" PRId64 "\n", retcode);
  ProcessDispatcher::ExitCurrent(retcode);
}

// zx_status_t zx_process_read_memory
zx_status_t sys_process_read_memory(zx_handle_t handle, zx_vaddr_t vaddr, user_out_ptr<void> buffer,
                                    size_t buffer_size, user_out_ptr<size_t> actual) {
  LTRACEF("vaddr 0x%" PRIxPTR ", size %zu\n", vaddr, buffer_size);

  if (!buffer)
    return ZX_ERR_INVALID_ARGS;
  if (buffer_size == 0 || buffer_size > kMaxDebugReadBlock)
    return ZX_ERR_INVALID_ARGS;

  auto up = ProcessDispatcher::GetCurrent();

  fbl::RefPtr<ProcessDispatcher> process;
  zx_status_t status = up->handle_table().GetDispatcherWithRights(
      *up, handle, ZX_RIGHT_READ | ZX_RIGHT_WRITE, &process);
  if (status != ZX_OK)
    return status;

  auto aspace = process->aspace_at(vaddr);
  if (!aspace)
    return ZX_ERR_BAD_STATE;

  auto region = aspace->FindRegion(vaddr);
  if (!region)
    return ZX_ERR_NOT_FOUND;

  auto vm_mapping = region->as_vm_mapping();
  if (!vm_mapping)
    return ZX_ERR_NOT_FOUND;

  auto vmo = vm_mapping->vmo();
  if (!vmo)
    return ZX_ERR_NOT_FOUND;

  uint64_t offset = vaddr - vm_mapping->base() + vm_mapping->object_offset();
  // TODO(https://fxbug.dev/42106495): While this limits reading to the mapped address space of
  // this VMO, it should be reading from multiple VMOs, not a single one.
  // Additionally, it is racy with the mapping going away.
  buffer_size = ktl::min(buffer_size, vm_mapping->size() - (vaddr - vm_mapping->base()));
  auto [st, out_actual] = vmo->ReadUser(buffer.reinterpret<char>(), offset, buffer_size,
                                        VmObjectReadWriteOptions::TrimLength);
  if (st != ZX_OK) {
    // Do not write |out_actual| to |actual| on error
    return st;
  }
  if (out_actual == 0) {
    // If our partial read returned 0 bytes, it means that offset is past the end of the VMO.
    return ZX_ERR_OUT_OF_RANGE;
  }

  return actual.copy_to_user(out_actual);
}

// zx_status_t zx_process_write_memory
zx_status_t sys_process_write_memory(zx_handle_t handle, zx_vaddr_t vaddr,
                                     user_in_ptr<const void> buffer, size_t buffer_size,
                                     user_out_ptr<size_t> actual) {
  LTRACEF("vaddr 0x%" PRIxPTR ", size %zu\n", vaddr, buffer_size);

  if (!BootOptions::Get()->enable_debugging_syscalls) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  if (!buffer) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (buffer_size == 0 || buffer_size > kMaxDebugWriteBlock) {
    return ZX_ERR_INVALID_ARGS;
  }

  auto up = ProcessDispatcher::GetCurrent();

  fbl::RefPtr<ProcessDispatcher> process;
  zx_status_t status =
      up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_WRITE, &process);
  if (status != ZX_OK) {
    return status;
  }

  auto aspace = process->aspace_at(vaddr);
  if (!aspace) {
    return ZX_ERR_BAD_STATE;
  }

  auto region = aspace->FindRegion(vaddr);
  if (!region) {
    return ZX_ERR_NOT_FOUND;
  }

  auto vm_mapping = region->as_vm_mapping();
  if (!vm_mapping) {
    return ZX_ERR_NOT_FOUND;
  }

  // TODO(https://fxbug.dev/42106188): Inform the mapping that we are going to ignore its
  // permissions and perform a write. This provides it the chance to ensure our writes go to a
  // mapping local clone to avoid corrupting a potentially read-only VMO.
  auto result = vm_mapping->ForceWritable();
  if (result.is_error()) {
    return result.error_value();
  }
  vm_mapping = *result;

  auto vmo = vm_mapping->vmo();
  if (!vmo) {
    return ZX_ERR_NOT_FOUND;
  }

  if (VDso::vmo_is_vdso(vmo)) {
    // Don't allow writes to the vDSO.
    return ZX_ERR_ACCESS_DENIED;
  }

  uint64_t offset = vaddr - vm_mapping->base() + vm_mapping->object_offset();
  // TODO(https://fxbug.dev/42106495): While this limits writing to the mapped address space of
  // this VMO, it should be writing to multiple VMOs, not a single one.
  // Additionally, it is racy with the mapping going away.
  buffer_size = ktl::min(buffer_size, vm_mapping->size() - (vaddr - vm_mapping->base()));
  auto [st, out_actual] = vmo->WriteUser(buffer.reinterpret<const char>(), offset, buffer_size,
                                         VmObjectReadWriteOptions::TrimLength,
                                         /*on_bytes_transferred=*/nullptr);
  if (st != ZX_OK) {
    // Do not write |out_actual| to |actual| on error~
    return st;
  }
  if (out_actual == 0) {
    // If our partial write returned 0 bytes, it means that offset is past the end of the VMO.
    return ZX_ERR_OUT_OF_RANGE;
  }

  return actual.copy_to_user(out_actual);
}

// helper routine for sys_task_kill
template <typename T>
static zx_status_t kill_task(fbl::RefPtr<Dispatcher> dispatcher) {
  auto task = DownCastDispatcher<T>(&dispatcher);
  if (!task)
    return ZX_ERR_WRONG_TYPE;

  task->Kill(ZX_TASK_RETCODE_SYSCALL_KILL);
  return ZX_OK;
}

// zx_status_t zx_task_kill
zx_status_t sys_task_kill(zx_handle_t task_handle) {
  LTRACEF("handle %x\n", task_handle);

  auto up = ProcessDispatcher::GetCurrent();

  fbl::RefPtr<Dispatcher> dispatcher;
  auto status =
      up->handle_table().GetDispatcherWithRights(*up, task_handle, ZX_RIGHT_DESTROY, &dispatcher);
  if (status != ZX_OK)
    return status;

  // See if it's a process or job and dispatch accordingly. Killing a thread is not supported.
  switch (dispatcher->get_type()) {
    case ZX_OBJ_TYPE_JOB:
      return kill_task<JobDispatcher>(ktl::move(dispatcher));
    case ZX_OBJ_TYPE_PROCESS:
      return kill_task<ProcessDispatcher>(ktl::move(dispatcher));
    default:
      return ZX_ERR_WRONG_TYPE;
  }
}
