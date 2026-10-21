// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/types.h>

#include <arch/user_copy.h>

#include "user-copy.h"

// CopyContext crosses to the Rust copy routines as its underlying value.
static_assert(static_cast<uint8_t>(CopyContext::kBlockingAllowed) == 0);
static_assert(static_cast<uint8_t>(CopyContext::kBlockingNotAllowed) == 1);

// FFI wrappers to unpack the Rust result into UserCopyCaptureFaultsResult, which contains a
// ktl::optional that is currently not possible to get across the language barrier.
UserCopyCaptureFaultsResult arch_copy_from_user_capture_faults(void* dst, const void* src,
                                                               size_t len, CopyContext context) {
  Arm64UserCopyCaptureFaultsRet ret =
      rust_arch_copy_from_user_capture_faults(dst, src, len, context);
  if (ret.has_fault_info) {
    return {ret.copy.status, {ret.copy.pf_va, ret.copy.pf_flags}};
  }
  return UserCopyCaptureFaultsResult{ret.copy.status};
}

UserCopyCaptureFaultsResult arch_copy_to_user_capture_faults(void* dst, const void* src, size_t len,
                                                             CopyContext context) {
  Arm64UserCopyCaptureFaultsRet ret = rust_arch_copy_to_user_capture_faults(dst, src, len, context);
  if (ret.has_fault_info) {
    return {ret.copy.status, {ret.copy.pf_va, ret.copy.pf_flags}};
  }
  return UserCopyCaptureFaultsResult{ret.copy.status};
}
