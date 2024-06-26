// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_BLUETOOTH_HCI_TRANSPORT_UART_BT_TRANSPORT_UART_H_
#define SRC_CONNECTIVITY_BLUETOOTH_HCI_TRANSPORT_UART_BT_TRANSPORT_UART_H_

#include <fidl/fuchsia.driver.compat/cpp/wire.h>
#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.bluetooth/cpp/wire.h>
#include <fidl/fuchsia.hardware.serialimpl/cpp/driver/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/async/cpp/wait.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fit/thread_checker.h>
#include <lib/zx/event.h>
#include <threads.h>
#include <zircon/device/bt-hci.h>

#include <mutex>

#include <sdk/lib/driver/logging/cpp/logger.h>

namespace bt_transport_uart {

class BtTransportUart
    : public fdf::DriverBase,
      public fidl::WireAsyncEventHandler<fuchsia_driver_framework::NodeController>,
      public fidl::WireServer<fuchsia_hardware_bluetooth::Hci>,
      public fdf::WireServer<fuchsia_hardware_serialimpl::Device> {
 public:
  // If |dispatcher| is non-null, it will be used instead of a new work thread.
  // tests.
  explicit BtTransportUart(fdf::DriverStartArgs start_args,
                           fdf::UnownedSynchronizedDispatcher driver_dispatcher);

  zx::result<> Start() override;
  void PrepareStop(fdf::PrepareStopCompleter completer) override;

  void handle_unknown_event(
      fidl::UnknownEventMetadata<fuchsia_driver_framework::NodeController> metadata) override {}

  // Request handlers for Hci protocol.
  void OpenCommandChannel(OpenCommandChannelRequestView request,
                          OpenCommandChannelCompleter::Sync& completer) override;
  void OpenAclDataChannel(OpenAclDataChannelRequestView request,
                          OpenAclDataChannelCompleter::Sync& completer) override;
  void OpenSnoopChannel(OpenSnoopChannelRequestView request,
                        OpenSnoopChannelCompleter::Sync& completer) override;
  void OpenScoDataChannel(OpenScoDataChannelRequestView request,
                          OpenScoDataChannelCompleter::Sync& completer) override;
  void OpenIsoDataChannel(OpenIsoDataChannelRequestView request,
                          OpenIsoDataChannelCompleter::Sync& completer) override;
  void ConfigureSco(ConfigureScoRequestView request,
                    ConfigureScoCompleter::Sync& completer) override;
  void ResetSco(ResetScoCompleter::Sync& completer) override;

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_bluetooth::Hci> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override;

  // fuchsia_hardware_serialimpl::Device FIDL request handler implementation.
  void GetInfo(fdf::Arena& arena, GetInfoCompleter::Sync& completer) override;
  void Config(ConfigRequestView request, fdf::Arena& arena,
              ConfigCompleter::Sync& completer) override;
  void Enable(EnableRequestView request, fdf::Arena& arena,
              EnableCompleter::Sync& completer) override;
  void Read(fdf::Arena& arena, ReadCompleter::Sync& completer) override;
  void Write(WriteRequestView request, fdf::Arena& arena, WriteCompleter::Sync& completer) override;
  void CancelAll(fdf::Arena& arena, CancelAllCompleter::Sync& completer) override;

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_serialimpl::Device> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

 private:
  // HCI UART packet indicators
  enum BtHciPacketIndicator : uint8_t {
    kHciNone = 0,
    kHciCommand = 1,
    kHciAclData = 2,
    kHciSco = 3,
    kHciEvent = 4,
  };

  struct HciWriteCtx {
    BtTransportUart* ctx;
    // Owned.
    uint8_t* buffer;
  };

  // This wrapper around async_wait enables us to get a BtTransportUart* in the handler.
  // We use this instead of async::WaitMethod because async::WaitBase isn't thread safe.
  struct Wait : public async_wait {
    explicit Wait(BtTransportUart* uart, zx::channel* channel);
    static void Handler(async_dispatcher_t* dispatcher, async_wait_t* async_wait,
                        zx_status_t status, const zx_packet_signal_t* signal);
    BtTransportUart* uart;
    // Indicates whether a wait has begun and not ended.
    bool pending = false;
    // The channel that this wait waits on.
    zx::channel* channel;
  };

  // Returns length of current event packet being received
  // Must only be called in the read callback (HciHandleUartReadEvents).
  size_t EventPacketLength();

  // Returns length of current ACL data packet being received
  // Must only be called in the read callback (HciHandleUartReadEvents).
  size_t AclPacketLength();

  // Returns length of current SCO data packet being received
  // Must only be called in the read callback (HciHandleUartReadEvents).
  size_t ScoPacketLength();

  void ChannelCleanupLocked(zx::channel* channel) __TA_REQUIRES(mutex_);

  void SnoopChannelWriteLocked(uint8_t flags, uint8_t* bytes, size_t length) __TA_REQUIRES(mutex_);

  void HciBeginShutdown() __TA_EXCLUDES(mutex_);

  void SerialWrite(uint8_t* buffer, size_t length) __TA_EXCLUDES(mutex_);

  void HciHandleClientChannel(zx::channel* chan, zx_signals_t pending) __TA_EXCLUDES(mutex_);

  // Queues a read callback for async serial on the dispatcher.
  void QueueUartRead();
  void HciHandleUartReadEvents(const uint8_t* buf, size_t length) __TA_EXCLUDES(mutex_);

  // Reads the next packet chunk from |uart_src| into |buffer| and increments |buffer_offset| and
  // |uart_src| by the number of bytes read. If a complete packet is read, it will be written to
  // |channel|.
  using PacketLengthFunction = size_t (BtTransportUart::*)();
  void ProcessNextUartPacketFromReadBuffer(uint8_t* buffer, size_t buffer_size,
                                           size_t* buffer_offset, const uint8_t** uart_src,
                                           const uint8_t* uart_end,
                                           PacketLengthFunction get_packet_length,
                                           zx::channel* channel, bt_hci_snoop_type_t snoop_type);

  void HciReadComplete(zx_status_t status, const uint8_t* buffer, size_t length)
      __TA_EXCLUDES(mutex_);

  void HciWriteComplete(zx_status_t status) __TA_EXCLUDES(mutex_);

  static int HciThread(void* arg) __TA_EXCLUDES(mutex_);

  void OnChannelSignal(Wait* wait, zx_status_t status, const zx_packet_signal_t* signal);

  zx_status_t HciOpenChannel(zx::channel* in_channel, zx_handle_t in) __TA_EXCLUDES(mutex_);

  zx_status_t ServeProtocols();

  // Adds the device.
  zx_status_t Bind() __TA_EXCLUDES(mutex_);

  // 1 byte packet indicator + 3 byte header + payload
  static constexpr uint32_t kCmdBufSize = 255 + 4;

  // The number of currently supported HCI channel endpoints. We currently have
  // one channel for command/event flow and one for ACL data flow. The sniff channel is managed
  // separately.
  static constexpr uint8_t kNumChannels = 2;

  // add one for the wakeup event
  static constexpr uint8_t kNumWaitItems = kNumChannels + 1;

  // The maximum HCI ACL frame size used for data transactions
  // (1024 + 4 bytes for the ACL header + 1 byte packet indicator)
  static constexpr uint32_t kAclMaxFrameSize = 1029;

  // The maximum HCI SCO frame size used for data transactions.
  // (255 byte payload + 3 bytes for the SCO header + 1 byte packet indicator)
  static constexpr uint32_t kScoMaxFrameSize = 259;

  // 1 byte packet indicator + 2 byte header + payload
  static constexpr uint32_t kEventBufSize = 255 + 3;

  fdf::WireClient<fuchsia_hardware_serialimpl::Device> serial_client_;

  zx::channel cmd_channel_ __TA_GUARDED(mutex_);
  Wait cmd_channel_wait_ __TA_GUARDED(mutex_){this, &cmd_channel_};

  zx::channel acl_channel_ __TA_GUARDED(mutex_);
  Wait acl_channel_wait_ __TA_GUARDED(mutex_){this, &acl_channel_};

  zx::channel sco_channel_ __TA_GUARDED(mutex_);
  Wait sco_channel_wait_ __TA_GUARDED(mutex_){this, &sco_channel_};

  zx::channel snoop_channel_ __TA_GUARDED(mutex_);

  std::atomic_bool shutting_down_ = false;

  // True if there is not a UART write pending. Set to false when a write is initiated, and set to
  // true when the write completes.
  bool can_write_ __TA_GUARDED(mutex_) = true;

  // type of current packet being read from the UART
  // Must only be used in the UART read callback (HciHandleUartReadEvents).
  BtHciPacketIndicator cur_uart_packet_type_ = kHciNone;

  // for accumulating HCI events
  // Must only be used in the UART read callback (HciHandleUartReadEvents).
  uint8_t event_buffer_[kEventBufSize];
  // Must only be used in the UART read callback (HciHandleUartReadEvents).
  size_t event_buffer_offset_ = 0;

  // for accumulating ACL data packets
  // Must only be used in the UART read callback (HciHandleUartReadEvents).
  uint8_t acl_buffer_[kAclMaxFrameSize];
  // Must only be used in the UART read callback (HciHandleUartReadEvents).
  size_t acl_buffer_offset_ = 0;

  // For accumulating SCO packets
  // Must only be used in the UART read callback (HciHandleUartReadEvents).
  uint8_t sco_buffer_[kScoMaxFrameSize];
  // Must only be used in the UART read callback (HciHandleUartReadEvents).
  size_t sco_buffer_offset_ = 0;

  // for sending outbound packets to the UART
  // kAclMaxFrameSize is the largest frame size sent.
  uint8_t write_buffer_[kAclMaxFrameSize] __TA_GUARDED(mutex_);

  // Save the serial device pid for vendor drivers to fetch.
  uint32_t serial_pid_ = 0;

  std::mutex mutex_;

  std::optional<async::Loop> loop_;
  // In production, this is loop_.dispatcher(). In tests, this is the test dispatcher.
  async_dispatcher_t* dispatcher_ = nullptr;

  fidl::WireClient<fuchsia_driver_framework::Node> node_;
  fidl::WireClient<fuchsia_driver_framework::NodeController> node_controller_;

  // The task which runs to queue a uart read.
  async::TaskClosureMethod<BtTransportUart, &BtTransportUart::QueueUartRead> queue_read_task_{this};

  compat::SyncInitializedDeviceServer compat_server_;
};

}  // namespace bt_transport_uart

#endif  // SRC_CONNECTIVITY_BLUETOOTH_HCI_TRANSPORT_UART_BT_TRANSPORT_UART_H_
