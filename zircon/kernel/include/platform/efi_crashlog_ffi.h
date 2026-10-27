// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_PLATFORM_EFI_CRASHLOG_FFI_H_
#define ZIRCON_KERNEL_INCLUDE_PLATFORM_EFI_CRASHLOG_FFI_H_

#include <stddef.h>
#include <stdio.h>
#include <zircon/boot/crash-reason.h>

struct EfiCrashlogRenderTarget {
  char* data;
  size_t len;
};

extern "C" {
EfiCrashlogRenderTarget rust_efi_crashlog_get_render_target();
void rust_efi_crashlog_finalize(zircon_crash_reason_t reason, size_t amt);
size_t rust_efi_crashlog_recover(const FILE* tgt);
void rust_efi_crashlog_enable_crashlog_uptime_updates(bool enabled);
void rust_efi_crashlog_set_last_crashlog_location(const char* data, size_t len);
}

#endif  // ZIRCON_KERNEL_INCLUDE_PLATFORM_EFI_CRASHLOG_FFI_H_
