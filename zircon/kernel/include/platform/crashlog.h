// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_PLATFORM_CRASHLOG_H_
#define ZIRCON_KERNEL_INCLUDE_PLATFORM_CRASHLOG_H_

extern "C" {

// TODO(https://fxbug.dev/507423574): Remove once all platform callers are ported to Rust.
// FFI wrapper around PlatformCrashlog::has_non_trivial_impl for C++ callers.
bool rust_platform_crashlog_has_non_trivial_impl();

}  // extern "C"

#endif  // ZIRCON_KERNEL_INCLUDE_PLATFORM_CRASHLOG_H_
