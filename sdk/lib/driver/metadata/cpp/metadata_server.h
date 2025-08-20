// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_METADATA_CPP_METADATA_SERVER_H_
#define LIB_DRIVER_METADATA_CPP_METADATA_SERVER_H_

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.driver.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/fidl.h>
#include <lib/async/default.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/logging/cpp/structured_logger.h>
#include <lib/driver/metadata/cpp/metadata.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/platform-device/cpp/pdev.h>

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)

namespace fdf_metadata {

// Serves metadata that can be retrieved using `fdf_metadata::GetMetadata<|FidlType|>()`.
// As an example, lets say there exists a FIDL type `fuchsia.hardware.test/Metadata` to be sent from
// a driver to its child driver:
//
//   library fuchsia.hardware.test;
//
//   // Make sure to annotate the type with `@serializable`.
//   @serializable
//   type Metadata = table {
//       1: test_property string:MAX;
//   };
//
// The parent driver can define a `MetadataServer<fuchsia_hardware_test::Metadata>` server
// instance as one its members:
//
//   class ParentDriver : public fdf::DriverBase {
//    private:
//     fdf_metadata::MetadataServer<fuchsia_hardware_test::Metadata> metadata_server_;
//   }
//
// When the parent driver creates a child node, it can offer the metadata server's service to the
// child node by adding the metadata server's offers to the node-add arguments:
//
//   auto args = fuchsia_driver_framework::NodeAddArgs args{{.offers2 =
//     std::vector{metadata_server_.MakeOffer()}}};
//
// The parent driver should also declare the metadata server's capability and offer it in the
// driver's component manifest like so:
//
//   capabilities: [
//     { service: "fuchsia.hardware.test.Metadata" },
//   ],
//   expose: [
//     {
//       service: "fuchsia.hardware.test.Metadata",
//       from: "self",
//     },
//   ],
//
template <typename FidlType>
class MetadataServer final : public fidl::WireServer<fuchsia_driver_metadata::Metadata> {
 public:
  // The caller's component manifest must specify `|FidlType|::kSerializableName` as a service
  // capability and expose it. Otherwise, other components will not be able to retrieve metadata.
  explicit MetadataServer(
      std::string instance_name = component::OutgoingDirectory::kDefaultServiceInstance)
      : instance_name_(std::move(instance_name)) {}

  // Deprecated. Do not use. Use `Serve()` instead.
  // TODO(b/439047765): Remove once no longer used.
  zx::result<> SetMetadata(const FidlType& metadata) {
    static_assert(fidl::IsFidlType<FidlType>::value, "|FidlType| must be a FIDL domain object.");
    static_assert(!fidl::IsResource<FidlType>::value,
                  "|FidlType| cannot be a resource type. Resources cannot be persisted.");

    fit::result persisted_metadata = fidl::Persist(metadata);
    if (persisted_metadata.is_error()) {
      fdf::error("Failed to persist metadata: {}",
                 persisted_metadata.error_value().FormatDescription());
      return zx::error(persisted_metadata.error_value().status());
    }
    persisted_metadata_.emplace(std::move(persisted_metadata.value()));

    return zx::ok();
  }

  // Deprecated. Do not use. Use `ForwardAndServe()` instead.
  // TODO(b/439047765): Remove once no longer used.
  zx::result<bool> SetMetadataFromPDevIfExists(
      fidl::UnownedClientEnd<fuchsia_hardware_platform_device::Device> pdev) {
    fidl::WireResult result = fidl::WireCall(pdev)->GetMetadata(
        fidl::StringView::FromExternal(FidlType::kSerializableName));
    if (!result.ok()) {
      fdf::error("Failed to send GetMetadata request: {}", result.status_string());
      return zx::error(result.status());
    }
    if (result->is_error()) {
      if (result->error_value() == ZX_ERR_NOT_FOUND) {
        return zx::ok(false);
      }
      fdf::error("Failed to get metadata: {}", zx_status_get_string(result->error_value()));
      return zx::error(result->error_value());
    }
    const auto persisted_metadata = result.value()->metadata.get();
    persisted_metadata_.emplace();
    persisted_metadata_->assign(persisted_metadata.begin(), persisted_metadata.end());

    return zx::ok(true);
  }

  // Deprecated. Do not use. Use `ForwardAndServe()` instead.
  // TODO(b/439047765): Remove once no longer used.
  zx::result<bool> SetMetadataFromPDevIfExists(
      fidl::ClientEnd<fuchsia_hardware_platform_device::Device>& pdev) {
    return SetMetadataFromPDevIfExists(pdev.borrow());
  }

