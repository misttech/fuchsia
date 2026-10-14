// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_H_
#define SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_H_

#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.driver.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.power/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.dci/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.descriptor/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.endpoint/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.phy/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.policy/cpp/common_types_format.h>
#include <fidl/fuchsia.hardware.usb.policy/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/fidl.h>
#include <lib/async/cpp/irq.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/dma-buffer/buffer.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/metadata/cpp/metadata_server.h>
#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/driver/platform-device/cpp/pdev.h>
#include <lib/fit/result.h>
#include <lib/inspect/component/cpp/component.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/result.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/threads.h>

#include <cstdint>
#include <format>
#include <memory>
#include <string_view>

#include <fbl/mutex.h>
#include <usb-endpoint/sdk/usb-endpoint-server.h>
#include <usb/descriptors.h>
#include <usb/sdk/request-fidl.h>

#include "src/devices/usb/drivers/dwc3/dwc3-event-fifo.h"
#include "src/devices/usb/drivers/dwc3/dwc3-metrics.h"
#include "src/devices/usb/drivers/dwc3/dwc3-trb-fifo.h"
#include "src/devices/usb/drivers/dwc3/dwc3_config.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace dwc3 {

// An extension class to extend the driver with SoC specific behavior for things such as power
// management.
class PlatformExtension {
 public:
  virtual ~PlatformExtension() = default;
  virtual zx::result<> Start() = 0;
  virtual void Stop() {}
  virtual zx::result<> Suspend() = 0;
  virtual zx::result<> Resume() = 0;
  virtual bool PowersDownCoreOnDisconnect() const { return true; }
  virtual zx::result<> SetConnectionSpeed(fuchsia_hardware_usb_descriptor::UsbSpeed speed) {
    return zx::ok();
  }
};

