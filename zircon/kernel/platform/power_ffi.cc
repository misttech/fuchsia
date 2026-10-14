// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <platform.h>
#include <zircon/boot/crash-reason.h>

#include <arch/regs.h>
#include <kernel/ffi.h>
#include <platform/halt_token.h>

static_assert(sizeof(platform_halt_action) == sizeof(uint32_t));
static_assert(alignof(platform_halt_action) == alignof(uint32_t));
static_assert(HALT_ACTION_HALT == 0);
static_assert(HALT_ACTION_REBOOT == 1);
static_assert(HALT_ACTION_REBOOT_BOOTLOADER == 2);
static_assert(HALT_ACTION_REBOOT_RECOVERY == 3);
static_assert(HALT_ACTION_SHUTDOWN == 4);

static_assert(sizeof(zircon_crash_reason_t) == sizeof(uint32_t));
static_assert(alignof(zircon_crash_reason_t) == alignof(uint32_t));
static_assert(static_cast<uint32_t>(ZirconCrashReason::Invalid) == 0);
static_assert(static_cast<uint32_t>(ZirconCrashReason::Unknown) == 1);
static_assert(static_cast<uint32_t>(ZirconCrashReason::NoCrash) == 2);
static_assert(static_cast<uint32_t>(ZirconCrashReason::Oom) == 3);
static_assert(static_cast<uint32_t>(ZirconCrashReason::Panic) == 4);
static_assert(static_cast<uint32_t>(ZirconCrashReason::SoftwareWatchdog) == 5);
static_assert(static_cast<uint32_t>(ZirconCrashReason::UserspaceRootJobTermination) == 6);

// Storage for the global halt token singleton
HaltToken HaltToken::g_instance;

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_platform_specific_halt(platform_halt_action suggested_action,
                                                  zircon_crash_reason_t reason,
                                                  bool halt_on_panic) {
  platform_specific_halt(suggested_action, reason, halt_on_panic);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_platform_panic_start() { platform_panic_start(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_platform_halt_cpu() { platform_halt_cpu(); }

}  // extern "C"
