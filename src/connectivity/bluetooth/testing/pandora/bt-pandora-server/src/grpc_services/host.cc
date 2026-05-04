// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "host.h"

#include <fidl/fuchsia.bluetooth.le/cpp/fidl.h>
#include <fidl/fuchsia.bluetooth.sys/cpp/markers.h>
#include <lib/component/incoming/cpp/protocol.h>

#include <algorithm>

#include <google/protobuf/io/coded_stream.h>
#include <google/protobuf/message.h>

using grpc::Status;
using grpc::StatusCode;

using namespace std::chrono_literals;

// TODO(https://fxbug.dev/316721276): Implement gRPCs necessary to enable GAP/A2DP testing.

HostService::HostService(async_dispatcher_t* dispatcher) {
  // Connect to fuchsia.bluetooth.affordances.PeripheralController
  zx::result peripheral_controller_client_end =
      component::Connect<fuchsia_bluetooth_affordances::PeripheralController>();
  if (peripheral_controller_client_end.is_ok()) {
    peripheral_controller_client_.Bind(std::move(*peripheral_controller_client_end));
  } else {
    FX_LOGS(ERROR) << "Error connecting to PeripheralController service: "
                   << peripheral_controller_client_end.status_string();
  }

  // Connect to fuchsia.bluetooth.affordances.HostController
  zx::result host_controller_client_end =
      component::Connect<fuchsia_bluetooth_affordances::HostController>();
  if (host_controller_client_end.is_ok()) {
    host_controller_client_.Bind(std::move(*host_controller_client_end));
  } else {
    FX_LOGS(ERROR) << "Error connecting to HostController service: "
                   << host_controller_client_end.status_string();
  }

  // Connect to fuchsia.bluetooth.affordances.PeerController
  zx::result peer_controller_client_end =
      component::Connect<fuchsia_bluetooth_affordances::PeerController>();
  if (peer_controller_client_end.is_ok()) {
    peer_controller_client_.Bind(std::move(*peer_controller_client_end));
  } else {
    FX_LOGS(ERROR) << "Error connecting to PeerController service: "
                   << peer_controller_client_end.status_string();
  }
}

