// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_TEST_UTIL_H_
#define SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_TEST_UTIL_H_

#include <lib/sync/completion.h>
#include <lib/zx/event.h>

#include <memory>
#include <vector>

#include <fbl/intrusive_double_list.h>
#include <gtest/gtest.h>

#include "definitions.h"
#include "device_interface.h"
#include "src/lib/testing/predicates/status.h"
#include "test_session.h"

namespace network::testing {

constexpr uint16_t kDefaultRxDepth = 16;
constexpr uint16_t kDefaultTxDepth = 16;
constexpr uint16_t kDefaultDescriptorCount = 256;
constexpr uint64_t kDefaultBufferLength = ZX_PAGE_SIZE / 2;
constexpr uint32_t kAutoReturnRxLength = 512;

class RxReturnTransaction;
class TxReturnTransaction;
using VmoProvider = fit::function<zx::unowned_vmo(uint8_t)>;

class TxFidlBuffer : public fbl::DoublyLinkedListable<std::unique_ptr<TxFidlBuffer>> {
 public:
  explicit TxFidlBuffer(const fuchsia_hardware_network_driver::wire::TxBuffer& buffer)
      : buffer_(buffer) {
    auto regions = buffer.data.get();
    for (size_t i = 0; i < regions.size(); ++i) {
      parts_[i] = regions[i];
    }
    buffer_.data =
        fidl::VectorView<fuchsia_hardware_network_driver::wire::BufferRegion>::FromExternal(
            parts_.data(), regions.size());
  }

  zx_status_t status() const { return status_; }

  void set_status(zx_status_t status) { status_ = status; }

  zx::result<std::vector<uint8_t>> GetData(const VmoProvider& vmo_provider);

  fuchsia_hardware_network_driver::wire::TxResult result() {
    return {
        .id = buffer_.id,
        .status = status_,
    };
  }

  fuchsia_hardware_network_driver::wire::TxBuffer& buffer() { return buffer_; }

 private:
  fuchsia_hardware_network_driver::wire::TxBuffer buffer_{};
  internal::BufferParts<fuchsia_hardware_network_driver::wire::BufferRegion> parts_{};
  zx_status_t status_ = ZX_OK;
};

class TxBuffer : public fbl::DoublyLinkedListable<std::unique_ptr<TxBuffer>> {
 public:
  explicit TxBuffer(const tx_buffer_t& buffer) : buffer_(buffer) {
    for (size_t i = 0; i < buffer_.data_count; i++) {
      parts_[i] = buffer_.data_list[i];
    }
    buffer_.data_list = parts_.data();
  }

  zx_status_t status() const { return status_; }

  void set_status(zx_status_t status) { status_ = status; }

  zx::result<std::vector<uint8_t>> GetData(const VmoProvider& vmo_provider) const;

  tx_result_t result() {
    return {
        .id = buffer_.id,
        .status = status_,
    };
  }

  tx_buffer_t& buffer() { return buffer_; }

 private:
  tx_buffer_t buffer_{};
  internal::BufferParts<buffer_region_t> parts_{};
  zx_status_t status_ = ZX_OK;
};

class RxFidlBuffer : public fbl::DoublyLinkedListable<std::unique_ptr<RxFidlBuffer>> {
 public:
  explicit RxFidlBuffer(const fuchsia_hardware_network_driver::wire::RxSpaceBuffer& space)
      : space_(space),
        return_part_({
            .id = space.id,
        }) {}

  zx_status_t WriteData(const std::vector<uint8_t>& data, const VmoProvider& vmo_provider) {
    return WriteData(cpp20::span(data.data(), data.size()), vmo_provider);
  }

  zx_status_t WriteData(cpp20::span<const uint8_t> data, const VmoProvider& vmo_provider);

  fuchsia_hardware_network_driver::wire::RxBufferPart& return_part() { return return_part_; }
  fuchsia_hardware_network_driver::wire::RxSpaceBuffer& space() { return space_; }

  void SetReturnLength(uint32_t length) { return_part_.length = length; }

 private:
  fuchsia_hardware_network_driver::wire::RxSpaceBuffer space_{};
  fuchsia_hardware_network_driver::wire::RxBufferPart return_part_{};
};

class RxBuffer : public fbl::DoublyLinkedListable<std::unique_ptr<RxBuffer>> {
 public:
  explicit RxBuffer(const rx_space_buffer_t& space)
      : space_(space),
        return_part_(rx_buffer_part_t{
            .id = space.id,
        }) {}

  zx_status_t WriteData(const std::vector<uint8_t>& data, const VmoProvider& vmo_provider) {
    return WriteData(cpp20::span(data.data(), data.size()), vmo_provider);
  }

