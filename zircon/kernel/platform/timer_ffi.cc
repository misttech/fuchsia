// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <platform/timer.h>

extern "C" {

zx_instant_mono_ticks_t rust_timer_current_mono_ticks() { return timer_current_mono_ticks(); }

zx_instant_boot_ticks_t rust_timer_current_boot_ticks() { return timer_current_boot_ticks(); }

}  // extern "C"
