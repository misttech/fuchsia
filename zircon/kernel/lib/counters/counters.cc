// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/counters.h>
#include <lib/special-sections/special-sections.h>
#include <platform.h>
#include <stdlib.h>

#include <arch/ops.h>
#include <kernel/percpu.h>
#include <kernel/spinlock.h>
#include <lk/init.h>

// kernel.ld uses this and fills in the descriptor table size after it and then
// places the sorted descriptor table after that (and then pads to page size),
// so as to fully populate the counters::DescriptorVmo layout.
static const uint64_t vmo_header SPECIAL_SECTION(".kcounter.desc.header", uint64_t)[] = {
    counters::DescriptorVmo::kMagic,
    SMP_MAX_CPUS,
};
static_assert(sizeof(vmo_header) == offsetof(counters::DescriptorVmo, descriptor_table_size));

// This counter tracks how long it takes for Zircon to reach the last init level
// It also can show if the target does not reset the internal clock upon reboot
// which is true also for mexec (netboot) scenario.
KCOUNTER(init_time, "init.target.time.msec")

static void counters_init(unsigned level) { init_time.Add(current_mono_time() / 1000000LL); }

LK_INIT_HOOK(kcounters, counters_init, LK_INIT_LEVEL_USER - 1)

// Provide access to kcounters to Rust.
extern "C" {
int64_t kcounter_sum_across_all_cpus_ffi(const counters::Descriptor* desc);
int64_t kcounter_max_across_all_cpus_ffi(const counters::Descriptor* desc);
int64_t kcounter_min_across_all_cpus_ffi(const counters::Descriptor* desc);
int64_t kcounter_value_curr_cpu_ffi(const counters::Descriptor* desc);
void kcounter_set_ffi(const counters::Descriptor* desc, uint64_t delta);
void kcounter_add_ffi(const counters::Descriptor* desc, int64_t delta);
void kcounter_min_ffi(const counters::Descriptor* desc, int64_t value);
void kcounter_max_ffi(const counters::Descriptor* desc, int64_t value);

int64_t kcounter_sum_across_all_cpus_ffi(const counters::Descriptor* desc) {
  return Counter(desc).SumAcrossAllCpus();
}
int64_t kcounter_max_across_all_cpus_ffi(const counters::Descriptor* desc) {
  return Counter(desc).MaxAcrossAllCpus();
}
int64_t kcounter_min_across_all_cpus_ffi(const counters::Descriptor* desc) {
  return Counter(desc).MinAcrossAllCpus();
}
int64_t kcounter_value_curr_cpu_ffi(const counters::Descriptor* desc) {
  return Counter(desc).ValueCurrCpu();
}
void kcounter_set_ffi(const counters::Descriptor* desc, uint64_t delta) {
  Counter(desc).Set(delta);
}
void kcounter_add_ffi(const counters::Descriptor* desc, int64_t delta) { Counter(desc).Add(delta); }
void kcounter_min_ffi(const counters::Descriptor* desc, int64_t value) { Counter(desc).Min(value); }
void kcounter_max_ffi(const counters::Descriptor* desc, int64_t value) { Counter(desc).Max(value); }
}
