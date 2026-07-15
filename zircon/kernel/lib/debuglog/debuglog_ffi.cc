// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/debuglog.h>

extern "C" {

zx_status_t cpp_dlog_shutdown(zx_instant_mono_t deadline);

zx_status_t cpp_dlog_shutdown(zx_instant_mono_t deadline) { return dlog_shutdown(deadline); }

}  // extern "C"