// Some platforms support fully powering down the dwc3 core. When powered down, accessing the MMIO
// will cause the system to crash or lock up. power_on_ indicates whether or not the core is powered
// down, and therefore whether or not it is safe to access the MMIO.
//
// UsbDci, Controller, or Endpoint FIDL methods may be safely called at any time regardless of
// the power state. Other methods must not be called when powered down, unless indicated by comments
// below.
class Dwc3 : public fdf::DriverBase2,
             public fidl::Server<fuchsia_hardware_usb_dci::UsbDci>,
             public fidl::Server<fuchsia_hardware_usb_policy::Controller> {
 public:
  Dwc3() : fdf::DriverBase2("dwc3") {}
  ~Dwc3() override;

  zx::result<> Start(fdf::DriverContext context) override;
  // fdf::DriverBase2 provides an asynchronous Stop method. Synchronous cleanup should be performed
  // in the destructor.
  void Stop(fdf::StopCompleter completer) override;

  const std::shared_ptr<fdf::Namespace>& incoming() const { return incoming_; }

  inspect::ComponentInspector& inspector() { return *inspector_; }

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

  void GetHardwareInfo(GetHardwareInfoCompleter::Sync& completer) override;
  void AllocEndpoint(AllocEndpointRequest& request,
                     AllocEndpointCompleter::Sync& completer) override;
  void FreeEndpoint(FreeEndpointRequest& request, FreeEndpointCompleter::Sync& completer) override;

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_usb_dci::UsbDci> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    fdf::warn("dwc3: received unknown UsbDci method: {}", metadata.method_ordinal);
  }

  // fuchsia_hardware_usb_policy::Controller protocol implementation.
  void WatchDeviceState(WatchDeviceStateCompleter::Sync& completer) override;

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_usb_policy::Controller> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    fdf::warn("dwc3: received unknown Controller method: {}", metadata.method_ordinal);
  }

  // Gating flag to enable enqueueing multiple TRBs at once for a given endpoint
  // type.
  bool AllowEnqueueManyTRBs(fdescriptor::EndpointType ep_type) const {
    return enable_enqueue_many_trbs_ && ep_type == fdescriptor::EndpointType::kBulk;
  }

  // For testing.
  bool poll_end_xfer() const { return poll_end_xfer_; }
  bool supports_gfladj() const { return supports_gfladj_; }
  std::optional<uint32_t> fladj() const { return fladj_; }
  bool fladj_refclk_lpm_sel() const { return fladj_refclk_lpm_sel_; }
  void SetEnableEnqueueManyTrbs(bool enable) { enable_enqueue_many_trbs_ = enable; }

 private:
  const std::string_view kScheduleProfileRole = "fuchsia.devices.usb.drivers.dwc3.interrupt";
  static inline const uint32_t kEp0BufferSize = UINT16_MAX + 1;
  static inline constexpr uint32_t kAlignmentLog2 = 12;

  // physical endpoint numbers.  We use 0 and 1 for EP0, and let the device-mode
  // driver use the rest.
  static inline constexpr uint8_t kEp0Out = 0;
  static inline constexpr uint8_t kEp0In = 1;
  static inline constexpr uint8_t kUserEndpointStartNum = 2;
  static inline constexpr size_t kEp0MaxPacketSize = 512;

  static inline constexpr zx::duration kHwResetTimeout{zx::msec(50)};
  static inline constexpr zx::duration kEndpointDeadline{zx::sec(10)};

  struct UserEndpoint;
  class EpServer : public usb::EndpointServer {
   public:
    EpServer(const zx::bti& bti, Dwc3* dwc3, UserEndpoint* uep)
        : usb::EndpointServer{bti, uep->ep.ep_num, usb::ScatterGatherSupport::kSupported},
          dwc3_{dwc3},
          uep_{uep} {}

    void CancelAll(zx_status_t reason, bool force = false);

    std::queue<usb::RequestVariant> queued_reqs;  // requests waiting to be processed
    struct RequestState {
      usb::RequestVariant request;
      size_t total_trbs;
      size_t completed_trbs;
      size_t completed_bytes;
    };
    std::queue<RequestState> active_reqs;  // requests currently being processed
    std::optional<zx_status_t> pending_cancel_reason;
    std::vector<CancelAllCompleter::Async> cancel_completers;
    void FlushCancelCompleters(zx_status_t status);

   private:
    // EndpointServer overrides
    void OnUnbound(fidl::UnbindInfo info,
                   fidl::ServerEnd<fuchsia_hardware_usb_endpoint::Endpoint> server_end) override {
      CancelAll(ZX_ERR_IO_NOT_PRESENT);
      FlushCancelCompleters(ZX_ERR_CANCELED);
      usb::EndpointServer::OnUnbound(info, std::move(server_end));
    }

    // fuchsia_hardware_usb_endpoint::Endpoint protocol implementation.
    void GetInfo(GetInfoCompleter::Sync& completer) override;
    void QueueRequests(QueueRequestsRequest& request,
                       QueueRequestsCompleter::Sync& completer) override;
    void CancelAll(CancelAllCompleter::Sync& completer) override;

    Dwc3* dwc3_{nullptr};         // Must outlive this instance.
    UserEndpoint* uep_{nullptr};  // Must outlive this instance.
  };

  struct Endpoint {
    Endpoint() = default;
    explicit Endpoint(uint8_t ep_num) : ep_num(ep_num) {}

    Endpoint(const Endpoint&) = delete;
    Endpoint& operator=(const Endpoint&) = delete;
    Endpoint(Endpoint&&) = delete;
    Endpoint& operator=(Endpoint&&) = delete;

    static inline constexpr bool IsOutput(uint8_t ep_num) { return (ep_num & 0x1) == 0; }
    static inline constexpr bool IsInput(uint8_t ep_num) { return (ep_num & 0x1) == 1; }

    bool IsOutput() const { return IsOutput(ep_num); }
    bool IsInput() const { return IsInput(ep_num); }

    static inline constexpr uint32_t kInvalidResourceId = UINT32_MAX;
    uint32_t rsrc_id{kInvalidResourceId};  // resource ID for current_req

    const uint8_t ep_num{0};
    fdescriptor::EndpointType type{fdescriptor::EndpointType::kControl};
    uint8_t interval{0};
    uint16_t max_packet_size{0};
    bool enabled{false};
    // TODO(voydanoff) USB 3 specific stuff here

    enum class TransferState {
      // Endpoint is Idle.
      kIdle,
      // Endpoint has requested a transfer start and is waiting for the resource
      // ID to move to kActiveSingle.
      kStartingSingle,
      // Endpoint has an active transfer with a `LST` marked TRB. Transfer ends
      // when that TRB is hit.
      kActiveSingle,
      // Endpoint has requested a transfer start and is waiting for the
      // resource ID to move to kActiveOngoing.
      kStartingOngoing,
      // Endpoint has an active transfer with no `LST` marked TRB. Transfer only
      // ends when command is issued.
      kActiveOngoing,
      // Endpoint is canceling transfer.
      kCanceling,
      // Endpoint is pending cancelation while waiting for a transfer start to
      // complete.
      kPendingCancel,
    };
    TransferState transfer_state{TransferState::kIdle};
    // TODO(https://fxbug.dev/527908652): Merge this boolean with transfer
    // state.
    bool got_not_ready{false};
    bool stalled{false};
    uint64_t total_transfers{0};
    uint64_t total_bytes{0};
    uint64_t command_failures{0};
    uint32_t stale_starts_to_end{0};
    uint32_t stale_cancels_to_ignore{0};
    uint8_t usb_endpoint_address{0};

    bool TransferStateIsActive() const {
      switch (transfer_state) {
        case TransferState::kIdle:
        case TransferState::kCanceling:
        case TransferState::kPendingCancel:
        case TransferState::kStartingSingle:
        case TransferState::kStartingOngoing:
          return false;
        case TransferState::kActiveSingle:
        case TransferState::kActiveOngoing:
          return true;
      }
    }
  };

  struct UserEndpoint {
    UserEndpoint() = default;
    UserEndpoint(const UserEndpoint&) = delete;
    UserEndpoint& operator=(const UserEndpoint&) = delete;
    UserEndpoint(UserEndpoint&&) = delete;
    UserEndpoint& operator=(UserEndpoint&&) = delete;

    TrbFifo fifo;
    Endpoint ep;
    std::optional<EpServer> server;
    std::vector<fit::callback<void(bool)>> on_idle_callbacks;
    uint64_t configure_generation{0};
  };

  // A small helper class which basically allows us to have a collection of user
  // endpoints which is dynamically allocated at startup, but which will never
  // change in size.  std::array is not an option here, as it is sized at
  // compile time, while std::vector would force us to make user endpoints
  // movable objects (which we really don't want to do).  Basically, this is a
  // lot of typing to get a C-style array which knows its size and supports
  // range based iteration.
  class UserEndpointCollection {
   public:
    void Init(size_t count, const zx::bti& bti, Dwc3* dwc3) {
      ZX_ASSERT(count <= (std::numeric_limits<uint8_t>::max() - kUserEndpointStartNum));
      ZX_ASSERT(count_ == 0);
      ZX_ASSERT(endpoints_.get() == nullptr);

      count_ = count;
      endpoints_ = std::make_unique<UserEndpoint[]>(count_);
      for (size_t i = 0; i < count_; ++i) {
        UserEndpoint& uep = endpoints_[i];
        const_cast<uint8_t&>(uep.ep.ep_num) = static_cast<uint8_t>(i) + kUserEndpointStartNum;
        uep.server.emplace(bti, dwc3, &uep);
      }
    }

    // Standard size and index-operator
    size_t size() const { return count_; }
    UserEndpoint& operator[](size_t ndx) {
      ZX_ASSERT(ndx < count_);
      return endpoints_[ndx];
    }

    // Support for range-based for loops.
    UserEndpoint* begin() { return endpoints_.get() + 0; }
    UserEndpoint* end() { return endpoints_.get() + count_; }
    const UserEndpoint* begin() const { return endpoints_.get() + 0; }
    const UserEndpoint* end() const { return endpoints_.get() + count_; }

   private:
    size_t count_{0};
    std::unique_ptr<UserEndpoint[]> endpoints_;
  };

  struct Ep0 {
    Ep0() : out(kEp0Out), in(kEp0In) {}

    Ep0(const Ep0&) = delete;
    Ep0& operator=(const Ep0&) = delete;
    Ep0(Ep0&&) = delete;
    Ep0& operator=(Ep0&&) = delete;

    enum class State {
      // Controller is not programmed to receive control directives.
      None,

      // Controller programmed to accept a Setup token from the host. Upon XferComplete, the machine
      // will transition to one of:
      //   TwoStage: The control directive is a 2-stage dataless control-write.
      //   DataOut: The control directive is a 3-stage control-write involving a data stage.
      //   DataIn: The control directive is a 3-stage control-read involving a data stage.
      //
      // In this, and all following states, errors will result in stalling the transfer and
      // transitioning back to the Setup state.
      Setup,

      // Two-stage transfer states.
      //
      // With two-stage transfers, there is an inherent race between the controller generating an
      // XferNotReady(Status) event and the effects of the transfer having been applied. These
      // states account for the race and ensure the Status state is not entered until the stack has
      // had a chance to apply the incoming control directive.
      //
      // Upon entering the TwoStage state, the race will be started. Errors aside, one of the two
      // following conditions will occur.
      //   1. The stack applies the effects of the control directive before the controller issues a
      //      XferNotReady(Status) event. Transition from TwoStage to WaitHost.
      //   2. The controller issues XferNotReady(Status) before the stack finishes applying the
      //      control directive. Transition to the WaitFidl state.
      //
      // Once in the WaitHost or WaitFidl state, the conditions to transition to the Status state
      // are different:
      //   WaitHost: Upon receipt of a XferNotReady(Status) event, transition to the Status state.
      //   WaitFidl: Upon fully applying the control directive (i.e. the Control() RPC completes),
      //     transition to the Status state.
      TwoStage,
      WaitFidl,
      WaitHost,

      // Three-stage data states.
      //
      // These states serve to move data to/from the host depending on whether the transfer is a
      // control-read or control-write. Once in one of the data states, data will be exchanged with
      // the host commensurate with the transfer direction. Upon receipt of the XferComplete event,
      // the corresponding WaitNreadyOut/In state will be entered and await a further
      // XferNotReady(Status) for the given endpoint.
      DataOut,
      DataIn,
      WaitZlpIn,
      WaitNrdyOut,  // Waiting on ep0.
      WaitNrdyIn,   // Waiting on ep1.

      // Final Status state to either issue or receive an ACK to/from the host depending on transfer
      // type. Upon XferComplete, transition to the Setup state.
      Status,
    };

    TrbFifo shared_fifo;
    std::unique_ptr<dma_buffer::ContiguousBuffer> buffer;
    State state = Ep0::State::None;
    size_t cur_transfer_len = 0;
    Endpoint out;
    Endpoint in;
    fdescriptor::wire::UsbSetup cur_setup;
    fdescriptor::wire::UsbSpeed cur_speed{fdescriptor::wire::UsbSpeed::kUndefined};
    uint64_t setup_generation{0};
    // Latched 8-bit USB 2.0 test mode selector from the upper byte of wIndex (wIndex[15:8]).
    uint8_t test_mode{0};
  };

  friend struct std::formatter<Ep0::State>;
  friend struct std::formatter<Endpoint::TransferState>;
  friend class Dwc3Metrics;
  template <bool manage_lifetime, typename gtest_base>
  friend class TestFixture;
  friend class Dwc3TestHelper;

  constexpr bool is_ep0_num(uint8_t ep_num) { return ((ep_num == kEp0Out) || (ep_num == kEp0In)); }

  UserEndpoint* get_user_endpoint(uint8_t ep_num) {
    if (ep_num >= kUserEndpointStartNum) {
      const uint8_t ndx = ep_num - kUserEndpointStartNum;
      return (ndx < user_endpoints_.size()) ? &user_endpoints_[ndx] : nullptr;
    }
    return nullptr;
  }

  fdf::MmioBuffer* get_mmio() { return &*mmio_; }
  static uint8_t UsbAddressToEpNum(uint8_t addr) {
    return static_cast<uint8_t>((usb_ep_num(addr) << 1) | usb_ep_is_in(addr));
  }

  bool power_on() const { return power_on_; }

  zx::eventpair AcquireWakeLease();

  zx_status_t AcquirePDevResources();
  zx::result<> LoadMetadata(fdf::PDev& pdev);
  zx_status_t Init();

  // This method is safe to call with the core powered down.
  void ReleaseResources();

  // IRQ thread's two top level event decoders.
  void HandleEvent(uint32_t event);
  void HandleEpEvent(uint32_t event);

  // Handlers for global events posted to the event buffer by the controller HW.
  void HandleResetEvent();
  void HandleConnectionDoneEvent();
  void HandleDisconnectedEvent();

  // Handlers for end-point specific events posted to the event buffer by the controller HW.
  void HandleEpTransferCompleteEvent(uint8_t ep_num);
  void HandleEpTransferInProgressEvent(uint8_t ep_num);
  void HandleEpTransferNotReadyEvent(uint8_t ep_num, uint32_t stage);
  void HandleEpTransferStartedEvent(uint8_t ep_num, uint32_t rsrc_id);
  void HandleEpTransferEndedEvent(uint8_t ep_num);
  void UserEpCompleteTransfers(UserEndpoint& uep);

  [[nodiscard]] zx_status_t CheckHwVersion();
  [[nodiscard]] zx_status_t ResetHw();
  void ConfigureCoreRegisters();
  void StartEvents();
  void StopEvents();
  void SetDeviceAddress(uint32_t address);

  // EP0 stuff
  zx_status_t Ep0Init();
  // This method is safe to call with the core powered down.
  void Ep0Reset();
  void Ep0Start();
  void Ep0QueueSetup();
  void Ep0StartEndpoints();
  void HandleEp0Setup(size_t length);
  void DoControlCall(fuchsia_hardware_usb_descriptor::wire::UsbSetup setup, size_t length);
  void HandleEp0TransferCompleteEvent(uint8_t ep_num);
  void HandleEp0TransferNotReadyEvent(uint8_t ep_num, uint32_t stage);
  // This method clears the fifo, ends any ongoing transfers, and stalls the endpoint.
  // It's used in preparation of gracefully restarting a failed control transfer.
  void Ep0EndAndStall(Endpoint& ep);

  // General EP stuff
  void EpEnable(Endpoint& ep, bool enable);
  void EpSetConfig(Endpoint& ep, bool enable);
  zx_status_t EpSetStall(Endpoint& ep, bool stall);
  void EpStartSingleTransfer(Endpoint& ep, TrbFifo& fifo, uint32_t type, zx_paddr_t buffer,
                             size_t length);
  // This method is safe to call with the core powered down.
  void EpReset(Endpoint& ep);

  // Methods specific to user endpoints
  // This method is safe to call with the core powered down.
  // When `force` is false (default, used when the event FIFO remains active such as USB bus reset),
  // active transfers enter `kCanceling` (`CMDIOC=1`) and defer `DALEPENA` clearing until
  // `DEPEVT_CMD_CMPL` arrives. When `force` is true (used prior to `StopEvents()` / power-off),
  // active transfers are ended synchronously (`CMDIOC=0`) so `DALEPENA` is cleared immediately.
  void UserEpReset(UserEndpoint& uep, bool force = false);
  void UserEpQueueNext(UserEndpoint& uep);
  void UserEpQueueNextSingle(UserEndpoint& uep);
  void UserEpQueueNextOngoing(UserEndpoint& uep, bool start_transfer);
  bool IsEndpointIdle(const UserEndpoint& uep) const;
  void WaitForEndpointIdle(UserEndpoint& uep, fit::callback<void(bool)> callback);
  void WaitForAllUserEndpointsIdle(fit::callback<void(bool)> callback);
  void NotifyEndpointIdle(UserEndpoint& uep);
  void CancelAllEndpointIdleCallbacks();

  // Resets only user endpoints without touching EP0 or its state.
  void ResetUserEndpoints(bool force = false);

  enum class QueueRequestError {
    kEmpty,
    kError,
    kNotEnoughSpace,
  };
  using QueueRequestResult = fit::result<QueueRequestError, zx_paddr_t>;
  QueueRequestResult QueueNextRequest(UserEndpoint& uep, bool is_single);

  // This method is safe to call with the core powered down.
  void ResetEndpoints(bool force = false);

  // Commands
  bool CmdStartNewConfig(const Endpoint& ep, uint32_t rsrc_id_base);
  void CmdEpSetConfig(const Endpoint& ep, bool modify);
  void CmdEpTransferConfig(const Endpoint& ep);
  void CmdEpStartTransfer(const Endpoint& ep, zx_paddr_t trb_phys);
  void CmdEpUpdateTransfer(const Endpoint& ep);
  // This method is safe to call with the core powered down.
  bool CmdEpEndTransfer(const Endpoint& ep, bool cmd_ioc = true);
  void CmdEpSetStall(const Endpoint& ep);
  void CmdEpClearStall(const Endpoint& ep);

  // Start to operate in peripheral mode.
  void StartPeripheralMode();
  void ResetConfiguration();

  void QuiesceCore();
  void PowerDownCore();
  // Reports the core state to the PHY over fuchsia.hardware.usb.phy/UsbPhy.ConnectStatusChanged.
  // `connected = true`: core powered and running (sent at power-up and at SET_CONFIGURATION).
  // `connected = false`: core quiesced, DCI disconnect drained and powered down (`power_on_` is
  // false); the PHY may remove the core's clocks, resets and power after this. Only
  // ApplyConnectStatusChanged() may send `false`.
  void NotifyPhyConnectStatus(bool connected);
  // Sends UsbDciInterface.SetConnected(false) and invokes `on_done` once usb-peripheral has
  // replied (synchronously if no DCI interface is bound). `disconnect_in_flight_` is true in
  // between.
  void NotifyDciDisconnectedThen(fit::callback<void()> on_done);
  // Applies a connect status handed over by the PHY: powers the core up/down, (re)starts
  // peripheral mode and tells the PHY and usb-peripheral about it. For a disconnect this spans the
  // asynchronous DCI drain; `on_done` runs once the status is fully applied and the PHY has been
  // informed. Statuses are applied strictly one at a time: OnConnectStatusChanged() does not
  // re-arm the hanging get until `on_done` runs.
  void ApplyConnectStatusChanged(bool connected, fit::callback<void()> on_done = {});

  void HandleIrq(async_dispatcher_t* dispatcher, async::IrqBase* irq, zx_status_t status,
                 const zx_packet_interrupt_t* interrupt);

  // OnConnectStatusChanged() is how the phy notifies the dwc3 driver of plug/unplug changes.
  void OnConnectStatusChanged(
      fidl::Result<fuchsia_hardware_usb_phy::ConnectionWatcher::WatchConnectStatusChanged>& result);
  // Issues the next ConnectionWatcher.WatchConnectStatusChanged hanging get. Only called once the
  // previously returned status has been fully applied (see ApplyConnectStatusChanged()), so the PHY
  // may treat a parked hanging get that follows a disconnect as "DWC3 no longer touches the core".
  // `setup_wake_lease` is handed back to the PHY to keep the system awake across the hand-over.
  void RearmConnectionWatcher(zx::eventpair setup_wake_lease);

  fdf::PDev pdev_;

  fidl::WireClient<fuchsia_hardware_usb_dci::UsbDciInterface> dci_intf_;

  std::optional<fdf::MmioBuffer> mmio_;

  zx::bti bti_;
  bool has_pinned_memory_{false};

  zx::interrupt irq_;

  EventFifo event_fifo_;
  async::IrqMethod<Dwc3, &Dwc3::HandleIrq> irq_handler_{this};
  // If true, `StartController()` has been called by the client. If false, it has not been called or
  // `StopController()` was called most recently.
  bool controller_started_{false};
  bool power_on_{true};
  bool link_connected_{false};

  // Returns true if PHY power is on, the controller has been started by the client, and no
  // disconnect drain is in flight.
  bool is_active() const { return power_on_ && controller_started_ && !disconnect_in_flight_; }
  fuchsia_hardware_usb_descriptor::UsbSpeed connection_speed() const { return connection_speed_; }

  // True if the EndTransfer core command can be polled via CmdAct instead of waiting on a
  // CommandComplete endpoint irq event. Only available to core versions >= 3.10a. Set during
  // initialization based on core version.
  bool poll_end_xfer_{false};

  fuchsia_hardware_usb_descriptor::UsbSpeed connection_speed_{
      fuchsia_hardware_usb_descriptor::UsbSpeed::kUndefined};

  Ep0 ep0_;
  UserEndpointCollection user_endpoints_;
  fuchsia_hardware_usb_dci::DciHardwareInfo dci_hardware_info_;

  std::unique_ptr<PlatformExtension> platform_extension_;

  fidl::Client<fuchsia_hardware_usb_phy::UsbPhy> phy_;
  fidl::Client<fuchsia_hardware_usb_phy::ConnectionWatcher> connection_watcher_;
  zx::eventpair connection_lease_;
  // True while UsbDciInterface.SetConnected(false) is being drained by usb-peripheral: the core is
  // kept powered only so function drivers can disable their endpoints.
  bool disconnect_in_flight_{false};
  bool stopping_{false};

  fidl::ServerBindingGroup<fuchsia_hardware_usb_dci::UsbDci> dci_bindings_;
  fidl::SyncClient<fuchsia_driver_framework::NodeController> child_;

  fdf_metadata::MetadataServer<fuchsia_boot_metadata::MacAddressMetadata>
      mac_address_metadata_server_;
  fdf_metadata::MetadataServer<fuchsia_boot_metadata::SerialNumberMetadata>
      serial_number_metadata_server_;
  fdf_metadata::MetadataServer<fuchsia_hardware_usb_phy::Metadata> usb_phy_metadata_server_;

  std::shared_ptr<fdf::Namespace> incoming_;
  std::optional<dwc3_config::Config> config_;
  uint32_t interrupt_moderation_ = 0;
  std::optional<uint32_t> fladj_;
  bool fladj_refclk_lpm_sel_ = false;
  bool supports_gfladj_ = false;

  std::optional<inspect::ComponentInspector> inspector_;

  // The basic model here is:
  //   * Record data outside of Inspect structures for better memory efficiency
  //   * Set up lazy nodes for the Inspect structures to pull data only when needed
  //   * Package all of this up in a subsystem to minimize its intrusiveness into the main
  //     driver code.
  inspect::LazyNode dwc3_root_;
  Dwc3Metrics metrics_;

  // USB Policy and Health
  fidl::ServerBindingGroup<fuchsia_hardware_usb_policy::Controller> policy_bindings_;
  fuchsia_hardware_usb_policy::DeviceState device_state_ =
      fuchsia_hardware_usb_policy::DeviceState::kNotAttached;
  uint8_t assigned_address_ = 0;
  std::vector<WatchDeviceStateCompleter::Async> pending_completers_;
  bool has_new_device_state_ = true;

  // Knobs for testing
  bool enable_enqueue_many_trbs_ = true;

  uint32_t num_in_eps_ = 0;
  uint32_t num_out_eps_ = 0;

  void SetDeviceState(fuchsia_hardware_usb_policy::DeviceState state);
  void SetDeviceState(fuchsia_hardware_usb_policy::DeviceState state, uint8_t address);

  bool WaitForCmdAct(const char* caller_name, const uint8_t ep_num);
};

}  // namespace dwc3

