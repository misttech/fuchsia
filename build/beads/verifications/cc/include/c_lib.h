// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef BUILD_BEADS_VERIFICATIONS_CC_INCLUDE_C_LIB_H_
#define BUILD_BEADS_VERIFICATIONS_CC_INCLUDE_C_LIB_H_

#if !defined(EXAMPLE_MACRO) || EXAMPLE_MACRO != 1
#error "EXAMPLE_MACRO is not properly defined!"
#endif

#ifdef __cplusplus
extern "C" {
#endif

int c_example_add(int a, int b);

#ifdef __cplusplus
}
#endif

#endif  // BUILD_BEADS_VERIFICATIONS_CC_INCLUDE_C_LIB_H_
