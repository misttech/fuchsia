// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <align.h>
#include <debug.h>
#include <lib/arch/asm.h>
#include <lib/boot-options/boot-options.h>
#include <lib/debuglog.h>
#include <lib/fit/defer.h>
#include <lib/instrumentation/asan.h>
#include <lib/page/size.h>
#include <lib/syscalls/forward.h>
#include <lib/zbi-format/kernel.h>
#include <lib/zbi-format/zbi.h>
#include <lib/zbitl/checking.h>
#include <lib/zbitl/view.h>
#include <lib/zircon-internal/macros.h>
#include <platform.h>
#include <string.h>
#include <sys/types.h>
#include <trace.h>
#include <zircon/boot/crash-reason.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/status.h>
#include <zircon/syscalls/resource.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <cstddef>
#include <cstdint>
#include <cstdio>

#include <arch/arch_ops.h>
#include <arch/mp.h>
#include <arch/ops.h>
#include <dev/hw_watchdog.h>
#include <dev/interrupt.h>
#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <kernel/cpu.h>
#include <kernel/ffi.h>
#include <kernel/mp.h>
#include <kernel/percpu.h>
#include <kernel/range_check.h>
#include <kernel/scheduler.h>
#include <kernel/thread.h>
#include <ktl/byte.h>
#include <ktl/span.h>
#include <ktl/unique_ptr.h>
#include <object/event_dispatcher.h>
#include <object/handle.h>
#include <object/job_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/vm_object_dispatcher.h>
#include <phys/handoff/handoff.h>
#include <platform/halt_helper.h>
#include <vm/handoff-end.h>
#include <vm/physmap.h>
#include <vm/pmm.h>
#include <vm/vm.h>
#include <vm/vm_aspace.h>

#include "system_priv.h"

#include <ktl/enforce.h>

#define LOCAL_TRACE 0

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE void cpp_scheduler_update_processing_rates(
    zx_cpu_performance_info_t* info, size_t count) {
  Scheduler::UpdateProcessingRates(ktl::span{info, count});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE void cpp_scheduler_update_processing_limits(zx_cpu_perf_limit_t* info,
                                                                         size_t count) {
  Scheduler::UpdateProcessingLimits(ktl::span{info, count});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE void cpp_scheduler_get_performance_scales(
    zx_cpu_performance_info_t* info, size_t count) {
  Scheduler::GetPerformanceScales(ktl::span{info, count});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE void cpp_scheduler_get_default_performance_scales(
    zx_cpu_performance_info_t* info, size_t count) {
  Scheduler::GetDefaultPerformanceScales(ktl::span{info, count});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE void cpp_scheduler_get_processing_limits(zx_cpu_perf_limit_t* info,
                                                                      size_t count) {
  Scheduler::GetProcessingLimits(ktl::span{info, count});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE size_t cpp_percpu_processor_count() {
  return percpu::processor_count();
}
