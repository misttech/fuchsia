// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-peripheral/usb-function.h"

#include <fidl/fuchsia.hardware.usb.endpoint/cpp/wire.h>
#include <lib/async/cpp/task.h>
#include <lib/ddk/metadata.h>
#include <lib/trace/event.h>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/usb/cpp/bind.h>
#include <fbl/alloc_checker.h>
#include <fbl/auto_lock.h>

#include "src/devices/usb/drivers/usb-peripheral/usb-peripheral.h"

namespace usb_peripheral {

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

zx::result<> UsbFunction::AddChild(fidl::UnownedClientEnd<fuchsia_driver_framework::Node> parent,
                                   const std::string& child_node_name,
                                   const std::shared_ptr<fdf::Namespace>& incoming,
                                   const std::shared_ptr<fdf::OutgoingDirectory>& outgoing) {
  TRACE_DURATION("usb-peripheral", __func__);
  outgoing_ = outgoing;
  child_node_name_ = child_node_name;
  if (child_.is_valid()) {
    return zx::error(ZX_ERR_ALREADY_BOUND);
  }

  {
    compat::DeviceServer::BanjoConfig banjo_config;
    banjo_config.callbacks[ZX_PROTOCOL_USB_FUNCTION] = banjo_server_.callback();
    zx::result result = compat_server_.Initialize(
        incoming, outgoing, std::string{UsbPeripheral::kChildNodeName}, child_node_name,
        compat::ForwardMetadata::None(), std::move(banjo_config));
    if (result.is_error()) {
      fdf::error("Failed to initialize compat server: {}", result);
      return result.take_error();
    }
  }

  auto& mac_address_metadata_server = mac_address_metadata_server_.emplace(child_node_name);
  if (zx::result result = mac_address_metadata_server.ForwardMetadataIfExists(incoming);
      result.is_error()) {
    fdf::error("Failed to forward mac address metadata: {}", result);
    return result.take_error();
  }
  if (zx::result result = mac_address_metadata_server.Serve(*outgoing, dispatcher_);
      result.is_error()) {
    fdf::error("Failed to serve mac address metadata: {}", result);
    return result.take_error();
  }

  auto& serial_number_metadata_server = serial_number_metadata_server_.emplace(child_node_name);
  if (zx::result result = serial_number_metadata_server.ForwardMetadataIfExists(incoming);
      result.is_error()) {
    fdf::error("Failed to forward serial number metadata: {}", result);
    return result.take_error();
  }
  if (zx::result result = serial_number_metadata_server.Serve(*outgoing, dispatcher_);
      result.is_error()) {
    fdf::error("Failed to serve serial number metadata: {}", result);
    return result.take_error();
  }

  bindings_.set_empty_set_handler([weak_this = weak_from_this()]() {
    std::shared_ptr self = weak_this.lock();
    if (!self) {
      return;
    }
    if (!self->alloc_resources_over_fidl_) {
      return;
    }
    // We need to release all allocated resources when our channel is closed.
    // Which also means that we need to close our connection with the function
    // interface to make sure that resources are not used after they've been
    // released.
    if (self->function_intf_fidl_.is_valid()) {
      self->function_intf_fidl_.AsyncTeardown();
      self->function_intf_fidl_ = {};
    }
    self->peripheral_->ReleaseResources(self->index_);
  });

  zx::result result = outgoing->AddService<fuchsia_hardware_usb_function::UsbFunctionService>(
      fuchsia_hardware_usb_function::UsbFunctionService::InstanceHandler({
          .device = bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure),
      }),
      child_node_name);
  if (result.is_error()) {
    fdf::error("Failed to add usb-function service: {}", result);
    return result.take_error();
  }

  auto& desc = GetFunctionDescriptor();

  std::vector props = {
      fdf::MakeProperty2(bind_fuchsia::PROTOCOL, bind_fuchsia_usb::BIND_PROTOCOL_FUNCTION),
      fdf::MakeProperty2(bind_fuchsia::USB_CLASS, static_cast<uint32_t>(desc.interface_class)),
      fdf::MakeProperty2(bind_fuchsia::USB_SUBCLASS,
                         static_cast<uint32_t>(desc.interface_subclass)),
      fdf::MakeProperty2(bind_fuchsia::USB_PROTOCOL,
                         static_cast<uint32_t>(desc.interface_protocol)),
      fdf::MakeProperty2(bind_fuchsia::USB_VID,
                         static_cast<uint32_t>(peripheral_->device_desc().id_vendor)),
      fdf::MakeProperty2(bind_fuchsia::USB_PID,
                         static_cast<uint32_t>(peripheral_->device_desc().id_product)),
  };