  zx_status_t WriteData(cpp20::span<const uint8_t> data, const VmoProvider& vmo_provider);

  rx_buffer_part_t& return_part() { return return_part_; }
  rx_space_buffer_t& space() { return space_; }

  void SetReturnLength(uint32_t length) { return_part_.length = length; }

 private:
  rx_space_buffer_t space_{};
  rx_buffer_part_t return_part_{};
};

class RxFidlReturn : public fbl::DoublyLinkedListable<std::unique_ptr<RxFidlReturn>> {
 public:
  RxFidlReturn()
      : buffer_({
            .meta =
                {
                    .info = netdriver::wire::FrameInfo::WithNoInfo(netdriver::wire::NoInfo{
                        static_cast<uint8_t>(netdev::wire::InfoType::kNoInfo)}),
                    .info_type = netdev::wire::InfoType::kNoInfo,
                    .frame_type = netdev::wire::FrameType::kEthernet,
                },
            .data =
                fidl::VectorView<fuchsia_hardware_network_driver::wire::RxBufferPart>::FromExternal(
                    parts_.data(), 0),
        }) {}
  // RxReturn can't be moved because it keeps pointers to the return buffer internally.
  RxFidlReturn(RxFidlReturn&&) = delete;
  RxFidlReturn(std::unique_ptr<RxFidlBuffer> buffer, uint8_t port_id) : RxFidlReturn() {
    PushPart(std::move(buffer));
    buffer_.meta.port = port_id;
  }

  // Pushes buffer space into the return buffer.
  //
  // NB: We don't really need the unique pointer here, we just copy the information we need. But
  // requiring the unique pointer to be passed enforces the buffer ownership semantics. Also
  // RxBuffers usually sit in the available queue as a pointer already.
  void PushPart(std::unique_ptr<RxFidlBuffer> buffer) {
    ZX_ASSERT(buffer_.data.count() < parts_.size());
    parts_[buffer_.data.count()] = buffer->return_part();
    buffer_.data.set_count(buffer_.data.count() + 1);
  }

  const fuchsia_hardware_network_driver::wire::RxBuffer& buffer() const { return buffer_; }
  fuchsia_hardware_network_driver::wire::RxBuffer& buffer() { return buffer_; }

 private:
  internal::BufferParts<fuchsia_hardware_network_driver::wire::RxBufferPart> parts_{};
  fuchsia_hardware_network_driver::wire::RxBuffer buffer_{};
};

class RxReturn : public fbl::DoublyLinkedListable<std::unique_ptr<RxReturn>> {
 public:
  RxReturn()
      : buffer_(rx_buffer_t{
            .meta =
                {
                    .info_type = static_cast<uint32_t>(netdev::wire::InfoType::kNoInfo),
                    .frame_type = static_cast<uint8_t>(netdev::wire::FrameType::kEthernet),
                },
            .data_list = parts_.begin(),
            .data_count = 0,
        }) {}
  // RxReturn can't be moved because it keeps pointers to the return buffer internally.
  RxReturn(RxReturn&&) = delete;
  RxReturn(std::unique_ptr<RxBuffer> buffer, uint8_t port_id) : RxReturn() {
    PushPart(std::move(buffer));
    buffer_.meta.port = port_id;
  }

  // Pushes buffer space into the return buffer.
  //
  // NB: We don't really need the unique pointer here, we just copy the information we need. But
  // requiring the unique pointer to be passed enforces the buffer ownership semantics. Also
  // RxBuffers usually sit in the available queue as a pointer already.
  void PushPart(std::unique_ptr<RxBuffer> buffer) {
    ZX_ASSERT(buffer_.data_count < parts_.size());
    parts_[buffer_.data_count++] = buffer->return_part();
  }

  const rx_buffer_t& buffer() const { return buffer_; }
  rx_buffer_t& buffer() { return buffer_; }

