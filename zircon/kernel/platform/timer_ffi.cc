// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <platform/timer.h>

extern "C" {

zx_instant_mono_ticks_t cpp_timer_current_mono_ticks();
zx_instant_boot_ticks_t cpp_timer_current_boot_ticks();
zx_instant_mono_t cpp_current_mono_time();
zx_instant_boot_t cpp_current_boot_time();

zx_instant_mono_ticks_t cpp_timer_current_mono_ticks() { return timer_current_mono_ticks(); }
zx_instant_boot_ticks_t cpp_timer_current_boot_ticks() { return timer_current_boot_ticks(); }
zx_instant_mono_t cpp_current_mono_time() { return current_mono_time(); }
zx_instant_boot_t cpp_current_boot_time() { return current_boot_time(); }

}  // extern "C"
