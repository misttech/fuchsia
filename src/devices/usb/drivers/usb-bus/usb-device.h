// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_BUS_USB_DEVICE_H_
#define SRC_DEVICES_USB_DRIVERS_USB_BUS_USB_DEVICE_H_

#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.device/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.hci/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb/cpp/fidl.h>
#include <fuchsia/hardware/usb/bus/cpp/banjo.h>
#include <fuchsia/hardware/usb/cpp/banjo.h>
#include <fuchsia/hardware/usb/hci/cpp/banjo.h>
#include <fuchsia/hardware/usb/hub/cpp/banjo.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/sync/completion.h>
#include <threads.h>

#include <optional>

#include <ddktl/device.h>
#include <fbl/array.h>
#include <fbl/mutex.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <usb/descriptors.h>
#include <usb/request-cpp.h>
#include <usb/usb-request.h>
#include <usb/usb.h>

namespace usb_bus {

// Abstract waiter class for waiting on a sync_completion_t.
// This is necessary to allow injection of a timer by a test
// into the UsbDevice class, allowing for a simulated clock.
class UsbWaiterInterface : public fbl::RefCounted<UsbWaiterInterface> {
 public:
  virtual zx_status_t Wait(sync_completion_t* completion, zx_duration_t duration) = 0;
  virtual ~UsbWaiterInterface() = default;
};

class UsbDevice;
using UsbDeviceType = ddk::Device<UsbDevice, ddk::GetProtocolable,
                                  ddk::Messageable<fuchsia_hardware_usb_device::Device>::Mixin>;

class UsbDevice : public UsbDeviceType,
                  public ddk::UsbProtocol<UsbDevice, ddk::base_protocol>,
                  public fbl::RefCounted<UsbDevice> {
 public:
  class UsbProtocolServer : public fidl::Server<fuchsia_hardware_usb::Usb> {
   public:
    explicit UsbProtocolServer(UsbDevice* owner) : owner_(owner) {}
    void ConnectToEndpoint(ConnectToEndpointRequest& request,
                           ConnectToEndpointCompleter::Sync& completer) override;
    void GetDeviceSpeed(GetDeviceSpeedCompleter::Sync& completer) override;
    void GetDeviceId(GetDeviceIdCompleter::Sync& completer) override;
    void GetDeviceDescriptor(GetDeviceDescriptorCompleter::Sync& completer) override;
    void GetConfigurationDescriptor(GetConfigurationDescriptorRequest& request,
                                    GetConfigurationDescriptorCompleter::Sync& completer) override;
    void GetConfiguration(GetConfigurationCompleter::Sync& completer) override;
    void SetConfiguration(SetConfigurationRequest& request,
                          SetConfigurationCompleter::Sync& completer) override;
    void SetInterface(SetInterfaceRequest& request,
                      SetInterfaceCompleter::Sync& completer) override;
    void ControlTransfer(ControlTransferRequest& request,
                         ControlTransferCompleter::Sync& completer) override;
    void EnableEndpoint(EnableEndpointRequest& request,
                        EnableEndpointCompleter::Sync& completer) override;
    void ResetEndpoint(ResetEndpointRequest& request,
                       ResetEndpointCompleter::Sync& completer) override;
    void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_usb::Usb> metadata,
                               fidl::UnknownMethodCompleter::Sync& completer) override;

    void Bind(async_dispatcher_t* dispatcher,
              fidl::ServerEnd<fuchsia_hardware_usb::Usb> server_end) {
      bindings_.AddBinding(dispatcher, std::move(server_end), this, fidl::kIgnoreBindingClosure);
    }

    fidl::ProtocolHandler<fuchsia_hardware_usb::Usb> CreateHandler(async_dispatcher_t* dispatcher) {
      return bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure);
    }

