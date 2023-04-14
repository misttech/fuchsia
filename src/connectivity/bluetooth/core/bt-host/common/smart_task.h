// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_BLUETOOTH_CORE_BT_HOST_COMMON_SMART_TASK_H_
#define SRC_CONNECTIVITY_BLUETOOTH_CORE_BT_HOST_COMMON_SMART_TASK_H_

#include <pw_async/dispatcher.h>

namespace bt {

// SmartTask is a utility that wraps a pw::async::Task and adds features like
// cancelation upon destruction and state tracking.
class SmartTask {
 public:
  SmartTask(pw::async::Dispatcher& dispatcher)
      : dispatcher_(dispatcher), task_([this](pw::async::Context& ctx, pw::async::Status status) {
          pending_ = false;
          func_(ctx, status);
        }) {}
  ~SmartTask() {
    if (pending_) {
      BT_ASSERT(Cancel());
    }
  }
  void PostAfter(chrono::SystemClock::duration delay) {
    pending_ = true;
    dispatcher_.PostAfter(task_, delay);
  }
  bool Cancel() {
    pending_ = false;
    dispatcher_.Cancel(task_);
  }

  void set_function(pw::async::TaskFunction&& func) { func_ = std::move(func); }

 private:
  pw::async::Dispatcher& dispatcher_;
  pw::async::Task task_;
  pw::async::TaskFunction func_ = nullptr;
  bool pending_ = false;
};

}  // namespace bt

#endif  // SRC_CONNECTIVITY_BLUETOOTH_CORE_BT_HOST_COMMON_SMART_TASK_H_