  // Deprecated. Do not use. Use `ForwardAndServe()` instead.
  // TODO(b/439047765): Remove once no longer used.
  zx::result<bool> SetMetadataFromPDevIfExists(fdf::PDev& pdev) {
    return SetMetadataFromPDevIfExists(pdev.borrow());
  }

  // Deprecated. Do not use. Use `ForwardAndServe()` instead.
  // TODO(b/439047765): Remove once no longer used.
  zx::result<> ForwardMetadata(
      const std::shared_ptr<fdf::Namespace>& incoming,
      std::string_view instance_name = component::OutgoingDirectory::kDefaultServiceInstance) {
    fidl::WireSyncClient<fuchsia_driver_metadata::Metadata> client{};
    {
      zx::result result = ConnectToMetadataProtocol(incoming->svc_dir(),
                                                    FidlType::kSerializableName, instance_name);
      if (result.is_error()) {
        fdf::error("Failed to connect to metadata server: {}", result);
        return result.take_error();
      }
      client.Bind(std::move(result.value()));
    }

    fidl::WireResult<fuchsia_driver_metadata::Metadata::GetPersistedMetadata> result =
        client->GetPersistedMetadata();
    if (!result.ok()) {
      fdf::error("Failed to send GetPersistedMetadata request: {}", result.status_string());
      return zx::error(result.status());
    }
    if (result->is_error()) {
      fdf::error("Failed to get persisted metadata: {}",
                 zx_status_get_string(result->error_value()));
      return result->take_error();
    }
    cpp20::span<uint8_t> persisted_metadata = result.value()->persisted_metadata.get();
    std::vector<uint8_t> copy;
    copy.insert(copy.begin(), persisted_metadata.begin(), persisted_metadata.end());
    persisted_metadata_.emplace(std::move(copy));

    return zx::ok();
  }

  // Deprecated. Do not use. Use `ForwardAndServe()` instead.
  // TODO(b/439047765): Remove once no longer used.
  zx::result<bool> ForwardMetadataIfExists(
      const std::shared_ptr<fdf::Namespace>& incoming,
      std::string_view instance_name = component::OutgoingDirectory::kDefaultServiceInstance) {
    fidl::WireSyncClient<fuchsia_driver_metadata::Metadata> client{};
    {
      zx::result result = ConnectToMetadataProtocol(incoming->svc_dir(),
                                                    FidlType::kSerializableName, instance_name);
      if (result.is_error()) {
        fdf::debug("Failed to connect to metadata server: {}", result);
        return zx::ok(false);
      }
      client.Bind(std::move(result.value()));
    }

    fidl::WireResult<fuchsia_driver_metadata::Metadata::GetPersistedMetadata> result =
        client->GetPersistedMetadata();
    if (!result.ok()) {
      if (result.status() == ZX_ERR_PEER_CLOSED) {
        // We assume that the metadata does not exist because we assume that the FIDL server does
        // not exist because we received a peer closed status.
        fdf::debug("Failed to send GetPersistedMetadata request: {}", result.status_string());
        return zx::ok(false);
      }
      fdf::error("Failed to send GetPersistedMetadata request: {}", result.status_string());
      return zx::error(result.status());
    }
    if (result->is_error()) {
      if (result->error_value() == ZX_ERR_NOT_FOUND) {
        fdf::debug("Failed to get persisted metadata: {}",
                   zx_status_get_string(result->error_value()));
        return zx::ok(false);
      }
      fdf::error("Failed to get persisted metadata: {}",
                 zx_status_get_string(result->error_value()));
      return result->take_error();
    }
    cpp20::span<uint8_t> persisted_metadata = result.value()->persisted_metadata.get();
    std::vector<uint8_t> copy;
    copy.insert(copy.begin(), persisted_metadata.begin(), persisted_metadata.end());
    persisted_metadata_.emplace(std::move(copy));

    return zx::ok(true);
  }

  // Deprecated. Do not use. Use non-deprecated overloads of `Serve()` instead.
  // TODO(b/439047765): Remove once no longer used.
  zx::result<> Serve(fdf::OutgoingDirectory& outgoing, async_dispatcher_t* dispatcher) {
    return Serve(outgoing.component(), dispatcher);
  }