template <>
struct std::formatter<dwc3::Dwc3::Ep0::State> : std::formatter<std::string> {
  auto format(const dwc3::Dwc3::Ep0::State state, format_context& ctx) const {
    std::string fmt;
    switch (state) {
      case dwc3::Dwc3::Ep0::State::None:
        fmt = "None";
        break;
      case dwc3::Dwc3::Ep0::State::Setup:
        fmt = "Setup";
        break;
      case dwc3::Dwc3::Ep0::State::TwoStage:
        fmt = "TwoStage";
        break;
      case dwc3::Dwc3::Ep0::State::WaitFidl:
        fmt = "WaitFidl";
        break;
      case dwc3::Dwc3::Ep0::State::WaitHost:
        fmt = "WaitHost";
        break;
      case dwc3::Dwc3::Ep0::State::DataOut:
        fmt = "DataOut";
        break;
      case dwc3::Dwc3::Ep0::State::DataIn:
        fmt = "DataIn";
        break;
      case dwc3::Dwc3::Ep0::State::WaitZlpIn:
        fmt = "WaitZlpIn";
        break;
      case dwc3::Dwc3::Ep0::State::WaitNrdyOut:
        fmt = "WaitNrdyOut";
        break;
      case dwc3::Dwc3::Ep0::State::WaitNrdyIn:
        fmt = "WaitNrdyIn";
        break;
      case dwc3::Dwc3::Ep0::State::Status:
        fmt = "Status";
        break;
    }
    return std::formatter<std::string>::format(fmt, ctx);
  }
};

template <>
struct std::formatter<dwc3::Dwc3::Endpoint::TransferState> : std::formatter<std::string_view> {
  auto format(const dwc3::Dwc3::Endpoint::TransferState state, format_context& ctx) const {
    std::string_view fmt;
    switch (state) {
      case dwc3::Dwc3::Endpoint::TransferState::kIdle:
        fmt = "idle";
        break;
      case dwc3::Dwc3::Endpoint::TransferState::kCanceling:
        fmt = "canceling";
        break;
      case dwc3::Dwc3::Endpoint::TransferState::kPendingCancel:
        fmt = "pending-cancel";
        break;
      case dwc3::Dwc3::Endpoint::TransferState::kStartingSingle:
        fmt = "starting-single";
        break;
      case dwc3::Dwc3::Endpoint::TransferState::kActiveSingle:
        fmt = "active-single";
        break;
      case dwc3::Dwc3::Endpoint::TransferState::kStartingOngoing:
        fmt = "starting-ongoing";
        break;
      case dwc3::Dwc3::Endpoint::TransferState::kActiveOngoing:
        fmt = "active-ongoing";
        break;
    }
    return std::formatter<std::string_view>::format(fmt, ctx);
  }
};

#endif  // SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_H_
