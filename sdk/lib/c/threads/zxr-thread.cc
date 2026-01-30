// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "zxr-thread.h"

#include <atomic>

using State = zxr_thread_t::State;

bool zxr_thread_detached(zxr_thread_t* thread) {
  return thread->state.load(std::memory_order_acquire) == State::DETACHED;
}
