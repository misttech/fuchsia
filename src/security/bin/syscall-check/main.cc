// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <stdio.h>
#include <zircon/process.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/debug.h>

int main(int argc, char** argv) {
  printf("--- syscall-check ---\n");

#define SYSCALL_STATUS_LABEL(label, syscall, default_args) \
  printf(label ": ");                                      \
  if (syscall default_args == ZX_ERR_NOT_SUPPORTED) {      \
    printf("disabled\n");                                  \
  } else {                                                 \
    printf("enabled\n");                                   \
  }

#define SYSCALL_STATUS(syscall, default_args) SYSCALL_STATUS_LABEL(#syscall, syscall, default_args)

  // TODO(scottmg): Add an output backend to zither to emit a full list of stub
  // calls to include here.
  size_t actual;
  SYSCALL_STATUS(zx_debug_read, (ZX_HANDLE_INVALID, nullptr, 0, &actual));
  SYSCALL_STATUS(zx_debug_send_command, (ZX_HANDLE_INVALID, nullptr, 0));
  SYSCALL_STATUS(zx_debug_write, (nullptr, 0));
  SYSCALL_STATUS(zx_process_write_memory, (ZX_HANDLE_INVALID, 0, nullptr, 0, &actual));
  SYSCALL_STATUS_LABEL("zx_thread_write_state(ZX_THREAD_STATE_DEBUG_REGS)", zx_thread_write_state,
                       (zx_thread_self(), ZX_THREAD_STATE_DEBUG_REGS, nullptr, 0));
  SYSCALL_STATUS_LABEL("zx_thread_write_state(ZX_THREAD_STATE_SINGLE_STEP)", zx_thread_write_state,
                       (zx_thread_self(), ZX_THREAD_STATE_SINGLE_STEP, nullptr, 0));
  SYSCALL_STATUS(zx_vmo_op_range,
                 (ZX_HANDLE_INVALID, ZX_VMO_OP_CACHE_INVALIDATE, 0, 0, nullptr, 0));

#undef SYSCALL_STATUS
#undef SYSCALL_STATUS_LABEL

  return 0;
}
