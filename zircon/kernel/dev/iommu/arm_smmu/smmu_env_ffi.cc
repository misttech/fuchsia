// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// C++ half of the `SmmuEnv` environment seam.
//
// In production kernel builds (`!defined(ARM_SMMU_HOST_TEST)`), these `extern "C"` shims forward
// to the Zircon kernel thread sleep and arm64 peripheral map helpers. In standalone host test
// builds (`defined(ARM_SMMU_HOST_TEST)`), additional entry points resolve against the host
// simulation mocks (`PmmMock`, `InterruptsMock`, `ObjectPinTracker`, `PteWriteLog`, and
// `FakeSmmuMmioRouter`).

#include "smmu_env_ffi.h"

#include <stddef.h>
#include <stdint.h>
#include <zircon/types.h>

#include <kernel/ffi.h>

#if !defined(ARM_SMMU_HOST_TEST)
#include <kernel/thread.h>
#else
#include <lib/arch/cache.h>
#include <zircon/errors.h>

#include <kernel/spinlock.h>
#endif

extern "C" {

// Sleeps the current thread for `delay_ns` nanoseconds (relative).
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
zx_status_t cpp_arm_smmu_sleep_relative(zx_duration_t delay_ns) {
#if !defined(ARM_SMMU_HOST_TEST)
  return Thread::Current::SleepRelative(delay_ns);
#else
  Thread::Current::SleepRelative(delay_ns);
  return ZX_OK;
#endif
}

}  // extern "C"

#if defined(ARM_SMMU_HOST_TEST)

extern "C" {

// Cleans CPU data cache lines covering `[start, start + len)` to the Point of Coherency (PoC).
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_clean_dcache_range(uintptr_t start, size_t len) {
  arch::CleanDataCacheRange(start, len);
}

// Notifies the host simulation hardware model of a PTE write before storing `*slot = new_val`.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE
void cpp_arm_smmu_on_pte_write(const uint64_t* slot, uint64_t new_val, uint32_t level) {
  arch::OnPteWrite(slot, new_val, level);
}

}  // extern "C"
#endif  // defined(ARM_SMMU_HOST_TEST)
