// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_DWC2_DWC2_H_
#define SRC_DEVICES_USB_DRIVERS_DWC2_DWC2_H_

#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.dci/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.phy/cpp/driver/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/mmio/mmio.h>
#include <lib/zx/interrupt.h>
#include <threads.h>

#include <atomic>
#include <mutex>
#include <queue>

#include <ddktl/device.h>
#include <ddktl/metadata_server.h>
#include <usb/dwc2/metadata.h>

#include "src/devices/usb/drivers/dwc2/dwc2_config.h"
#include "src/devices/usb/drivers/dwc2/usb_dwc_regs.h"
#include "src/devices/usb/lib/usb-endpoint/include/usb-endpoint/usb-endpoint-server.h"

namespace dwc2 {

class Dwc2;
using Dwc2Type = ddk::Device<Dwc2, ddk::Initializable, ddk::Unbindable, ddk::Suspendable>;

class Dwc2 : public Dwc2Type, public fidl::Server<fuchsia_hardware_usb_dci::UsbDci> {
 public:
  explicit Dwc2(zx_device_t* parent, async_dispatcher_t* dispatcher)
      : Dwc2Type(parent), dispatcher_(dispatcher), outgoing_(dispatcher) {}

  // Neither copyable nor movable.
  Dwc2(Dwc2&&) = delete;
  Dwc2(const Dwc2&) = delete;
  Dwc2& operator=(Dwc2&&) = delete;
  Dwc2& operator=(const Dwc2&) = delete;

  static zx_status_t Create(void* ctx, zx_device_t* parent);
  zx_status_t Init(const dwc2_config::Config& config);
  int IrqThread();

  // Device protocol implementation.
  void DdkInit(ddk::InitTxn txn);
  void DdkUnbind(ddk::UnbindTxn txn);
  void DdkRelease();
  void DdkSuspend(ddk::SuspendTxn txn);

  // fuchsia_hardware_usb_dci::UsbDci protocol implementation.
  void ConnectToEndpoint(ConnectToEndpointRequest& request,
                         ConnectToEndpointCompleter::Sync& completer) override;

  void SetInterface(SetInterfaceRequest& request, SetInterfaceCompleter::Sync& completer) override;

  void StartController(StartControllerCompleter::Sync& completer) override;

  void StopController(StopControllerCompleter::Sync& completer) override;

  void ConfigureEndpoint(ConfigureEndpointRequest& request,
                         ConfigureEndpointCompleter::Sync& completer) override;

  void DisableEndpoint(DisableEndpointRequest& request,
                       DisableEndpointCompleter::Sync& completer) override;

  void EndpointSetStall(EndpointSetStallRequest& request,
                        EndpointSetStallCompleter::Sync& completer) override;

  void EndpointClearStall(EndpointClearStallRequest& request,
                          EndpointClearStallCompleter::Sync& completer) override;

  void CancelAll(CancelAllRequest& request, CancelAllCompleter::Sync& completer) override;

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_usb_dci::UsbDci> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  // Allows tests to configure a fake interrupt.
  void SetInterrupt(zx::interrupt irq) { irq_ = std::move(irq); }

  const zx::bti& bti() const { return bti_; }

 private:
  zx_status_t DoControl(const fuchsia_hardware_usb_descriptor::wire::UsbSetup& setup,
                        const uint8_t* write_buffer, size_t write_size, uint8_t* out_read_buffer,
                        size_t read_size, size_t* out_read_actual);

  enum class Ep0State {
    DISCONNECTED,
    IDLE,
    DATA,
    STATUS,
    STALL,
    TIMEOUT_RECOVERY,
  };

  // clang-format off
  const char* Ep0StateToStr(Ep0State s) {
    switch (s) {
      case Ep0State::DISCONNECTED:     return "DISCONNECTED";
      case Ep0State::IDLE:             return "IDLE";
      case Ep0State::DATA:             return "DATA";
      case Ep0State::STATUS:           return "STATUS";
      case Ep0State::STALL:            return "STALL";
      case Ep0State::TIMEOUT_RECOVERY: return "TIMEOUT_RECOVERY";
    }
  }
  // clang-format on

  class Endpoint : public usb::EndpointServer {
   public:
    Endpoint(uint8_t ep_num, Dwc2* dwc2) : usb::EndpointServer(dwc2->bti_, ep_num), dwc2_(dwc2) {
      loop_.StartThread("dwc2-enpdoint-loop");
    }

