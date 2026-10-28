// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-composite/usb-interface.h"

#include <fuchsia/hardware/usb/c/banjo.h>
#include <fuchsia/hardware/usb/composite/c/banjo.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/logger.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <fbl/alloc_checker.h>
#include <usb/descriptors.h>
#include <usb/usb-request.h>

#include "src/devices/usb/drivers/usb-composite/usb-composite.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_composite {

zx::result<std::unique_ptr<UsbInterface>> UsbInterface::Create(
    UsbComposite* composite, const ddk::UsbProtocolClient& usb,
    fidl::ClientEnd<fuchsia_hardware_usb::Usb> usb_new,
    const usb_interface_descriptor_t* interface_desc, size_t desc_length,
    async_dispatcher_t* dispatcher) {
  fbl::AllocChecker ac;
  auto interface =
      fbl::make_unique_checked<UsbInterface>(&ac, composite, usb, std::move(usb_new), dispatcher);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  auto* device_desc = composite->device_descriptor();
  uint8_t usb_class, usb_subclass, usb_protocol;

  if (interface_desc->b_interface_class == fdescriptor::UsbClass::kPerInterface) {
    usb_class = device_desc->b_device_class;
    usb_subclass = device_desc->b_device_sub_class;
    usb_protocol = device_desc->b_device_protocol;
  } else {
    // class/subclass/protocol defined per-interface
    usb_class = interface_desc->b_interface_class;
    usb_subclass = interface_desc->b_interface_sub_class;
    usb_protocol = interface_desc->b_interface_protocol;
  }

  auto status = interface->Init(interface_desc, desc_length, interface_desc->b_interface_number,
                                usb_class, usb_subclass, usb_protocol);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  status = interface->ConfigureEndpoints(interface_desc->b_interface_number, 0);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  return zx::ok(std::move(interface));
}

