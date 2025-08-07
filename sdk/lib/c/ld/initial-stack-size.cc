// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/ld/abi.h>

#include "../startup/start-main.h"

namespace LIBC_NAMESPACE_DECL {

PageRoundedSize InitialStackSize() { return PageRoundedSize{ld::abi::_ld_abi.stack_size}; }

}  // namespace LIBC_NAMESPACE_DECL
