// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "gatt.h"

#include <lib/syslog/cpp/macros.h>

#include "src/connectivity/bluetooth/testing/bt-affordances/ffi_c/bindings.h"

using grpc::Status;
using grpc::StatusCode;

Status GattService::ExchangeMTU(::grpc::ServerContext* context,
                                const ::pandora::ExchangeMTURequest* request,
                                ::pandora::ExchangeMTUResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status GattService::WriteAttFromHandle(::grpc::ServerContext* context,
                                       const ::pandora::WriteRequest* request,
                                       ::pandora::WriteResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status GattService::DiscoverServiceByUuid(::grpc::ServerContext* context,
                                          const ::pandora::DiscoverServiceByUuidRequest* request,
                                          ::pandora::DiscoverServicesResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status GattService::DiscoverServices(::grpc::ServerContext* context,
                                     const ::pandora::DiscoverServicesRequest* request,
                                     ::pandora::DiscoverServicesResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status GattService::DiscoverServicesSdp(::grpc::ServerContext* context,
                                        const ::pandora::DiscoverServicesSdpRequest* request,
                                        ::pandora::DiscoverServicesSdpResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status GattService::ClearCache(::grpc::ServerContext* context,
                               const ::pandora::ClearCacheRequest* request,
                               ::pandora::ClearCacheResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status GattService::ReadCharacteristicFromHandle(
    ::grpc::ServerContext* context, const ::pandora::ReadCharacteristicRequest* request,
    ::pandora::ReadCharacteristicResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status GattService::ReadCharacteristicsFromUuid(
    ::grpc::ServerContext* context, const ::pandora::ReadCharacteristicsFromUuidRequest* request,
    ::pandora::ReadCharacteristicsFromUuidResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status GattService::ReadCharacteristicDescriptorFromHandle(
    ::grpc::ServerContext* context, const ::pandora::ReadCharacteristicDescriptorRequest* request,
    ::pandora::ReadCharacteristicDescriptorResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status GattService::RegisterService(::grpc::ServerContext* context,
                                    const ::pandora::RegisterServiceRequest* request,
                                    ::pandora::RegisterServiceResponse* response) {
  pandora::GattCharacteristicParams characteristic = request->service().characteristics()[0];

  // Use arbitrary handles.
  if (publish_service(
          /*handle=*/0x123, /*uuid=*/request->service().uuid().c_str(),
          /*characteristic_handle=*/0x456,
          /*characteristic_uuid=*/characteristic.uuid().c_str(),
          /*characteristic_properties=*/static_cast<uint16_t>(characteristic.properties()),
          /*characteristic_permissions=*/static_cast<uint16_t>(characteristic.permissions())) !=
      ZX_OK) {
    return Status(StatusCode::INTERNAL, "Error in Rust affordances (check logs)");
  }

  // TODO(https://fxbug.dev/42075291): Expose attribute handles for testing.
  //
  // PTS asks for attribute handles. Sapphire assigns them deterministically, so we can rely on the
  // value 0xe being assigned here for now, but this is not a safe assumption in the long term. We
  // should modify the API or create a new API that exposes attribute handles for testing.
  response->mutable_service()->add_characteristics()->set_handle(0xe);

  return {/*OK*/};
}

Status GattService::SetCharacteristicNotificationFromHandle(
    ::grpc::ServerContext* context,
    const ::pandora::SetCharacteristicNotificationFromHandleRequest* request,
    ::pandora::SetCharacteristicNotificationFromHandleResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status GattService::WaitCharacteristicNotification(
    ::grpc::ServerContext* context, const ::pandora::WaitCharacteristicNotificationRequest* request,
    ::pandora::WaitCharacteristicNotificationResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status GattService::NotifyOnCharacteristic(::grpc::ServerContext* context,
                                           const ::pandora::NotifyOnCharacteristicRequest* request,
                                           ::pandora::NotifyOnCharacteristicResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}

Status GattService::IndicateOnCharacteristic(
    ::grpc::ServerContext* context, const ::pandora::IndicateOnCharacteristicRequest* request,
    ::pandora::IndicateOnCharacteristicResponse* response) {
  return Status(StatusCode::UNIMPLEMENTED, "");
}