zx::result<std::unique_ptr<UsbInterface>> UsbInterface::Create(
    UsbComposite* composite, const ddk::UsbProtocolClient& usb,
    fidl::ClientEnd<fuchsia_hardware_usb::Usb> usb_new,
    const usb_interface_assoc_descriptor_t* assoc_desc, size_t desc_length,
    async_dispatcher_t* dispatcher) {
  fbl::AllocChecker ac;
  auto interface =
      fbl::make_unique_checked<UsbInterface>(&ac, composite, usb, std::move(usb_new), dispatcher);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  auto* device_desc = composite->device_descriptor();
  uint8_t usb_class, usb_subclass, usb_protocol;

  if (assoc_desc->b_function_class == 0) {
    usb_class = device_desc->b_device_class;
    usb_subclass = device_desc->b_device_sub_class;
    usb_protocol = device_desc->b_device_protocol;
  } else {
    // class/subclass/protocol defined per-interface
    usb_class = assoc_desc->b_function_class;
    usb_subclass = assoc_desc->b_function_sub_class;
    usb_protocol = assoc_desc->b_function_protocol;
  }

  // Interfaces in an IAD interface collection must be contiguous.
  auto last_interface_id = assoc_desc->b_first_interface + assoc_desc->b_interface_count - 1;
  auto status = interface->Init(assoc_desc, desc_length, static_cast<uint8_t>(last_interface_id),
                                usb_class, usb_subclass, usb_protocol);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  usb::UnownedInterfaceList assoc_list(const_cast<usb_interface_assoc_descriptor_t*>(assoc_desc),
                                       desc_length, true);
  for (const auto& intf : assoc_list) {
    if (!intf.association() || intf.association() != assoc_desc) {
      break;
    }
    if (!intf.descriptor()) {
      fdf::error("Malformed USB descriptor detected!");
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    zx_status_t status = interface->ConfigureEndpoints(intf.descriptor()->b_interface_number, 0);
    if (status != ZX_OK) {
      return zx::error(status);
    }
  }

  return zx::ok(std::move(interface));
}

UsbInterface::~UsbInterface() {
  if (controller_.is_valid() && !node_removed_) {
    fidl::OneWayStatus result = fidl::WireCall(controller_)->Remove();
    if (!result.ok()) {
      fdf::debug("Failed to remove node {}: {}", name_, result.FormatDescription());
    }
  }
  if (outgoing_ != nullptr) {
    if (zx::result result = outgoing_->RemoveService<fuchsia_hardware_usb::UsbService>(name_);
        result.is_error()) {
      fdf::warn("Failed to remove UsbService instance {}: {}", name_, result);
    }
  }
}

zx::result<fidl::ClientEnd<fuchsia_driver_framework::NodeController>> UsbInterface::AddChild(
    fidl::UnownedClientEnd<fuchsia_driver_framework::Node> parent,
    const std::shared_ptr<fdf::OutgoingDirectory>& outgoing, std::string_view name,
    std::vector<fuchsia_driver_framework::NodeProperty2> properties) {
  if (controller_.is_valid()) {
    return zx::error(ZX_ERR_ALREADY_BOUND);
  }
  name_ = name;

  // Serve the Banjo protocols that DFv1 returned from DdkGetProtocol().
  compat::DeviceServer::BanjoConfig banjo_config{.default_proto_id = ZX_PROTOCOL_USB};
  banjo_config.callbacks[ZX_PROTOCOL_USB] = usb_banjo_server_.callback();
  banjo_config.callbacks[ZX_PROTOCOL_USB_COMPOSITE] = usb_composite_banjo_server_.callback();
  compat_server_.Initialize(name_, std::nullopt, std::move(banjo_config));
  if (zx_status_t status = compat_server_.Serve(dispatcher_, outgoing.get()); status != ZX_OK) {
    fdf::error("Failed to serve compat device server: {}", zx_status_get_string(status));
    return zx::error(status);
  }

  zx::result result = outgoing->AddService<fuchsia_hardware_usb::UsbService>(
      fuchsia_hardware_usb::UsbService::InstanceHandler({
          .device = bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure),
      }),
      name_);
  if (result.is_error()) {
    fdf::error("Failed to add UsbService: {}", result);
    return result.take_error();
  }
  outgoing_ = outgoing;

  std::vector offers = compat_server_.CreateOffers2();
  offers.push_back(fdf::MakeOffer2<fuchsia_hardware_usb::UsbService>(name_));

  fuchsia_driver_framework::NodeAddArgs args{{
      .name = name_,
      .offers2 = std::move(offers),
      .properties2 = std::move(properties),
  }};

  auto [controller_client_end, controller_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
  fidl::Result add_child_result =
      fidl::Call(parent)->AddChild({std::move(args), std::move(controller_server_end), {}});
  if (add_child_result.is_error()) {
    fdf::error("Failed to add child {}: {}", name_,
               add_child_result.error_value().FormatDescription());
    return zx::error(add_child_result.error_value().is_framework_error()
                         ? add_child_result.error_value().framework_error().status()
                         : ZX_ERR_INTERNAL);
  }

  return zx::ok(std::move(controller_client_end));
}

zx_status_t UsbInterface::SetController(
    fidl::ClientEnd<fuchsia_driver_framework::NodeController> controller) {
  controller_ = std::move(controller);
  controller_wait_.set_object(controller_.channel().get());
  controller_wait_.set_trigger(ZX_CHANNEL_PEER_CLOSED);
  if (zx_status_t status = controller_wait_.Begin(dispatcher_); status != ZX_OK) {
    fdf::error("Failed to wait on node controller for {}: {}", name_, zx_status_get_string(status));
    // The destructor removes the node, since |controller_| is valid.
    return status;
  }
  if (remove_requested_) {
    // AsyncRemove() was called before the controller was available.
    AsyncRemove();
  }
  return ZX_OK;
}

void UsbInterface::AsyncRemove() {
  remove_requested_ = true;
  if (!controller_.is_valid()) {
    // SetController() will request the removal.
    return;
  }
  fidl::OneWayStatus result = fidl::WireCall(controller_)->Remove();
  if (!result.ok()) {
    fdf::warn("Failed to remove node {}: {}", name_, result.FormatDescription());
  }
}

void UsbInterface::OnControllerClosed(async_dispatcher_t* dispatcher, async::WaitBase* wait,
                                      zx_status_t status, const zx_packet_signal_t* signal) {
  if (status != ZX_OK) {
    // The wait was cancelled, e.g. because the dispatcher is shutting down.
    return;
  }
  OnNodeRemoved();
}

void UsbInterface::OnNodeRemoved() {
  node_removed_ = true;
  // This destroys |this|, so it must be the last thing done here. That is safe from within
  // OnControllerClosed() because |controller_wait_| is no longer pending when its handler runs.
  composite_->RemoveInterface(this);
}

void UsbInterface::ConnectToEndpoint(ConnectToEndpointRequest& request,
                                     ConnectToEndpointCompleter::Sync& completer) {
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
    fdf::error("Failed to call ConnectToEndpoint(): {}", result.FormatDescription());
    completer.Reply(fit::as_error(result.status()));
    return;
  }
  if (result->is_error()) {
    fdf::error("ConnectToEndpoint() error: {}", zx_status_get_string(result->error_value()));
    completer.Reply(fit::as_error(result->error_value()));
    return;
  }
  completer.Reply(fit::ok());
}

