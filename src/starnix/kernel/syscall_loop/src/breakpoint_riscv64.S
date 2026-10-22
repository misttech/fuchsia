// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <starnix_asm_utils/asm.h>

FUNCTION(breakpoint_for_module_changes)
  ebreak
  ret
END_FUNCTION(breakpoint_for_module_changes)
