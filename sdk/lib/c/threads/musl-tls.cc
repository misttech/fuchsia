// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/assert.h>

#include "libc.h"
#include "thread-storage.h"
#include "threads_impl.h"

namespace LIBC_NAMESPACE_DECL {

// This completes the ThreadStorage implementation when using the legacy
// integrated dynamic linker.

elfldltl::TlsLayout<> ThreadStorage::GetTlsLayout() {
  // The tls_size actually includes space for the initial DTV array too, but
  // that's off the end of where static TLS block assignments went, so it's
  // equivalent to a final TLS block of sizeof(void*[libc.tls_cnt]) and
  // _dl_copy_tls will expect to use it there.
  const struct tls_layout layout = _dl_tls_layout();
  return elfldltl::TlsLayout<>{layout.size - sizeof(Thread), layout.align};
}

void ThreadStorage::InitializeTls(std::span<std::byte> thread_block, size_t tp_offset) {
  unsigned char* mem = reinterpret_cast<unsigned char*>(thread_block.data());
  pthread* td = _dl_copy_tls(mem, thread_block.size_bytes());
  [[maybe_unused]] void* tp = pthread_to_tp(td);
  [[maybe_unused]] void* mem_tp = static_cast<void*>(mem + tp_offset);
  ZX_DEBUG_ASSERT_MSG(tp == mem_tp,
                      ": $tp %p / pthread* %p != %p / [%p, %p) + tp_offset %zu"
                      "; TLS %#zx aligned to %zu; sizeof(pthread) == %zu",
                      tp, td, mem_tp, thread_block.data(), &thread_block.back() + 1, tp_offset,
                      _dl_tls_layout().size, _dl_tls_layout().align, sizeof(pthread));
}

}  // namespace LIBC_NAMESPACE_DECL
