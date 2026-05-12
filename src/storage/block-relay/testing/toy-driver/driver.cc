// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.block.volume/cpp/fidl.h>
#include <fidl/fuchsia.storage.block/cpp/fidl.h>
#include <fidl/fuchsia.testing.simple/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/logging/cpp/logger.h>

namespace toy_driver {

class ToyDriver : public fdf::DriverBase2, public fidl::WireServer<fuchsia_testing_simple::Simple> {
 public:
  ToyDriver() : fdf::DriverBase2("toy_driver") {}

  zx::result<> Start(fdf::DriverContext context) override {
    fdf::info("ToyDriver::Start()");

    // Connect to the volume service.
    zx::result connect_result =
        context.incoming().Connect<fuchsia_hardware_block_volume::Service::Volume>();
    if (connect_result.is_error()) {
      fdf::error("Failed to connect to Volume service: {}", connect_result);
      return connect_result.take_error();
    }
    block_client_.Bind(std::move(connect_result.value()));

    // Verify the partition label, which also verifies basic interaction with the block device.
    fidl::Result result = block_client_->GetMetadata();
    if (result.is_error()) {
      fdf::error("GetMetadata failed: {}", result.error_value().FormatDescription());
      return zx::error(ZX_ERR_IO);
    }
    const auto& metadata = result.value();
    if (!metadata.name().has_value()) {
      fdf::error("Partition has no name");
      return zx::error(ZX_ERR_BAD_PATH);
    }
    if (metadata.name().value() != "my-part") {
      fdf::error("Partition name mismatch: expected 'my-part', got '{}'", metadata.name().value());
      return zx::error(ZX_ERR_BAD_PATH);
    }
    fdf::info("ToyDriver::Start() OK");

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

FUCHSIA_DRIVER_EXPORT2(toy_driver::ToyDriver);