 private:
  internal::BufferParts<rx_buffer_part_t> parts_{};
  rx_buffer_t buffer_{};
};

constexpr zx_signals_t kEventStart = ZX_USER_SIGNAL_0;
constexpr zx_signals_t kEventStop = ZX_USER_SIGNAL_1;
constexpr zx_signals_t kEventTx = ZX_USER_SIGNAL_2;
constexpr zx_signals_t kEventSessionStarted = ZX_USER_SIGNAL_3;
constexpr zx_signals_t kEventRxAvailable = ZX_USER_SIGNAL_4;
constexpr zx_signals_t kEventPortRemoved = ZX_USER_SIGNAL_5;
constexpr zx_signals_t kEventPortActiveChanged = ZX_USER_SIGNAL_6;

struct PortInfo {
  netdev::wire::DeviceClass port_class;
  std::vector<netdev::wire::FrameType> rx_types;
  std::vector<netdev::wire::FrameTypeSupport> tx_types;
};

struct PortStatus {
  uint32_t mtu;
  netdev::wire::StatusFlags flags;
};

class FakeFidlNetworkPortImpl
    : public fdf::WireServer<fuchsia_hardware_network_driver::NetworkPort> {
 public:
  using OnSetActiveCallback = fit::function<void(bool)>;
  FakeFidlNetworkPortImpl();
  ~FakeFidlNetworkPortImpl() override;

  void GetInfo(fdf::Arena& arena, GetInfoCompleter::Sync& completer) override;
  void GetStatus(fdf::Arena& arena, GetStatusCompleter::Sync& completer) override;
  void SetActive(fuchsia_hardware_network_driver::wire::NetworkPortSetActiveRequest* request,
                 fdf::Arena& arena, SetActiveCompleter::Sync& completer) override;
  void GetMac(fdf::Arena& arena, GetMacCompleter::Sync& completer) override;
  void Removed(fdf::Arena& arena, RemovedCompleter::Sync& completer) override;

  PortInfo& port_info() { return port_info_; }
  const PortStatus& status() const { return status_; }
  zx_status_t AddPort(uint8_t port_id, const fdf::Dispatcher& dispatcher,
                      fidl::WireSyncClient<netdev::Device> device,
                      FakeFidlNetworkDeviceImpl& ifc_client);
  zx_status_t AddPortNoWait(uint8_t port_id, const fdf::Dispatcher& dispatcher,
                            fidl::WireSyncClient<netdev::Device> device,
                            FakeFidlNetworkDeviceImpl& ifc_client);
  void RemoveSync();
  void SetMac(fdf::ClientEnd<fuchsia_hardware_network_driver::MacAddr> client) {
    mac_client_end_ = std::move(client);
  }
  void SetOnSetActiveCallback(OnSetActiveCallback cb) { on_set_active_ = std::move(cb); }
  void SetSupportedRxType(netdev::wire::FrameType frame_type) {
    port_info_.rx_types = {frame_type};
  }
  void SetSupportedTxType(netdev::wire::FrameType frame_type) {
    port_info_.tx_types = {{.type = frame_type,
                            .features = netdev::wire::kFrameFeaturesRaw,
                            .supported_flags = netdev::wire::TxFlags(0)}};
  }

  void WaitPortRemoved();

  void WaitForPortRemoval() {
    ASSERT_OK(sync_completion_wait_deadline(&wait_removed_, zx::time::infinite().get()));
  }

  bool active() const { return port_active_; }
  bool removed() const { return port_removed_; }
  uint8_t id() const { return id_; }

  const zx::event& events() const { return event_; }

  void SetOnline(bool online);
  void SetStatus(const PortStatus& status);

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(FakeFidlNetworkPortImpl);

  std::optional<fdf::ServerBindingRef<fuchsia_hardware_network_driver::NetworkPort>> binding_;
  FakeFidlNetworkDeviceImpl* parent_ = nullptr;
  std::optional<fdf::ClientEnd<fuchsia_hardware_network_driver::MacAddr>> mac_client_end_;
  sync_completion_t wait_removed_;
  OnSetActiveCallback on_set_active_;
  uint8_t id_;
  PortInfo port_info_;
  std::atomic_bool port_active_ = false;
  PortStatus status_;
  zx::event event_;
  bool port_removed_ = false;
  bool port_added_ = false;
  std::optional<fidl::WireSyncClient<netdev::Device>> device_;
};

class FakeNetworkPortImpl : public ddk::NetworkPortProtocol<FakeNetworkPortImpl> {
 public:
  using OnSetActiveCallback = fit::function<void(bool)>;
  FakeNetworkPortImpl();
  ~FakeNetworkPortImpl();

  void NetworkPortGetInfo(port_base_info_t* out_info);
  void NetworkPortGetStatus(port_status_t* out_status);
  void NetworkPortSetActive(bool active);
  void NetworkPortGetMac(mac_addr_protocol_t** out_mac_ifc);
  void NetworkPortRemoved();

