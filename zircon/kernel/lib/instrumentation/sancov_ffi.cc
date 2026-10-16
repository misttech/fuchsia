// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "private.h"

#ifndef HAVE_SANCOV
#error "build system regression"
#endif

#if !HAVE_SANCOV

InstrumentationDataVmo SancovGetPcVmo() { return {}; }
InstrumentationDataVmo SancovGetCountsVmo() { return {}; }

#else  // HAVE_SANCOV

#include <lib/instrumentation/kernel-mapped-vmo.h>
#include <stdint.h>

#include <ktl/string_view.h>
#include <lk/init.h>

#include <ktl/enforce.h>

extern "C" {

size_t rust_sancov_data_size(size_t guards_count);
void rust_sancov_init(uint32_t* guards_start, size_t guards_count, KernelMappedVmo* pc_vmo,
                      KernelMappedVmo* counts_vmo);
void rust_sancov_trace_pc_guard_init(const uint32_t* start, const uint32_t* end,
                                     const uint32_t* expected_start, const uint32_t* expected_end);
void rust_sancov_trace_pc_guard(uint32_t* guard_ptr, const uint32_t* guards_start,
                                uintptr_t return_address, uintptr_t executable_start);

}  // extern "C"

namespace {

// The sancov tool matches "<binaryname>" to "<binaryname>.%u.sancov".
constexpr ktl::string_view kPcVmoName = "data/vmzircon.1.sancov";

// This follows the sancov PCs file name just for consistency.
constexpr ktl::string_view kCountsVmoName = "data/vmzircon.1.sancov-counts";

// These are defined by the linker script.  The __sancov_guards section is
// populated by the compiler with one slot corresponding to each instrumented
// PC location.
extern "C" uint32_t __start___sancov_guards[], __stop___sancov_guards[];

[[gnu::const]] size_t GuardsCount() { return __stop___sancov_guards - __start___sancov_guards; }

[[gnu::const]] size_t DataSize() { return rust_sancov_data_size(GuardsCount()); }

KernelMappedVmo gSancovPcVmo, gSancovCountsVmo;

void InitSancov(uint level) {
  rust_sancov_init(__start___sancov_guards, GuardsCount(), &gSancovPcVmo, &gSancovCountsVmo);
}

// This needs to happen after the full VM system is available, but while the
// kernel is still running only in the initial thread on the boot CPU.
LK_INIT_HOOK(InitSancov, InitSancov, LK_INIT_LEVEL_KERNEL)

}  // namespace

extern "C" {

// This is run along with static constructors, pretty early in startup.
// It's always run on the boot CPU before secondary CPUs are started up.
void __sanitizer_cov_trace_pc_guard_init(uint32_t* start, uint32_t* end) {
  rust_sancov_trace_pc_guard_init(start, end, __start___sancov_guards, __stop___sancov_guards);
}

// This is called every time through a covered event.
// This might be run before __sanitizer_cov_trace_pc_guard_init has run.
void __sanitizer_cov_trace_pc_guard(uint32_t* guard_ptr) {
  rust_sancov_trace_pc_guard(guard_ptr, __start___sancov_guards,
                             reinterpret_cast<uintptr_t>(__builtin_return_address(0)),
                             reinterpret_cast<uintptr_t>(__executable_start));
}

}  // extern "C"

InstrumentationDataVmo SancovGetPcVmo() {
  return {
      .announce = "SanitizerCoverage",
      .sink_name = "sancov",
      .units = "PCs",
      .scale = sizeof(uint64_t),
      .handle = gSancovPcVmo.Publish(kPcVmoName, DataSize()),
  };
}

InstrumentationDataVmo SancovGetCountsVmo() {
  return {
      .announce = "SanitizerCoverage Counts",
      .sink_name = "sancov-counts",
      .units = "counters",
      .scale = sizeof(uint64_t),
      .handle = gSancovCountsVmo.Publish(kCountsVmoName, DataSize()),
  };
}

#endif  // HAVE_SANCOV
