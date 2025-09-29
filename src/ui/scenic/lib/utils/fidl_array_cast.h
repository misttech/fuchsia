// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fidl/cpp/wire/array.h>

#include <type_traits>

#ifndef SRC_UI_SCENIC_LIB_UTILS_FIDL_ARRAY_CAST_H_
#define SRC_UI_SCENIC_LIB_UTILS_FIDL_ARRAY_CAST_H_

namespace utils {

template <typename T, size_t N>
fidl::Array<T, N>& ReinterpretStdArrayAsFidlArray(std::array<T, N>& std_array) {
  using FidlArray = fidl::Array<T, N>;
  using StdArray = std::array<T, N>;
  static_assert(sizeof(FidlArray) == sizeof(StdArray));
  static_assert(alignof(FidlArray) == alignof(StdArray));
  static_assert(std::is_standard_layout_v<FidlArray>);
  static_assert(std::is_standard_layout_v<StdArray>);
  static_assert(std::is_trivially_copyable_v<FidlArray>);
  static_assert(std::is_trivially_copyable_v<StdArray>);

  return *reinterpret_cast<FidlArray*>(&std_array);
}

}  // namespace utils

#endif  // SRC_UI_SCENIC_LIB_UTILS_FIDL_ARRAY_CAST_H_
