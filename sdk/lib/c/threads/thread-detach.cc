// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <cassert>

#include "thread-list.h"
#include "thread-storage.h"
#include "thread.h"

namespace LIBC_NAMESPACE_DECL {

zx::result<> ThreadDetach(Thread& thread) {
  // This does the synchronization with the thread exit.
  zx_status_t status = zxr_thread_detach(&thread.zxr_thread);
  switch (status) {
    case ZX_OK:
      return zx::ok();

    case ZX_ERR_BAD_STATE:
      // It already died before it knew to deallocate itself.
      break;

    case ZX_ERR_INVALID_ARGS:
      // The thread isn't joinable.  It was already joined or detached.
      return zx::error{ZX_ERR_INVALID_ARGS};

    default:
      return zx::error{ZX_ERR_NOT_FOUND};
  }

  // Now the Thread object can be removed from the list of all threads.
  __thread_list_erase(&thread);

  // Move the ThreadStorage out of the Thread itself; it's inside that storage.
  // Then immediately let the storage object die, freeing the thread block.
  [[maybe_unused]] auto storage = ThreadStorage::FromThread(thread, true);
  assert(storage.vmar());

  return zx::ok();
}

}  // namespace LIBC_NAMESPACE_DECL
