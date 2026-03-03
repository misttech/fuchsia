// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.math/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/wire_types.h>

int main() {
  // Use a C++ natural bindings method that is not defined in a header file.
  fuchsia_sysmem2::wire::BufferCollectionConstraints cpp_constraints;
  [[maybe_unused]] bool is_empty_cpp = cpp_constraints.IsEmpty();
  assert(is_empty_cpp);

  fuchsia_math::Vec vec(1, 2);
  return vec.x() + vec.y();
}
