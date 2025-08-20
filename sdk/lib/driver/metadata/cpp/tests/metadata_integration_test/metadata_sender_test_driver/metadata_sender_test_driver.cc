// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "sdk/lib/driver/metadata/cpp/tests/metadata_integration_test/metadata_sender_test_driver/metadata_sender_test_driver.h"

#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/logging/cpp/structured_logger.h>

#include <bind/fuchsia_driver_metadata_test_bind_library/cpp/bind.h>

namespace fdf_metadata::test {

zx::result<> MetadataSenderTestDriver::Start() {
  zx::result result = outgoing()->AddService<fuchsia_hardware_test::MetadataSenderService>(
      fuchsia_hardware_test::MetadataSenderService::InstanceHandler(
          {.device = bindings_.CreateHandler(this, dispatcher(), fidl::kIgnoreBindingClosure)}));
  if (result.is_error()) {
    fdf::error("Failed to add service: {}", result);
    return result.take_error();
  }

  return zx::ok();
}

void MetadataSenderTestDriver::ServeMetadata(ServeMetadataRequest& request,
                                             ServeMetadataCompleter::Sync& completer) {
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  if (zx::result result = metadata_server_.SetMetadata(request.metadata()); result.is_error()) {
    fdf::error("Failed to set metadata: {}", result);
    completer.Reply(fit::error(result.error_value()));
    return;
  }

  if (zx::result result = metadata_server_.Serve(*outgoing(), dispatcher()); result.is_error()) {
    fdf::error("Failed to serve metadata: {}", result);
    completer.Reply(fit::error(result.error_value()));
    return;
  }

  offer_metadata_to_child_nodes_ = true;
  completer.Reply(fit::ok());
#else
  fdf::error("Serving metadata not supported at current Fuchsia API level.");
  completer.Reply(fit::error(ZX_ERR_NOT_SUPPORTED));
#endif
}

void MetadataSenderTestDriver::AddMetadataRetrieverNode(
    AddMetadataRetrieverNodeRequest& request, AddMetadataRetrieverNodeCompleter::Sync& completer) {
  bool uses_metadata_fidl_service = request.uses_metadata_fidl_service();

  std::vector<fuchsia_driver_framework::NodeProperty> node_properties = {
      fdf::MakeProperty(bind_fuchsia_driver_metadata_test::PURPOSE,
                        bind_fuchsia_driver_metadata_test::PURPOSE_RETRIEVE_METADATA),
      fdf::MakeProperty(bind_fuchsia_driver_metadata_test::USES_METADATA_FIDL_SERVICE,
                        uses_metadata_fidl_service)};

  const std::string node_name =
      std::format("retriever-{}-{}", uses_metadata_fidl_service ? "use" : "no_use",
                  metadata_recipients_.size());
  zx_status_t status = AddChildNode(node_name, node_properties);
  if (status != ZX_OK) {
    // Don't log. AddMetadataNode() performs error logging.
    completer.Reply(fit::error(status));
    return;
  }

  completer.Reply(fit::ok());
}

void MetadataSenderTestDriver::AddMetadataForwarderNode(
    AddMetadataForwarderNodeCompleter::Sync& completer) {
  static const std::vector<fuchsia_driver_framework::NodeProperty> kNodeProperties{
      fdf::MakeProperty(bind_fuchsia_driver_metadata_test::PURPOSE,
                        bind_fuchsia_driver_metadata_test::PURPOSE_FORWARD_METADATA)};

  const std::string node_name = std::format("forwarder-{}", metadata_recipients_.size());
  zx_status_t status = AddChildNode(node_name, kNodeProperties);
  if (status != ZX_OK) {
    // Don't log. AddMetadataNode() performs error logging.
    completer.Reply(fit::error(status));
    return;
  }

  completer.Reply(fit::ok());
}

zx_status_t MetadataSenderTestDriver::AddChildNode(
    std::string_view node_name,
    const fuchsia_driver_framework::NodePropertyVector& node_properties) {
  std::vector<fuchsia_driver_framework::Offer> offers;
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  if (offer_metadata_to_child_nodes_) {
    offers.emplace_back(metadata_server_.MakeOffer());
  }
#endif
  zx::result result = AddChild(node_name, node_properties, offers);
  if (result.is_error()) {
    fdf::error("Failed to add child: {}", result);
    return result.status_value();
  }
  metadata_recipients_.emplace_back(std::move(result.value()));
  return ZX_OK;
}

}  // namespace fdf_metadata::test

FUCHSIA_DRIVER_EXPORT(fdf_metadata::test::MetadataSenderTestDriver);
