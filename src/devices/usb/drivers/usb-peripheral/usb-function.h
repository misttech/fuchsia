// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_PERIPHERAL_USB_FUNCTION_H_
#define SRC_DEVICES_USB_DRIVERS_USB_PERIPHERAL_USB_FUNCTION_H_

#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.function/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.peripheral/cpp/fidl.h>
#include <fuchsia/hardware/usb/dci/cpp/banjo.h>
#include <fuchsia/hardware/usb/function/cpp/banjo.h>
#include <lib/async/cpp/wait.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/metadata/cpp/metadata_server.h>
#include <lib/trace/event.h>
#include <threads.h>

#include <fbl/array.h>
#include <usb/usb.h>

namespace usb_peripheral {

class UsbPeripheral;

// This class represents a USB function in the peripheral role configurations.
// USB function drivers bind to this.
class UsbFunction
    : public ddk::UsbFunctionProtocol<UsbFunction>,
      public fidl::Server<fuchsia_hardware_usb_function::UsbFunction>,
      public fidl::WireAsyncEventHandler<fuchsia_hardware_usb_function::UsbFunctionInterface> {
 public:
  UsbFunction(size_t index, UsbPeripheral* peripheral,
              fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor desc, uint8_t configuration,
              async_dispatcher_t* dispatcher)
      : index_(index),
        configuration_(configuration),
        peripheral_(peripheral),
        function_descriptor_(desc),
        dispatcher_(dispatcher) {}

  // UsbFunctionProtocol implementation.
  zx_status_t UsbFunctionSetInterface(const usb_function_interface_protocol_t* interface);
  zx_status_t UsbFunctionAllocInterface(uint8_t* out_intf_num);
  zx_status_t UsbFunctionAllocEp(uint8_t direction, uint8_t* out_address);
  zx_status_t UsbFunctionConfigEp(const usb_endpoint_descriptor_t* ep_desc,
                                  const usb_ss_ep_comp_descriptor_t* ss_comp_desc);
  zx_status_t UsbFunctionDisableEp(uint8_t address);
  zx_status_t UsbFunctionAllocStringDesc(const char* str, uint8_t* out_index);
  void UsbFunctionRequestQueue(usb_request_t* usb_request,
                               const usb_request_complete_callback_t* complete_cb);
  zx_status_t UsbFunctionEpSetStall(uint8_t ep_address);
  zx_status_t UsbFunctionEpClearStall(uint8_t ep_address);
  size_t UsbFunctionGetRequestSize();

  void SetConfigured(bool configured, usb_speed_t speed,
                     fit::callback<void(zx_status_t)> completer);
  void SetInterface(uint8_t interface, uint8_t alt_setting,
                    fit::callback<void(zx_status_t)> completer);
  zx::result<std::vector<uint8_t>> Control(
      const fuchsia_hardware_usb_descriptor::wire::UsbSetup& setup,
      cpp20::span<uint8_t> write_buffer);
  uint8_t configuration() const { return configuration_; }

  // fidl::WireAsyncEventHandler<fuchsia_hardware_usb_function::UsbFunctionInterface>
  void on_fidl_error(fidl::UnbindInfo info) override;
  void handle_unknown_event(
      fidl::UnknownEventMetadata<fuchsia_hardware_usb_function::UsbFunctionInterface> metadata)
      override;

  inline const usb_descriptor_header_t* GetDescriptors(size_t* out_length) const {
    *out_length = descriptors_.size();
    return reinterpret_cast<usb_descriptor_header_t*>(descriptors_.data());
  }

  inline const fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor& GetFunctionDescriptor()
      const {
    return function_descriptor_;
  }

  inline uint8_t GetNumInterfaces() const { return num_interfaces_; }

  zx_status_t UsbFunctionCancelAll(uint8_t ep_address);

  // fuchsia_hardware_usb_function.UsbFunction protocol implementation.
  void ConnectToEndpoint(ConnectToEndpointRequest& request,
                         ConnectToEndpointCompleter::Sync& completer) override;
  void Configure(ConfigureRequest& request, ConfigureCompleter::Sync& completer) override;
  void AllocResources(AllocResourcesRequest& request,
                      AllocResourcesCompleter::Sync& completer) override;
  void EndpointSetStall(EndpointSetStallRequest& request,
                        EndpointSetStallCompleter::Sync& completer) override;
  void EndpointClearStall(EndpointClearStallRequest& request,
                          EndpointClearStallCompleter::Sync& completer) override;

  zx::result<> AddChild(fidl::UnownedClientEnd<fuchsia_driver_framework::Node> parent,
                        const std::string& child_node_name,
                        const std::shared_ptr<fdf::Namespace>& incoming,
                        const std::shared_ptr<fdf::OutgoingDirectory>& outgoing);
  bool registered() const { return function_intf_.is_valid() || function_intf_fidl_.is_valid(); }

 private:
  zx_status_t CommonEndpointSetStall(uint8_t ep_address);
  zx_status_t CommonEndpointClearStall(uint8_t ep_address);

  DISALLOW_COPY_ASSIGN_AND_MOVE(UsbFunction);

  const size_t index_;
  uint8_t configuration_;
  // This is a cheeky guard to prevent FIDL-releated side-effects until function
  // drivers are all moved to FIDL. It prevents us from releasing allocated
  // resources if they have not been allocated via the FIDL API.
  //
  // TODO(https://fxbug.dev/439593030): Remove this flag once we decide to move
  // all resources to the FIDL interface.
  bool alloc_resources_over_fidl_ = false;
  UsbPeripheral* peripheral_;
  ddk::UsbFunctionInterfaceProtocolClient function_intf_;
  fidl::WireSharedClient<fuchsia_hardware_usb_function::UsbFunctionInterface> function_intf_fidl_;
  thrd_t thread_;
  int CompletionThread();
  const fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor function_descriptor_;

  uint8_t num_interfaces_ = 0;
  fbl::Array<uint8_t> descriptors_;

  async_dispatcher_t* dispatcher_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb_function::UsbFunction> bindings_;
  fidl::ClientEnd<fuchsia_driver_framework::NodeController> child_;
  compat::SyncInitializedDeviceServer compat_server_;
  compat::BanjoServer banjo_server_{ZX_PROTOCOL_USB_FUNCTION, this, &usb_function_protocol_ops_};
  std::optional<fdf_metadata::MetadataServer<fuchsia_boot_metadata::MacAddressMetadata>>
      mac_address_metadata_server_;
  std::optional<fdf_metadata::MetadataServer<fuchsia_boot_metadata::SerialNumberMetadata>>
      serial_number_metadata_server_;
};

}  // namespace usb_peripheral

#endif  // SRC_DEVICES_USB_DRIVERS_USB_PERIPHERAL_USB_FUNCTION_H_
