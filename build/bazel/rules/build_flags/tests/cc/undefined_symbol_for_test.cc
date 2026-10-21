// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

extern void undefined_function_for_executable_link_deps_test(void);

void trigger_undefined_symbol(void) { undefined_function_for_executable_link_deps_test(); }