  port_base_info_t& port_info() { return port_info_; }
  const port_status_t& status() const { return status_; }
  zx_status_t AddPort(uint8_t port_id, ddk::NetworkDeviceIfcProtocolClient ifc_client);
  void RemoveSync();
  void SetMac(mac_addr_protocol_t proto) { mac_proto_ = proto; }
  void SetOnSetActiveCallback(OnSetActiveCallback cb) { on_set_active_ = std::move(cb); }
  void SetSupportedRxType(netdev::wire::FrameType frame_type) {
    rx_types_ = {static_cast<uint8_t>(frame_type)};
  }
  void SetSupportedTxType(netdev::wire::FrameType frame_type) {
    tx_types_ = {frame_type_support_t{
        .type = static_cast<uint8_t>(frame_type),
        .features = netdev::wire::kFrameFeaturesRaw,
        .supported_flags = 0,
    }};
  }

  network_port_protocol_t protocol() {
    return {
        .ops = &network_port_protocol_ops_,
        .ctx = this,
    };
  }

  bool active() const { return port_active_; }
  bool removed() const { return port_removed_; }
  uint8_t id() const { return id_; }

  const zx::event& events() const { return event_; }

  void SetOnline(bool online);
  void SetStatus(const port_status_t& status);

 private:
  using OnRemovedCallback = fit::callback<void()>;

  DISALLOW_COPY_ASSIGN_AND_MOVE(FakeNetworkPortImpl);

  std::array<uint8_t, netdev::wire::kMaxFrameTypes> rx_types_;
  std::array<frame_type_support_t, netdev::wire::kMaxFrameTypes> tx_types_;
  ddk::NetworkDeviceIfcProtocolClient device_client_;
  OnRemovedCallback on_removed_;
  OnSetActiveCallback on_set_active_;
  uint8_t id_;
  mac_addr_protocol_t mac_proto_{};
  port_base_info_t port_info_{};
  std::atomic_bool port_active_ = false;
  port_status_t status_;
  zx::event event_;
  bool port_removed_ = false;
  bool port_added_ = false;
};

struct DeviceInfo {
  uint32_t device_features;
  uint16_t tx_depth;
  uint16_t rx_depth;
  uint16_t rx_threshold;
  uint8_t max_buffer_parts;
  uint32_t max_buffer_length;
  uint32_t buffer_alignment;
  uint32_t min_rx_buffer_length;
  uint32_t min_tx_buffer_length;
  uint16_t tx_head_length;
  uint16_t tx_tail_length;
  std::vector<netdev::wire::RxAcceleration> rx_accel;
  std::vector<netdev::wire::TxAcceleration> tx_accel;
  // std::vector<netdev::wire::RxAcceleration> accel_rx;
  // std::vector<netdev::wire::TxAcceleration> accel_tx;
};

class FakeFidlNetworkDeviceImpl
    : public fdf::WireServer<fuchsia_hardware_network_driver::NetworkDeviceImpl> {
 public:
  using PrepareVmoHandler =
      fit::function<void(uint8_t, const zx::vmo&, PrepareVmoCompleter::Sync&)>;
  FakeFidlNetworkDeviceImpl();
  ~FakeFidlNetworkDeviceImpl() override;

  zx::result<std::unique_ptr<NetworkDeviceInterface>> CreateChild(fdf::Dispatcher* impl_dispatcher,
                                                                  fdf::Dispatcher* ifc_dispatcher,
                                                                  fdf::Dispatcher* port_dispatcher);

  void Init(fuchsia_hardware_network_driver::wire::NetworkDeviceImplInitRequest* request,
            fdf::Arena& arena, InitCompleter::Sync& completer) override;
  void Start(fdf::Arena& arena, StartCompleter::Sync& completer) override;
  void Stop(fdf::Arena& arena, StopCompleter::Sync& completer) override;
  void GetInfo(fdf::Arena& arena, GetInfoCompleter::Sync& completer) override;
  void QueueTx(fuchsia_hardware_network_driver::wire::NetworkDeviceImplQueueTxRequest* request,
               fdf::Arena& arena, QueueTxCompleter::Sync& completer) override;
  void QueueRxSpace(
      fuchsia_hardware_network_driver::wire::NetworkDeviceImplQueueRxSpaceRequest* request,
      fdf::Arena& arena, QueueRxSpaceCompleter::Sync& completer) override;
  void PrepareVmo(
      fuchsia_hardware_network_driver::wire::NetworkDeviceImplPrepareVmoRequest* request,
      fdf::Arena& arena, PrepareVmoCompleter::Sync& completer) override;
  void ReleaseVmo(
      fuchsia_hardware_network_driver::wire::NetworkDeviceImplReleaseVmoRequest* request,
      fdf::Arena& arena, ReleaseVmoCompleter::Sync& completer) override;
  void SetSnoop(fuchsia_hardware_network_driver::wire::NetworkDeviceImplSetSnoopRequest* request,
                fdf::Arena& arena, SetSnoopCompleter::Sync& completer) override;

  fit::function<zx::unowned_vmo(uint8_t)> VmoGetter();

  const zx::event& events() const { return event_; }

  DeviceInfo& info() { return info_; }

  std::unique_ptr<RxFidlBuffer> PopRxBuffer() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    return rx_buffers_.pop_front();
  }