zx_status_t UsbInterface::Init(const void* descriptors, size_t desc_length,
                               uint8_t last_interface_id, uint8_t usb_class, uint8_t usb_subclass,
                               uint8_t usb_protocol) {
  fbl::AllocChecker ac;
  auto desc_bytes = new (&ac) uint8_t[desc_length];
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  memcpy(desc_bytes, descriptors, desc_length);
  descriptors_.reset(desc_bytes, desc_length);
  last_interface_id_ = last_interface_id;
  usb_class_ = usb_class;
  usb_subclass_ = usb_subclass;
  usb_protocol_ = usb_protocol;

  return ZX_OK;
}

// for determining index into active_endpoints[]
// b_endpoint_address has 4 lower order bits, plus high bit to signify direction
// shift high bit to bit 4 so index is in range 0 - 31.
static inline uint8_t GetEndpointIndex(const usb_endpoint_descriptor_t* ep) {
  return static_cast<uint8_t>(((ep)->b_endpoint_address & 0x0F) | ((ep)->b_endpoint_address >> 3));
}

zx_status_t UsbInterface::ConfigureEndpoints(uint8_t interface_id, uint8_t alt_setting) {
  std::optional<usb::Endpoint> new_endpoints[USB_MAX_EPS];
  bool interface_endpoints[USB_MAX_EPS] = {};
  zx_status_t status = ZX_OK;

  // iterate through our descriptors to find which endpoints should be active
  usb::UnownedInterfaceList intf_list(reinterpret_cast<void*>(descriptors_.data()),
                                      descriptors_.size(), false);

  bool enable_endpoints = false;

  for (const auto& intf : intf_list) {
    if (!intf.descriptor()) {
      fdf::error("Malformed USB descriptor detected!");
      return false;
    }
    if (intf.descriptor()->b_interface_number != interface_id) {
      continue;
    }
    enable_endpoints = (intf.descriptor()->b_alternate_setting == alt_setting);
    for (const auto& ep : intf.GetEndpointList()) {
      auto ep_index = GetEndpointIndex(ep.descriptor());
      interface_endpoints[ep_index] = true;
      if (enable_endpoints) {
        new_endpoints[ep_index] = ep;
      }
    }
  }

  // update to new set of endpoints
  // FIXME - how do we recover if we fail half way through processing the endpoints?
  for (size_t i = 0; i < std::size(new_endpoints); i++) {
    if (interface_endpoints[i]) {
      auto* old_ep = active_endpoints_[i];
      auto new_ep = new_endpoints[i];
      if (old_ep != (new_ep.has_value() ? new_ep->descriptor() : nullptr)) {
        if (old_ep) {
          zx_status_t ret = usb_.EnableEndpoint(old_ep, nullptr, false);
          if (ret != ZX_OK) {
            fdf::error("EnableEndpoint failed: {}", zx_status_get_string(ret));
            status = ret;
          }
        }
        if (new_ep) {
          zx_status_t ret = usb_.EnableEndpoint(new_ep->descriptor(),
                                                new_ep->ss_companion().value_or(nullptr), true);
          if (ret != ZX_OK) {
            fdf::error("EnableEndpoint failed: {}", zx_status_get_string(ret));
            status = ret;
          }
        }
        active_endpoints_[i] = new_ep.has_value() ? new_ep->descriptor() : nullptr;
      }
    }
  }
  return status;
}

