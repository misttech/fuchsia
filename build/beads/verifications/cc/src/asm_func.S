// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "asm_func.h"

#if !defined(EXAMPLE_MACRO) || EXAMPLE_MACRO != 1
#error "EXAMPLE_MACRO is not properly defined!"
#endif

.text
.globl ASM_EXAMPLE_SYMBOL
#if defined(__x86_64__)
ASM_EXAMPLE_SYMBOL:
    mov %edi, %eax
    ret
#elif defined(__aarch64__)
ASM_EXAMPLE_SYMBOL:
    ret
#else
ASM_EXAMPLE_SYMBOL:
    ret
#endif
