// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/persistent-debuglog.h>

void persistent_dlog_init_early() { rust_persistent_dlog_init_early(); }

void persistent_dlog_set_location(void* vaddr, size_t len) {
  rust_persistent_dlog_set_location(vaddr, len);
}

void persistent_dlog_write(const ktl::string_view str) {
  rust_persistent_dlog_write(str.data(), str.size());
}

void persistent_dlog_invalidate() { rust_persistent_dlog_invalidate(); }

ktl::string_view persistent_dlog_get_recovered_log() {
  size_t len = 0;
  const char* ptr = rust_persistent_dlog_get_recovered_log(&len);
  return ktl::string_view{ptr, len};
}
