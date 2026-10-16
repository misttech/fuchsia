// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef BUILD_BEADS_VERIFICATIONS_CC_INCLUDE_CC_LIB_H_
#define BUILD_BEADS_VERIFICATIONS_CC_INCLUDE_CC_LIB_H_

#if !defined(EXAMPLE_MACRO) || EXAMPLE_MACRO != 1
#error "EXAMPLE_MACRO is not properly defined!"
#endif

namespace cc_example {

int Multiply(int a, int b);

}  // namespace cc_example

#endif  // BUILD_BEADS_VERIFICATIONS_CC_INCLUDE_CC_LIB_H_