  std::unique_ptr<TxFidlBuffer> PopTxBuffer() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    return tx_buffers_.pop_front();
  }

  fbl::SizedDoublyLinkedList<std::unique_ptr<TxFidlBuffer>> TakeTxBuffers() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    fbl::SizedDoublyLinkedList<std::unique_ptr<TxFidlBuffer>> r;
    tx_buffers_.swap(r);
    return r;
  }

  fbl::SizedDoublyLinkedList<std::unique_ptr<RxFidlBuffer>> TakeRxBuffers() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    fbl::SizedDoublyLinkedList<std::unique_ptr<RxFidlBuffer>> r;
    rx_buffers_.swap(r);
    return r;
  }

  size_t rx_buffer_count() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    return rx_buffers_.size();
  }

  size_t tx_buffer_count() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    return tx_buffers_.size();
  }

  size_t queue_rx_space_called() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    ZX_ASSERT(!queue_rx_space_called_.empty());
    const size_t front = queue_rx_space_called_.front();
    queue_rx_space_called_.pop_front();
    return front;
  }

  size_t queue_tx_called() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    ZX_ASSERT(!queue_tx_called_.empty());
    const size_t front = queue_tx_called_.front();
    queue_tx_called_.pop_front();
    return front;
  }

  std::optional<uint8_t> first_vmo_id() {
    for (size_t i = 0; i < vmos_.size(); i++) {
      if (vmos_[i].is_valid()) {
        return i;
      }
    }
    return std::nullopt;
  }

  void set_auto_start(std::optional<zx_status_t> auto_start) { auto_start_ = auto_start; }

  void set_auto_stop(bool auto_stop) { auto_stop_ = auto_stop; }

  bool TriggerStart();
  bool TriggerStop();

  void set_immediate_return_tx(bool auto_return) { immediate_return_tx_ = auto_return; }
  void set_immediate_return_rx(bool auto_return) { immediate_return_rx_ = auto_return; }
  void set_prepare_vmo_handler(PrepareVmoHandler handler) {
    prepare_vmo_handler_ = std::move(handler);
  }

  fdf::WireSharedClient<fuchsia_hardware_network_driver::NetworkDeviceIfc>& client() {
    return device_client_;
  }

  void WaitReleased() {
    // TODO(nahurley): Figure out why we need to wait based on a signal. Why
    // isn't shutting down the server/client sufficient?
    bool all_released = true;
    for (auto& vmo : vmos_) {
      if (vmo.is_valid()) {
        all_released = false;
      }
    }

    if (!all_released) {
      ASSERT_OK(sync_completion_wait_deadline(&released_completer_, zx::time::infinite().get()));
    }
  }

  cpp20::span<const zx::vmo> vmos() { return cpp20::span(vmos_.begin(), vmos_.end()); }

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(FakeFidlNetworkDeviceImpl);

  class Factory : public NetworkDeviceImplBinder {
   public:
    Factory(FakeFidlNetworkDeviceImpl* parent, fdf::Dispatcher* dispatcher)
        : parent_(parent), dispatcher_(dispatcher) {}
    zx::result<fdf::ClientEnd<netdriver::NetworkDeviceImpl>> Bind() override;

   private:
    DISALLOW_COPY_ASSIGN_AND_MOVE(Factory);

    FakeFidlNetworkDeviceImpl* parent_ = nullptr;
    std::optional<fdf::ServerBindingRef<netdriver::NetworkDeviceImpl>> binding_;
    fdf::Dispatcher* dispatcher_ = nullptr;
  };

  fbl::Mutex lock_;
  sync_completion_t released_completer_;
  fdf::Dispatcher* dispatcher_ = nullptr;
  std::array<zx::vmo, MAX_VMOS> vmos_;
  DeviceInfo info_{};
  fdf::WireSharedClient<fuchsia_hardware_network_driver::NetworkDeviceIfc> device_client_;
  fbl::SizedDoublyLinkedList<std::unique_ptr<RxFidlBuffer>> rx_buffers_ __TA_GUARDED(lock_);
  fbl::SizedDoublyLinkedList<std::unique_ptr<TxFidlBuffer>> tx_buffers_ __TA_GUARDED(lock_);
  std::deque<size_t> queue_tx_called_ __TA_GUARDED(lock_);
  std::deque<size_t> queue_rx_space_called_ __TA_GUARDED(lock_);
  zx::event event_;
  std::optional<zx_status_t> auto_start_ = ZX_OK;
  bool auto_stop_ = true;
  bool immediate_return_tx_ = false;
  bool immediate_return_rx_ = false;
  bool device_started_ __TA_GUARDED(lock_) = false;
  fit::function<void()> pending_start_callback_ __TA_GUARDED(lock_);
  fit::function<void()> pending_stop_callback_ __TA_GUARDED(lock_);
  PrepareVmoHandler prepare_vmo_handler_;
};