  // Deprecated. Do not use. Use non-deprecated overloads of `Serve()` instead.
  // TODO(b/439047765): Remove once no longer used.
  zx::result<> Serve(component::OutgoingDirectory& outgoing, async_dispatcher_t* dispatcher) {
    fuchsia_driver_metadata::Service::InstanceHandler handler{
        {.metadata = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure)}};
    zx::result result =
        outgoing.AddService(std::move(handler), FidlType::kSerializableName, instance_name_);
    if (result.is_error()) {
      fdf::error("Failed to add service: {}", result);
      return result.take_error();
    }
    return zx::ok();
  }

  // Retrieves |FidlType| from |pdev| and serves it to |outgoing|. If the metadata was unable to be
  // retrieved then nothing is served. Returns true if the metadata was retrieved and false
  // otherwise.
  zx::result<bool> ForwardAndServe(
      fdf::OutgoingDirectory& outgoing, async_dispatcher_t* dispatcher,
      fidl::UnownedClientEnd<fuchsia_hardware_platform_device::Device> pdev) {
    fidl::WireResult result = fidl::WireCall(pdev)->GetMetadata(
        fidl::StringView::FromExternal(FidlType::kSerializableName));
    if (!result.ok()) {
      fdf::debug("Failed to send GetMetadata request: {}", result.status_string());
      return zx::ok(false);
    }
    if (result->is_error()) {
      fdf::debug("Failed to get metadata: {}", zx_status_get_string(result->error_value()));
      return zx::ok(false);
    }
    const auto persisted_metadata = result.value()->metadata.get();
    persisted_metadata_.emplace();
    persisted_metadata_->assign(persisted_metadata.begin(), persisted_metadata.end());

    if (zx::result result = AddService(outgoing, dispatcher); result.is_error()) {
      return result.take_error();
    }

    return zx::ok(true);
  }

  zx::result<bool> ForwardAndServe(
      fdf::OutgoingDirectory& outgoing, async_dispatcher_t* dispatcher,
      fidl::ClientEnd<fuchsia_hardware_platform_device::Device>& pdev) {
    return ForwardAndServe(outgoing, dispatcher, pdev.borrow());
  }

  zx::result<bool> ForwardAndServe(fdf::OutgoingDirectory& outgoing, async_dispatcher_t* dispatcher,
                                   fdf::PDev& pdev) {
    return ForwardAndServe(outgoing, dispatcher, pdev.borrow());
  }

  // Retrieves |FidlType| from |instance_name| in |incoming| and serves it to |outgoing|. If the
  // metadata was unable to be retrieved then nothing is served. Returns true if the metadata was
  // retrieved and false otherwise.
  zx::result<bool> ForwardAndServe(
      fdf::OutgoingDirectory& outgoing, async_dispatcher_t* dispatcher,
      fidl::UnownedClientEnd<fuchsia_io::Directory> incoming,
      std::string_view instance_name = component::OutgoingDirectory::kDefaultServiceInstance) {
    fidl::WireSyncClient<fuchsia_driver_metadata::Metadata> client{};
    {
      zx::result result =
          ConnectToMetadataProtocol(incoming, FidlType::kSerializableName, instance_name);
      if (result.is_error()) {
        fdf::error("Failed to connect to metadata server: {}", result);
        return zx::ok(false);
      }
      client.Bind(std::move(result.value()));
    }

    {
      fidl::WireResult<fuchsia_driver_metadata::Metadata::GetPersistedMetadata> result =
          client->GetPersistedMetadata();
      if (!result.ok()) {
        fdf::debug("Failed to send GetPersistedMetadata request: {}", result.status_string());
        return zx::ok(false);
      }
      if (result->is_error()) {
        fdf::debug("Failed to get persisted metadata: {}",
                   zx_status_get_string(result->error_value()));
        return zx::ok(false);
      }
      cpp20::span<uint8_t> persisted_metadata = result.value()->persisted_metadata.get();
      std::vector<uint8_t> copy;
      copy.insert(copy.begin(), persisted_metadata.begin(), persisted_metadata.end());
      persisted_metadata_.emplace(std::move(copy));
    }

    if (zx::result result = AddService(outgoing, dispatcher); result.is_error()) {
      return result.take_error();
    }

    return zx::ok(true);
  }

  zx::result<bool> ForwardAndServe(
      fdf::OutgoingDirectory& outgoing, async_dispatcher_t* dispatcher,
      const std::shared_ptr<fdf::Namespace>& incoming,
      std::string_view instance_name = component::OutgoingDirectory::kDefaultServiceInstance) {
    return ForwardAndServe(outgoing, dispatcher, incoming->svc_dir(), instance_name);
  }