  std::vector offers = compat_server_.CreateOffers2();
  offers.push_back(
      fdf::MakeOffer2<fuchsia_hardware_usb_function::UsbFunctionService>(child_node_name));
  offers.push_back(mac_address_metadata_server.MakeOffer());
  offers.push_back(serial_number_metadata_server.MakeOffer());

  zx::result child =
      fdf::AddChild(parent, *fdf::Logger::GlobalInstance(), child_node_name, props, offers);
  if (child.is_error()) {
    fdf::error("Failed to add child: {}", child);
    return child.take_error();
  }
  child_.Bind(std::move(child.value()), dispatcher_,
              std::make_unique<NodeControllerEventHandler>(this));

  return zx::ok();
}

UsbFunction::~UsbFunction() {
  if (deconfigure_completer_.has_value()) {
    deconfigure_completer_->Reply(fit::ok());
    deconfigure_completer_.reset();
  }

  if (outgoing_ && !child_node_name_.empty()) {
    // We explicitly remove the child node and its associated services here to ensure
    // that they are scrubbed before the UsbFunction is destroyed. This prevents
    // resource name collisions if the driver re-initializes new functions without
    // a full driver restart (e.g. during a ClearFunctions/SetConfiguration cycle).
    // This also ensures teardown synchronization in the parent, keeping the system
    // clean by the time the parent's completion callbacks execute.
    fdf::info("UsbFunction destructor: cleaning up child {}", child_node_name_);
    if (zx::result result =
            outgoing_->RemoveService<fuchsia_hardware_usb_function::UsbFunctionService>(
                child_node_name_);
        result.is_error()) {
      fdf::warn("Failed to remove usb-function service for {}: {}", child_node_name_, result);
    }
    if (mac_address_metadata_server_) {
      if (zx::result result = outgoing_->RemoveService(
              fuchsia_boot_metadata::MacAddressMetadata::kSerializableName, child_node_name_);
          result.is_error()) {
        fdf::warn("Failed to remove mac address metadata service for {}: {}", child_node_name_,
                  result);
      }
    }
    if (serial_number_metadata_server_) {
      if (zx::result result = outgoing_->RemoveService(
              fuchsia_boot_metadata::SerialNumberMetadata::kSerializableName, child_node_name_);
          result.is_error()) {
        fdf::warn("Failed to remove serial number metadata service for {}: {}", child_node_name_,
                  result);
      }
    }

    if (child_.is_valid()) {
      fidl::Status result = child_->Remove();
      if (!result.ok()) {
        fdf::error("Failed to send Remove request to child node {}: {}", child_node_name_,
                   result.FormatDescription());
      }
    }
  }
}

// UsbFunctionProtocol implementation.
zx_status_t UsbFunction::UsbFunctionSetInterface(
    const usb_function_interface_protocol_t* function_intf) {
  TRACE_DURATION("usb-peripheral", __func__);
  auto func_intf = ddk::UsbFunctionInterfaceProtocolClient(function_intf);
  if (!func_intf.is_valid()) {
    bool was_valid = function_intf_.is_valid();
    function_intf_.clear();
    fdf::info("Taking peripheral device offline until ready");
    return was_valid ? peripheral_->DeviceStateChanged() : ZX_OK;
  }
  if (function_intf_.is_valid()) {
    fdf::error("Function interface already bound");
    return ZX_ERR_ALREADY_BOUND;
  }

  function_intf_ = func_intf;

  size_t length = function_intf_.GetDescriptorsSize();
  fbl::AllocChecker ac;
  auto* descriptors = new (&ac) uint8_t[length];
  if (!ac.check()) {
    fdf::error("UsbFunctionSetInterface failed due to no memory.");
    return ZX_ERR_NO_MEMORY;
  }

  size_t actual;
  function_intf_.GetDescriptors(descriptors, length, &actual);
  if (actual != length) {
    fdf::error("UsbFunctionInterfaceClient::GetDescriptors() failed");
    delete[] descriptors;
    return ZX_ERR_INTERNAL;
  }

  zx::result<uint8_t> validate_result = peripheral_->ValidateFunction(index_, descriptors, length);
  if (validate_result.is_error()) {
    fdf::error("UsbFunctionInterfaceClient::ValidateFunction() failed: {}", validate_result);
    delete[] descriptors;
    return validate_result.error_value();
  }
  num_interfaces_ = validate_result.value();

  descriptors_.reset(descriptors, length);
  return peripheral_->FunctionRegistered();
}

