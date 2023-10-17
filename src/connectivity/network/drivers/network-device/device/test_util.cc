// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "test_util.h"

#include <iostream>

#include <gtest/gtest.h>

#include "network_device_shim.h"
#include "src/lib/testing/predicates/status.h"

namespace network::testing {

zx::result<std::vector<uint8_t>> TxFidlBuffer::GetData(const VmoProvider& vmo_provider) {
  if (!vmo_provider) {
    return zx::error(ZX_ERR_INTERNAL);
  }
  // We don't support copying chained buffers.
  if (buffer_.data.count() != 1) {
    return zx::error(ZX_ERR_INTERNAL);
  }
  const fuchsia_hardware_network_driver::wire::BufferRegion& region = buffer_.data.at(0);
  zx::unowned_vmo vmo = vmo_provider(region.vmo);
  if (!vmo->is_valid()) {
    return zx::error(ZX_ERR_INTERNAL);
  }
  std::vector<uint8_t> copy;
  copy.resize(region.length);
  zx_status_t status = vmo->read(copy.data(), region.offset, region.length);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(copy));
}

zx::result<std::vector<uint8_t>> TxBuffer::GetData(const VmoProvider& vmo_provider) const {
  if (!vmo_provider) {
    return zx::error(ZX_ERR_INTERNAL);
  }
  // We don't support copying chained buffers.
  if (buffer_.data_count != 1) {
    return zx::error(ZX_ERR_INTERNAL);
  }
  const buffer_region_t& region = buffer_.data_list[0];
  zx::unowned_vmo vmo = vmo_provider(region.vmo);
  if (!vmo->is_valid()) {
    return zx::error(ZX_ERR_INTERNAL);
  }
  std::vector<uint8_t> copy;
  copy.resize(region.length);
  zx_status_t status = vmo->read(copy.data(), region.offset, region.length);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(copy));
}

zx_status_t RxFidlBuffer::WriteData(cpp20::span<const uint8_t> data,
                                    const VmoProvider& vmo_provider) {
  if (!vmo_provider) {
    return ZX_ERR_INTERNAL;
  }
  if (data.size() > space_.region.length) {
    return ZX_ERR_INVALID_ARGS;
  }
  zx::unowned_vmo vmo = vmo_provider(space_.region.vmo);
  return_part_.length = static_cast<uint32_t>(data.size());
  return vmo->write(data.data(), space_.region.offset, data.size());
}

zx_status_t RxBuffer::WriteData(cpp20::span<const uint8_t> data, const VmoProvider& vmo_provider) {
  if (!vmo_provider) {
    return ZX_ERR_INTERNAL;
  }
  if (data.size() > space_.region.length) {
    return ZX_ERR_INVALID_ARGS;
  }
  zx::unowned_vmo vmo = vmo_provider(space_.region.vmo);
  return_part_.length = static_cast<uint32_t>(data.size());
  return vmo->write(data.data(), space_.region.offset, data.size());
}

FakeFidlNetworkPortImpl::FakeFidlNetworkPortImpl()
    : port_info_({
          .port_class = netdev::wire::DeviceClass::kEthernet,
          .rx_types = {netdev::wire::FrameType::kEthernet},
          .tx_types = {{.type = netdev::wire::FrameType::kEthernet,
                        .features = netdev::wire::kFrameFeaturesRaw,
                        .supported_flags = netdev::wire::TxFlags(0)}},

      }) {
  EXPECT_OK(zx::event::create(0, &event_));
}

FakeFidlNetworkPortImpl::~FakeFidlNetworkPortImpl() {
  if (port_added_) {
    EXPECT_TRUE(port_removed_) << "port was added but remove was not called";
  }
}

void FakeFidlNetworkPortImpl::WaitPortRemoved() {
  if (port_added_) {
    WaitForPortRemoval();
    ASSERT_TRUE(port_removed_) << "port was added but remove was not called";
  }
}

void FakeFidlNetworkPortImpl::GetInfo(fdf::Arena& arena, GetInfoCompleter::Sync& completer) {
  fidl::Arena fidl_arena;
  auto builder = fuchsia_hardware_network::wire::PortBaseInfo::Builder(fidl_arena);
  auto rx_types = fidl::VectorView<netdev::wire::FrameType>::FromExternal(port_info_.rx_types);
  auto tx_types =
      fidl::VectorView<netdev::wire::FrameTypeSupport>::FromExternal(port_info_.tx_types);

  builder.port_class(port_info_.port_class)
      .tx_types(fidl::ObjectView<decltype(tx_types)>::FromExternal(&tx_types))
      .rx_types(fidl::ObjectView<decltype(rx_types)>::FromExternal(&rx_types));

  completer.buffer(arena).Reply(builder.Build());
}

