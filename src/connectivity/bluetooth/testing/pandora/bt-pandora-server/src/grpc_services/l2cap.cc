// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "l2cap.h"

#include <lib/component/incoming/cpp/protocol.h>
#include <lib/syslog/cpp/macros.h>

#include <cstdlib>

#include "src/connectivity/bluetooth/testing/bt-affordances/ffi_c/bindings.h"

using grpc::Status;
using grpc::StatusCode;

L2capService::L2capService() {
  // Connect to fuchsia.bluetooth.bredr.Profile
  zx::result profile_client_end = component::Connect<fuchsia_bluetooth_bredr::Profile>();
  if (profile_client_end.is_ok()) {
    profile_client_.Bind(std::move(*profile_client_end));
  } else {
    FX_LOGS(ERROR) << "Error connection to Profile service: " << profile_client_end.status_string();
  }
}

grpc::Status L2capService::Connect(::grpc::ServerContext* context,
                                   const ::pandora::l2cap::ConnectRequest* request,
                                   ::pandora::l2cap::ConnectResponse* response) {
  fuchsia_bluetooth::PeerId peer_id = fuchsia_bluetooth::PeerId{
      std::strtoul(request->connection().cookie().value().c_str(), nullptr, /*base=*/10)};
  uint64_t psm = static_cast<uint16_t>(request->basic().psm());
  fuchsia_bluetooth_bredr::L2capParameters l2cap_params;
  l2cap_params.psm(psm);
  fuchsia_bluetooth_bredr::ConnectParameters connect_params =
      fuchsia_bluetooth_bredr::ConnectParameters::WithL2cap(std::move(l2cap_params));

  auto result = profile_client_->Connect({peer_id, std::move(connect_params)});
  if (result.is_error()) {
    return Status(StatusCode::INTERNAL, "fuchsia.bluetooth.bredr.Profile/Connect error: " +
                                            result.error_value().FormatDescription());
  }

  auto& channel = result->channel();
  if (!channel.socket().has_value()) {
    return Status(StatusCode::INTERNAL, "Connected channel has no socket");
  }

  {
    std::scoped_lock lock(m_l2cap_socket_);
    l2cap_socket_ = std::move(channel.socket().value());
  }

  return Status::OK;
}

::grpc::Status L2capService::WaitConnection(::grpc::ServerContext* context,
                                            const ::pandora::l2cap::WaitConnectionRequest* request,
                                            ::pandora::l2cap::WaitConnectionResponse* response) {
  // PSM must match `TSPX_psm` IXIT value to pass PTS tests.
  uint64_t maybe_peer_id = advertise_service(/*psm=*/29, /*timeout=*/5 /*seconds*/);
  if (!maybe_peer_id) {
    return Status(StatusCode::INTERNAL, "Error in Rust affordances (check logs)");
  }
  if (maybe_peer_id == 1) {
    FX_LOGS(WARNING)
        << "It is likely that no connection was established on the advertised PSM before timeout.";
  }
  response->mutable_channel()->mutable_cookie()->set_value(std::to_string(maybe_peer_id));
  return {/*OK*/};
}

::grpc::Status L2capService::Disconnect(::grpc::ServerContext* context,
                                        const ::pandora::l2cap::DisconnectRequest* request,
                                        ::pandora::l2cap::DisconnectResponse* response) {
  std::scoped_lock lock(m_l2cap_socket_);
  if (!l2cap_socket_.is_valid()) {
    return Status(StatusCode::FAILED_PRECONDITION, "L2CAP channel not connected");
  }
  l2cap_socket_.reset();
  response->mutable_success();
  return Status::OK;
}

::grpc::Status L2capService::WaitDisconnection(
    ::grpc::ServerContext* context, const ::pandora::l2cap::WaitDisconnectionRequest* request,
    ::pandora::l2cap::WaitDisconnectionResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

::grpc::Status L2capService::Receive(
    ::grpc::ServerContext* context, const ::pandora::l2cap::ReceiveRequest* request,
    ::grpc::ServerWriter<::pandora::l2cap::ReceiveResponse>* writer) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

::grpc::Status L2capService::Send(::grpc::ServerContext* context,
                                  const ::pandora::l2cap::SendRequest* request,
                                  ::pandora::l2cap::SendResponse* response) {
  if (write_l2cap(reinterpret_cast<const uint8_t*>(request->data().data()),
                  request->data().size()) != ZX_OK) {
    return Status(StatusCode::INTERNAL, "Error in Rust affordances (check logs)");
  }
  response->mutable_success();
  return Status::OK;
}
