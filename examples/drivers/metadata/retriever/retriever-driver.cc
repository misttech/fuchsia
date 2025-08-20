// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.examples.metadata/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/metadata/cpp/metadata.h>

namespace examples::drivers::metadata {

// This driver demonstrates how to retrieve the metadata from its parent driver,
// Forwarder. It implements the `fuchsia_examples_metadata::Retriever` protocol
// for testing purposes.
class RetrieverDriver final : public fdf::DriverBase,
                              public fidl::Server<fuchsia_examples_metadata::Retriever> {
 public:
  RetrieverDriver(fdf::DriverStartArgs start_args,
                  fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DriverBase("retriever", std::move(start_args), std::move(driver_dispatcher)) {}

  // fdf::DriverBase implementation.
  zx::result<> Start() override {
    zx::result result = outgoing()->AddService<fuchsia_examples_metadata::RetrieverService>(
        fuchsia_examples_metadata::RetrieverService::InstanceHandler(
            {.device = bindings_.CreateHandler(this, dispatcher(), fidl::kIgnoreBindingClosure)}));
    if (result.is_error()) {
      fdf::error("Failed to add service: {}", result);
      return result.take_error();
    }

    return zx::ok();
  }

  // fidl::Server<fuchsia_examples_metadata::Retriever> implementation.
  void GetMetadata(GetMetadataCompleter::Sync& completer) override {
    zx::result metadata =
        fdf_metadata::GetMetadata<fuchsia_examples_metadata::Metadata>(incoming());
    if (metadata.is_error()) {
      fdf::error("Failed to get metadata: {}", metadata);
      completer.Reply(fit::error(metadata.status_value()));
      return;
    }

    completer.Reply(fit::ok(std::move(metadata.value())));
  }

 private:
  fidl::ServerBindingGroup<fuchsia_examples_metadata::Retriever> bindings_;
};

}  // namespace examples::drivers::metadata

FUCHSIA_DRIVER_EXPORT(examples::drivers::metadata::RetrieverDriver);