void FakeFidlNetworkPortImpl::GetStatus(fdf::Arena& arena, GetStatusCompleter::Sync& completer) {
  fidl::Arena fidl_arena;
  auto builder = fuchsia_hardware_network::wire::PortStatus::Builder(fidl_arena);
  builder.mtu(status_.mtu).flags(status_.flags);
  completer.buffer(arena).Reply(builder.Build());
}

void FakeFidlNetworkPortImpl::SetActive(
    fuchsia_hardware_network_driver::wire::NetworkPortSetActiveRequest* request, fdf::Arena& arena,
    SetActiveCompleter::Sync& completer) {
  port_active_ = request->active;
  if (on_set_active_) {
    on_set_active_(request->active);
  }
  ASSERT_OK(event_.signal(0, kEventPortActiveChanged));
}

void FakeFidlNetworkPortImpl::Removed(fdf::Arena& arena, RemovedCompleter::Sync& completer) {
  ASSERT_FALSE(port_removed_) << "removed same port twice";
  port_removed_ = true;
  sync_completion_signal(&wait_removed_);
}

void FakeFidlNetworkPortImpl::GetMac(fdf::Arena& arena, GetMacCompleter::Sync& completer) {
  fdf::ClientEnd<fuchsia_hardware_network_driver::MacAddr> client{};
  if (mac_client_end_.has_value()) {
    client = std::move(*mac_client_end_);
    mac_client_end_ = {};
  }
  completer.buffer(arena).Reply(std::move(client));
}

zx_status_t FakeFidlNetworkPortImpl::AddPort(uint8_t port_id, const fdf::Dispatcher& dispatcher,
                                             fidl::WireSyncClient<netdev::Device> device,
                                             FakeFidlNetworkDeviceImpl& parent) {
  if (port_added_) {
    return ZX_ERR_ALREADY_EXISTS;
  }
  id_ = port_id;
  parent_ = &parent;

  zx::result fidl_endpoints = fidl::CreateEndpoints<fuchsia_hardware_network::PortWatcher>();
  if (fidl_endpoints.is_error()) {
    return fidl_endpoints.status_value();
  }
  auto status = device->GetPortWatcher(std::move(fidl_endpoints->server));
  if (!status.ok()) {
    return status.status();
  }
  fidl::WireSyncClient port_watcher(std::move(fidl_endpoints->client));

  bool found_idle = false;
  while (!found_idle) {
    auto result = port_watcher->Watch();
    if (!result.ok()) {
      return result.status();
    }
    found_idle = result->event.Which() == netdev::wire::DevicePortEvent::Tag::kIdle;
  }

  auto endpoints = fdf::CreateEndpoints<fuchsia_hardware_network_driver::NetworkPort>();
  if (endpoints.is_error()) {
    return endpoints.status_value();
  }

  binding_ = fdf::BindServer(dispatcher.get(), std::move(endpoints->server), this);

  fdf::Arena arena('NETD');
  auto add_port_status =
      parent.client().sync().buffer(arena)->AddPort(port_id, std::move(endpoints->client));
  if (!add_port_status.ok() || add_port_status->status != ZX_OK) {
    return add_port_status.ok() ? add_port_status->status : add_port_status.status();
  }

  auto result = port_watcher->Watch();
  if (!result.ok()) {
    return result.status();
  }

  if (result->event.Which() != netdev::wire::DevicePortEvent::Tag::kAdded) {
    return ZX_ERR_BAD_STATE;
  }

  port_added_ = true;
  device_ = std::move(device);
  return ZX_OK;
}

zx_status_t FakeFidlNetworkPortImpl::AddPortNoWait(uint8_t port_id,
                                                   const fdf::Dispatcher& dispatcher,
                                                   fidl::WireSyncClient<netdev::Device> device,
                                                   FakeFidlNetworkDeviceImpl& parent) {
  if (port_added_) {
    return ZX_ERR_ALREADY_EXISTS;
  }
  id_ = port_id;
  parent_ = &parent;

  auto endpoints = fdf::CreateEndpoints<fuchsia_hardware_network_driver::NetworkPort>();
  if (endpoints.is_error()) {
    return endpoints.status_value();
  }

  binding_ = fdf::BindServer(
      dispatcher.get(), std::move(endpoints->server), this,
      [](fdf::WireServer<fuchsia_hardware_network_driver::NetworkPort>*, fidl::UnbindInfo foo,
         fdf::ServerEnd<fuchsia_hardware_network_driver::NetworkPort> /*unused*/) {});

  fdf::Arena arena('NETD');
  auto status =
      parent.client().sync().buffer(arena)->AddPort(port_id, std::move(endpoints->client));
  if (!status.ok() || status->status != ZX_OK) {
    return status.ok() ? status->status : status.status();
  }

  port_added_ = true;
  device_ = std::move(device);
  return ZX_OK;
}