zx_status_t UsbFunction::UsbFunctionCancelAll(uint8_t ep_address) {
  TRACE_DURATION("usb-peripheral", __func__);
  return peripheral_->UsbDciCancelAll(ep_address);
}

zx_status_t UsbFunction::UsbFunctionAllocInterface(uint8_t* out_intf_num) {
  TRACE_DURATION("usb-peripheral", __func__);
  zx::result result = peripheral_->AllocResources(index_, 1, {}, {});
  if (result.is_error()) {
    return result.error_value();
  }
  *out_intf_num = result->interface_nums[0];
  return ZX_OK;
}

zx_status_t UsbFunction::UsbFunctionAllocEp(uint8_t direction, uint8_t* out_address) {
  TRACE_DURATION("usb-peripheral", __func__);
  fuchsia_hardware_usb_function::EndpointDirection fidl_direction;
  switch (direction) {
    case USB_DIR_OUT:
      fidl_direction = fuchsia_hardware_usb_function::EndpointDirection::kOut;
      break;
    case USB_DIR_IN:
      fidl_direction = fuchsia_hardware_usb_function::EndpointDirection::kIn;
      break;
    default:
      return ZX_ERR_INVALID_ARGS;
  }
  fuchsia_hardware_usb_function::EndpointResource ep{{
      .direction = fidl_direction,
  }};
  zx::result result = peripheral_->AllocResources(index_, 0, {&ep, 1}, {});
  if (result.is_error()) {
    return result.error_value();
  }
  *out_address = result->endpoint_addrs[0];
  return ZX_OK;
}

zx_status_t UsbFunction::UsbFunctionConfigEp(const usb_endpoint_descriptor_t* ep_desc,
                                             const usb_ss_ep_comp_descriptor_t* ss_comp_desc) {
  TRACE_DURATION("usb-peripheral", __func__);
  fidl::Arena arena;

  fuchsia_hardware_usb_function::EndpointConfiguration ep_config;
  fuchsia_hardware_usb_function::EndpointDescriptor desc;
  desc.bm_attributes(ep_desc->bm_attributes);
  // The FIDL type here is host endian.
  desc.w_max_packet_size(le16toh(ep_desc->w_max_packet_size));
  desc.b_interval(ep_desc->b_interval);
  ep_config.descriptor(std::move(desc));

  if (ss_comp_desc != nullptr) {  // Only applies to 3.x devices.
    fuchsia_hardware_usb_function::SuperSpeedEndpointCompanionDescriptor ss;
    ss.b_max_burst(ss_comp_desc->b_max_burst);
    ss.bm_attributes(ss_comp_desc->bm_attributes);
    // The FIDL type here is host endian.
    ss.w_bytes_per_interval(le16toh(ss_comp_desc->w_bytes_per_interval));
    ep_config.super_speed_companion(std::move(ss));
  }

  return CommonEndpointConfigure(ep_desc->b_endpoint_address, std::move(ep_config));
}

zx_status_t UsbFunction::UsbFunctionDisableEp(uint8_t address) {
  TRACE_DURATION("usb-peripheral", __func__);
  fidl::Arena arena;
  auto result = peripheral_->dci_new().buffer(arena)->DisableEndpoint(address);

  if (!result.ok()) {
    fdf::debug("Failed to send DisableEndpoint request: {}", result.status_string());
  } else if (result->is_error() && result->error_value() == ZX_ERR_NOT_SUPPORTED) {
    fdf::debug("Failed to disable endpoint: {}", result.status_string());
  } else if (result->is_error() && result->error_value() != ZX_ERR_NOT_SUPPORTED) {
    return result->error_value();
  } else {
    return ZX_OK;
  }

  fdf::debug("could not DisableEndpoint() over FIDL, falling back to banjo");
  return peripheral_->dci().DisableEp(address);
}