zx_status_t UsbInterface::UsbControlOut(uint8_t request_type, uint8_t request, uint16_t value,
                                        uint16_t index, zx_time_t timeout,
                                        const uint8_t* write_buffer, size_t write_size) {
  return usb_.ControlOut(request_type, request, value, index, timeout, write_buffer, write_size);
}

zx_status_t UsbInterface::UsbControlIn(uint8_t request_type, uint8_t request, uint16_t value,
                                       uint16_t index, zx_time_t timeout, uint8_t* out_read_buffer,
                                       size_t read_size, size_t* out_read_actual) {
  return usb_.ControlIn(request_type, request, value, index, timeout, out_read_buffer, read_size,
                        out_read_actual);
}

void UsbInterface::UsbRequestQueue(usb_request_t* usb_request,
                                   const usb_request_complete_callback_t* complete_cb) {
  usb_.RequestQueue(usb_request, complete_cb);
}

usb_speed_t UsbInterface::UsbGetSpeed() { return usb_.GetSpeed(); }

zx_status_t UsbInterface::UsbSetInterface(uint8_t interface_number, uint8_t alt_setting) {
  return composite_->SetInterface(interface_number, alt_setting);
}

uint8_t UsbInterface::UsbGetConfiguration() { return usb_.GetConfiguration(); }

zx_status_t UsbInterface::UsbSetConfiguration(uint8_t configuration) {
  return usb_.SetConfiguration(configuration);
}

zx_status_t UsbInterface::UsbEnableEndpoint(const usb_endpoint_descriptor_t* ep_desc,
                                            const usb_ss_ep_comp_descriptor_t* ss_com_desc,
                                            bool enable) {
  return ZX_ERR_NOT_SUPPORTED;
}

zx_status_t UsbInterface::UsbResetEndpoint(uint8_t ep_address) {
  return usb_.ResetEndpoint(ep_address);
}

zx_status_t UsbInterface::UsbResetDevice() { return usb_.ResetDevice(); }

size_t UsbInterface::UsbGetMaxTransferSize(uint8_t ep_address) {
  return usb_.GetMaxTransferSize(ep_address);
}

uint32_t UsbInterface::UsbGetDeviceId() { return usb_.GetDeviceId(); }

void UsbInterface::UsbGetDeviceDescriptor(usb_device_descriptor_t* out_desc) {
  return usb_.GetDeviceDescriptor(out_desc);
}

zx_status_t UsbInterface::UsbGetConfigurationDescriptorLength(uint8_t configuration,
                                                              size_t* out_length) {
  return usb_.GetConfigurationDescriptorLength(configuration, out_length);
}

zx_status_t UsbInterface::UsbGetConfigurationDescriptor(uint8_t configuration,
                                                        uint8_t* out_desc_buffer, size_t desc_size,
                                                        size_t* out_desc_actual) {
  return usb_.GetConfigurationDescriptor(configuration, out_desc_buffer, desc_size,
                                         out_desc_actual);
}

size_t UsbInterface::UsbGetDescriptorsLength() { return descriptors_.size(); }

