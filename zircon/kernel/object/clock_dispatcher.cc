// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/clock_dispatcher.h"

#include <lib/arch/intrin.h>
#include <lib/fasttime/clock.h>

#include <platform/timer.h>

#include <ktl/enforce.h>

namespace fasttime {

// Supply the kernel specific implementation of ArchYield and reference timer
// accessors in order to describe libfasttime's ClockTransformation class as the
// kernel instantiates it.
struct KernelClockTransformationAdapter {
  static inline void ArchYield() { arch::Yield(); }
  static zx_instant_mono_ticks_t GetMonoTicks() { return current_mono_ticks(); }
  static zx_instant_boot_ticks_t GetBootTicks() { return current_boot_ticks(); }
};

// The clock transformation for a mappable clock lives in a page which is shared
// with (and read directly by) user mode: the VDSO reinterpret_casts the mapping
// to a `fasttime::ClockTransformation<Adapter>`.  See
// //zircon/kernel/lib/userabi/vdso/zx_clock_read_mapped.cc,
// //zircon/kernel/lib/userabi/vdso/zx_clock_get_details_mapped.cc, and
// //src/starnix/kernel/vdso/vdso_calculate_utc.cc.
//
// The only producer of that memory is the Rust `ClockTransformation` in
// //zircon/kernel/object/clock_dispatcher.rs, which pins its own layout with a
// matching set of assertions.  The assertions below pin the C++ view of the
// very same bytes, so that the two can never drift apart silently.
struct ClockTransformationLayoutCheck {
  using Transformation = ClockTransformation<KernelClockTransformationAdapter>;
  using Params = Transformation::Params;

  static_assert(sizeof(Transformation) <= ClockDispatcher::kMappedSize);
  static_assert(alignof(Transformation) <= ClockDispatcher::kMappedSize);

  static_assert(sizeof(Transformation) == 112);
  static_assert(alignof(Transformation) == 8);
  static_assert(offsetof(Transformation, options_) == 0);
  static_assert(offsetof(Transformation, backstop_time_) == 8);
  static_assert(offsetof(Transformation, seq_lock_) == 16);
  static_assert(offsetof(Transformation, reference_ticks_to_synthetic_) == 24);
  static_assert(offsetof(Transformation, params_) == 48);

  static_assert(sizeof(Params) == 64);
  static_assert(alignof(Params) == 8);
  static_assert(offsetof(Params, reference_to_synthetic) == 0);
  static_assert(offsetof(Params, error_bound) == 24);
  static_assert(offsetof(Params, last_value_update_ticks) == 32);
  static_assert(offsetof(Params, last_rate_adjust_update_ticks) == 40);
  static_assert(offsetof(Params, last_error_bounds_update_ticks) == 48);
  static_assert(offsetof(Params, cur_ppm_adj) == 56);
  static_assert(offsetof(Params, _padding) == 60);

  // The storage the Rust `ClockDispatcherState` reserves for a non-mappable
  // clock must be able to hold the transformation.
  static_assert(sizeof(Transformation) <= kClockTransformationStorageSize);
  static_assert(alignof(Transformation) <= kClockTransformationStorageAlign);
};

}  // namespace fasttime

ClockDispatcher::ClockDispatcher(uint64_t options, zx_time_t backstop_time,
                                 fbl::RefPtr<VmObjectPaged> vmo)
    : Dispatcher(0) {
  DISPATCHER_VERIFY_OFFSET(ClockDispatcher, kClockDispatcherStateOffset);
  rust_clock_dispatcher_state_init(&opaque_storage_, *this, options, backstop_time,
                                   fbl::ExportToRawPtr(&vmo));
}

IMPLEMENT_DISPATCHER_RUST_STATE(ClockDispatcher, rust_clock_dispatcher_state_get_lock,
                                rust_clock_dispatcher_state_destroy)