zx_status_t UsbFunction::UsbFunctionAllocStringDesc(const char* str, uint8_t* out_index) {
  TRACE_DURATION("usb-peripheral", __func__);
  std::string string_desc(str);
  auto result = peripheral_->AllocResources(index_, 0, {}, {&string_desc, 1});
  if (result.is_error()) {
    return result.error_value();
  }
  *out_index = result->string_indices[0];
  return ZX_OK;
}

void UsbFunction::UsbFunctionRequestQueue(usb_request_t* usb_request,
                                          const usb_request_complete_callback_t* complete_cb) {
  TRACE_DURATION("usb-peripheral", __func__);
  peripheral_->UsbPeripheralRequestQueue(usb_request, complete_cb);
}

zx_status_t UsbFunction::UsbFunctionEpSetStall(uint8_t ep_address) {
  TRACE_DURATION("usb-peripheral", __func__, "ep_address", ep_address);
  return CommonEndpointSetStall(ep_address);
}

zx_status_t UsbFunction::UsbFunctionEpClearStall(uint8_t ep_address) {
  TRACE_DURATION("usb-peripheral", __func__, "ep_address", ep_address);
  return CommonEndpointClearStall(ep_address);
}

size_t UsbFunction::UsbFunctionGetRequestSize() {
  TRACE_DURATION("usb-peripheral", __func__);
  return peripheral_->ParentRequestSize();
}

void UsbFunction::ConnectToEndpoint(ConnectToEndpointRequest& request,
                                    ConnectToEndpointCompleter::Sync& completer) {
  TRACE_DURATION("usb-peripheral", __func__);
  auto status = peripheral_->ConnectToEndpoint(request.ep_addr(), std::move(request.ep()));
  if (status != ZX_OK) {
    completer.Reply(fit::as_error(status));
    return;
  }
  completer.Reply(fit::ok());
}

void UsbFunction::AllocResources(AllocResourcesRequest& request,
                                 AllocResourcesCompleter::Sync& completer) {
  TRACE_DURATION("usb-peripheral", __func__);

  zx::result result = peripheral_->AllocResources(index_, request.interface_count(),
                                                  request.endpoints(), request.strings());
  if (result.is_error()) {
    completer.Reply(fit::as_error(result.error_value()));
    return;
  }

  fuchsia_hardware_usb_function::UsbFunctionAllocResourcesResponse response;
  response.interface_nums(std::move(result->interface_nums));
  response.endpoint_addrs(std::move(result->endpoint_addrs));
  response.string_indices(std::move(result->string_indices));
  alloc_resources_over_fidl_ = true;
  completer.Reply(fit::ok(std::move(response)));
}

void UsbFunction::EndpointSetStall(EndpointSetStallRequest& request,
                                   EndpointSetStallCompleter::Sync& completer) {
  TRACE_DURATION("usb-peripheral", __func__, "ep_address", request.endpoint_address());
  zx_status_t status = CommonEndpointSetStall(request.endpoint_address());
  if (status != ZX_OK) {
    completer.Reply(fit::as_error(status));
    return;
  }
  completer.Reply(fit::ok());
}

void UsbFunction::EndpointClearStall(EndpointClearStallRequest& request,
                                     EndpointClearStallCompleter::Sync& completer) {
  TRACE_DURATION("usb-peripheral", __func__, "ep_address", request.endpoint_address());
  zx_status_t status = CommonEndpointClearStall(request.endpoint_address());
  if (status != ZX_OK) {
    completer.Reply(fit::as_error(status));
    return;
  }
  completer.Reply(fit::ok());
}

void UsbFunction::ConfigureEndpoint(ConfigureEndpointRequest& request,
                                    ConfigureEndpointCompleter::Sync& completer) {
  TRACE_DURATION("usb-peripheral", __func__, "ep_address", request.endpoint_address());
  zx_status_t status =
      CommonEndpointConfigure(request.endpoint_address(), request.endpoint_configuration());
  if (status != ZX_OK) {
    completer.Reply(fit::as_error(status));
    return;
  }
  completer.Reply(fit::ok());
}