  // Serves the fuchsia.driver.metadata/Service service to |outgoing| under the service name
  // `|FidlType|::kSerializableName` and instance name `MetadataServer::instance_name_`. |metadata|
  // is the metadata to be served.
  zx::result<> Serve(fdf::OutgoingDirectory& outgoing, async_dispatcher_t* dispatcher,
                     const FidlType& metadata) {
    static_assert(fidl::IsFidlType<FidlType>::value, "|FidlType| must be a FIDL domain object.");
    static_assert(!fidl::IsResource<FidlType>::value,
                  "|FidlType| cannot be a resource type. Resources cannot be persisted.");

    fit::result persisted_metadata = fidl::Persist(metadata);
    if (persisted_metadata.is_error()) {
      fdf::error("Failed to persist metadata: {}",
                 persisted_metadata.error_value().FormatDescription());
      return zx::error(persisted_metadata.error_value().status());
    }
    persisted_metadata_.emplace(std::move(persisted_metadata.value()));

    return AddService(outgoing, dispatcher);
  }

  // Deprecated. Do not use. Use `CreateOffer()` instead.
  // TODO(b/439047765): Remove once no longer used.
  fuchsia_driver_framework::Offer MakeOffer() {
    return fuchsia_driver_framework::Offer::WithZirconTransport(
        fdf::MakeOffer(FidlType::kSerializableName, instance_name_));
  }

  // Deprecated. Do not use. Use `CreateOffer()` instead.
  // TODO(b/439047765): Remove once no longer used.
  fuchsia_driver_framework::wire::Offer MakeOffer(fidl::AnyArena& arena) {
    return fuchsia_driver_framework::wire::Offer::WithZirconTransport(
        arena, fdf::MakeOffer(arena, FidlType::kSerializableName, instance_name_));
  }

  // Creates an offer for this `MetadataServer` instance's fuchsia.driver.metadata/Service
  // service. Returns an std::nullopt if the metadata server is not serving metadata.
  std::optional<fuchsia_driver_framework::Offer> CreateOffer() {
    if (!persisted_metadata_.has_value()) {
      return std::nullopt;
    }
    return fuchsia_driver_framework::Offer::WithZirconTransport(
        fdf::MakeOffer(FidlType::kSerializableName, instance_name_));
  }

  // Creates an offer for this `MetadataServer` instance's fuchsia.driver.metadata/Service
  // service. Returns an std::nullopt if the metadata server is not serving metadata.
  std::optional<fuchsia_driver_framework::wire::Offer> CreateOffer(fidl::AnyArena& arena) {
    if (!persisted_metadata_.has_value()) {
      return std::nullopt;
    }
    return fuchsia_driver_framework::wire::Offer::WithZirconTransport(
        arena, fdf::MakeOffer(arena, FidlType::kSerializableName, instance_name_));
  }

 private:
  zx::result<> AddService(fdf::OutgoingDirectory& outgoing, async_dispatcher_t* dispatcher) {
    fuchsia_driver_metadata::Service::InstanceHandler handler(
        {.metadata = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure)});

    zx::result result = outgoing.component().AddService(
        std::move(handler), FidlType::kSerializableName, instance_name_);
    if (result.is_error()) {
      fdf::error("Failed to add service: {}", result);
      return result.take_error();
    }

    return zx::ok();
  }

  // fuchsia.driver.metadata/Metadata protocol implementation.
  void GetPersistedMetadata(GetPersistedMetadataCompleter::Sync& completer) override {
    if (!persisted_metadata_.has_value()) {
      fdf::warn("Metadata not set");
      completer.ReplyError(ZX_ERR_NOT_FOUND);
      return;
    }
    completer.ReplySuccess(fidl::VectorView<uint8_t>::FromExternal(persisted_metadata_.value()));
  }

  fidl::ServerBindingGroup<fuchsia_driver_metadata::Metadata> bindings_;

  // Persisted metadata that will be served in this instance's fuchsia.driver.metadata/Metadata
  // protocol.
  std::optional<std::vector<uint8_t>> persisted_metadata_;

  // Name of the instance directory that will serve this instance's fuchsia.driver.metadata/Service
  // service.
  std::string instance_name_;
};

}  // namespace fdf_metadata

#endif

#endif  // LIB_DRIVER_METADATA_CPP_METADATA_SERVER_H_
