// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_ANNOTATIONS_BATTERY_INFO_PROVIDER_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_ANNOTATIONS_BATTERY_INFO_PROVIDER_H_

#include <fidl/fuchsia.power.battery/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>
#include <lib/sys/cpp/service_directory.h>

#include <memory>

#include "src/developer/forensics/feedback/annotations/provider.h"
#include "src/developer/forensics/feedback/annotations/types.h"
#include "src/lib/backoff/backoff.h"

namespace forensics::feedback {

// Responsible for collecting annotations from
// fuchsia.power.battery/BatteryInfoProvider::GetBatteryInfo.
class BatteryInfoProvider : public DynamicAsyncAnnotationProvider,
                            public fidl::AsyncEventHandler<fuchsia_power_battery::BatteryManager> {
 public:
  BatteryInfoProvider(async_dispatcher_t* dispatcher,
                      std::shared_ptr<sys::ServiceDirectory> services,
                      std::unique_ptr<backoff::Backoff> backoff);

  virtual ~BatteryInfoProvider() = default;

  void on_fidl_error(fidl::UnbindInfo error) override;

  static std::set<std::string> GetAnnotationKeys();

  std::set<std::string> GetKeys() const override;

  void Get(::fit::callback<void(Annotations)> callback) override;

 private:
  void Connect();

  async_dispatcher_t* dispatcher_;
  std::shared_ptr<sys::ServiceDirectory> services_;
  std::unique_ptr<backoff::Backoff> backoff_;
  fidl::Client<fuchsia_power_battery::BatteryManager> client_;

  async::TaskClosureMethod<BatteryInfoProvider, &BatteryInfoProvider::Connect> reconnect_task_{
      this};
};

}  // namespace forensics::feedback

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_ANNOTATIONS_BATTERY_INFO_PROVIDER_H_