void FakeFidlNetworkPortImpl::RemoveSync() {
  // Already removed.
  if (!port_added_ || port_removed_) {
    return;
  }
  fdf::Arena arena('NETD');
  EXPECT_TRUE(parent_->client().buffer(arena)->RemovePort(id_).ok());
  WaitForPortRemoval();
}

void FakeFidlNetworkPortImpl::SetOnline(bool online) {
  PortStatus status = status_;
  status.flags = online ? netdev::wire::StatusFlags::kOnline : netdev::wire::StatusFlags();
  SetStatus(status);
}

void FakeFidlNetworkPortImpl::SetStatus(const PortStatus& status) {
  status_ = status;
  if (parent_ != nullptr && parent_->client().is_valid()) {
    fidl::Arena fidl_arena;
    auto builder = fuchsia_hardware_network::wire::PortStatus::Builder(fidl_arena);
    builder.mtu(status_.mtu).flags(status_.flags);
    fdf::Arena arena('NETD');
    EXPECT_TRUE(parent_->client().buffer(arena)->PortStatusChanged(id_, builder.Build()).ok());
  }
}

FakeNetworkPortImpl::FakeNetworkPortImpl()
    : port_info_({
          .port_class = static_cast<uint8_t>(netdev::wire::DeviceClass::kEthernet),
          .rx_types_list = rx_types_.data(),
          .rx_types_count = 1,
          .tx_types_list = tx_types_.data(),
          .tx_types_count = 1,
      }) {
  rx_types_[0] = static_cast<uint8_t>(netdev::wire::FrameType::kEthernet);
  tx_types_[0] = {
      .type = static_cast<uint8_t>(netdev::wire::FrameType::kEthernet),
      .features = netdev::wire::kFrameFeaturesRaw,
      .supported_flags = 0,
  };
  EXPECT_OK(zx::event::create(0, &event_));
}

FakeNetworkPortImpl::~FakeNetworkPortImpl() {
  if (port_added_) {
    EXPECT_TRUE(port_removed_) << "port was added but remove was not called";
  }
}

void FakeNetworkPortImpl::NetworkPortGetInfo(port_base_info_t* out_info) { *out_info = port_info_; }

void FakeNetworkPortImpl::NetworkPortGetStatus(port_status_t* out_status) { *out_status = status_; }

void FakeNetworkPortImpl::NetworkPortSetActive(bool active) {
  port_active_ = active;
  if (on_set_active_) {
    on_set_active_(active);
  }
  ASSERT_OK(event_.signal(0, kEventPortActiveChanged));
}

void FakeNetworkPortImpl::NetworkPortGetMac(mac_addr_protocol_t** out_mac_ifc) {
  if (out_mac_ifc) {
    *out_mac_ifc = &mac_proto_;
  }
}

void FakeNetworkPortImpl::NetworkPortRemoved() {
  EXPECT_FALSE(port_removed_) << "removed same port twice";
  port_removed_ = true;
  if (on_removed_) {
    on_removed_();
  }
}

zx_status_t FakeNetworkPortImpl::AddPort(uint8_t port_id,
                                         ddk::NetworkDeviceIfcProtocolClient ifc_client) {
  if (port_added_) {
    return ZX_ERR_ALREADY_EXISTS;
  }
  zx_status_t status = ifc_client.AddPort(port_id, this, &network_port_protocol_ops_);
  if (status != ZX_OK) {
    return status;
  }
  id_ = port_id;
  port_added_ = true;
  device_client_ = ifc_client;
  return ZX_OK;
}

void FakeNetworkPortImpl::RemoveSync() {
  // Already removed.
  if (!port_added_ || port_removed_) {
    return;
  }
  sync_completion_t signal;
  on_removed_ = [&signal]() { sync_completion_signal(&signal); };
  device_client_.RemovePort(id_);
  sync_completion_wait(&signal, zx::time::infinite().get());
}

void FakeNetworkPortImpl::SetOnline(bool online) {
  port_status_t status = status_;
  status.flags = static_cast<uint32_t>(online ? netdev::wire::StatusFlags::kOnline
                                              : netdev::wire::StatusFlags());
  SetStatus(status);
}

void FakeNetworkPortImpl::SetStatus(const port_status_t& status) {
  status_ = status;
  if (device_client_.is_valid()) {
    device_client_.PortStatusChanged(id_, &status);
  }
}