void UsbFunction::DisableEndpoint(DisableEndpointRequest& request,
                                  DisableEndpointCompleter::Sync& completer) {
  TRACE_DURATION("usb-peripheral", __func__, "ep_address", request.endpoint_address());
  zx_status_t status = CommonEndpointDisable(request.endpoint_address());
  if (status != ZX_OK) {
    completer.Reply(fit::as_error(status));
    return;
  }
  completer.Reply(fit::ok());
}

void UsbFunction::Configure(ConfigureRequest& request, ConfigureCompleter::Sync& completer) {
  TRACE_DURATION("usb-peripheral", __func__);
  if (function_intf_fidl_.is_valid() || function_intf_.is_valid()) {
    fdf::error("Function interface already bound");
    completer.Reply(fit::as_error(ZX_ERR_ALREADY_BOUND));
    return;
  }

  size_t length = request.configuration().size();
  fbl::AllocChecker ac;
  auto* descriptors = new (&ac) uint8_t[length];
  if (!ac.check()) {
    fdf::error("UsbFunction::Configure failed due to no memory.");
    completer.Reply(fit::as_error(ZX_ERR_NO_MEMORY));
    return;
  }

  memcpy(descriptors, request.configuration().data(), length);
  num_interfaces_ = 0;

  zx::result validate_result = peripheral_->ValidateFunction(index_, descriptors, length);
  if (validate_result.is_error()) {
    fdf::error("UsbFunctionInterfaceClient::ValidateFunction() failed: {}", validate_result);
    delete[] descriptors;
    completer.Reply(fit::as_error(validate_result.error_value()));
    return;
  }
  num_interfaces_ = validate_result.value();

  descriptors_.reset(descriptors, length);
  function_intf_fidl_.Bind(std::move(request.iface()), dispatcher_,
                           std::make_unique<FunctionEventHandler>(this));
  zx_status_t status = peripheral_->FunctionRegistered();
  if (status != ZX_OK) {
    completer.Reply(fit::as_error(status));
    fdf::error("FunctionRegistered failed: {}", zx_status_get_string(status));
    function_intf_fidl_ = {};
    descriptors_.reset();
    return;
  }

  completer.Reply(fit::ok());
}

void UsbFunction::Deconfigure(DeconfigureCompleter::Sync& completer) {
  TRACE_DURATION("usb-peripheral", __func__);
  ZX_ASSERT_MSG(!function_intf_.is_valid(), "mixed banjo and FIDL");
  if (deconfigure_completer_.has_value()) {
    completer.Reply(fit::as_error(ZX_ERR_UNAVAILABLE));
    return;
  }
  if (!function_intf_fidl_.is_valid()) {
    fdf::warn("Deconfigure called with no function interface bound");
    completer.Reply(fit::ok());
    return;
  }
  deconfigure_completer_.emplace(completer.ToAsync());
  function_intf_fidl_.AsyncTeardown();
}

void UsbFunction::FunctionEventHandler::on_fidl_error(fidl::UnbindInfo info) {
  switch (info.status()) {
    case ZX_ERR_PEER_CLOSED:
    case ZX_OK:
    case ZX_ERR_CANCELED:
      if (std::shared_ptr parent = parent_.lock()) {
        parent->CloseFunctionInterface();
      }
      break;
    default:
      fdf::error("Unexpected FIDL error on function interface: {}",
                 zx_status_get_string(info.status()));
      return;
  }
}

void UsbFunction::FunctionEventHandler::handle_unknown_event(
    fidl::UnknownEventMetadata<fuchsia_hardware_usb_function::UsbFunctionInterface> metadata) {
  fdf::error("Unknown event on function interface: {}", metadata.event_ordinal);
}

UsbFunction::FunctionEventHandler::~FunctionEventHandler() {
  if (std::shared_ptr parent = parent_.lock()) {
    parent->CloseFunctionInterface();
  }
}

void UsbFunction::CloseFunctionInterface() {
  function_intf_fidl_ = {};
  descriptors_.reset();
  peripheral_->DeviceStateChanged();
  if (deconfigure_completer_.has_value()) {
    deconfigure_completer_->Reply(fit::ok());
    deconfigure_completer_.reset();
  }
}

