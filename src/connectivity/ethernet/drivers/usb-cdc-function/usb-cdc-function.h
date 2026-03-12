// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_ETHERNET_DRIVERS_USB_CDC_FUNCTION_USB_CDC_FUNCTION_H_
#define SRC_CONNECTIVITY_ETHERNET_DRIVERS_USB_CDC_FUNCTION_USB_CDC_FUNCTION_H_

#include <endian.h>
#include <fidl/fuchsia.hardware.network.driver/cpp/driver/wire.h>
#include <fidl/fuchsia.hardware.network/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.endpoint/cpp/fidl.h>
#include <fuchsia/hardware/usb/function/cpp/banjo.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/sync/cpp/completion.h>
#include <zircon/listnode.h>

#include <array>
#include <mutex>

#include <usb-endpoint/usb-endpoint-client.h>
#include <usb/cdc.h>
#include <usb/request-fidl.h>
#include <usb/usb.h>

#include "src/lib/vmo_store/vmo_store.h"

namespace usb_cdc_function {

namespace fnetdev = fuchsia_hardware_network_driver;

#define BULK_REQ_SIZE 2048
#define INTR_COUNT 8

#define BULK_MAX_PACKET 512  // FIXME(voydanoff) USB 3.0 support
#define INTR_MAX_PACKET sizeof(usb_cdc_speed_change_notification_t)

// Total Ethernet MTU is 1514 bytes. 1500 bytes for IP and 14 bytes for Ethernet
// header.
#define ETH_MTU 1514
#define ETH_MAC_SIZE 6

class UsbCdcFunction : public fdf::DriverBase,
                       public fdf::WireServer<fnetdev::NetworkDeviceImpl>,
                       public fdf::WireServer<fnetdev::NetworkPort>,
                       public fdf::WireServer<fnetdev::MacAddr>,
                       public ddk::UsbFunctionInterfaceProtocol<UsbCdcFunction> {
 public:
  static constexpr std::string_view kDriverName = "usb_cdc_function";
  static constexpr uint8_t kPortId = 1;
  static constexpr size_t kTxDepth = 16;
  static constexpr size_t kRxDepth = 16;
  static constexpr fdf_arena_tag_t kArenaTag = 'CDCE';

  UsbCdcFunction(fdf::DriverStartArgs start_args,
                 fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DriverBase(kDriverName, std::move(start_args), std::move(driver_dispatcher)),
        vmo_store_({
            .map =
                vmo_store::MapOptions{
                    .vm_option = ZX_VM_PERM_READ | ZX_VM_PERM_WRITE | ZX_VM_REQUIRE_NON_RESIZABLE,
                    .vmar = nullptr,
                },
        }) {}

  // Unhide the inherited Start and Stop methods to avoid ambiguity with the
  // fnetdev::NetworkDeviceImpl::Start and Stop methods.
  using fdf::DriverBase::Start;
  using fdf::DriverBase::Stop;

  // fdf::DriverBase implementation.
  zx::result<> Start() override;
  void PrepareStop(fdf::PrepareStopCompleter completer) override;

  // NetworkDeviceImpl protocol:
  void Init(fnetdev::wire::NetworkDeviceImplInitRequest *request, fdf::Arena &arena,
            InitCompleter::Sync &completer) override;
  void Start(fdf::Arena &arena, StartCompleter::Sync &completer) override;
  void Stop(fdf::Arena &arena, StopCompleter::Sync &completer) override;
  void GetInfo(
      fdf::Arena &arena,
      fdf::WireServer<fnetdev::NetworkDeviceImpl>::GetInfoCompleter::Sync &completer) override;
  void QueueTx(fnetdev::wire::NetworkDeviceImplQueueTxRequest *request, fdf::Arena &arena,
               QueueTxCompleter::Sync &completer) override;
  void QueueRxSpace(fnetdev::wire::NetworkDeviceImplQueueRxSpaceRequest *request, fdf::Arena &arena,
                    QueueRxSpaceCompleter::Sync &completer) override;
  void PrepareVmo(fnetdev::wire::NetworkDeviceImplPrepareVmoRequest *request, fdf::Arena &arena,
                  PrepareVmoCompleter::Sync &completer) override;
  void ReleaseVmo(fnetdev::wire::NetworkDeviceImplReleaseVmoRequest *request, fdf::Arena &arena,
                  ReleaseVmoCompleter::Sync &completer) override;

  // NetworkPort protocol:
  void GetInfo(fdf::Arena &arena,
               fdf::WireServer<fnetdev::NetworkPort>::GetInfoCompleter::Sync &completer) override;
  void GetStatus(fdf::Arena &arena, GetStatusCompleter::Sync &completer) override;
  void SetActive(fnetdev::wire::NetworkPortSetActiveRequest *request, fdf::Arena &arena,
                 SetActiveCompleter::Sync &completer) override;
  void GetMac(fdf::Arena &arena, GetMacCompleter::Sync &completer) override;
  void Removed(fdf::Arena &arena, RemovedCompleter::Sync &completer) override;

  // MacAddr protocol:
  void GetAddress(fdf::Arena &arena, GetAddressCompleter::Sync &completer) override;
  void GetFeatures(fdf::Arena &arena, GetFeaturesCompleter::Sync &completer) override;
  void SetMode(fnetdev::wire::MacAddrSetModeRequest *request, fdf::Arena &arena,
               SetModeCompleter::Sync &completer) override;

  // UsbFunctionInterface methods.
  size_t UsbFunctionInterfaceGetDescriptorsSize();
  void UsbFunctionInterfaceGetDescriptors(uint8_t *out_descriptors_buffer, size_t descriptors_size,
                                          size_t *out_descriptors_actual);
  zx_status_t UsbFunctionInterfaceControl(const usb_setup_t *setup, const uint8_t *write_buffer,
                                          size_t write_size, uint8_t *out_read_buffer,
                                          size_t read_size, size_t *out_read_actual);
  zx_status_t UsbFunctionInterfaceSetConfigured(bool configured, usb_speed_t speed);
  zx_status_t UsbFunctionInterfaceSetInterface(uint8_t interface, uint8_t alt_setting);

  zx_status_t cdc_generate_mac_address();
  void CdcIntrComplete(std::vector<fuchsia_hardware_usb_endpoint::Completion> completion);
  void cdc_send_notifications();
  void CdcRxComplete(std::vector<fuchsia_hardware_usb_endpoint::Completion> completions);
  void CdcTxComplete(std::vector<fuchsia_hardware_usb_endpoint::Completion> completions);
  void ProcessRxCompletions(std::vector<fuchsia_hardware_usb_endpoint::Completion> completions)
      __TA_REQUIRES(rx_mutex_);

  // test helpers.
  bool HasPendingRxCompletions();

 private:
  zx_status_t AddNetworkDevice();
  fuchsia_hardware_network::PortStatus ReadStatus() const __TA_REQUIRES(state_mutex_);
  void UpdatePortStatus() __TA_REQUIRES(state_mutex_);

  ddk::UsbFunctionProtocolClient function_;

  compat::SyncInitializedDeviceServer child_;

  fidl::ClientEnd<fuchsia_driver_framework::NodeController> child_controller_;

  fdf::SynchronizedDispatcher dispatcher_;

  // In-direction (TX to host).
  usb::EndpointClient<UsbCdcFunction> intr_ep_{usb::EndpointType::INTERRUPT, this,
                                               std::mem_fn(&UsbCdcFunction::CdcIntrComplete)};

  // Out-direction (RX from host).
  usb::EndpointClient<UsbCdcFunction> bulk_out_ep_{usb::EndpointType::BULK, this,
                                                   std::mem_fn(&UsbCdcFunction::CdcRxComplete)};

  // In-direction (TX to host).
  usb::EndpointClient<UsbCdcFunction> bulk_in_ep_{usb::EndpointType::BULK, this,
                                                  std::mem_fn(&UsbCdcFunction::CdcTxComplete)};

  // Queue of buffer IDs that were sent to USB hardware and are awaiting
  // completion. This mirrors the order of requests submitted to bulk_in_ep_.
  std::queue<uint32_t> tx_completion_queue_ __TA_GUARDED(tx_mutex_);
  // Queue of requests that were not completed because there was no space buffer
  // available.
  std::vector<fuchsia_hardware_usb_endpoint::Completion> rx_completion_queue_
      __TA_GUARDED(rx_mutex_);
  void DiscardPendingTxBuffers(zx_status_t status) __TA_EXCLUDES(tx_mutex_);
  void ReturnPendingRxSpace() __TA_EXCLUDES(rx_mutex_);
  void ContinueStop();

  std::atomic_bool unbound_ = false;  // set to true when device is going away.
  bool dispatcher_shutdown_ = false;

  // Device attributes
  std::array<uint8_t, ETH_MAC_SIZE> mac_addr_;
  // Lock for network device state and ifc_
  mutable std::mutex state_mutex_ __TA_ACQUIRED_AFTER(tx_mutex_);
  fdf::WireSharedClient<fnetdev::NetworkDeviceIfc> netdevice_ifc_;
  bool online_ __TA_GUARDED(state_mutex_) = false;
  bool configured_ = false;
  usb_speed_t speed_ = 0;
  // TX lock -- Must be acquired before state_mutex
  // when both locks are held.
  std::mutex &tx_mutex_ = bulk_in_ep_.mutex();
  std::mutex &rx_mutex_ = bulk_out_ep_.mutex();
  std::mutex &intr_mutex_ = intr_ep_.mutex();

  uint8_t bulk_out_addr_ = 0;
  uint8_t bulk_in_addr_ = 0;
  uint8_t intr_addr_ = 0;

  using VmoStore = vmo_store::VmoStore<vmo_store::SlabStorage<uint8_t>>;
  VmoStore vmo_store_ __TA_GUARDED(state_mutex_);

  std::queue<fnetdev::wire::RxSpaceBuffer> rx_space_buffers_ __TA_GUARDED(rx_mutex_);
  std::optional<fdf::PrepareStopCompleter> stop_completer_;

  struct {
    usb_interface_descriptor_t comm_intf;
    usb_cs_header_interface_descriptor_t cdc_header;
    usb_cs_union_interface_descriptor_1_t cdc_union;
    usb_cs_ethernet_interface_descriptor_t cdc_eth;
    usb_endpoint_descriptor_t intr_ep;
    usb_interface_descriptor_t cdc_intf_0;
    usb_interface_descriptor_t cdc_intf_1;
    usb_endpoint_descriptor_t bulk_out_ep;
    usb_endpoint_descriptor_t bulk_in_ep;
  } descriptors_ = {
      .comm_intf =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = USB_DT_INTERFACE,
              .b_interface_number = 0,  // set later
              .b_alternate_setting = 0,
              .b_num_endpoints = 1,
              .b_interface_class = USB_CLASS_COMM,
              .b_interface_sub_class = USB_CDC_SUBCLASS_ETHERNET,
              .b_interface_protocol = 0,
              .i_interface = 0,
          },
      .cdc_header =
          {
              .bLength = sizeof(usb_cs_header_interface_descriptor_t),
              .bDescriptorType = USB_DT_CS_INTERFACE,
              .bDescriptorSubType = USB_CDC_DST_HEADER,
              .bcdCDC = 0x120,
          },
      .cdc_union =
          {
              .bLength = sizeof(usb_cs_union_interface_descriptor_1_t),
              .bDescriptorType = USB_DT_CS_INTERFACE,
              .bDescriptorSubType = USB_CDC_DST_UNION,
              .bControlInterface = 0,      // set later
              .bSubordinateInterface = 0,  // set later
          },
      .cdc_eth =
          {
              .bLength = sizeof(usb_cs_ethernet_interface_descriptor_t),
              .bDescriptorType = USB_DT_CS_INTERFACE,
              .bDescriptorSubType = USB_CDC_DST_ETHERNET,
              .iMACAddress = 0,  // set later
              .bmEthernetStatistics = 0,
              .wMaxSegmentSize = ETH_MTU,
              .wNumberMCFilters = 0,
              .bNumberPowerFilters = 0,
          },
      .intr_ep =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = USB_DT_ENDPOINT,
              .b_endpoint_address = 0,  // set later
              .bm_attributes = USB_ENDPOINT_INTERRUPT,
              .w_max_packet_size = htole16(INTR_MAX_PACKET),
              .b_interval = 8,
          },
      .cdc_intf_0 =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = USB_DT_INTERFACE,
              .b_interface_number = 0,  // set later
              .b_alternate_setting = 0,
              .b_num_endpoints = 0,
              .b_interface_class = USB_CLASS_CDC,
              .b_interface_sub_class = 0,
              .b_interface_protocol = 0,
              .i_interface = 0,
          },
      .cdc_intf_1 =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = USB_DT_INTERFACE,
              .b_interface_number = 0,  // set later
              .b_alternate_setting = 1,
              .b_num_endpoints = 2,
              .b_interface_class = USB_CLASS_CDC,
              .b_interface_sub_class = 0,
              .b_interface_protocol = 0,
              .i_interface = 0,
          },
      .bulk_out_ep =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = USB_DT_ENDPOINT,
              .b_endpoint_address = 0,  // set later
              .bm_attributes = USB_ENDPOINT_BULK,
              .w_max_packet_size = htole16(BULK_MAX_PACKET),
              .b_interval = 0,
          },
      .bulk_in_ep =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = USB_DT_ENDPOINT,
              .b_endpoint_address = 0,  // set later
              .bm_attributes = USB_ENDPOINT_BULK,
              .w_max_packet_size = htole16(BULK_MAX_PACKET),
              .b_interval = 0,
          },
  };
};

}  // namespace usb_cdc_function

#endif  // SRC_CONNECTIVITY_ETHERNET_DRIVERS_USB_CDC_FUNCTION_USB_CDC_FUNCTION_H_