FakeFidlNetworkDeviceImpl::FakeFidlNetworkDeviceImpl()
    : info_({
          .tx_depth = kDefaultTxDepth,
          .rx_depth = kDefaultRxDepth,
          .rx_threshold = kDefaultRxDepth / 2,
          .max_buffer_length = ZX_PAGE_SIZE / 2,
          .buffer_alignment = ZX_PAGE_SIZE,
      }) {
  EXPECT_OK(zx::event::create(0, &event_));
}

FakeFidlNetworkDeviceImpl::~FakeFidlNetworkDeviceImpl() {
  // ensure that all VMOs were released
  for (auto& vmo : vmos_) {
    ZX_ASSERT(!vmo.is_valid());
  }
}

void FakeFidlNetworkDeviceImpl::Init(
    fuchsia_hardware_network_driver::wire::NetworkDeviceImplInitRequest* request, fdf::Arena& arena,
    InitCompleter::Sync& completer) {
  device_client_ = fdf::WireSharedClient(std::move(request->iface), dispatcher_->get());
  completer.buffer(arena).Reply(ZX_OK);
}

void FakeFidlNetworkDeviceImpl::Start(fdf::Arena& arena, StartCompleter::Sync& completer) {
  fbl::AutoLock lock(&lock_);
  EXPECT_FALSE(device_started_) << "called start on already started device";
  if (auto_start_.has_value()) {
    const zx_status_t auto_start = auto_start_.value();
    if (auto_start == ZX_OK) {
      device_started_ = true;
    }
    completer.buffer(arena).Reply(auto_start);
  } else {
    ZX_ASSERT(!(pending_start_callback_ || pending_stop_callback_));
    pending_start_callback_ = [completer = completer.ToAsync(), this]() mutable {
      {
        fbl::AutoLock lock(&lock_);
        device_started_ = true;
      }
      fdf::Arena arena('NETD');
      completer.buffer(arena).Reply(ZX_OK);
    };
  }
  EXPECT_OK(event_.signal(0, kEventStart));
}

void FakeFidlNetworkDeviceImpl::Stop(fdf::Arena& arena, StopCompleter::Sync& completer) {
  fbl::AutoLock lock(&lock_);
  EXPECT_TRUE(device_started_) << "called stop on already stopped device";
  device_started_ = false;
  zx_signals_t clear;
  if (auto_stop_) {
    RxFidlReturnTransaction rx_return(this);
    while (!rx_buffers_.is_empty()) {
      std::unique_ptr rx_buffer = rx_buffers_.pop_front();
      // Return unfulfilled buffers with zero length and an invalid port number.
      // Zero length buffers are returned to the pool and the port metadata is ignored.
      rx_buffer->return_part().length = 0;
      rx_return.Enqueue(std::move(rx_buffer), MAX_PORTS);
    }
    rx_return.Commit();

    TxFidlReturnTransaction tx_return(this);
    while (!tx_buffers_.is_empty()) {
      std::unique_ptr tx_buffer = tx_buffers_.pop_front();
      tx_buffer->set_status(ZX_ERR_UNAVAILABLE);
      tx_return.Enqueue(std::move(tx_buffer));
    }
    tx_return.Commit();
    fdf::Arena arena('NETD');
    completer.buffer(arena).Reply();
    //  Must clear the queue signals if we're clearing the queues automatically.
    clear = kEventTx | kEventRxAvailable;
  } else {
    ZX_ASSERT(!(pending_start_callback_ || pending_stop_callback_));
    pending_stop_callback_ = [completer = completer.ToAsync()]() mutable {
      fdf::Arena arena('NETD');
      completer.buffer(arena).Reply();
    };
    clear = 0;
  }
  EXPECT_OK(event_.signal(clear, kEventStop));
}

void FakeFidlNetworkDeviceImpl::GetInfo(fdf::Arena& arena, GetInfoCompleter::Sync& completer) {
  fidl::Arena fidl_arena;
  auto builder = fuchsia_hardware_network_driver::wire::DeviceImplInfo::Builder(fidl_arena);

  auto tx_accel = fidl::VectorView<netdev::wire::TxAcceleration>::FromExternal(info_.tx_accel);
  auto rx_accel = fidl::VectorView<netdev::wire::RxAcceleration>::FromExternal(info_.rx_accel);

  builder.device_features(info_.device_features)
      .tx_depth(info_.tx_depth)
      .rx_depth(info_.rx_depth)
      .rx_threshold(info_.rx_threshold)
      .max_buffer_parts(info_.max_buffer_parts)
      .max_buffer_length(info_.max_buffer_length)
      .buffer_alignment(info_.buffer_alignment)
      .buffer_alignment(info_.buffer_alignment)
      .min_rx_buffer_length(info_.min_rx_buffer_length)
      .min_tx_buffer_length(info_.min_tx_buffer_length)
      .tx_head_length(info_.tx_head_length)
      .tx_tail_length(info_.tx_tail_length)
      .tx_accel(fidl::ObjectView<decltype(tx_accel)>::FromExternal(&tx_accel))
      .rx_accel(fidl::ObjectView<decltype(rx_accel)>::FromExternal(&rx_accel));

  completer.buffer(arena).Reply(builder.Build());
}