   private:
    UsbDevice* const owner_;
    fidl::ServerBindingGroup<fuchsia_hardware_usb::Usb> bindings_;
  };

  UsbDevice(zx_device_t* parent, const ddk::UsbHciProtocolClient& hci,
            fidl::ClientEnd<fuchsia_hardware_usb_hci::UsbHci> hci_new, uint32_t device_id,
            uint32_t hub_id, usb_speed_t speed, fbl::RefPtr<UsbWaiterInterface> waiter,
            async_dispatcher_t* dispatcher)
      : UsbDeviceType(parent),
        device_id_(device_id),
        hub_id_(hub_id),
        speed_(speed),
        hci_(hci),
        bus_(parent),
        waiter_(waiter),
        usb_server_(this),
        outgoing_(dispatcher) {
    ZX_ASSERT(hci_new.is_valid());
    hci_new_.Bind(std::move(hci_new));
  }

  static zx_status_t Create(zx_device_t* parent, const ddk::UsbHciProtocolClient& hci,
                            fidl::ClientEnd<fuchsia_hardware_usb_hci::UsbHci> hci_new,
                            uint32_t device_id, uint32_t hub_id, usb_speed_t speed,
                            async_dispatcher_t* dispatcher, fbl::RefPtr<UsbDevice>* out_device);

  // Device protocol implementation.
  zx_status_t DdkGetProtocol(uint32_t proto_id, void* out);
  void DdkRelease();

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

  // FIDL messages (fuchsia.hardware.usb.device.Device).
  void GetDeviceSpeed(GetDeviceSpeedCompleter::Sync& completer) override;
  void GetDeviceDescriptor(GetDeviceDescriptorCompleter::Sync& completer) override;
  void GetConfigurationDescriptorSize(
      GetConfigurationDescriptorSizeRequestView request,
      GetConfigurationDescriptorSizeCompleter::Sync& completer) override;
  void GetConfigurationDescriptor(GetConfigurationDescriptorRequestView request,
                                  GetConfigurationDescriptorCompleter::Sync& completer) override;
  void GetStringDescriptor(GetStringDescriptorRequestView request,
                           GetStringDescriptorCompleter::Sync& completer) override;
  void SetInterface(SetInterfaceRequestView request,
                    SetInterfaceCompleter::Sync& completer) override;
  void GetDeviceId(GetDeviceIdCompleter::Sync& completer) override;
  void GetHubDeviceId(GetHubDeviceIdCompleter::Sync& completer) override;
  void GetConfiguration(GetConfigurationCompleter::Sync& completer) override;
  void SetConfiguration(SetConfigurationRequestView request,
                        SetConfigurationCompleter::Sync& completer) override;

  // Hub support.
  void SetHubInterface(const usb_hub_interface_protocol_t* hub_intf);
  zx_status_t HubResetPort(uint32_t port);

  zx_status_t GetDescriptor(uint16_t type, uint8_t index, uint16_t language, void* data,
                            size_t length, size_t* out_actual);
  zx_status_t GetDescriptor(fuchsia_hardware_usb_descriptor::DescriptorType desc_type,
                            uint8_t index, uint16_t language, void* data, size_t length,
                            size_t* out_actual) {
    return GetDescriptor(static_cast<uint16_t>(fidl::ToUnderlying(desc_type)), index, language,
                         data, length, out_actual);
  }
  zx_status_t ClearFeature(uint8_t request_type, uint16_t feature, uint16_t index,
                           zx_duration_t timeout) {
    return UsbControlOut(
        request_type,
        fidl::ToUnderlying(fuchsia_hardware_usb_descriptor::StandardRequest::kClearFeature),
        feature, index, timeout, nullptr, 0);
  }
  zx_status_t ClearFeature(uint8_t request_type,
                           fuchsia_hardware_usb_descriptor::FeatureSelector feature, uint16_t index,
                           zx_duration_t timeout) {
    return ClearFeature(request_type, static_cast<uint16_t>(fidl::ToUnderlying(feature)), index,
                        timeout);
  }
  zx_status_t SetFeature(uint8_t request_type, uint16_t feature, uint16_t index,
                         zx_duration_t timeout) {
    return UsbControlOut(
        request_type,
        fidl::ToUnderlying(fuchsia_hardware_usb_descriptor::StandardRequest::kSetFeature), feature,
        index, timeout, nullptr, 0);
  }
  zx_status_t SetFeature(uint8_t request_type,
                         fuchsia_hardware_usb_descriptor::FeatureSelector feature, uint16_t index,
                         zx_duration_t timeout) {
    return SetFeature(request_type, static_cast<uint16_t>(fidl::ToUnderlying(feature)), index,
                      timeout);
  }
  zx_status_t Reinitialize();

  inline uint32_t GetHubId() const { return hub_id_; }
  inline usb_speed_t GetSpeed() const { return speed_; }
  zx_status_t Init(async_dispatcher_t* dispatcher);

  uint32_t device_id() const { return device_id_; }

  void BindUsbServer(async_dispatcher_t* dispatcher,
                     fidl::ServerEnd<fuchsia_hardware_usb::Usb> server_end) {
    usb_server_.Bind(dispatcher, std::move(server_end));
  }

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(UsbDevice);

  struct RequestData {
    // True if the request is ready to be processed by the client during the next callback.
    bool ready_for_client;
    bool require_callback;
    size_t silent_completions_count;
  };

  using Request = usb::Request<void>;
  using UnownedRequest = usb::BorrowedRequest<RequestData>;
  using UnownedRequestList = usb::BorrowedRequestList<RequestData>;
  using UnownedRequestQueue = usb::BorrowedRequestQueue<RequestData>;

  struct Endpoint {
    // Requests that have not yet had an associated callback to the client.
    UnownedRequestList pending_reqs __TA_GUARDED(lock);
    fbl::Mutex lock;
  };

  int CallbackThread();
  void StartCallbackThread();
  void StopCallbackThread();

  Endpoint* GetEndpoint(uint8_t ep_address);
  // Updates the endpoint state with the completed request.
  // As erroneous requests may complete out of order, the request queued prior to it will also
  // get a callback. If that prior request has also already completed,
  // |out_additional_callback| will be populated.
  // Returns true if a callback is required.
  bool UpdateEndpoint(Endpoint* ep, usb_request_t* completed_req);

  void RequestComplete(usb_request_t* req);
  void QueueCallback(usb_request_t* req);
  static void ControlComplete(void* ctx, usb_request_t* req);
  zx_status_t Control(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                      zx_duration_t timeout, const void* write_buffer, size_t write_size,
                      void* out_read_buffer, size_t read_size, size_t* out_read_actual);
  const usb_configuration_descriptor_t* GetConfigDesc(uint8_t config);

  // ID assigned by host controller driver.
  const uint32_t device_id_;
  // device_id of the hub we are attached to (or zero for root hub).
  const uint32_t hub_id_;
  const usb_speed_t speed_;

  // Parent's HCI protocol.
  ddk::UsbHciProtocolClient hci_;
  fidl::WireSyncClient<fuchsia_hardware_usb_hci::UsbHci> hci_new_;

  // Protocol of parent (USB BUS).
  ddk::UsbBusProtocolClient bus_;

  // Hub interface, for devices that are hubs.
  ddk::UsbHubInterfaceProtocolClient hub_intf_;

  usb_device_descriptor_t device_desc_;

  // list of all configuration descriptors
  fbl::Array<fbl::Array<uint8_t>> config_descs_;
  uint8_t current_config_index_ __TA_GUARDED(state_lock_);

  std::optional<usb_langid_desc_t> lang_ids_ __TA_GUARDED(state_lock_);

  bool resetting_ __TA_GUARDED(state_lock_) = false;
  // mutex that protects the resetting state member
  fbl::Mutex state_lock_;

  Endpoint eps_[USB_MAX_EPS];

  // thread for calling client's usb request complete callback
  thrd_t callback_thread_ = 0;
  bool callback_thread_stop_ __TA_GUARDED(callback_lock_) = false;
  // completion used for signalling callback_thread
  sync_completion_t callback_thread_completion_;
  // list of requests that need to have client's completion callback called
  UnownedRequestQueue completed_reqs_ __TA_GUARDED(callback_lock_);
  // mutex that protects the callback_* members above
  fbl::Mutex callback_lock_;

  // Pool of requests USB control requests with zero data.
  usb::RequestPool<void> free_reqs_;

  size_t parent_req_size_;

  fbl::RefPtr<UsbWaiterInterface> waiter_;

  UsbProtocolServer usb_server_;
  component::OutgoingDirectory outgoing_;
};

}  // namespace usb_bus

#endif  // SRC_DEVICES_USB_DRIVERS_USB_BUS_USB_DEVICE_H_
