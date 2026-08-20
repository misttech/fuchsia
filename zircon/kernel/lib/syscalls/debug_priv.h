// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_SYSCALLS_DEBUG_PRIV_H_
#define ZIRCON_KERNEL_LIB_SYSCALLS_DEBUG_PRIV_H_

#include <lib/user_copy/user_ptr.h>
#include <stdbool.h>
#include <stddef.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>

__BEGIN_CDECLS

void cpp_persistent_dlog_write(const char* ptr, size_t len);
void cpp_dlog_serial_write(const char* ptr, size_t len);
zx_status_t cpp_console_run_script(const char* str);
zx_status_t cpp_ktrace_read_user(user_out_ptr<void> ptr, uint32_t offset, size_t len,
                                 size_t* out_actual);
zx_status_t cpp_ktrace_control(uint32_t action, uint32_t options);

__END_CDECLS

#endif  // ZIRCON_KERNEL_LIB_SYSCALLS_DEBUG_PRIV_H_
