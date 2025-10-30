// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/affine/ratio.h>
#include <lib/concurrent/seqlock.inc.h>
#include <lib/fasttime/clock.h>

#include "src/starnix/kernel/vdso/vdso_calculate_time.h"
#include "src/starnix/kernel/vdso/vdso_platform.h"

// This structure implements the methods needed to satisfy the ClockTransformationAdapter concept
// used by the fasttime library to read memory-mapped clocks.
struct StarnixClockTransformationAdapter {
  static void ArchYield() {}
  static zx_instant_mono_ticks_t GetMonoTicks() { return calculate_monotonic_ticks(); }
  static zx_instant_boot_ticks_t GetBootTicks() { return calculate_boot_ticks(); }
};

using StarnixClockTransformation = fasttime::ClockTransformation<StarnixClockTransformationAdapter>;

// This is performance sensitive code. Run benchmarks before
// and after to verify the impact of changes. At the time of this writing,
// the relevant setup was:
//
// ```
// fx set ... --with-test //src/starnix/tests:gvisor:tests
// ```
//
// then for example:
//
// ```
// fx test starnix_gvisor_clock_gettime_benchmark -o
// ```
//
// See for example of impact: https://fxbug.dev/456248727
int64_t calculate_utc_time_nsec() {
  // SAFETY: initialization should ensure that `vvar` is placed at
  // start of the memory region mapped to the memory-mapped UTC clock. Note, however
  // as we declared this as a `char` in the linker script, not an array, so don't
  // forget to take a reference to convert it to a pointer. Don't ask how I know. :)
  auto* actual_utc_clock = reinterpret_cast<StarnixClockTransformation*>(&vvar);

  zx_clock_details_v1_t clock_details;
  zx_status_t status = actual_utc_clock->GetDetails(&clock_details);
  if (status != ZX_OK) {
    return kUtcInvalid;
  }

  // Check if the UTC clock was started. Avoids a read if it is not.
  // `reference_ticks` is only zero on an unstarted clock.
  if (clock_details.reference_to_synthetic.rate.reference_ticks != 0) {
    // The UTC clock is started.
    int64_t maybe_utc_now = 0;
    status = actual_utc_clock->Read(&maybe_utc_now);
    if (status != ZX_OK) {
      // Hopefully someone notices the nonsense return value. There isn't much
      // wiggle room in error reporting.
      return kUtcInvalid;
    }
    return maybe_utc_now;
  }

  // The UTC clock is not started. Manufacture a timestamp from the
  // "fake" UTC clock params. The fake UTC clock always starts from backstop UTC
  // at zero boot time, and ticks at a rate of 1sec/1sec.
  int64_t reference_boot_instant = calculate_boot_time_nsec();
  int64_t boot_to_utc_reference_offset = 0;
  int64_t boot_to_utc_synthetic_offset = clock_details.backstop_time;
  affine::Ratio boot_to_utc_ratio(1, 1);
  return boot_to_utc_ratio.Scale(reference_boot_instant - boot_to_utc_reference_offset) +
         boot_to_utc_synthetic_offset;
}
