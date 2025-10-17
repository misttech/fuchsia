// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/syscalls/forward.h>
#include <lib/user_copy/user_ptr.h>
#include <zircon/syscalls/object.h>
#include <zircon/types.h>

#include <object/exception_dispatcher.h>
#include <object/job_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/socket_dispatcher.h>
#include <object/stream_dispatcher.h>
#include <object/vm_object_dispatcher.h>

#if ARCH_X86
static zx_status_t RequireCurrentThread(fbl::RefPtr<Dispatcher> dispatcher) {
  auto thread_dispatcher = DownCastDispatcher<ThreadDispatcher>(&dispatcher);
  if (!thread_dispatcher) {
    return ZX_ERR_WRONG_TYPE;
  }
  if (thread_dispatcher.get() != ThreadDispatcher::GetCurrent()) {
    return ZX_ERR_ACCESS_DENIED;
  }
  return ZX_OK;
}
#endif

// zx_status_t zx_object_get_property
zx_status_t sys_object_get_property(zx_handle_t handle_value, uint32_t property,
                                    user_out_ptr<void> _value, size_t size) {
  if (!_value)
    return ZX_ERR_INVALID_ARGS;

  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<Dispatcher> dispatcher;
  zx_status_t status = up->handle_table().GetDispatcherWithRights(
      *up, handle_value, ZX_RIGHT_GET_PROPERTY, &dispatcher);
  if (status != ZX_OK)
    return status;
  switch (property) {
    case ZX_PROP_NAME: {
      if (size < ZX_MAX_NAME_LEN)
        return ZX_ERR_BUFFER_TOO_SMALL;
      char name[ZX_MAX_NAME_LEN] = {};
      status = dispatcher->get_name(name);
      if (status != ZX_OK) {
        return status;
      }
      if (_value.reinterpret<char>().copy_array_to_user(name, ZX_MAX_NAME_LEN) != ZX_OK)
        return ZX_ERR_INVALID_ARGS;
      return ZX_OK;
    }
    case ZX_PROP_PROCESS_DEBUG_ADDR: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
      if (!process)
        return ZX_ERR_WRONG_TYPE;
      uintptr_t value = process->get_debug_addr();
      return _value.reinterpret<uintptr_t>().copy_to_user(value);
    }
    case ZX_PROP_PROCESS_BREAK_ON_LOAD: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
      if (!process)
        return ZX_ERR_WRONG_TYPE;
      uintptr_t value = process->get_dyn_break_on_load();
      return _value.reinterpret<uintptr_t>().copy_to_user(value);
    }
    case ZX_PROP_PROCESS_VDSO_BASE_ADDRESS: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
      if (!process)
        return ZX_ERR_WRONG_TYPE;
      uintptr_t value = process->vdso_base_address();
      return _value.reinterpret<uintptr_t>().copy_to_user(value);
    }
    case ZX_PROP_PROCESS_HW_TRACE_CONTEXT_ID: {
      if (!gBootOptions->enable_debugging_syscalls) {
        return ZX_ERR_NOT_SUPPORTED;
      }
#if ARCH_X86
      if (size < sizeof(uintptr_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
      if (!process) {
        return ZX_ERR_WRONG_TYPE;
      }
      uintptr_t value = process->hw_trace_context_id();
      return _value.reinterpret<uintptr_t>().copy_to_user(value);
#else
      return ZX_ERR_NOT_SUPPORTED;
#endif
    }
    case ZX_PROP_SOCKET_RX_THRESHOLD: {
      if (size < sizeof(size_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto socket = DownCastDispatcher<SocketDispatcher>(&dispatcher);
      if (!socket)
        return ZX_ERR_WRONG_TYPE;
      size_t value = socket->GetReadThreshold();
      return _value.reinterpret<size_t>().copy_to_user(value);
    }
    case ZX_PROP_SOCKET_TX_THRESHOLD: {
      if (size < sizeof(size_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto socket = DownCastDispatcher<SocketDispatcher>(&dispatcher);
      if (!socket)
        return ZX_ERR_WRONG_TYPE;
      size_t value = socket->GetWriteThreshold();
      return _value.reinterpret<size_t>().copy_to_user(value);
    }
    case ZX_PROP_EXCEPTION_STATE: {
      if (size < sizeof(uint32_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto exception = DownCastDispatcher<ExceptionDispatcher>(&dispatcher);
      if (!exception) {
        return ZX_ERR_WRONG_TYPE;
      }

      return _value.reinterpret<uint32_t>().copy_to_user(exception->GetDisposition());
    }
    case ZX_PROP_EXCEPTION_STRATEGY: {
      if (size < sizeof(uint32_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto exception = DownCastDispatcher<ExceptionDispatcher>(&dispatcher);
      if (!exception) {
        return ZX_ERR_WRONG_TYPE;
      }

      bool second_chance = exception->IsSecondChance();
      return _value.reinterpret<uint32_t>().copy_to_user(
          second_chance ? ZX_EXCEPTION_STRATEGY_SECOND_CHANCE : ZX_EXCEPTION_STRATEGY_FIRST_CHANCE);
    }
    case ZX_PROP_VMO_CONTENT_SIZE: {
      if (size < sizeof(uint64_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto vmo = DownCastDispatcher<VmObjectDispatcher>(&dispatcher);
      if (!vmo) {
        return ZX_ERR_WRONG_TYPE;
      }

      uint64_t value = vmo->GetContentSize();
      return _value.reinterpret<uint64_t>().copy_to_user(value);
    }
    case ZX_PROP_STREAM_MODE_APPEND: {
      if (size < sizeof(uint8_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto stream = DownCastDispatcher<StreamDispatcher>(&dispatcher);
      if (!stream) {
        return ZX_ERR_WRONG_TYPE;
      }

      uint8_t value = stream->IsInAppendMode();
      return _value.reinterpret<uint8_t>().copy_to_user(value);
    }
#if ARCH_X86
    case ZX_PROP_REGISTER_FS: {
      if (size < sizeof(uintptr_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      status = RequireCurrentThread(ktl::move(dispatcher));
      if (status != ZX_OK) {
        return status;
      }
      uintptr_t value = read_msr(X86_MSR_IA32_FS_BASE);
      return _value.reinterpret<uintptr_t>().copy_to_user(value);
    }
    case ZX_PROP_REGISTER_GS: {
      if (size < sizeof(uintptr_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      status = RequireCurrentThread(ktl::move(dispatcher));
      if (status != ZX_OK) {
        return status;
      }
      uintptr_t value = read_msr(X86_MSR_IA32_KERNEL_GS_BASE);
      return _value.reinterpret<uintptr_t>().copy_to_user(value);
    }
#endif

    default:
      return ZX_ERR_NOT_SUPPORTED;
  }

  __UNREACHABLE;
}

// zx_status_t zx_object_set_property
zx_status_t sys_object_set_property(zx_handle_t handle_value, uint32_t property,
                                    user_in_ptr<const void> _value, size_t size) {
  if (!_value)
    return ZX_ERR_INVALID_ARGS;

  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<Dispatcher> dispatcher;

  zx_rights_t rights;
  const zx_status_t get_dispatcher_status = up->handle_table().GetDispatcherWithRights(
      *up, handle_value, ZX_RIGHT_SET_PROPERTY, &dispatcher, &rights);
  if (get_dispatcher_status != ZX_OK)
    return get_dispatcher_status;

  switch (property) {
    case ZX_PROP_NAME: {
      if (size >= ZX_MAX_NAME_LEN)
        size = ZX_MAX_NAME_LEN - 1;
      char name[ZX_MAX_NAME_LEN - 1];
      if (_value.reinterpret<const char>().copy_array_from_user(name, size) != ZX_OK)
        return ZX_ERR_INVALID_ARGS;
      return dispatcher->set_name(name, size);
    }
#if ARCH_X86
    case ZX_PROP_REGISTER_FS: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      zx_status_t status = RequireCurrentThread(ktl::move(dispatcher));
      if (status != ZX_OK)
        return status;
      uintptr_t addr;
      status = _value.reinterpret<const uintptr_t>().copy_from_user(&addr);
      if (status != ZX_OK)
        return status;
      if (!x86_is_vaddr_canonical(addr))
        return ZX_ERR_INVALID_ARGS;
      write_msr(X86_MSR_IA32_FS_BASE, addr);
      return ZX_OK;
    }
    case ZX_PROP_REGISTER_GS: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      zx_status_t status = RequireCurrentThread(ktl::move(dispatcher));
      if (status != ZX_OK)
        return status;
      uintptr_t addr;
      status = _value.reinterpret<const uintptr_t>().copy_from_user(&addr);
      if (status != ZX_OK)
        return status;
      if (!x86_is_vaddr_canonical(addr))
        return ZX_ERR_INVALID_ARGS;
      write_msr(X86_MSR_IA32_KERNEL_GS_BASE, addr);
      return ZX_OK;
    }
#endif
    case ZX_PROP_PROCESS_DEBUG_ADDR: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
      if (!process)
        return ZX_ERR_WRONG_TYPE;
      uintptr_t value = 0;
      zx_status_t status = _value.reinterpret<const uintptr_t>().copy_from_user(&value);
      if (status != ZX_OK)
        return status;
      return process->set_debug_addr(value);
    }
    case ZX_PROP_PROCESS_BREAK_ON_LOAD: {
      if (size < sizeof(uintptr_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto process = DownCastDispatcher<ProcessDispatcher>(&dispatcher);
      if (!process)
        return ZX_ERR_WRONG_TYPE;
      uintptr_t value = 0;
      zx_status_t status = _value.reinterpret<const uintptr_t>().copy_from_user(&value);
      if (status != ZX_OK)
        return status;
      return process->set_dyn_break_on_load(value);
    }
    case ZX_PROP_SOCKET_RX_THRESHOLD: {
      if (size < sizeof(size_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto socket = DownCastDispatcher<SocketDispatcher>(&dispatcher);
      if (!socket)
        return ZX_ERR_WRONG_TYPE;
      size_t value = 0;
      zx_status_t status = _value.reinterpret<const size_t>().copy_from_user(&value);
      if (status != ZX_OK)
        return status;
      return socket->SetReadThreshold(value);
    }
    case ZX_PROP_SOCKET_TX_THRESHOLD: {
      if (size < sizeof(size_t))
        return ZX_ERR_BUFFER_TOO_SMALL;
      auto socket = DownCastDispatcher<SocketDispatcher>(&dispatcher);
      if (!socket)
        return ZX_ERR_WRONG_TYPE;
      size_t value = 0;
      zx_status_t status = _value.reinterpret<const size_t>().copy_from_user(&value);
      if (status != ZX_OK)
        return status;
      return socket->SetWriteThreshold(value);
    }
    case ZX_PROP_JOB_KILL_ON_OOM: {
      auto job = DownCastDispatcher<JobDispatcher>(&dispatcher);
      if (!job)
        return ZX_ERR_WRONG_TYPE;
      size_t value = 0;
      zx_status_t status = _value.reinterpret<const size_t>().copy_from_user(&value);
      if (status != ZX_OK)
        return status;
      if (value == 0u) {
        job->set_kill_on_oom(false);
      } else if (value == 1u) {
        job->set_kill_on_oom(true);
      } else {
        return ZX_ERR_INVALID_ARGS;
      }
      return ZX_OK;
    }
    case ZX_PROP_EXCEPTION_STATE: {
      if (size < sizeof(uint32_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto exception = DownCastDispatcher<ExceptionDispatcher>(&dispatcher);
      if (!exception) {
        return ZX_ERR_WRONG_TYPE;
      }
      uint32_t value = 0;
      zx_status_t status = _value.reinterpret<const uint32_t>().copy_from_user(&value);
      if (status != ZX_OK) {
        return status;
      }
      if (value == ZX_EXCEPTION_STATE_HANDLED) {
        exception->SetDisposition(ZX_EXCEPTION_STATE_HANDLED);
      } else if (value == ZX_EXCEPTION_STATE_TRY_NEXT) {
        exception->SetDisposition(ZX_EXCEPTION_STATE_TRY_NEXT);
      } else if (value == ZX_EXCEPTION_STATE_THREAD_EXIT) {
        exception->SetDisposition(ZX_EXCEPTION_STATE_THREAD_EXIT);
      } else {
        return ZX_ERR_INVALID_ARGS;
      }
      return ZX_OK;
    }
    case ZX_PROP_EXCEPTION_STRATEGY: {
      if (size < sizeof(uint32_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto exception = DownCastDispatcher<ExceptionDispatcher>(&dispatcher);
      if (!exception) {
        return ZX_ERR_WRONG_TYPE;
      }

      // Invalid if the exception handle is not held by a debugger.
      const zx_info_thread_t info = exception->thread()->GetInfoForUserspace();
      if (info.wait_exception_channel_type != ZX_EXCEPTION_CHANNEL_TYPE_DEBUGGER) {
        return ZX_ERR_BAD_STATE;
      }

      uint32_t value = 0;
      const zx_status_t status = _value.reinterpret<const uint32_t>().copy_from_user(&value);
      if (status != ZX_OK) {
        return status;
      }
      if (value == ZX_EXCEPTION_STRATEGY_FIRST_CHANCE) {
        exception->SetWhetherSecondChance(false);
      } else if (value == ZX_EXCEPTION_STRATEGY_SECOND_CHANCE) {
        exception->SetWhetherSecondChance(true);
      } else {
        return ZX_ERR_INVALID_ARGS;
      }
      return ZX_OK;
    }
    case ZX_PROP_VMO_CONTENT_SIZE: {
      if ((rights & ZX_RIGHT_WRITE) == 0) {
        return ZX_ERR_ACCESS_DENIED;
      }
      if (size < sizeof(uint64_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto vmo = DownCastDispatcher<VmObjectDispatcher>(&dispatcher);
      if (!vmo) {
        return ZX_ERR_WRONG_TYPE;
      }
      uint64_t value = 0;
      zx_status_t status = _value.reinterpret<const uint64_t>().copy_from_user(&value);
      if (status != ZX_OK) {
        return status;
      }
      return vmo->SetContentSize(value);
    }
    case ZX_PROP_STREAM_MODE_APPEND: {
      if (size < sizeof(uint8_t)) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      auto stream = DownCastDispatcher<StreamDispatcher>(&dispatcher);
      if (!stream) {
        return ZX_ERR_WRONG_TYPE;
      }
      uint8_t value = 0;
      zx_status_t status = _value.reinterpret<const uint8_t>().copy_from_user(&value);
      if (status != ZX_OK) {
        return status;
      }
      return stream->SetAppendMode(value);
    }
    default:
      return ZX_ERR_NOT_SUPPORTED;
  }

  __UNREACHABLE;
}
