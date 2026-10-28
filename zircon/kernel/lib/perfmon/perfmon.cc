// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// A note on the distribution of code between us and the userspace driver:
// The default location for code is the userspace driver. Reasons for
// putting code here are: implementation requirement (need ring zero to write
// MSRs), stability, and performance. The device driver should do as much
// error checking as possible before calling us.
// Note that we do a lot of verification of the input configuration:
// We don't want to be compromised if the userspace driver gets compromised.

// A note on terminology: "events" vs "counters": A "counter" is an
// "event", but some events are not counters. Internally, we use the
// term "counter" when we know the event is a counter.

#include <lib/perfmon.h>

#include <ktl/enforce.h>

PerfmonStateBase::PerfmonStateBase(unsigned n_cpus) { rust_perfmon_state_base_init(this, n_cpus); }

PerfmonStateBase::~PerfmonStateBase() { rust_perfmon_state_base_destroy(this); }

bool PerfmonStateBase::AllocatePerCpuData() {
  return rust_perfmon_state_base_allocate_per_cpu_data(this);
}
