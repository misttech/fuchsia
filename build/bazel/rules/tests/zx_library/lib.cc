// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/stdcompat/utility.h>

#include <zx_library_test/lib.h>

int zx_library_test_call(fit::function<int()> f) { return f(); }
