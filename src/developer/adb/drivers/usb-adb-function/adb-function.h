// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_ADB_DRIVERS_USB_ADB_FUNCTION_ADB_FUNCTION_H_
#define SRC_DEVELOPER_ADB_DRIVERS_USB_ADB_FUNCTION_ADB_FUNCTION_H_

#include <endian.h>
#include <fidl/fuchsia.hardware.adb/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.function/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async/cpp/task.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/devfs/cpp/connector.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/sync/cpp/completion.h>
#include <zircon/compiler.h>

#include <queue>

#include <usb-endpoint/usb-endpoint-client.h>
#include <usb-inspect/usb-inspect.h>
#include <usb/descriptors.h>
#include <usb/usb.h>

namespace usb_adb_function {

class UsbAdbTestHelper;
namespace fdescriptor = fuchsia_hardware_usb_descriptor;

constexpr uint32_t kBulkTxCount = 16;
constexpr uint32_t kBulkRxCount = 16;
constexpr size_t kVmoDataSize = 4096;

constexpr uint16_t kBulkMaxPacket = 512;

constexpr char kDeviceName[] = "usb-adb-function";

namespace fadb = fuchsia_hardware_adb;
namespace fendpoint = fuchsia_hardware_usb_endpoint;

// The driver's internal state machine. Begins in kAwaitingUsbConnection.
enum class State : uint8_t {
  // In kAwaitingUsbConnection, we have called function_.Configure(), and
  // are waiting for usb-peripheral to call SetConfigured(true). Calls to
  // SetConfigured(false) complete immediately with ZX_OK.
  //
  // Once SetConfigured(true) has been called, we:
  // - Configure and enable the bulk endpoints,
  // - Queue USB receive requests to the bulk OUT endpoint,
  // - Tell any connected UsbAdbImpl clients that the device is online, and
  // - Move to kOnline.
  //
  // If Stop is called, we move to kStoppingForUnbind.
  kAwaitingUsbConnection,

  // In kOnline, the USB connection is live, and we respond to any
  // QueueTx/Receive requests from UsbAdbImpl clients.
  //
  // If any of the following happen:
  // - A fadb::Device client calls StopAdb().
  // - A UsbAdbImpl client closes their channel.
  // - usb-peripheral calls SetConfigured(false).
  // - Stop gets called.
  //
  // ... we disable endpoints, cancel in-flight requests, and move to
  // kStoppingForReconnect or kStoppingForUnbind.
  kOnline,

  // In kStoppingForUnbind, we wait for all outstanding USB requests to be completed.
  // Once they have been, we close any UsbAdbImpl connection, reply to any pending
  // StopAdb()/SetConfigured(false) completers, deconfigure the USB function, and
  // complete the driver Stop callback.
  kStoppingForUnbind,