class FakeNetworkDeviceImpl : public ddk::NetworkDeviceImplProtocol<FakeNetworkDeviceImpl> {
 public:
  using PrepareVmoHandler =
      fit::function<void(uint8_t, const zx::vmo&, network_device_impl_prepare_vmo_callback, void*)>;
  FakeNetworkDeviceImpl();
  ~FakeNetworkDeviceImpl();

  zx::result<std::unique_ptr<NetworkDeviceInterface>> CreateChild(
      fdf::Dispatcher* impl_dispatcher, fdf::Dispatcher* ifc_dispatcher,
      fdf::Dispatcher* port_dispatcher, fdf::Dispatcher* shim_dispatcher,
      fdf::Dispatcher* shim_port_dispatcher);

  zx_status_t NetworkDeviceImplInit(const network_device_ifc_protocol_t* iface);
  void NetworkDeviceImplStart(network_device_impl_start_callback callback, void* cookie);
  void NetworkDeviceImplStop(network_device_impl_stop_callback callback, void* cookie);
  void NetworkDeviceImplGetInfo(device_impl_info_t* out_info);
  void NetworkDeviceImplQueueTx(const tx_buffer_t* buf_list, size_t buf_count);
  void NetworkDeviceImplQueueRxSpace(const rx_space_buffer_t* buf_list, size_t buf_count);
  void NetworkDeviceImplPrepareVmo(uint8_t vmo_id, zx::vmo vmo,
                                   network_device_impl_prepare_vmo_callback callback,
                                   void* cookie) {
    zx::vmo& slot = vmos_[vmo_id];
    EXPECT_FALSE(slot.is_valid()) << "vmo " << static_cast<uint32_t>(vmo_id) << " already prepared";
    slot = std::move(vmo);
    if (prepare_vmo_handler_) {
      prepare_vmo_handler_(vmo_id, slot, callback, cookie);
    } else {
      callback(cookie, ZX_OK);
    }
  }
  void NetworkDeviceImplReleaseVmo(uint8_t vmo_id) {
    zx::vmo& slot = vmos_[vmo_id];
    EXPECT_TRUE(slot.is_valid()) << "vmo " << static_cast<uint32_t>(vmo_id) << " already released";
    slot.reset();
  }
  void NetworkDeviceImplSetSnoop(bool snoop) { /* do nothing , only auto-snooping is allowed */
  }

  fit::function<zx::unowned_vmo(uint8_t)> VmoGetter();

  const zx::event& events() const { return event_; }

  device_impl_info_t& info() { return info_; }