void FakeFidlNetworkDeviceImpl::QueueTx(
    fuchsia_hardware_network_driver::wire::NetworkDeviceImplQueueTxRequest* request,
    fdf::Arena& arena, QueueTxCompleter::Sync& completer) {
  EXPECT_NE(request->buffers.count(), 0u);
  ASSERT_TRUE(device_client_.is_valid());

  fbl::AutoLock lock(&lock_);
  cpp20::span buffers = request->buffers.get();
  queue_tx_called_.push_back(buffers.size());
  if (immediate_return_tx_ || !device_started_) {
    const zx_status_t return_status = device_started_ ? ZX_OK : ZX_ERR_UNAVAILABLE;
    ASSERT_LE(request->buffers.count(), kDefaultTxDepth);
    std::array<fuchsia_hardware_network_driver::wire::TxResult, kDefaultTxDepth> results;
    auto results_iter = results.begin();
    for (const fuchsia_hardware_network_driver::wire::TxBuffer& buff : buffers) {
      *results_iter++ = {
          .id = buff.id,
          .status = return_status,
      };
    }
    auto output = fidl::VectorView<fuchsia_hardware_network_driver::wire::TxResult>::FromExternal(
        results.data(), request->buffers.count());
    EXPECT_TRUE(device_client_.buffer(arena)->CompleteTx(output).ok());
    return;
  }

  for (const fuchsia_hardware_network_driver::wire::TxBuffer& buff : buffers) {
    auto back = std::make_unique<TxFidlBuffer>(buff);
    tx_buffers_.push_back(std::move(back));
  }
  EXPECT_OK(event_.signal(0, kEventTx));
}

void FakeFidlNetworkDeviceImpl::QueueRxSpace(
    fuchsia_hardware_network_driver::wire::NetworkDeviceImplQueueRxSpaceRequest* request,
    fdf::Arena& arena, QueueRxSpaceCompleter::Sync& completer) {
  ASSERT_TRUE(device_client_.is_valid());
  size_t buf_count = request->buffers.count();

  fbl::AutoLock lock(&lock_);
  queue_rx_space_called_.push_back(buf_count);
  auto buffers = request->buffers.get();
  if (immediate_return_rx_ || !device_started_) {
    const uint32_t length = device_started_ ? kAutoReturnRxLength : 0;
    ASSERT_TRUE(buf_count < kDefaultTxDepth);
    std::array<fuchsia_hardware_network_driver::wire::RxBuffer, kDefaultTxDepth> results;
    std::array<fuchsia_hardware_network_driver::wire::RxBufferPart, kDefaultTxDepth> parts;
    auto results_iter = results.begin();
    auto parts_iter = parts.begin();
    for (const fuchsia_hardware_network_driver::wire::RxSpaceBuffer& space : buffers) {
      fuchsia_hardware_network_driver::wire::RxBufferPart& part = *parts_iter++;
      fuchsia_hardware_network_driver::wire::RxBuffer& rx_buffer = *results_iter++;
      part = {
          .id = space.id,
          .length = length,
      };
      rx_buffer = {
          .meta =
              {
                  .info = netdriver::wire::FrameInfo::WithNoInfo(netdriver::wire::NoInfo{
                      static_cast<uint8_t>(netdev::wire::InfoType::kNoInfo)}),
                  .frame_type = fuchsia_hardware_network::wire::FrameType::kEthernet,
              },
          .data =
              fidl::VectorView<fuchsia_hardware_network_driver::wire::RxBufferPart>::FromExternal(
                  &part, 1),
      };
    }
    auto output = fidl::VectorView<fuchsia_hardware_network_driver::wire::RxBuffer>::FromExternal(
        results.data(), buf_count);
    EXPECT_TRUE(device_client_.buffer(arena)->CompleteRx(output).ok());
    return;
  }

  for (const fuchsia_hardware_network_driver::wire::RxSpaceBuffer& buff : buffers) {
    auto back = std::make_unique<RxFidlBuffer>(buff);
    rx_buffers_.push_back(std::move(back));
  }
  EXPECT_OK(event_.signal(0, kEventRxAvailable));
}