void UsbInterface::UsbGetDescriptors(uint8_t* out_descs_buffer, size_t descs_size,
                                     size_t* out_descs_actual) {
  size_t length = descriptors_.size();
  if (length > descs_size) {
    length = descs_size;
  }
  memcpy(out_descs_buffer, descriptors_.data(), length);
  *out_descs_actual = length;
}

size_t UsbInterface::UsbCompositeGetAdditionalDescriptorLength() {
  auto* config = composite_->GetConfigurationDescriptor();
  usb::UnownedInterfaceList intf_list(const_cast<usb_configuration_descriptor_t*>(config),
                                      le16toh(config->w_total_length), true);
  for (const auto& intf : intf_list) {
    if (!intf.descriptor()) {
      fdf::error("Malformed USB descriptor detected!");
      return false;
    }
    // We are only interested in descriptors past the last stored descriptor
    // for the current interface.
    if (intf.descriptor()->b_interface_number > last_interface_id_) {
      return le16toh(config->w_total_length) -
             (reinterpret_cast<uintptr_t>(intf.descriptor()) - reinterpret_cast<uintptr_t>(config));
    }
  }
  return 0;
}

zx_status_t UsbInterface::UsbCompositeGetAdditionalDescriptorList(uint8_t* out_desc_list,
                                                                  size_t desc_count,
                                                                  size_t* out_desc_actual) {
  return composite_->GetAdditionalDescriptorList(last_interface_id_, out_desc_list, desc_count,
                                                 out_desc_actual);
}

zx_status_t UsbInterface::UsbGetStringDescriptor(uint8_t desc_id, uint16_t lang_id,
                                                 uint16_t* out_lang_id, uint8_t* out_string_buffer,
                                                 size_t string_size, size_t* out_string_actual) {
  return usb_.GetStringDescriptor(desc_id, lang_id, out_lang_id, out_string_buffer, string_size,
                                  out_string_actual);
}

zx_status_t UsbInterface::UsbCancelAll(uint8_t ep_address) { return usb_.CancelAll(ep_address); }

uint64_t UsbInterface::UsbGetCurrentFrame() { return usb_.GetCurrentFrame(); }

size_t UsbInterface::UsbGetRequestSize() { return usb_.GetRequestSize(); }

zx_status_t UsbInterface::UsbCompositeClaimInterface(const usb_interface_descriptor_t* desc,
                                                     uint32_t length) {
  auto status = composite_->ClaimInterface(desc->b_interface_number);
  if (status != ZX_OK) {
    return status;
  }
  // Copy claimed interface descriptors to end of descriptor array.
  fbl::AllocChecker ac;
  size_t old_length = descriptors_.size();
  size_t new_length = old_length + length;
  auto* new_descriptors = new (&ac) uint8_t[new_length];
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  memcpy(new_descriptors, descriptors_.data(), old_length);
  memcpy(new_descriptors + old_length, desc, length);
  descriptors_.reset(new_descriptors, new_length);

  if (desc->b_interface_number > last_interface_id_) {
    last_interface_id_ = desc->b_interface_number;
  }
  return ZX_OK;
}

bool UsbInterface::ContainsInterface(uint8_t interface_id) {
  usb::UnownedInterfaceList intf_list(reinterpret_cast<void*>(descriptors_.data()),
                                      descriptors_.size(), true);
  for (const auto& intf : intf_list) {
    if (!intf.descriptor()) {
      fdf::error("Malformed USB descriptor detected!");
      return false;
    }
    if (intf.descriptor()->b_interface_number == interface_id) {
      return true;
    }
  }
  return false;
}

zx_status_t UsbInterface::SetAltSetting(uint8_t interface_id, uint8_t alt_setting) {
  zx_status_t status = ConfigureEndpoints(interface_id, alt_setting);
  if (status != ZX_OK) {
    return status;
  }

  return UsbControlOut(kStandardInterfaceOut,
                       fidl::ToUnderlying(fdescriptor::StandardRequest::kSetInterface), alt_setting,
                       interface_id, ZX_TIME_INFINITE, nullptr, 0);
}

}  // namespace usb_composite