    // fuchsia_hardware_usb_endpoint::Endpoint protocol implementation.
    void GetInfo(GetInfoCompleter::Sync& completer) override {
      completer.Reply(fit::as_error(ZX_ERR_NOT_SUPPORTED));
    }
    void QueueRequests(QueueRequestsRequest& request,
                       QueueRequestsCompleter::Sync& completer) override;
    void CancelAll(CancelAllCompleter::Sync& completer) override {
      CancelAll();
      completer.Reply(fit::ok());
    }

    void QueueRequest(usb::FidlRequest request);
    void CancelAll();

    async_dispatcher_t* dispatcher() { return loop_.dispatcher(); }

    // Requests waiting to be processed.
    std::queue<usb::RequestVariant> queued_reqs __TA_GUARDED(lock);
    // Request currently being processed.
    std::optional<usb::RequestVariant> current_req __TA_GUARDED(lock);

    // Values for current USB request
    uint32_t req_offset = 0;
    uint32_t req_xfersize = 0;
    uint32_t req_length = 0;
    uint32_t phys = 0;

    // Used for synchronizing endpoint state and ep specific hardware registers.
    // This should be acquired before Dwc2.lock_ if acquiring both locks.
    std::mutex lock;

    uint16_t max_packet_size = 0;
    bool enabled = false;

   private:
    async::Loop loop_{&kAsyncLoopConfigNeverAttachToThread};

    Dwc2* dwc2_;
  };

  void FlushTxFifo(uint32_t fifo_num);
  void FlushRxFifo();
  void FlushTxFifoRetryIndefinite(uint32_t fifo_num);
  void FlushRxFifoRetryIndefinite();
  zx_status_t InitController();
  void SetConnected(bool connected);
  void StartEp0();
  void StartEndpoints();
  void HandleEp0Setup();
  void HandleEp0Status(bool is_in);
  void HandleEp0TimeoutRecovery();
  void HandleEp0TransferComplete(bool is_in);
  void HandleTransferComplete(uint8_t ep_num);
  void EnableEp(uint8_t ep_num, bool enable);
  void QueueNextRequest(Endpoint* ep) __TA_REQUIRES(ep->lock);
  void StartTransfer(Endpoint* ep, uint32_t length) __TA_REQUIRES(ep->lock);
  void SoftDisconnect() __TA_REQUIRES(lock_);
  uint32_t ReadTransfered(Endpoint* ep);

  // Interrupt handlers
  void HandleReset();
  void HandleSuspend();
  void HandleEnumDone();
  void HandleInEpInterrupt();
  void HandleOutEpInterrupt();

  zx_status_t HandleSetupRequest(size_t* out_actual);
  void SetAddress(uint8_t address);

  inline fdf::MmioBuffer* get_mmio() { return &*mmio_; }

  // Used for debugging.
  void dump_regs();

  std::optional<Endpoint> endpoints_[DWC_MAX_EPS];

  // Used for synchronizing global state
  // and non ep specific hardware registers.
  // Endpoint.lock should be acquired first
  // when acquiring both locks.
  std::mutex lock_;

  zx::bti bti_;
  // DMA buffer for endpoint zero requests
  ddk::IoBuffer ep0_buffer_;
  // Current endpoint zero request
  fuchsia_hardware_usb_descriptor::wire::UsbSetup cur_setup_ = {};
  Ep0State ep0_state_ = Ep0State::DISCONNECTED;

  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDciInterface> dci_intf_;
  fdf::WireSyncClient<fuchsia_hardware_usb_phy::UsbPhy> phy_;

  std::optional<fdf::MmioBuffer> mmio_;

  zx::interrupt irq_;
  thrd_t irq_thread_;
  // True if |irq_thread_| can be joined.
  std::atomic_bool irq_thread_started_ = false;

  dwc2_metadata_t metadata_;
  bool connected_ = false;
  bool configured_ = false;
  // The length of the last IN-data sent to the host.
  uint32_t last_transmission_len_;
  // Raw IRQ timestamp from kernel
  zx::time_boot irq_timestamp_;
  // Timestamp we were dispatched at
  zx::time_boot irq_dispatch_timestamp_;
  // Timestamp when we started waiting for the interrupt
  zx::time_boot wait_start_time_;
  bool shutting_down_ __TA_GUARDED(lock_) = false;

  async_dispatcher_t* dispatcher_;
  component::OutgoingDirectory outgoing_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb_dci::UsbDci> bindings_;

  ddk::MetadataServer<fuchsia_boot_metadata::MacAddressMetadata> mac_address_metadata_server_;
  ddk::MetadataServer<fuchsia_boot_metadata::SerialNumberMetadata> serial_number_metadata_server_;
};

}  // namespace dwc2

#endif  // SRC_DEVICES_USB_DRIVERS_DWC2_DWC2_H_