  std::unique_ptr<RxBuffer> PopRxBuffer() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    return rx_buffers_.pop_front();
  }

  std::unique_ptr<TxBuffer> PopTxBuffer() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    return tx_buffers_.pop_front();
  }

  fbl::SizedDoublyLinkedList<std::unique_ptr<TxBuffer>> TakeTxBuffers() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    fbl::SizedDoublyLinkedList<std::unique_ptr<TxBuffer>> r;
    tx_buffers_.swap(r);
    return r;
  }

  fbl::SizedDoublyLinkedList<std::unique_ptr<RxBuffer>> TakeRxBuffers() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    fbl::SizedDoublyLinkedList<std::unique_ptr<RxBuffer>> r;
    rx_buffers_.swap(r);
    return r;
  }

  size_t rx_buffer_count() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    return rx_buffers_.size();
  }

  size_t tx_buffer_count() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    return tx_buffers_.size();
  }

  size_t queue_rx_space_called() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    const size_t front = queue_rx_space_called_.front();
    queue_rx_space_called_.pop_front();
    return front;
  }

  size_t queue_tx_called() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    const size_t front = queue_tx_called_.front();
    queue_tx_called_.pop_front();
    return front;
  }

  std::optional<uint8_t> first_vmo_id() {
    for (size_t i = 0; i < vmos_.size(); i++) {
      if (vmos_[i].is_valid()) {
        return i;
      }
    }
    return std::nullopt;
  }

  void set_auto_start(std::optional<zx_status_t> auto_start) { auto_start_ = auto_start; }

  void set_auto_stop(bool auto_stop) { auto_stop_ = auto_stop; }

  bool TriggerStart();
  bool TriggerStop();

  network_device_impl_protocol_t proto() {
    return network_device_impl_protocol_t{.ops = &network_device_impl_protocol_ops_, .ctx = this};
  }

  void set_immediate_return_tx(bool auto_return) { immediate_return_tx_ = auto_return; }
  void set_immediate_return_rx(bool auto_return) { immediate_return_rx_ = auto_return; }
  void set_prepare_vmo_handler(PrepareVmoHandler handler) {
    prepare_vmo_handler_ = std::move(handler);
  }

  ddk::NetworkDeviceIfcProtocolClient& client() { return device_client_; }

  cpp20::span<const zx::vmo> vmos() { return cpp20::span(vmos_.begin(), vmos_.end()); }

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(FakeNetworkDeviceImpl);

  fbl::Mutex lock_;
  std::array<zx::vmo, MAX_VMOS> vmos_;
  device_impl_info_t info_{};
  ddk::NetworkDeviceIfcProtocolClient device_client_;
  fbl::SizedDoublyLinkedList<std::unique_ptr<RxBuffer>> rx_buffers_ __TA_GUARDED(lock_);
  fbl::SizedDoublyLinkedList<std::unique_ptr<TxBuffer>> tx_buffers_ __TA_GUARDED(lock_);
  std::deque<size_t> queue_tx_called_ __TA_GUARDED(lock_);
  std::deque<size_t> queue_rx_space_called_ __TA_GUARDED(lock_);
  zx::event event_;
  std::optional<zx_status_t> auto_start_ = ZX_OK;
  bool auto_stop_ = true;
  bool immediate_return_tx_ = false;
  bool immediate_return_rx_ = false;
  bool device_started_ __TA_GUARDED(lock_) = false;
  fit::function<void()> pending_start_callback_ __TA_GUARDED(lock_);
  fit::function<void()> pending_stop_callback_ __TA_GUARDED(lock_);
  PrepareVmoHandler prepare_vmo_handler_;
};

class FakeFidlNetworkDeviceIfc : public fdf::WireServer<netdriver::NetworkDeviceIfc> {
 public:
  FakeFidlNetworkDeviceIfc() = default;
  zx::result<fdf::ClientEnd<netdriver::NetworkDeviceIfc>> Bind(fdf::Dispatcher* dispatcher);

  // NetworkDeviceIfc implementation.
  void PortStatusChanged(netdriver::wire::NetworkDeviceIfcPortStatusChangedRequest* request,
                         fdf::Arena& arena, PortStatusChangedCompleter::Sync& completer) override;
  void AddPort(netdriver::wire::NetworkDeviceIfcAddPortRequest* request, fdf::Arena& arena,
               AddPortCompleter::Sync& completer) override;
  void RemovePort(netdriver::wire::NetworkDeviceIfcRemovePortRequest* request, fdf::Arena& arena,
                  RemovePortCompleter::Sync& completer) override;
  void CompleteRx(netdriver::wire::NetworkDeviceIfcCompleteRxRequest* request, fdf::Arena& arena,
                  CompleteRxCompleter::Sync& completer) override;
  void CompleteTx(netdriver::wire::NetworkDeviceIfcCompleteTxRequest* request, fdf::Arena& arena,
                  CompleteTxCompleter::Sync& completer) override;
  void Snoop(netdriver::wire::NetworkDeviceIfcSnoopRequest* request, fdf::Arena& arena,
             SnoopCompleter::Sync& completer) override;

  // If assigned, these functions are called when the corresponding FIDL call is served.
  fit::function<void(netdriver::wire::NetworkDeviceIfcPortStatusChangedRequest*, fdf::Arena&,
                     PortStatusChangedCompleter::Sync&)>
      port_status_changed_;
  fit::function<void(netdriver::wire::NetworkDeviceIfcAddPortRequest*, fdf::Arena&,
                     AddPortCompleter::Sync&)>
      add_port_;
  fit::function<void(netdriver::wire::NetworkDeviceIfcRemovePortRequest*, fdf::Arena&,
                     RemovePortCompleter::Sync&)>
      remove_port_;
  fit::function<void(netdriver::wire::NetworkDeviceIfcCompleteRxRequest*, fdf::Arena&,
                     CompleteRxCompleter::Sync&)>
      complete_rx_;
  fit::function<void(netdriver::wire::NetworkDeviceIfcCompleteTxRequest*, fdf::Arena&,
                     CompleteTxCompleter::Sync&)>
      complete_tx_;
  fit::function<void(netdriver::wire::NetworkDeviceIfcSnoopRequest*, fdf::Arena&,
                     SnoopCompleter::Sync&)>
      snoop_;

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(FakeFidlNetworkDeviceIfc);
};