void FakeFidlNetworkDeviceImpl::PrepareVmo(
    fuchsia_hardware_network_driver::wire::NetworkDeviceImplPrepareVmoRequest* request,
    fdf::Arena& arena, PrepareVmoCompleter::Sync& completer) {
  zx::vmo& slot = vmos_[request->id];
  EXPECT_FALSE(slot.is_valid()) << "vmo " << static_cast<uint32_t>(request->id)
                                << " already prepared";
  slot = std::move(request->vmo);
  if (prepare_vmo_handler_) {
    prepare_vmo_handler_(request->id, slot, completer);
  } else {
    completer.buffer(arena).Reply(ZX_OK);
  }
}
void FakeFidlNetworkDeviceImpl::ReleaseVmo(
    fuchsia_hardware_network_driver::wire::NetworkDeviceImplReleaseVmoRequest* request,
    fdf::Arena& arena, ReleaseVmoCompleter::Sync& completer) {
  zx::vmo& slot = vmos_[request->id];
  EXPECT_TRUE(slot.is_valid()) << "vmo " << static_cast<uint32_t>(request->id)
                               << " already released";
  slot.reset();

  bool all_released = true;
  for (auto& vmo : vmos_) {
    if (vmo.is_valid()) {
      all_released = false;
    }
  }

  if (all_released) {
    sync_completion_signal(&released_completer_);
  }
  completer.buffer(arena).Reply();
}

void FakeFidlNetworkDeviceImpl::SetSnoop(
    fuchsia_hardware_network_driver::wire::NetworkDeviceImplSetSnoopRequest* request,
    fdf::Arena& arena, SetSnoopCompleter::Sync& completer) {
  // Do nothing , only auto-snooping is allowed.
}

fit::function<zx::unowned_vmo(uint8_t)> FakeFidlNetworkDeviceImpl::VmoGetter() {
  return [this](uint8_t id) { return zx::unowned_vmo(vmos_[id]); };
}

bool FakeFidlNetworkDeviceImpl::TriggerStart() {
  fbl::AutoLock lock(&lock_);
  auto cb = std::move(pending_start_callback_);
  lock.release();

  if (cb) {
    cb();
    return true;
  }
  return false;
}

bool FakeFidlNetworkDeviceImpl::TriggerStop() {
  fbl::AutoLock lock(&lock_);
  auto cb = std::move(pending_stop_callback_);
  lock.release();

  if (cb) {
    cb();
    return true;
  }
  return false;
}

zx::result<fdf::ClientEnd<fuchsia_hardware_network_driver::NetworkDeviceImpl>>
FakeFidlNetworkDeviceImpl::Factory::Bind() {
  auto endpoints = fdf::CreateEndpoints<
      fuchsia_hardware_network_driver::Service::NetworkDeviceImpl::ProtocolType>();
  if (endpoints.is_error()) {
    return endpoints.take_error();
  }
  binding_ = fdf::BindServer(dispatcher_->get(), std::move(endpoints->server), parent_);
  return zx::ok(std::move(endpoints->client));
}

zx::result<std::unique_ptr<NetworkDeviceInterface>> FakeFidlNetworkDeviceImpl::CreateChild(
    fdf::Dispatcher* impl_dispatcher, fdf::Dispatcher* ifc_dispatcher,
    fdf::Dispatcher* port_dispatcher) {
  dispatcher_ = impl_dispatcher;
  auto factory = std::make_unique<Factory>(this, impl_dispatcher);
  zx::result device = internal::DeviceInterface::Create(
      DeviceInterfaceDispatchers{impl_dispatcher, ifc_dispatcher, port_dispatcher},
      std::move(factory));

  if (device.is_error()) {
    return device.take_error();
  }

  auto& value = device.value();
  value->evt_session_started_ = [this](const char* session) {
    event_.signal(0, kEventSessionStarted);
  };
  return zx::ok(std::move(value));
}

FakeNetworkDeviceImpl::FakeNetworkDeviceImpl()
    : info_({
          .tx_depth = kDefaultTxDepth,
          .rx_depth = kDefaultRxDepth,
          .rx_threshold = kDefaultRxDepth / 2,
          .max_buffer_length = ZX_PAGE_SIZE / 2,
          .buffer_alignment = ZX_PAGE_SIZE,
      }) {
  EXPECT_OK(zx::event::create(0, &event_));
}

FakeNetworkDeviceImpl::~FakeNetworkDeviceImpl() {
  // ensure that all VMOs were released
  for (auto& vmo : vmos_) {
    ZX_ASSERT(!vmo.is_valid());
  }
}

zx_status_t FakeNetworkDeviceImpl::NetworkDeviceImplInit(
    const network_device_ifc_protocol_t* iface) {
  device_client_ = ddk::NetworkDeviceIfcProtocolClient(iface);
  return ZX_OK;
}

