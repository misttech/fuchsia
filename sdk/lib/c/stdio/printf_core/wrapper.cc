// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "wrapper.h"

#include "src/stdio/printf_core/printf_main.h"

namespace LIBC_NAMESPACE::printf_core {

constexpr cpp::string_view kNewline = "\n";
constexpr cpp::string_view kEmpty = "";

int PrintfImpl(int (*write)(std::string_view str, void* hook), void* hook, std::span<char> buffer,
               PrintfNewline newline, const char* format, va_list args) {
  struct WriteBufferHook {
    int (*write)(std::string_view str, void* hook);
    void* hook;
  } write_buffer_hook = {.write = write, .hook = hook};

  FlushingBuffer write_buffer{
      buffer.data(),
      buffer.size(),
      [](cpp::string_view str, void* arg) -> int {
        auto [write, hook] = *static_cast<const WriteBufferHook*>(arg);
        return write({str.data(), str.size()}, hook);
      },
      &write_buffer_hook,
  };
  Writer writer{write_buffer};

  internal::ArgList arg_list{args};
  auto wrote = printf_main(&writer, format, arg_list);
  write_buffer.flush_to_stream(newline == PrintfNewline::kYes ? kNewline : kEmpty);

  // Any error code is lost, but it ultimately came from the callback anyway.
  return wrote ? static_cast<int>(*wrote) : -1;
}

}  // namespace LIBC_NAMESPACE::printf_core