void UsbFunction::RequestRemoval() {
  if (!child_.is_valid()) {
    fdf::warn(
        "UsbFunction: RequestRemoval called on function {} which has no valid child node "
        "controller.",
        child_node_name_);
    // If there is no child, we're already effectively cleared.
    peripheral_->FunctionCleared();
    return;
  }

  fdf::debug("UsbFunction: Sending Remove() to node Controller for {}.", child_node_name_);
  fidl::Status result = child_->Remove();
  if (!result.ok()) {
    fdf::error("Failed to send Remove request to child node {}: {}", child_node_name_,
               result.FormatDescription());
  }
}

void UsbFunction::OnNodeControllerUnbound(fidl::UnbindInfo info) {
  fdf::debug("UsbFunction: OnNodeControllerUnbound called for {}.", child_node_name_);
  if (info.is_peer_closed()) {
    fdf::info("UsbFunction: Node for {} unbound (peer closed).", child_node_name_);
  } else if (!info.is_user_initiated()) {
    fdf::error("UsbFunction: Node for {} unbound with error: {}", child_node_name_,
               info.FormatDescription());
  } else {
    fdf::info("UsbFunction: Node for {} unbound (user initiated).", child_node_name_);
  }
  peripheral_->FunctionCleared();
}

void UsbFunction::SetConfigured(bool configured, usb_speed_t speed,
                                fit::callback<void(zx_status_t)> completer) {
  TRACE_DURATION("usb-peripheral", __func__);
  if (function_intf_fidl_) {
    fdescriptor::wire::UsbSpeed fspeed = static_cast<fdescriptor::wire::UsbSpeed>(speed);
    function_intf_fidl_->SetConfigured(configured, fspeed)
        .ThenExactlyOnce(
            [completer = std::move(completer)](
                fidl::WireUnownedResult<
                    fuchsia_hardware_usb_function::UsbFunctionInterface::SetConfigured>&
                    result) mutable {
              if (!result.ok()) {
                fdf::error("UsbFunctionInterface.SetConfigured FIDL call failed: {}",
                           result.FormatDescription());
                completer(result.status());
                return;
              }
              if (result->is_error()) {
                fdf::error("UsbFunctionInterface.SetConfigured error: {}",
                           zx_status_get_string(result->error_value()));
                completer(result->error_value());
                return;
              }
              completer(ZX_OK);
            });
    return;
  }
  if (function_intf_.is_valid()) {
    fdf::warn("{}: FIDL client not valid, falling back to banjo", __func__);
    completer(function_intf_.SetConfigured(configured, speed));
    return;
  }
  fdf::error("SetConfigured failed as the interface is invalid.");
  completer(ZX_ERR_BAD_STATE);
}

void UsbFunction::SetInterface(uint8_t interface, uint8_t alt_setting,
                               fit::callback<void(zx_status_t)> completer) {
  TRACE_DURATION("usb-peripheral", __func__);
  if (function_intf_fidl_) {
    function_intf_fidl_->SetInterface(interface, alt_setting)
        .ThenExactlyOnce([completer = std::move(completer)](
                             fidl::WireUnownedResult<
                                 fuchsia_hardware_usb_function::UsbFunctionInterface::SetInterface>&
                                 result) mutable {
          if (!result.ok()) {
            fdf::error("UsbFunctionInterface.SetInterface FIDL call failed: {}",
                       result.FormatDescription());
            completer(result.status());
            return;
          }
          if (result->is_error()) {
            fdf::error("UsbFunctionInterface.SetInterface error: {}",
                       zx_status_get_string(result->error_value()));
            completer(result->error_value());
            return;
          }
          completer(ZX_OK);
        });
    return;
  }
  if (function_intf_.is_valid()) {
    fdf::warn("{}: FIDL client not valid, falling back to banjo", __func__);
    completer(function_intf_.SetInterface(interface, alt_setting));
    return;
  }
  fdf::error("SetInterface failed as the interface is invalid.");
  completer(ZX_ERR_BAD_STATE);
}

