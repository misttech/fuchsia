// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/kconcurrent/chainlock.h>
#include <lib/kconcurrent/chainlock_transaction.h>

#include <cstdint>

extern "C" {
void rust_chainlock_on_backoff(uint64_t backoff_count);
void rust_chainlock_update_contention_counters(zx_duration_mono_ticks_t contention_ticks);
}

void ChainLockTransaction::OnBackoff() { rust_chainlock_on_backoff(backoff_count()); }

void ChainLockTransaction::UpdateContentionCounters(zx_duration_mono_ticks_t contention_ticks) {
  rust_chainlock_update_contention_counters(contention_ticks);
}
