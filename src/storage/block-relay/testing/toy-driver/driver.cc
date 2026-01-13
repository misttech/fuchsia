// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.block.volume/cpp/fidl.h>
#include <fidl/fuchsia.storage.block/cpp/fidl.h>
#include <fidl/fuchsia.testing.simple/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/logging/cpp/logger.h>

namespace toy_driver {

class ToyDriver : public fdf::DriverBase, public fidl::WireServer<fuchsia_testing_simple::Simple> {
 public:
  ToyDriver(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DriverBase("toy_driver", std::move(start_args), std::move(driver_dispatcher)) {}

  zx::result<> Start() override {
    FDF_LOG(INFO, "ToyDriver::Start()");

    // Connect to the volume service.
    zx::result connect_result =
        incoming()->Connect<fuchsia_hardware_block_volume::Service::Volume>();
    if (connect_result.is_error()) {
      FDF_LOG(ERROR, "Failed to connect to Volume service: %s", connect_result.status_string());
      return connect_result.take_error();
    }
    block_client_.Bind(std::move(connect_result.value()));

    // Verify the partition label, which also verifies basic interaction with the block device.
    fidl::Result result = block_client_->GetMetadata();
    if (result.is_error()) {
      FDF_LOG(ERROR, "GetMetadata failed: %s", result.error_value().FormatDescription().c_str());
      return zx::error(ZX_ERR_IO);
    }
    const auto& metadata = result.value();
    if (!metadata.name().has_value()) {
      FDF_LOG(ERROR, "Partition has no name");
      return zx::error(ZX_ERR_BAD_PATH);
    }
    if (metadata.name().value() != "my-part") {
      FDF_LOG(ERROR, "Partition name mismatch: expected 'my-part', got '%s'",
              metadata.name().value().c_str());
      return zx::error(ZX_ERR_BAD_PATH);
    }
    FDF_LOG(INFO, "ToyDriver::Start() OK");

    return outgoing()->AddService<fuchsia_testing_simple::Service>(
        fuchsia_testing_simple::Service::InstanceHandler({
            .simple = bindings_.CreateHandler(this, dispatcher(), fidl::kIgnoreBindingClosure),
        }));
  }

  // fuchsia.testing.simple.Simple
  void OnStart(OnStartCompleter::Sync& completer) override { completer.ReplySuccess(); }

 private:
  fidl::SyncClient<fuchsia_storage_block::Block> block_client_;
  fidl::ServerBindingGroup<fuchsia_testing_simple::Simple> bindings_;
};

}  // namespace toy_driver

FUCHSIA_DRIVER_EXPORT(toy_driver::ToyDriver);
