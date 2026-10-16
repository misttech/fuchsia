// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef BUILD_BEADS_VERIFICATIONS_CC_INCLUDE_ASM_FUNC_H_
#define BUILD_BEADS_VERIFICATIONS_CC_INCLUDE_ASM_FUNC_H_

#if !defined(EXAMPLE_MACRO) || EXAMPLE_MACRO != 1
#error "EXAMPLE_MACRO is not properly defined!"
#endif

#define ASM_EXAMPLE_SYMBOL asm_example_identity

#ifndef __ASSEMBLER__
#ifdef __cplusplus
extern "C" {
#endif

int asm_example_identity(int x);

#ifdef __cplusplus
}
#endif
#endif  // !__ASSEMBLER__

#endif  // BUILD_BEADS_VERIFICATIONS_CC_INCLUDE_ASM_FUNC_H_
