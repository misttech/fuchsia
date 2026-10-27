// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "debug_ffi.h"

#include <stdarg.h>
#include <zircon/compiler.h>

#include <__verbose_abort>

// Note <ktl/enforce.h> can't be used here because we need to define a function
// that was declared in the std:: namespace by <__verbose_abort>.

// Inline functions in libc++ headers call this.
[[noreturn]] void std::__libcpp_verbose_abort(const char* format, ...) noexcept {
  va_list ap;
  va_start(ap, format);
  rust_vpanic(__GET_CALLER(), __GET_FRAME(), format, ap);
}