void FakeNetworkDeviceImpl::NetworkDeviceImplStart(network_device_impl_start_callback callback,
                                                   void* cookie) {
  fbl::AutoLock lock(&lock_);
  EXPECT_FALSE(device_started_) << "called start on already started device";
  if (auto_start_.has_value()) {
    const zx_status_t auto_start = auto_start_.value();
    if (auto_start == ZX_OK) {
      device_started_ = true;
    }
    callback(cookie, auto_start);
  } else {
    ZX_ASSERT(!(pending_start_callback_ || pending_stop_callback_));
    pending_start_callback_ = [cookie, callback, this]() {
      {
        fbl::AutoLock lock(&lock_);
        device_started_ = true;
      }
      callback(cookie, ZX_OK);
    };
  }
  EXPECT_OK(event_.signal(0, kEventStart));
}

void FakeNetworkDeviceImpl::NetworkDeviceImplStop(network_device_impl_stop_callback callback,
                                                  void* cookie) {
  fbl::AutoLock lock(&lock_);
  EXPECT_TRUE(device_started_) << "called stop on already stopped device";
  device_started_ = false;
  zx_signals_t clear;
  if (auto_stop_) {
    RxReturnTransaction rx_return(this);
    while (!rx_buffers_.is_empty()) {
      std::unique_ptr rx_buffer = rx_buffers_.pop_front();
      // Return unfulfilled buffers with zero length and an invalid port number.
      // Zero length buffers are returned to the pool and the port metadata is ignored.
      rx_buffer->return_part().length = 0;
      rx_return.Enqueue(std::move(rx_buffer), MAX_PORTS);
    }
    rx_return.Commit();

    TxReturnTransaction tx_return(this);
    while (!tx_buffers_.is_empty()) {
      std::unique_ptr tx_buffer = tx_buffers_.pop_front();
      tx_buffer->set_status(ZX_ERR_UNAVAILABLE);
      tx_return.Enqueue(std::move(tx_buffer));
    }
    tx_return.Commit();
    callback(cookie);
    // Must clear the queue signals if we're clearing the queues automatically.
    clear = kEventTx | kEventRxAvailable;
  } else {
    ZX_ASSERT(!(pending_start_callback_ || pending_stop_callback_));
    pending_stop_callback_ = [cookie, callback]() { callback(cookie); };
    clear = 0;
  }
  EXPECT_OK(event_.signal(clear, kEventStop));
}

void FakeNetworkDeviceImpl::NetworkDeviceImplGetInfo(device_impl_info_t* out_info) {
  *out_info = info_;
}

void FakeNetworkDeviceImpl::NetworkDeviceImplQueueTx(const tx_buffer_t* buf_list,
                                                     size_t buf_count) {
  EXPECT_NE(buf_count, 0u);
  ASSERT_TRUE(device_client_.is_valid());

  fbl::AutoLock lock(&lock_);
  queue_tx_called_.push_back(buf_count);
  cpp20::span buffers(buf_list, buf_count);
  if (immediate_return_tx_ || !device_started_) {
    const zx_status_t return_status = device_started_ ? ZX_OK : ZX_ERR_UNAVAILABLE;
    ASSERT_LE(buf_count, info_.tx_depth);
    std::vector<tx_result_t> results(info_.tx_depth);
    auto results_iter = results.begin();
    for (const tx_buffer_t& buff : buffers) {
      *results_iter++ = {
          .id = buff.id,
          .status = return_status,
      };
    }
    device_client_.CompleteTx(results.data(), buf_count);
    return;
  }

  for (const tx_buffer_t& buff : buffers) {
    auto back = std::make_unique<TxBuffer>(buff);
    tx_buffers_.push_back(std::move(back));
  }
  EXPECT_OK(event_.signal(0, kEventTx));
}

void FakeNetworkDeviceImpl::NetworkDeviceImplQueueRxSpace(const rx_space_buffer_t* buf_list,
                                                          size_t buf_count) {
  ASSERT_TRUE(device_client_.is_valid());

  fbl::AutoLock lock(&lock_);
  queue_rx_space_called_.push_back(buf_count);
  cpp20::span buffers(buf_list, buf_count);
  if (immediate_return_rx_ || !device_started_) {
    const uint32_t length = device_started_ ? kAutoReturnRxLength : 0;
    ASSERT_TRUE(buf_count < info_.rx_depth);
    std::vector<rx_buffer_t> results(info_.rx_depth);
    std::vector<rx_buffer_part_t> parts(info_.rx_depth);
    auto results_iter = results.begin();
    auto parts_iter = parts.begin();
    for (const rx_space_buffer_t& space : buffers) {
      rx_buffer_part_t& part = *parts_iter++;
      rx_buffer_t& rx_buffer = *results_iter++;
      part = {
          .id = space.id,
          .length = length,
      };
      rx_buffer = {
          .meta =
              {
                  .frame_type =
                      static_cast<uint8_t>(fuchsia_hardware_network::wire::FrameType::kEthernet),
              },
          .data_list = &part,
          .data_count = 1,
      };
    }
    device_client_.CompleteRx(results.data(), buf_count);
    return;
  }

  for (const rx_space_buffer_t& buff : buffers) {
    auto back = std::make_unique<RxBuffer>(buff);
    rx_buffers_.push_back(std::move(back));
  }
  EXPECT_OK(event_.signal(0, kEventRxAvailable));
}