  // In kStoppingForReconnect, we wait for all outstanding USB requests to be completed.
  // Once they have been:
  // - If the stop was initiated by ADB while online (reset_usb_on_stop_ is true),
  //   we call function_.Deconfigure() and StartUsb() (function_.Configure()) to
  //   bounce the USB connection for adbd re-enumeration, then move to
  //   kAwaitingUsbConnection.
  // - If the stop was initiated by SetConfigured(false) (host reset or physical unplug),
  //   we keep usb_function_binding_ and adb_binding_ bound, notify adb_binding_
  //   that the device is offline, reply to set_configured_completers_, and move
  //   back to kAwaitingUsbConnection.
  kStoppingForReconnect,
};

// Implements the USB ADB function driver.
class UsbAdbDevice : public fdf::DriverBase2,
                     public fidl::Server<fuchsia_hardware_usb_function::UsbFunctionInterface>,
                     public fidl::WireServer<fadb::Device>,
                     public fidl::Server<fadb::UsbAdbImpl> {
 public:
  friend class UsbAdbTestHelper;
  UsbAdbDevice() : fdf::DriverBase2("usb_adb") {}

  // Driver lifecycle methods.
  zx::result<> Start(fdf::DriverContext context) override;
  void Stop(fdf::StopCompleter completer) override;

  // UsbFunctionInterface methods.
  void Control(ControlRequest& request, ControlCompleter::Sync& completer) override;
  void SetConfigured(SetConfiguredRequest& request,
                     SetConfiguredCompleter::Sync& completer) override;
  void SetInterface(SetInterfaceRequest& request, SetInterfaceCompleter::Sync& completer) override;
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_usb_function::UsbFunctionInterface> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  // fadb::Device methods.
  void StartAdb(StartAdbRequestView request, StartAdbCompleter::Sync& completer) override;
  void StopAdb(StopAdbCompleter::Sync& completer) override;

  // fadb::UsbAdbImpl methods.
  void QueueTx(QueueTxRequest& request, QueueTxCompleter::Sync& completer) override;
  void Receive(ReceiveCompleter::Sync& completer) override;

  uint8_t bulk_out_addr() const { return descriptors_.bulk_out_ep.b_endpoint_address; }
  uint8_t bulk_in_addr() const { return descriptors_.bulk_in_ep.b_endpoint_address; }
  inspect::Inspector GetInspectorForTesting() {
    if (component_inspector_.has_value()) {
      return component_inspector_->inspector();
    }
    return {};
  }
  usb_inspect::ThroughputTracker& GetThroughputTrackerForTesting() { return *throughput_tracker_; }

 private:
  State state_ = State::kAwaitingUsbConnection;

  // State transition helpers.
  void SetState(State new_state);
  zx::result<> StartUsb();
  zx::result<> EnableEndpoints();
  void PurgeOfflineQueues();
  void ResetOrStopUsb(State stop_state);
  void CheckUsbStopComplete();

  fidl::ServerBindingGroup<fadb::Device> device_bindings_;

  // Structure to store pending transfer requests when there are not enough USB request buffers.
  struct txn_req_t {
    QueueTxRequest request;
    size_t start = 0;
    QueueTxCompleter::Async completer;
  };

  // Helper methods to get free request buffer and queue the request for transmitting.
  void SendQueued();

  // Helper methods to get free request buffer and queue the request for receiving.
  void ReceiveQueued();

  // USB request completion callback methods.
  void TxComplete(std::vector<fendpoint::Completion> completion);
  void RxComplete(std::vector<fendpoint::Completion> completion);

  fidl::SyncClient<fuchsia_hardware_usb_function::UsbFunction> function_;
  // Owning ServerBinding for USB function interface.
  std::optional<fidl::ServerBinding<fuchsia_hardware_usb_function::UsbFunctionInterface>>
      usb_function_binding_;

  // UsbAdbImpl service binding. ServerEnds passed into StartAdb() end up here.
  std::optional<fidl::ServerBinding<fadb::UsbAdbImpl>> adb_binding_;

  // Callbacks to call when the USB stack has been brought down and it's safe to
  // call AdbStart().
  std::vector<StopAdbCompleter::Async> stop_completers_;
  // Completers for SetConfigured(false) held until CancelAll and in-flight requests finish.
  std::vector<SetConfiguredCompleter::Async> set_configured_completers_;
  // Holds Stop callback to be invoked once shutdown is complete.
  std::optional<fdf::StopCompleter> shutdown_callback_;

  // USB ADB interface descriptor.
  struct {
    usb_interface_descriptor_t adb_intf;
    usb_endpoint_descriptor_t bulk_out_ep;
    usb_endpoint_descriptor_t bulk_in_ep;
  } descriptors_ [[maybe_unused]] = {
      .adb_intf =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
              .b_interface_number = 0,  // set later during AllocInterface
              .b_alternate_setting = 0,
              .b_num_endpoints = 2,
              .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kVendor),
              .b_interface_sub_class =
                  fidl::ToUnderlying(fdescriptor::VendorSubclass::kAdbFastboot),
              .b_interface_protocol = fidl::ToUnderlying(fdescriptor::AdbFastbootProtocol::kAdb),
              .i_interface = 0,  // This is set in adb
          },
      .bulk_out_ep =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
              .b_endpoint_address = 0,  // set later during AllocEp
              .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
              .w_max_packet_size = htole16(kBulkMaxPacket),
              .b_interval = 0,
          },
      .bulk_in_ep =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
              .b_endpoint_address = 0,  // set later during AllocEp
              .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
              .w_max_packet_size = htole16(kBulkMaxPacket),
              .b_interval = 0,
          },
  };

  zx_status_t InitEndpoint(fidl::ClientEnd<fuchsia_hardware_usb_endpoint::Endpoint> endpoint_client,
                           usb::EndpointClient<UsbAdbDevice>& ep, uint32_t req_count);

  // Bulk OUT/RX endpoint
  usb::EndpointClient<UsbAdbDevice> bulk_out_ep_{usb::EndpointType::BULK, this,
                                                 std::mem_fn(&UsbAdbDevice::RxComplete)};
  // Queue of pending Receive requests from client.
  std::queue<ReceiveCompleter::Async> rx_requests_;
  // pending_replies_ only used for bulk_out_ep_
  std::queue<fendpoint::Completion> pending_replies_;

  // Bulk IN/TX endpoint
  usb::EndpointClient<UsbAdbDevice> bulk_in_ep_{usb::EndpointType::BULK, this,
                                                std::mem_fn(&UsbAdbDevice::TxComplete)};
  // Queue of pending transfer requests that need to be transmitted once the BULK IN request buffers
  // become available.
  std::queue<txn_req_t> tx_pending_reqs_;

  bool bulk_in_cancelled_ = false;
  bool bulk_out_cancelled_ = false;
  bool pending_reconfigure_ = false;
  bool reset_usb_on_stop_ = false;
  bool usb_disconnect_observed_ = false;
  bool adb_connected_during_stop_ = false;

  bool CancelAllCompleted() const { return bulk_in_cancelled_ && bulk_out_cancelled_; }
  bool AllRequestsReturned() {
    bool bulk_in_ready = bulk_in_ep_.RequestsFull() || !bulk_in_ep_.client().is_valid();
    bool bulk_out_ready = bulk_out_ep_.RequestsFull() || !bulk_out_ep_.client().is_valid();
    return bulk_in_ready && bulk_out_ready;
  }

  // Inspect diagnostics
  std::optional<inspect::ComponentInspector> component_inspector_;
  inspect::Node inspect_node_;
  usb_inspect::FunctionDriverInspect function_inspect_;
  usb_inspect::EndpointInspect bulk_in_inspect_;
  usb_inspect::EndpointInspect bulk_out_inspect_;

  void RecordEvent(std::string_view event_name) { function_inspect_.RecordEvent(event_name); }
  void UpdateQueueStats() {
    bulk_in_inspect_.UpdateTxQueue(tx_pending_reqs_.size());
    bulk_out_inspect_.UpdateRxQueue(rx_requests_.size());
    bulk_out_inspect_.UpdateRxPendingProcessing(pending_replies_.size());
  }

  // Traffic and throughput calculation
  std::optional<usb_inspect::ThroughputTracker> throughput_tracker_;

  static std::string StateToString(State state) {
    switch (state) {
      case State::kAwaitingUsbConnection:
        return "kAwaitingUsbConnection";
      case State::kOnline:
        return "kOnline";
      case State::kStoppingForUnbind:
        return "kStoppingForUnbind";
      case State::kStoppingForReconnect:
        return "kStoppingForReconnect";
    }
    return "unknown";
  }
};

}  // namespace usb_adb_function

#endif  // SRC_DEVELOPER_ADB_DRIVERS_USB_ADB_FUNCTION_ADB_FUNCTION_H_
