// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.examples.metadata/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/metadata/cpp/metadata_server.h>

#include <bind/fuchsia_examples_metadata_bind_library/cpp/bind.h>

namespace examples::drivers::metadata {

// This driver demonstrates how it can forward the
// `fuchsia.examples.metadata.Metadata` metadata from its parent
// driver, `Sender`, to its children.
class ForwarderDriver final : public fdf::DriverBase {
 public:
  ForwarderDriver(fdf::DriverStartArgs start_args,
                  fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DriverBase("forwarder", std::move(start_args), std::move(driver_dispatcher)) {}

  zx::result<> Start() override {
    if (zx::result result = metadata_server_.ForwardMetadata(incoming()); result.is_error()) {
      fdf::error("Failed to forward metadata: {}", result);
      return result.take_error();
    }

    // Serve the metadata to the driver's child nodes.
    if (zx::result result = metadata_server_.Serve(*outgoing(), dispatcher()); result.is_error()) {
      fdf::error("Failed to serve metadata: {}", result);
      return result.take_error();
    }

    static const std::vector<fuchsia_driver_framework::NodeProperty> kProperties = {
        fdf::MakeProperty(bind_fuchsia_examples_metadata::CHILD_TYPE,
                          bind_fuchsia_examples_metadata::CHILD_TYPE_RETRIEVER)};

    // Offer the metadata service to the child node.
    std::vector offers = {metadata_server_.MakeOffer()};

    zx::result child = AddChild("forwarder", kProperties, offers);
    if (child.is_error()) {
      fdf::error("Failed to add child: {}", child);
      return child.take_error();
    }

    metadata_recipient_ = std::move(child.value());

    return zx::ok();
  }

 private:
  // Responsible for forwarding metadata.
  fdf_metadata::MetadataServer<fuchsia_examples_metadata::Metadata> metadata_server_;

  fidl::ClientEnd<fuchsia_driver_framework::NodeController> metadata_recipient_;
};

}  // namespace examples::drivers::metadata

FUCHSIA_DRIVER_EXPORT(examples::drivers::metadata::ForwarderDriver);
