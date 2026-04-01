// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_TESTS_UTILS_PROMISE_H_
#define SRC_UI_SCENIC_TESTS_UTILS_PROMISE_H_

#include <lib/async/dispatcher.h>
#include <lib/fpromise/promise.h>

namespace integration_tests {

// Runs the given promise and waits for it to complete.
bool RunPromise(async_dispatcher_t* dispatcher, fit::function<void()> run,
                fpromise::promise<> promise);

}  // namespace integration_tests

#endif  // SRC_UI_SCENIC_TESTS_UTILS_PROMISE_H_
