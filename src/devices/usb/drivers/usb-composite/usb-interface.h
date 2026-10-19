// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_COMPOSITE_USB_INTERFACE_H_
#define SRC_DEVICES_USB_DRIVERS_USB_COMPOSITE_USB_INTERFACE_H_

#include <endian.h>
#include <fidl/fuchsia.hardware.usb/cpp/fidl.h>
#include <fuchsia/hardware/usb/composite/cpp/banjo.h>
#include <fuchsia/hardware/usb/cpp/banjo.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/ddk/debug.h>

#include <ddktl/device.h>
#include <fbl/array.h>
#include <fbl/mutex.h>
#include <usb/descriptors.h>
#include <usb/usb.h>

#include "src/devices/usb/drivers/usb-composite/usb-composite.h"

namespace usb_composite {

class UsbComposite;
class UsbInterface;
using UsbInterfaceType = ddk::Device<UsbInterface, ddk::GetProtocolable, ddk::Unbindable>;

// This class represents a USB interface in a composite device.
class UsbInterface : public UsbInterfaceType,
                     public ddk::UsbProtocol<UsbInterface, ddk::base_protocol>,
                     public ddk::UsbCompositeProtocol<UsbInterface>,
                     public fidl::Server<fuchsia_hardware_usb::Usb> {
 public:
  UsbInterface(zx_device_t* parent, UsbComposite* composite, const ddk::UsbProtocolClient& usb,
               fidl::ClientEnd<fuchsia_hardware_usb::Usb> usb_new, async_dispatcher_t* dispatcher)
      : UsbInterfaceType(parent),
        composite_(composite),
        usb_(usb),
        usb_new_(std::move(usb_new)),
        outgoing_(dispatcher) {}

  static zx::result<fidl::ClientEnd<fuchsia_io::Directory>> Create(
      zx_device_t* parent, UsbComposite* composite, const ddk::UsbProtocolClient& usb,
      fidl::ClientEnd<fuchsia_hardware_usb::Usb> usb_new,
      const usb_interface_descriptor_t* interface_desc, size_t desc_length,
      async_dispatcher_t* dispatcher, std::unique_ptr<UsbInterface>* out_interface);
  static zx::result<fidl::ClientEnd<fuchsia_io::Directory>> Create(
      zx_device_t* parent, UsbComposite* composite, const ddk::UsbProtocolClient& usb,
      fidl::ClientEnd<fuchsia_hardware_usb::Usb> usb_new,
      const usb_interface_assoc_descriptor_t* assoc_desc, size_t desc_length,
      async_dispatcher_t* dispatcher, std::unique_ptr<UsbInterface>* out_interface);

  // Device protocol implementation.
  zx_status_t DdkGetProtocol(uint32_t proto_id, void* out);
  void DdkUnbind(ddk::UnbindTxn txn);
  void DdkRelease();

  // fuchsia_hardware_usb::Usb protocol implementation.
  void ConnectToEndpoint(ConnectToEndpointRequest& request,
                         ConnectToEndpointCompleter::Sync& completer) override {
    constexpr uint32_t kMask = 0x7FU;
    constexpr uint32_t kReservedMask = 0x70U;
    const uint32_t ep_addr = request.ep_addr();
    const uint32_t ep_num = ep_addr & kMask;
    if (ep_num == 0 || ep_num > 15 || (ep_addr & kReservedMask) != 0) {
      completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
      return;
    }
    fidl::WireResult<fuchsia_hardware_usb::Usb::ConnectToEndpoint> result =
        usb_new_->ConnectToEndpoint(request.ep_addr(), std::move(request.ep()));
    if (!result.ok()) {
      zxlogf(ERROR, "Failed to call ConnectToEndpoint(): %s",
             result.error().FormatDescription().c_str());
      completer.Reply(fit::as_error(result.status()));
      return;
    }
    if (result->is_error()) {
      zxlogf(ERROR, "ConnectToEndpoint() error: %s", zx_status_get_string(result->error_value()));
      completer.Reply(fit::as_error(result->error_value()));
      return;
    }
    completer.Reply(fit::ok());
  }
  void GetDeviceSpeed(GetDeviceSpeedCompleter::Sync& completer) override {
    completer.Reply(
        {{.speed = static_cast<fuchsia_hardware_usb_descriptor::UsbSpeed>(UsbGetSpeed())}});
  }
  void GetDeviceId(GetDeviceIdCompleter::Sync& completer) override {
    completer.Reply({{.device_id = UsbGetDeviceId()}});
  }
  void GetDeviceDescriptor(GetDeviceDescriptorCompleter::Sync& completer) override {
    usb_device_descriptor_t raw_desc{};
    UsbGetDeviceDescriptor(&raw_desc);
    fuchsia_hardware_usb_descriptor::UsbDeviceDescriptor desc{{
        .b_length = raw_desc.b_length,
        .b_descriptor_type = raw_desc.b_descriptor_type,
        .bcd_usb = le16toh(raw_desc.bcd_usb),
        .b_device_class = raw_desc.b_device_class,
        .b_device_sub_class = raw_desc.b_device_sub_class,
        .b_device_protocol = raw_desc.b_device_protocol,
        .b_max_packet_size0 = raw_desc.b_max_packet_size0,
        .id_vendor = le16toh(raw_desc.id_vendor),
        .id_product = le16toh(raw_desc.id_product),
        .bcd_device = le16toh(raw_desc.bcd_device),
        .i_manufacturer = raw_desc.i_manufacturer,
        .i_product = raw_desc.i_product,
        .i_serial_number = raw_desc.i_serial_number,
        .b_num_configurations = raw_desc.b_num_configurations,
    }};
    completer.Reply({{.descriptor = desc}});
  }
  void GetConfigurationDescriptor(GetConfigurationDescriptorRequest& request,
                                  GetConfigurationDescriptorCompleter::Sync& completer) override {
    size_t length = 0;
    zx_status_t status = usb_.GetConfigurationDescriptorLength(request.config(), &length);
    if (status != ZX_OK) {
      completer.Reply(fit::as_error(status));
      return;
    }
    if (length > fuchsia_hardware_usb::kMaxDescriptorLen) {
      completer.Reply(fit::as_error(ZX_ERR_OUT_OF_RANGE));
      return;
    }
    std::vector<uint8_t> desc(length);
    size_t actual = 0;
    status = usb_.GetConfigurationDescriptor(request.config(), desc.data(), desc.size(), &actual);
    if (status != ZX_OK) {
      completer.Reply(fit::as_error(status));
      return;
    }
    desc.resize(actual);
    completer.Reply(fit::ok(fuchsia_hardware_usb::UsbGetConfigurationDescriptorResponse{
        {.descriptor = std::move(desc)}}));
  }
  void GetConfiguration(GetConfigurationCompleter::Sync& completer) override {
    completer.Reply({{.configuration = UsbGetConfiguration()}});
  }
  void SetConfiguration(SetConfigurationRequest& request,
                        SetConfigurationCompleter::Sync& completer) override {
    completer.Reply(fit::as_error(ZX_ERR_NOT_SUPPORTED));
  }
  void SetInterface(SetInterfaceRequest& request, SetInterfaceCompleter::Sync& completer) override {
    zx_status_t status = UsbSetInterface(request.interface_number(), request.alt_setting());
    if (status != ZX_OK) {
      completer.Reply(fit::as_error(status));
      return;
    }
    completer.Reply(fit::ok());
  }
  void ControlTransfer(ControlTransferRequest& request,
                       ControlTransferCompleter::Sync& completer) override {
    const zx_duration_t timeout = request.timeout_ns() > 0 ? request.timeout_ns() : ZX_SEC(5);
    const fuchsia_hardware_usb_descriptor::UsbSetup& setup = request.setup();
    const bool is_in = usb_request_is_in(setup.bm_request_type());
    if (is_in) {
      if (setup.w_length() > fuchsia_hardware_usb::kMaxControlRequestLen ||
          !request.write_data().empty()) {
        completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
        return;
      }
      std::vector<uint8_t> buf(setup.w_length());
      size_t actual = 0;
      zx_status_t status = UsbControlIn(setup.bm_request_type(), setup.b_request(), setup.w_value(),
                                        setup.w_index(), timeout, buf.data(), buf.size(), &actual);
      if (status != ZX_OK) {
        completer.Reply(fit::as_error(status));
        return;
      }
      buf.resize(std::min(actual, buf.size()));
      completer.Reply(
          fit::ok(fuchsia_hardware_usb::UsbControlTransferResponse{{.read_data = std::move(buf)}}));
      return;
    }

    if (request.write_data().size() != setup.w_length()) {
      completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
      return;
    }

    if (setup.bm_request_type() == kStandardDeviceOut &&
        setup.b_request() ==
            fidl::ToUnderlying(
                fuchsia_hardware_usb_descriptor::StandardRequest::kSetConfiguration)) {
      completer.Reply(fit::as_error(ZX_ERR_NOT_SUPPORTED));
      return;
    }

    if (setup.bm_request_type() == kStandardInterfaceOut &&
        setup.b_request() ==
            fidl::ToUnderlying(fuchsia_hardware_usb_descriptor::StandardRequest::kSetInterface)) {
      if (setup.w_length() != 0 || setup.w_index() > UINT8_MAX || setup.w_value() > UINT8_MAX) {
        completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
        return;
      }
      zx_status_t status = UsbSetInterface(static_cast<uint8_t>(setup.w_index()),
                                           static_cast<uint8_t>(setup.w_value()));
      if (status != ZX_OK) {
        completer.Reply(fit::as_error(status));
        return;
      }
      completer.Reply(fit::ok(fuchsia_hardware_usb::UsbControlTransferResponse{{.read_data = {}}}));
      return;
    }

    zx_status_t status =
        UsbControlOut(setup.bm_request_type(), setup.b_request(), setup.w_value(), setup.w_index(),
                      timeout, request.write_data().data(), request.write_data().size());
    if (status != ZX_OK) {
      completer.Reply(fit::as_error(status));
      return;
    }
    completer.Reply(fit::ok(fuchsia_hardware_usb::UsbControlTransferResponse{{.read_data = {}}}));
  }
  void EnableEndpoint(EnableEndpointRequest& request,
                      EnableEndpointCompleter::Sync& completer) override {
    completer.Reply(fit::as_error(ZX_ERR_NOT_SUPPORTED));
  }
  void ResetEndpoint(ResetEndpointRequest& request,
                     ResetEndpointCompleter::Sync& completer) override {
    constexpr uint32_t kMask = 0x7FU;
    constexpr uint32_t kReservedMask = 0x70U;
    const uint32_t ep_addr = request.ep_address();
    const uint32_t ep_num = ep_addr & kMask;
    if ((ep_num == 0 && ep_addr != 0) || ep_num > 15 || (ep_addr & kReservedMask) != 0) {
      completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
      return;
    }
    zx_status_t status = UsbResetEndpoint(request.ep_address());
    if (status != ZX_OK) {
      completer.Reply(fit::as_error(status));
      return;
    }
    completer.Reply(fit::ok());
  }
  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_usb::Usb> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  // USB protocol implementation.
  zx_status_t UsbControlOut(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                            int64_t timeout, const uint8_t* write_buffer, size_t write_size);
  zx_status_t UsbControlIn(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                           int64_t timeout, uint8_t* out_read_buffer, size_t read_size,
                           size_t* out_read_actual);
  void UsbRequestQueue(usb_request_t* usb_request,
                       const usb_request_complete_callback_t* complete_cb);
  usb_speed_t UsbGetSpeed();
  zx_status_t UsbSetInterface(uint8_t interface_number, uint8_t alt_setting);
  uint8_t UsbGetConfiguration();
  zx_status_t UsbSetConfiguration(uint8_t configuration);
  zx_status_t UsbEnableEndpoint(const usb_endpoint_descriptor_t* ep_desc,
                                const usb_ss_ep_comp_descriptor_t* ss_com_desc, bool enable);
  zx_status_t UsbResetEndpoint(uint8_t ep_address);
  zx_status_t UsbResetDevice();
  size_t UsbGetMaxTransferSize(uint8_t ep_address);
  uint32_t UsbGetDeviceId();
  void UsbGetDeviceDescriptor(usb_device_descriptor_t* out_desc);
  zx_status_t UsbGetConfigurationDescriptorLength(uint8_t configuration, size_t* out_length);
  zx_status_t UsbGetConfigurationDescriptor(uint8_t configuration, uint8_t* out_desc_buffer,
                                            size_t desc_size, size_t* out_desc_actual);
  size_t UsbGetDescriptorsLength();
  void UsbGetDescriptors(uint8_t* out_descs_buffer, size_t descs_size, size_t* out_descs_actual);
  zx_status_t UsbGetStringDescriptor(uint8_t desc_id, uint16_t lang_id, uint16_t* out_lang_id,
                                     uint8_t* out_string_buffer, size_t string_size,
                                     size_t* out_string_actual);
  zx_status_t UsbCancelAll(uint8_t ep_address);
  uint64_t UsbGetCurrentFrame();
  size_t UsbGetRequestSize();

  // USB composite protocol implementation.
  size_t UsbCompositeGetAdditionalDescriptorLength();
  zx_status_t UsbCompositeGetAdditionalDescriptorList(uint8_t* out_desc_list, size_t desc_count,
                                                      size_t* out_desc_actual);
  zx_status_t UsbCompositeClaimInterface(const usb_interface_descriptor_t* desc, uint32_t length);

  bool ContainsInterface(uint8_t interface_id);
  zx_status_t SetAltSetting(uint8_t interface_id, uint8_t alt_setting);

  inline uint8_t usb_class() const { return usb_class_; }
  inline uint8_t usb_subclass() const { return usb_subclass_; }
  inline uint8_t usb_protocol() const { return usb_protocol_; }

 private:
  template <auto* descriptors>
  friend class UsbInterfaceTest;

  zx_status_t Init(const void* descriptors, size_t desc_length, uint8_t last_interface_id,
                   uint8_t usb_class, uint8_t usb_subclass, uint8_t usb_protocol);
  zx_status_t ConfigureEndpoints(uint8_t interface_id, uint8_t alt_setting);

  zx::result<fidl::ClientEnd<fuchsia_io::Directory>> ServeOutgoing(async_dispatcher_t* dispatcher) {
    zx::result result = outgoing_.AddService<fuchsia_hardware_usb::UsbService>(
        fuchsia_hardware_usb::UsbService::InstanceHandler({
            .device = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
        }));
    auto endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
    if (endpoints.is_error()) {
      return zx::error(endpoints.status_value());
    }
    result = outgoing_.Serve(std::move(endpoints->server));
    if (result.is_error()) {
      zxlogf(ERROR, "Failed to service the outgoing directory");
      return zx::error(result.status_value());
    }

    return zx::ok(std::move(endpoints->client));
  }

  UsbComposite* composite_;
  const ddk::UsbProtocolClient usb_;
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb> usb_new_;

  uint8_t usb_class_;
  uint8_t usb_subclass_;
  uint8_t usb_protocol_;

  // ID of the last interface in the descriptor list.
  uint8_t last_interface_id_;

  fbl::Array<uint8_t> descriptors_;

  // Descriptors for currently active endpoints.
  // These point into descriptor_'s storage.
  const usb_endpoint_descriptor_t* active_endpoints_[USB_MAX_EPS] = {};

  component::OutgoingDirectory outgoing_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb::Usb> bindings_;
};

}  // namespace usb_composite

#endif  // SRC_DEVICES_USB_DRIVERS_USB_COMPOSITE_USB_INTERFACE_H_
