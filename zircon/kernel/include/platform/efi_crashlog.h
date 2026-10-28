// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_PLATFORM_EFI_CRASHLOG_H_
#define ZIRCON_KERNEL_INCLUDE_PLATFORM_EFI_CRASHLOG_H_

#include <ktl/span.h>
#include <ktl/string_view.h>
#include <platform/crashlog.h>
#include <platform/efi_crashlog_ffi.h>

class EfiCrashlog final : public PlatformCrashlog::Interface {
 public:
  EfiCrashlog() = default;

  ktl::span<char> GetRenderTarget() final {
    EfiCrashlogRenderTarget target = rust_efi_crashlog_get_render_target();
    return {target.data, target.len};
  }

  void Finalize(zircon_crash_reason_t reason, size_t amt) final {
    rust_efi_crashlog_finalize(reason, amt);
  }

  size_t Recover(FILE* tgt) final { return rust_efi_crashlog_recover(tgt); }

  void EnableCrashlogUptimeUpdates(bool enabled) final {
    rust_efi_crashlog_enable_crashlog_uptime_updates(enabled);
  }

  static void SetLastCrashlogLocation(ktl::string_view last_crashlog) {
    rust_efi_crashlog_set_last_crashlog_location(last_crashlog.data(), last_crashlog.size());
  }
};

#endif  // ZIRCON_KERNEL_INCLUDE_PLATFORM_EFI_CRASHLOG_H_