Status HostService::FactoryReset(grpc::ServerContext* context,
                                 const google::protobuf::Empty* request,
                                 google::protobuf::Empty* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status HostService::Reset(grpc::ServerContext* context, const google::protobuf::Empty* request,
                          google::protobuf::Empty* response) {
  // No-op for now; return OK status.
  return {/*OK*/};
}

Status HostService::ReadLocalAddress(grpc::ServerContext* context,
                                     const google::protobuf::Empty* request,
                                     pandora::ReadLocalAddressResponse* response) {
  auto result = host_controller_client_->GetHosts();
  if (result.is_error()) {
    return Status(StatusCode::INTERNAL,
                  "fuchsia.bluetooth.affordances.HostController/GetHosts error: " +
                      result.error_value().FormatDescription());
  }

  if (!result->hosts().has_value() || result->hosts().value().empty()) {
    return Status(StatusCode::NOT_FOUND, "No hosts available");
  }

  const auto& hosts = result->hosts().value();
  for (const auto& host : hosts) {
    if (host.active().has_value() && host.active().value()) {
      if (host.addresses().has_value() && !host.addresses().value().empty()) {
        std::array<uint8_t, 6> host_addr;
        std::ranges::copy(host.addresses().value()[0].bytes(), host_addr.begin());

        // Convert local address from little-endian to big-endian, as expected by Pandora.
        std::ranges::reverse(host_addr);
        response->set_address(host_addr.data(), 6);
        return Status::OK;
      }
    }
  }

  return Status(StatusCode::NOT_FOUND, "No active host with valid address found");
}

Status HostService::Connect(grpc::ServerContext* context, const pandora::ConnectRequest* request,
                            pandora::ConnectResponse* response) {
  std::string little_endian_addr(request->address().rbegin(), request->address().rend());
  uint64_t peer_id = get_peer_id(little_endian_addr.c_str());
  if (!peer_id) {
    return Status(StatusCode::NOT_FOUND, "Peer not found");
  }

  fuchsia_bluetooth_affordances::PeerSelector selector;
  selector.id() = fuchsia_bluetooth::PeerId{peer_id};
  auto result = peer_controller_client_->ConnectPeer(selector);
  if (result.is_error()) {
    return Status(StatusCode::INTERNAL,
                  "fuchsia.bluetooth.affordances.PeerController/ConnectPeer error: " +
                      result.error_value().FormatDescription());
  }

  response->mutable_connection()->mutable_cookie()->set_value(std::to_string(peer_id));
  return {/*OK*/};
}

Status HostService::WaitConnection(grpc::ServerContext* context,
                                   const pandora::WaitConnectionRequest* request,
                                   pandora::WaitConnectionResponse* response) {
  auto peer_it = WaitForPeer(request->address(), true);
  if (peer_it->id()) {
    response->mutable_connection()->mutable_cookie()->set_value(
        std::to_string(peer_it->id()->value()));
  }
  return {/*OK*/};
}

Status HostService::ConnectLE(::grpc::ServerContext* context,
                              const ::pandora::ConnectLERequest* request,
                              ::pandora::ConnectLEResponse* response) {
  if (!request->has_public_()) {
    return Status(StatusCode::INVALID_ARGUMENT, "Expected public address field");
  }
  if (request->public_().size() != 6) {
    return Status(StatusCode::INVALID_ARGUMENT, "Expected 6 byte BD_ADDR");
  }

  std::string little_endian_addr(request->public_().rbegin(), request->public_().rend());
  uint64_t peer_id = get_peer_id(little_endian_addr.c_str());
  if (!peer_id) {
    return Status(StatusCode::NOT_FOUND, "Could not find peer.");
  }

  if (connect_le(peer_id) != ZX_OK) {
    return Status(StatusCode::INTERNAL, "Error in Rust affordances (check logs)");
  }
  response->mutable_connection()->mutable_cookie()->set_value(std::to_string(peer_id));
  return {/*OK*/};
}

Status HostService::Disconnect(::grpc::ServerContext* context,
                               const ::pandora::DisconnectRequest* request,
                               ::google::protobuf::Empty* response) {
  uint64_t peer_id =
      std::strtoul(request->connection().cookie().value().c_str(), nullptr, /*base=*/10);
  if (disconnect_peer(peer_id) != ZX_OK) {
    return Status(StatusCode::INTERNAL, "Error in Rust affordances (check logs)");
  }
  return {/*OK*/};
}

Status HostService::WaitDisconnection(::grpc::ServerContext* context,
                                      const ::pandora::WaitDisconnectionRequest* request,
                                      ::google::protobuf::Empty* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status HostService::Advertise(::grpc::ServerContext* context,
                              const ::pandora::AdvertiseRequest* request,
                              ::grpc::ServerWriter<::pandora::AdvertiseResponse>* writer) {
  fuchsia_bluetooth_le::AdvertisingParameters parameters;
  fuchsia_bluetooth_le::AdvertisingData data;

  if (request->has_data()) {
    const pandora::DataTypes& req_data = request->data();

    if (req_data.le_discoverability_mode() == pandora::DiscoverabilityMode::DISCOVERABLE_GENERAL) {
      fuchsia_bluetooth_affordances::HostControllerSetDiscoverabilityRequest
          set_discoverability_request;
      set_discoverability_request.discoverable() = true;
      auto result = host_controller_client_->SetDiscoverability(set_discoverability_request);
      if (result.is_error()) {
        return Status(StatusCode::INTERNAL,
                      "fuchsia.bluetooth.affordances.HostController/SetDiscoverability error: " +
                          result.error_value().FormatDescription());
      }
    }
    if (req_data.include_complete_local_name() || req_data.include_shortened_local_name()) {
      data.name() = "sapphire";
    }
    if (!req_data.complete_service_class_uuids128().empty()) {
      std::vector<fuchsia_bluetooth::Uuid> uuids;
      for (const auto& uuid_str : req_data.complete_service_class_uuids128()) {
        UuidBytes bytes = uuid_from_string(uuid_str.c_str());
        std::array<uint8_t, 16> uuid_arr;
        std::ranges::copy(bytes.value, uuid_arr.begin());

        if (std::ranges::none_of(uuid_arr, [](uint8_t byte) { return byte != 0; })) {
          return Status(StatusCode::INVALID_ARGUMENT, "Failed to parse UUID");
        }
        uuids.emplace_back(uuid_arr);
      }

      data.service_uuids() = std::move(uuids);
    }
  }

  parameters.data() = std::move(data);
  parameters.connectable() = request->connectable();

  if (request->own_address_type() == pandora::OwnAddressType::RANDOM ||
      request->own_address_type() == pandora::OwnAddressType::RESOLVABLE_OR_RANDOM) {
    parameters.address_type() = fuchsia_bluetooth::AddressType::kRandom;
  } else {
    parameters.address_type() = fuchsia_bluetooth::AddressType::kPublic;
  }

  pandora::AdvertiseResponse response;
  auto result = peripheral_controller_client_->Advertise(
      {{.parameters = std::move(parameters), .timeout = 10 /*seconds*/}});
  if (result.is_error()) {
    auto err = result.error_value();
    if (err.is_domain_error() &&
        err.domain_error() == fuchsia_bluetooth_affordances::Error::kTimeout) {
      FX_LOGS(WARNING) << "Advertisement timed out without connection";
    } else {
      return Status(
          StatusCode::INTERNAL,
          std::format("fuchsia.bluetooth.affordances.PeripheralController/Advertise error: {}",
                      err.FormatDescription()));
    }
  } else {
    response.mutable_connection()->mutable_cookie()->set_value(
        std::to_string(result->peer_id().value().value()));
  }
  writer->Write(response);

  return Status::OK;
}

Status HostService::Scan(::grpc::ServerContext* context, const ::pandora::ScanRequest* request,
                         ::grpc::ServerWriter<::pandora::ScanningResponse>* writer) {
  {
    std::lock_guard lock(m_scan_rsp_writer_);
    scan_rsp_writer_ = writer;
  }

  if (start_le_scan(/*context=*/this, LeScanCb) != ZX_OK) {
    return Status(StatusCode::INTERNAL, "Failure to start_le_scan (check logs)");
  }

  // TODO(https://fxbug.dev/396500079): Potentially migrate to gRPC async callback API and remove
  // this timeout. Since we are using the sync API, `writer` is invalidated when this handler exits,
  // so we keep it alive for an arbitrary sleep period allowing the scan to proceed, after which we
  // cancel the scan. In practice, the mmi2gRPC client cancels the scan earlier (when the test peer
  // is found).
  std::this_thread::sleep_for(std::chrono::seconds(5));

  std::lock_guard lock(m_scan_rsp_writer_);
  scan_rsp_writer_ = nullptr;
  zx_status_t status = stop_le_scan();
  if (status == ZX_OK) {
    FX_LOGS(WARNING) << "LE scan stopped after timeout.";
  } else if (status == ZX_ERR_BAD_STATE) {
    FX_LOGS(INFO) << "LE scan was already stopped after timeout.";
  } else {
    return Status(StatusCode::INTERNAL, "Unexpected error in stop_le_scan");
  }
  return {/*OK*/};
}

Status HostService::Inquiry(::grpc::ServerContext* context,
                            const ::google::protobuf::Empty* request,
                            ::grpc::ServerWriter<::pandora::InquiryResponse>* writer) {
  fuchsia_bluetooth_affordances::PeerControllerSetDiscoveryRequest set_discovery_request;
  set_discovery_request.discovery() = true;
  auto set_discovery_result = peer_controller_client_->SetDiscovery(set_discovery_request);
  if (set_discovery_result.is_error()) {
    return Status(StatusCode::INTERNAL,
                  "fuchsia.bluetooth.affordances.PeerController/SetDiscovery error: " +
                      set_discovery_result.error_value().FormatDescription());
  }

  // Discover for an arbitrary period of 5 seconds, which passes PTS tests.
  //
  // TODO(https://fxbug.dev/396500079): Adopt a streaming API instead of a scan period with timeout.
  std::this_thread::sleep_for(std::chrono::seconds(5));

  auto get_known_peers_result = peer_controller_client_->GetKnownPeers();
  if (get_known_peers_result.is_error()) {
    return Status(StatusCode::INTERNAL,
                  "fuchsia.bluetooth.affordances.PeerController/GetKnownPeers error: " +
                      get_known_peers_result.error_value().FormatDescription());
  }

  if (get_known_peers_result->peers().has_value()) {
    for (const fuchsia_bluetooth_sys::Peer& peer : get_known_peers_result->peers().value()) {
      pandora::InquiryResponse inquiry_rsp;
      std::array<uint8_t, 6> peer_addr;
      std::ranges::copy(peer.address().value().bytes(), peer_addr.begin());
      // Convert peer address from little-endian to big-endian, as expected by Pandora.
      std::ranges::reverse(peer_addr);
      inquiry_rsp.set_address(peer_addr.data(), 6);

      if (!writer->Write(inquiry_rsp)) {
        FX_LOGS(INFO) << "Inquiry canceled by gRPC client.";
        break;
      }
    }
  }

  set_discovery_request.discovery() = false;
  set_discovery_result = peer_controller_client_->SetDiscovery(set_discovery_request);
  if (set_discovery_result.is_error()) {
    return Status(StatusCode::INTERNAL,
                  "fuchsia.bluetooth.affordances.PeerController/SetDiscovery error: " +
                      set_discovery_result.error_value().FormatDescription());
  }

  return Status::OK;
}

Status HostService::SetDiscoverabilityMode(::grpc::ServerContext* context,
                                           const ::pandora::SetDiscoverabilityModeRequest* request,
                                           ::google::protobuf::Empty* response) {
  fuchsia_bluetooth_affordances::HostControllerSetDiscoverabilityRequest
      set_discoverability_request;
  set_discoverability_request.discoverable() =
      request->mode() != ::pandora::DiscoverabilityMode::NOT_DISCOVERABLE;
  auto result = host_controller_client_->SetDiscoverability(set_discoverability_request);
  if (result.is_error()) {
    return Status(StatusCode::INTERNAL,
                  "fuchsia.bluetooth.affordances.HostController/SetDiscoverability error: " +
                      result.error_value().FormatDescription());
  }

  return Status::OK;
}

Status HostService::SetConnectabilityMode(::grpc::ServerContext* context,
                                          const ::pandora::SetConnectabilityModeRequest* request,
                                          ::google::protobuf::Empty* response) {
  if (set_connectability(request->mode() == ::pandora::ConnectabilityMode::CONNECTABLE) != ZX_OK) {
    return Status(StatusCode::INTERNAL, "Error in Rust affordances (check logs)");
  }
  return {/*OK*/};
}

std::vector<fuchsia_bluetooth_sys::Peer>::const_iterator HostService::WaitForPeer(
    const std::string& addr, bool enforce_connected) {
  std::vector<fuchsia_bluetooth_sys::Peer>::const_iterator peer_it;
  std::unique_lock<std::mutex> lock(m_access_);

  do {
    if (!peer_watching_) {
      peer_watching_ = true;
      access_client_->WatchPeers().Then(
          [this](fidl::Result<fuchsia_bluetooth_sys::Access::WatchPeers>& watch_peers) {
            if (watch_peers.is_error()) {
              fidl::Error err = watch_peers.error_value();
              FX_LOGS(ERROR) << "Host watcher error: " << err.error() << "\n";
              return;
            }

            std::unique_lock<std::mutex> lock(this->m_access_);
            peers_ = watch_peers->updated();
            peer_watching_ = false;
            cv_access_.notify_one();
          });
    }

    cv_access_.wait_for(lock, 1000ms, [this] { return !peer_watching_; });
  } while ((peer_it = std::find_if(
                peers_.begin(), peers_.end(),
                [&addr, enforce_connected](const fuchsia_bluetooth_sys::Peer& candidate) {
                  for (size_t i = 0; i < 6; ++i) {
                    if (candidate.address()->bytes()[5 - i] !=
                        static_cast<unsigned char>(addr[i])) {
                      return false;
                    }
                  }
                  return !enforce_connected || (candidate.connected() && *candidate.connected());
                })) == peers_.end());

  return peer_it;
}
