// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/tests/utils/promise.h"

#include <lib/async/cpp/executor.h>
#include <lib/fpromise/promise.h>

namespace integration_tests {

bool RunPromise(async_dispatcher_t* dispatcher, fit::function<void()> run,
                fpromise::promise<> promise) {
  async::Executor executor(dispatcher);
  bool success = false;
  executor.schedule_task(
      promise.then([&success](fpromise::result<>& result) { success = result.is_ok(); }));
  run();
  return success;
}

}  // namespace integration_tests
