// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//

#include <platform.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <ktl/enforce.h>

extern "C" {

void rust_platform_halt_cpu();
bool rust_platform_supports_suspend_cpu();
zx_status_t rust_platform_suspend_cpu(bool allow_domain);
bool rust_platform_supports_suspend_system();
zx_status_t rust_platform_suspend_system();
void rust_platform_panic_start(bool halt_others);
void rust_platform_specific_halt(platform_halt_action suggested_action,
                                 zircon_crash_reason_t reason, bool halt_on_panic) __NO_RETURN;

}  // extern "C"

void platform_halt_cpu() { rust_platform_halt_cpu(); }

bool platform_supports_suspend_cpu() { return rust_platform_supports_suspend_cpu(); }

zx_status_t platform_suspend_cpu(PlatformAllowDomainPowerDown allow_domain) {
  return rust_platform_suspend_cpu(allow_domain == PlatformAllowDomainPowerDown::Yes);
}

bool platform_supports_suspend_system() { return rust_platform_supports_suspend_system(); }

zx_status_t platform_suspend_system() { return rust_platform_suspend_system(); }

void platform_panic_start(PanicStartHaltOtherCpus option) {
  rust_platform_panic_start(option == PanicStartHaltOtherCpus::Yes);
}

void platform_specific_halt(platform_halt_action suggested_action, zircon_crash_reason_t reason,
                            bool halt_on_panic) {
  rust_platform_specific_halt(suggested_action, reason, halt_on_panic);
}
