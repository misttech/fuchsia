// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zx/result.h>

#include <tuple>

#include "thread-list.h"
#include "thread-storage.h"
#include "thread.h"

namespace LIBC_NAMESPACE_DECL {

zx::result<intptr_t> ThreadJoin(Thread& thread) {
  // This does the synchronization with the thread exit.
  zx_status_t status = zxr_thread_join(&thread.zxr_thread);
  if (status != ZX_OK) {
    return zx::error{status};
  }

  // Now the Thread object can be removed from the list of all threads.
  AllThreads().erase(thread);

  // Extract the return value passed to ThreadExit().
  intptr_t value = std::exchange(thread.join_value, 0);

  // Move the ThreadStorage out of the Thread itself; it's inside that storage.
  // Then immediately let the storage object die, freeing the thread block.
  std::ignore = ThreadStorage::FromThread(thread, true);

  return zx::ok(value);
}

}  // namespace LIBC_NAMESPACE_DECL