fit::function<zx::unowned_vmo(uint8_t)> FakeNetworkDeviceImpl::VmoGetter() {
  return [this](uint8_t id) { return zx::unowned_vmo(vmos_[id]); };
}

bool FakeNetworkDeviceImpl::TriggerStart() {
  fbl::AutoLock lock(&lock_);
  auto cb = std::move(pending_start_callback_);
  lock.release();

  if (cb) {
    cb();
    return true;
  }
  return false;
}

bool FakeNetworkDeviceImpl::TriggerStop() {
  fbl::AutoLock lock(&lock_);
  auto cb = std::move(pending_stop_callback_);
  lock.release();

  if (cb) {
    cb();
    return true;
  }
  return false;
}

zx::result<std::unique_ptr<NetworkDeviceInterface>> FakeNetworkDeviceImpl::CreateChild(
    fdf::Dispatcher* impl_dispatcher, fdf::Dispatcher* ifc_dispatcher,
    fdf::Dispatcher* port_dispatcher, fdf::Dispatcher* shim_dispatcher,
    fdf::Dispatcher* shim_port_dispatcher) {
  network_device_impl_protocol_t protocol = proto();
  std::unique_ptr shim =
      std::make_unique<NetworkDeviceShim>(ddk::NetworkDeviceImplProtocolClient(&protocol),
                                          ShimDispatchers{shim_dispatcher, shim_port_dispatcher});
  zx::result device = internal::DeviceInterface::Create(
      DeviceInterfaceDispatchers{impl_dispatcher, ifc_dispatcher, port_dispatcher},
      std::move(shim));

  if (device.is_error()) {
    return device.take_error();
  }

  auto& value = device.value();
  value->evt_session_started_ = [this](const char* session) {
    event_.signal(0, kEventSessionStarted);
  };
  return zx::ok(std::move(value));
}

zx::result<fdf::ClientEnd<netdriver::NetworkDeviceIfc>> FakeFidlNetworkDeviceIfc::Bind(
    fdf::Dispatcher* dispatcher) {
  auto endpoints = fdf::CreateEndpoints<netdriver::NetworkDeviceIfc>();
  if (endpoints.is_error()) {
    return endpoints.take_error();
  }

  fdf::BindServer(dispatcher->get(), std::move(endpoints->server), this);

  return zx::ok(std::move(endpoints->client));
}

void FakeFidlNetworkDeviceIfc::PortStatusChanged(
    netdriver::wire::NetworkDeviceIfcPortStatusChangedRequest* request, fdf::Arena& arena,
    PortStatusChangedCompleter::Sync& completer) {
  if (port_status_changed_) {
    port_status_changed_(request, arena, completer);
  }
}

void FakeFidlNetworkDeviceIfc::AddPort(netdriver::wire::NetworkDeviceIfcAddPortRequest* request,
                                       fdf::Arena& arena, AddPortCompleter::Sync& completer) {
  if (add_port_) {
    add_port_(request, arena, completer);
  }
}

void FakeFidlNetworkDeviceIfc::RemovePort(
    netdriver::wire::NetworkDeviceIfcRemovePortRequest* request, fdf::Arena& arena,
    RemovePortCompleter::Sync& completer) {
  if (remove_port_) {
    remove_port_(request, arena, completer);
  }
}

void FakeFidlNetworkDeviceIfc::CompleteRx(
    netdriver::wire::NetworkDeviceIfcCompleteRxRequest* request, fdf::Arena& arena,
    CompleteRxCompleter::Sync& completer) {
  if (complete_rx_) {
    complete_rx_(request, arena, completer);
  }
}

void FakeFidlNetworkDeviceIfc::CompleteTx(
    netdriver::wire::NetworkDeviceIfcCompleteTxRequest* request, fdf::Arena& arena,
    CompleteTxCompleter::Sync& completer) {
  if (complete_tx_) {
    complete_tx_(request, arena, completer);
  }
}

void FakeFidlNetworkDeviceIfc::Snoop(netdriver::wire::NetworkDeviceIfcSnoopRequest* request,
                                     fdf::Arena& arena, SnoopCompleter::Sync& completer) {
  if (snoop_) {
    snoop_(request, arena, completer);
  }
}

}  // namespace network::testing
