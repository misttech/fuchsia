// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_TOP_DEBUG_FFI_H_
#define ZIRCON_KERNEL_TOP_DEBUG_FFI_H_

#include <stdarg.h>
#include <zircon/compiler.h>

extern "C" void rust_vpanic(void* pc, void* frame, const char* fmt, va_list ap) __NO_RETURN;

#endif  // ZIRCON_KERNEL_TOP_DEBUG_FFI_H_