// TODO(https://fxbug.dev/493657863): This call should be async like
// SetConfigured and SetInterface once we can guarantee a single-dispatch of
// USB_RECIP_DEVICE requests to bound functions.
zx::result<std::vector<uint8_t>> UsbFunction::Control(
    const fuchsia_hardware_usb_descriptor::wire::UsbSetup& setup,
    cpp20::span<uint8_t> write_buffer) {
  TRACE_DURATION("usb-peripheral", __func__);
  if (function_intf_fidl_) {
    fidl::VectorView<uint8_t> write_data =
        fidl::VectorView<uint8_t>::FromExternal(write_buffer.data(), write_buffer.size());
    size_t expected_read_size = le16toh(setup.w_length);

    auto result = function_intf_fidl_.sync()->Control(setup, write_data);
    if (!result.ok()) {
      fdf::error("UsbFunctionInterface.Control FIDL call failed: {}", result.FormatDescription());
      return zx::error(result.status());
    }
    if (result->is_error()) {
      fdf::error("UsbFunctionInterface.Control error: {}",
                 zx_status_get_string(result->error_value()));
      return zx::error(result->error_value());
    }

    fuchsia_hardware_usb_function::wire::UsbFunctionInterfaceControlResponse* response =
        result->value();
    size_t actual_read = response->read.size();
    if (actual_read > expected_read_size) {
      fdf::error("Control read too much data: {} > {}", actual_read, expected_read_size);
      return zx::error(ZX_ERR_BUFFER_TOO_SMALL);
    }

    std::vector<uint8_t> read_data_vec(response->read.begin(), response->read.end());
    return zx::ok(std::move(read_data_vec));
  }
  if (function_intf_.is_valid()) {
    fdf::warn("{}: FIDL client not valid, falling back to banjo", __func__);
    uint8_t direction = setup.bm_request_type & USB_DIR_MASK;
    size_t expected_read_size = (direction == USB_DIR_IN) ? le16toh(setup.w_length) : 0;
    std::vector<uint8_t> read_data_vec(expected_read_size);
    size_t actual_read = 0;

    zx_status_t status = function_intf_.Control(
        reinterpret_cast<const usb_setup_t*>(&setup), write_buffer.data(), write_buffer.size(),
        read_data_vec.data(), expected_read_size, &actual_read);

    if (status != ZX_OK) {
      return zx::error(status);
    }
    if (actual_read > expected_read_size) {
      return zx::error(ZX_ERR_BUFFER_TOO_SMALL);
    }
    read_data_vec.resize(actual_read);
    return zx::ok(std::move(read_data_vec));
  }
  fdf::error("Control failed as the interface is invalid.");
  return zx::error(ZX_ERR_BAD_STATE);
}

zx_status_t UsbFunction::CommonEndpointSetStall(uint8_t ep_address) {
  if (!peripheral_->ValidateEndpoint(index_, ep_address)) {
    return ZX_ERR_NOT_FOUND;
  }

  fidl::Arena arena;
  if (peripheral_->dci_new().is_valid()) {
    auto result = peripheral_->dci_new().buffer(arena)->EndpointSetStall(ep_address);

    if (!result.ok()) {
      fdf::error("Failed to send EndpointSetStall request: {}", result.status_string());
      return result.status();
    }
    if (result->is_error()) {
      if (result->error_value() != ZX_ERR_NOT_SUPPORTED) {
        fdf::error("EndpointSetStall failed: {}", zx_status_get_string(result->error_value()));
      }
      return result->error_value();
    }
    return ZX_OK;
  }

  fdf::warn("could not EndointSetStall() over FIDL, falling back to banjo");
  return peripheral_->dci().EpSetStall(ep_address);
}

zx_status_t UsbFunction::CommonEndpointClearStall(uint8_t ep_address) {
  if (!peripheral_->ValidateEndpoint(index_, ep_address)) {
    return ZX_ERR_NOT_FOUND;
  }

  fidl::Arena arena;
  if (peripheral_->dci_new().is_valid()) {
    auto result = peripheral_->dci_new().buffer(arena)->EndpointClearStall(ep_address);

    if (!result.ok()) {
      fdf::error("Failed to send EndpointClearStall request: {}", result.status_string());
      return result.status();
    }
    if (result->is_error()) {
      if (result->error_value() != ZX_ERR_NOT_SUPPORTED) {
        fdf::error("EndpointClearStall failed: {}", zx_status_get_string(result->error_value()));
      }
      return result->error_value();
    }
    return ZX_OK;
  }

  fdf::warn("could not EndointClearStall() over FIDL, falling back to banjo");
  return peripheral_->dci().EpClearStall(ep_address);
}