class RxFidlReturnTransaction {
 public:
  explicit RxFidlReturnTransaction(FakeFidlNetworkDeviceImpl* impl)
      : return_buffers_(impl->info().rx_depth), client_(impl->client()) {}

  void Enqueue(std::unique_ptr<RxFidlReturn> buffer) {
    ZX_ASSERT(count_ < std::size(return_buffers_));
    return_buffers_[count_++] = buffer->buffer();
    buffers_.push_back(std::move(buffer));
  }

  void Enqueue(std::unique_ptr<RxFidlBuffer> buffer, uint8_t port_id) {
    Enqueue(std::make_unique<RxFidlReturn>(std::move(buffer), port_id));
  }

  void Commit() {
    size_t remaining = count_;
    size_t offset = 0;
    while (remaining > 0) {
      const size_t batch = std::min<size_t>(remaining, MAX_RX_BUFFERS);
      fdf::Arena arena('NETD');
      auto results =
          fidl::VectorView<fuchsia_hardware_network_driver::wire::RxBuffer>::FromExternal(
              return_buffers_.data() + offset, batch);

      auto result = client_.buffer(arena)->CompleteRx(results);
      EXPECT_OK(result.status());

      offset += batch;
      remaining -= batch;
    }
    count_ = 0;
    buffers_.clear();
  }

 private:
  std::vector<netdriver::wire::RxBuffer> return_buffers_;
  size_t count_ = 0;
  fdf::WireSharedClient<fuchsia_hardware_network_driver::NetworkDeviceIfc>& client_;
  fbl::DoublyLinkedList<std::unique_ptr<RxFidlReturn>> buffers_;

  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(RxFidlReturnTransaction);
};

class RxReturnTransaction {
 public:
  explicit RxReturnTransaction(FakeNetworkDeviceImpl* impl)
      : return_buffers_(impl->info().rx_depth), client_(impl->client()) {}

  void Enqueue(std::unique_ptr<RxReturn> buffer) {
    return_buffers_[count_++] = buffer->buffer();
    buffers_.push_back(std::move(buffer));
  }

  void Enqueue(std::unique_ptr<RxBuffer> buffer, uint8_t port_id) {
    Enqueue(std::make_unique<RxReturn>(std::move(buffer), port_id));
  }

  void Commit() {
    client_.CompleteRx(return_buffers_.data(), count_);
    count_ = 0;
    buffers_.clear();
  }

 private:
  std::vector<rx_buffer_t> return_buffers_{};
  size_t count_ = 0;
  ddk::NetworkDeviceIfcProtocolClient client_;
  fbl::DoublyLinkedList<std::unique_ptr<RxReturn>> buffers_;

  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(RxReturnTransaction);
};

class TxFidlReturnTransaction {
 public:
  explicit TxFidlReturnTransaction(FakeFidlNetworkDeviceImpl* impl)
      : return_buffers_(impl->info().tx_depth), client_(impl->client()) {}

  void Enqueue(std::unique_ptr<TxFidlBuffer> buffer) {
    ZX_ASSERT(count_ < std::size(return_buffers_));
    return_buffers_[count_++] = buffer->result();
  }

  void Commit() {
    fdf::Arena arena('NETD');
    auto results = fidl::VectorView<fuchsia_hardware_network_driver::wire::TxResult>::FromExternal(
        return_buffers_.data(), count_);
    EXPECT_TRUE(client_.buffer(arena)->CompleteTx(results).ok());
    count_ = 0;
  }

 private:
  std::vector<netdriver::wire::TxResult> return_buffers_;
  size_t count_ = 0;
  fdf::WireSharedClient<fuchsia_hardware_network_driver::NetworkDeviceIfc>& client_;

  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(TxFidlReturnTransaction);
};

class TxReturnTransaction {
 public:
  explicit TxReturnTransaction(FakeNetworkDeviceImpl* impl)
      : return_buffers_(impl->info().tx_depth), client_(impl->client()) {
    return_buffers_.reserve(impl->info().rx_depth);
  }

  void Enqueue(std::unique_ptr<TxBuffer> buffer) { return_buffers_[count_++] = buffer->result(); }

  void Commit() {
    client_.CompleteTx(return_buffers_.data(), count_);
    count_ = 0;
  }

 private:
  std::vector<tx_result_t> return_buffers_{};
  size_t count_ = 0;
  ddk::NetworkDeviceIfcProtocolClient client_;

  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(TxReturnTransaction);
};

}  // namespace network::testing

#endif  // SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_TEST_UTIL_H_
