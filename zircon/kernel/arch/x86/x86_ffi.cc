// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <arch/x86/feature.h>
#include <kernel/ffi.h>

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {
bool cpp_x86_feature_test(struct x86_cpuid_bit bit);

FFI_ALWAYS_INLINE bool cpp_x86_feature_test(struct x86_cpuid_bit bit) {
  return x86_feature_test(bit);
}
}