zx_status_t UsbFunction::CommonEndpointConfigure(
    uint8_t ep_address,
    fuchsia_hardware_usb_function::EndpointConfiguration endpoint_configuration) {
  if (!endpoint_configuration.descriptor().has_value()) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (!peripheral_->ValidateEndpoint(index_, ep_address)) {
    return ZX_ERR_NOT_FOUND;
  }

  if (peripheral_->dci_new().is_valid()) {
    fidl::Arena arena;

    fdescriptor::wire::UsbEndpointDescriptor fep_desc = {
        .b_endpoint_address = ep_address,
        .bm_attributes = endpoint_configuration.descriptor()->bm_attributes(),
        // TODO(https://fxbug.dev/497048374): FIDL types should use host
        // endianness.
        .w_max_packet_size = htole16(endpoint_configuration.descriptor()->w_max_packet_size()),
        .b_interval = endpoint_configuration.descriptor()->b_interval(),
    };
    fdescriptor::wire::UsbSsEpCompDescriptor fss_comp_desc;
    if (endpoint_configuration.super_speed_companion().has_value()) {
      fss_comp_desc = {
          .b_max_burst = endpoint_configuration.super_speed_companion()->b_max_burst(),
          .bm_attributes = endpoint_configuration.super_speed_companion()->bm_attributes(),
          // TODO(https://fxbug.dev/497048374): FIDL types should use host
          // endianness.
          .w_bytes_per_interval =
              htole16(endpoint_configuration.super_speed_companion()->w_bytes_per_interval()),
      };
    }
    auto result = peripheral_->dci_new().buffer(arena)->ConfigureEndpoint(fep_desc, fss_comp_desc);

    if (!result.ok()) {
      fdf::debug("Failed to send ConfigureEndpoint request: {}", result.status_string());
    } else if (result->is_error() && result->error_value() == ZX_ERR_NOT_SUPPORTED) {
      fdf::debug("Failed to configure endpoint: {}", result.status_string());
    } else if (result->is_error() && result->error_value() != ZX_ERR_NOT_SUPPORTED) {
      return result->error_value();
    } else {
      return ZX_OK;
    }
  }

  usb_endpoint_descriptor_t ep_desc = {
      .b_endpoint_address = ep_address,
      .bm_attributes = endpoint_configuration.descriptor()->bm_attributes(),
      .w_max_packet_size = htole16(endpoint_configuration.descriptor()->w_max_packet_size()),
      .b_interval = endpoint_configuration.descriptor()->b_interval(),
  };
  usb_ss_ep_comp_descriptor_t ss_comp_desc;
  usb_ss_ep_comp_descriptor_t* ss_comp_desc_ptr = nullptr;
  if (endpoint_configuration.super_speed_companion().has_value()) {
    ss_comp_desc = {
        .b_max_burst = endpoint_configuration.super_speed_companion()->b_max_burst(),
        .bm_attributes = endpoint_configuration.super_speed_companion()->bm_attributes(),
        .w_bytes_per_interval =
            htole16(endpoint_configuration.super_speed_companion()->w_bytes_per_interval()),
    };
    ss_comp_desc_ptr = &ss_comp_desc;
  }

  fdf::debug("could not ConfigureEndpoint() over FIDL, falling back to banjo");
  return peripheral_->dci().ConfigEp(&ep_desc, ss_comp_desc_ptr);
}

zx_status_t UsbFunction::CommonEndpointDisable(uint8_t ep_address) {
  if (!peripheral_->ValidateEndpoint(index_, ep_address)) {
    return ZX_ERR_NOT_FOUND;
  }

  fidl::Arena arena;
  auto result = peripheral_->dci_new().buffer(arena)->DisableEndpoint(ep_address);

  if (!result.ok()) {
    fdf::debug("Failed to send DisableEndpoint request: {}", result.status_string());
  } else if (result->is_error() && result->error_value() == ZX_ERR_NOT_SUPPORTED) {
    fdf::debug("Failed to disable endpoint: {}", result.status_string());
  } else if (result->is_error() && result->error_value() != ZX_ERR_NOT_SUPPORTED) {
    return result->error_value();
  } else {
    return ZX_OK;
  }

  fdf::debug("could not DisableEndpoint() over FIDL, falling back to banjo");
  return peripheral_->dci().DisableEp(ep_address);
}

}  // namespace usb_peripheral
