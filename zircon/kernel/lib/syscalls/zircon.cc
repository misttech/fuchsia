// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <inttypes.h>
#include <lib/counters.h>
#include <lib/crypto/global_prng.h>
#include <lib/syscalls/forward.h>
#include <lib/user_copy/user_ptr.h>
#include <platform.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/syscalls/log.h>
#include <zircon/syscalls/policy.h>
#include <zircon/types.h>

#include <explicit-memory/bytes.h>
#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <kernel/thread.h>
#include <ktl/algorithm.h>
#include <ktl/atomic.h>
#include <object/event_dispatcher.h>
#include <object/handle.h>
#include <object/log_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/resource.h>
#include <object/thread_dispatcher.h>

#include <ktl/enforce.h>

#define LOCAL_TRACE 0

KCOUNTER(syscalls_zx_ticks_get, "syscalls.zx_ticks_get")
KCOUNTER(syscalls_zx_ticks_get_boot, "syscalls.zx_ticks_get_boot")
KCOUNTER(syscalls_zx_clock_get_monotonic, "syscalls.zx_clock_get_monotonic")
KCOUNTER(syscalls_zx_clock_get_boot, "syscalls.zx_clock_get_boot")
KCOUNTER(syscalls_zx_nanosleep, "syscalls.zx_nanosleep")
KCOUNTER(syscalls_zx_nanosleep_zero_duration, "syscalls.zx_nanosleep_zero_duration")

constexpr size_t kMaxCPRNGDraw = ZX_CPRNG_DRAW_MAX_LEN;
constexpr size_t kMaxCPRNGSeed = ZX_CPRNG_ADD_ENTROPY_MAX_LEN;

// zx_status_t zx_nanosleep
zx_status_t sys_nanosleep(zx_instant_mono_t deadline) {
  LTRACEF("nseconds %" PRIi64 "\n", deadline);
  kcounter_add(syscalls_zx_nanosleep, 1);

  if (deadline <= 0) {
    kcounter_add(syscalls_zx_nanosleep_zero_duration, 1);
    return ZX_OK;
  }

  const zx_instant_mono_t now = current_mono_time();
  const auto up = ProcessDispatcher::GetCurrent();
  const Deadline slackDeadline(deadline, up->GetTimerSlackPolicy());

  ThreadDispatcher::AutoBlocked by(ThreadDispatcher::Blocked::SLEEPING);

  // This syscall is declared as "blocking", so a higher layer will automatically
  // retry if we return ZX_ERR_INTERNAL_INTR_RETRY.
  return Thread::Current::SleepEtc(slackDeadline, Interruptible::Yes, now);
}

zx_instant_mono_t sys_clock_get_monotonic_via_kernel() {
  kcounter_add(syscalls_zx_clock_get_monotonic, 1);
  return current_mono_time();
}

zx_instant_boot_t sys_clock_get_boot_via_kernel() {
  kcounter_add(syscalls_zx_clock_get_boot, 1);
  return current_boot_time();
}

zx_instant_mono_ticks_t sys_ticks_get_via_kernel() {
  kcounter_add(syscalls_zx_ticks_get, 1);
  return current_mono_ticks();
}

zx_instant_boot_ticks_t sys_ticks_get_boot_via_kernel() {
  kcounter_add(syscalls_zx_ticks_get_boot, 1);
  return current_boot_ticks();
}

// zx_status_t zx_cprng_draw_once
zx_status_t sys_cprng_draw_once(user_out_ptr<void> buffer, size_t len) {
  if (len > kMaxCPRNGDraw)
    return ZX_ERR_INVALID_ARGS;

  uint8_t kernel_buf[kMaxCPRNGDraw];
  // Ensure we get rid of the stack copy of the random data as this function returns.
  explicit_memory::ZeroDtor<uint8_t> zero_guard(kernel_buf, sizeof(kernel_buf));

  auto prng = crypto::global_prng::GetInstance();
  ASSERT(prng->is_thread_safe());
  prng->Draw(kernel_buf, len);

  if (buffer.reinterpret<uint8_t>().copy_array_to_user(kernel_buf, len) != ZX_OK)
    return ZX_ERR_INVALID_ARGS;
  return ZX_OK;
}

// zx_status_t zx_cprng_add_entropy
zx_status_t sys_cprng_add_entropy(user_in_ptr<const void> buffer, size_t buffer_size) {
  if (buffer_size > kMaxCPRNGSeed)
    return ZX_ERR_INVALID_ARGS;

  uint8_t kernel_buf[kMaxCPRNGSeed];
  // Ensure we get rid of the stack copy of the entropy as this function
  // returns.
  explicit_memory::ZeroDtor<uint8_t> zero_guard(kernel_buf, sizeof(kernel_buf));

  if (buffer.reinterpret<const uint8_t>().copy_array_from_user(kernel_buf, buffer_size) != ZX_OK)
    return ZX_ERR_INVALID_ARGS;

  auto prng = crypto::global_prng::GetInstance();
  ASSERT(prng->is_thread_safe());
  prng->AddEntropy(kernel_buf, buffer_size);

  return ZX_OK;
}
