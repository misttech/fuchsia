// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "netdevice_migration.h"

#include <fidl/fuchsia.hardware.network.driver/cpp/fidl.h>
#include <fidl/fuchsia.hardware.network/cpp/fidl.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/fit/defer.h>
#include <lib/zircon-internal/align.h>
#include <zircon/system/public/zircon/assert.h>

#include <algorithm>
#include <cinttypes>

#include <fbl/alloc_checker.h>

namespace {

fuchsia_hardware_network::wire::StatusFlags ToStatusFlags(uint32_t ethernet_status) {
  fuchsia_hardware_network::wire::StatusFlags flags;
  if (ethernet_status & ETHERNET_STATUS_ONLINE) {
    flags |= fuchsia_hardware_network::wire::StatusFlags::kOnline;
  }
  return flags;
}

}  // namespace

namespace netdevice_migration {

NetdeviceMigration::NetdeviceMigration(fdf::DriverStartArgs start_args,
                                       fdf::UnownedSynchronizedDispatcher driver_dispatcher)
    : fdf::DriverBase("netdevice-migration", std::move(start_args), std::move(driver_dispatcher)) {}

zx::result<> NetdeviceMigration::Start() {
  auto cleanup = fit::defer([this] { Shutdown(); });

  zx::result ethernet = compat::ConnectBanjo<ddk::EthernetImplProtocolClient>(incoming());
  if (ethernet.is_error()) {
    FDF_LOG(ERROR, "Failed to connect to Ethernet Impl protocol: %s", ethernet.status_string());
    return ethernet.take_error();
  }
  ethernet_ = ethernet.value();
  if (!ethernet_.is_valid()) {
    FDF_LOG(ERROR, "Received invalid ethernet impl client");
    return zx::error(ZX_ERR_INTERNAL);
  }

  vmo_store::Options opts = {
      .map =
          vmo_store::MapOptions{
              .vm_option = ZX_VM_PERM_READ | ZX_VM_PERM_WRITE | ZX_VM_REQUIRE_NON_RESIZABLE,
              .vmar = nullptr,
          },
  };
  ethernet_info_t eth_info;
  if (zx_status_t status = ethernet_.Query(0, &eth_info); status != ZX_OK) {
    FDF_LOG(ERROR, "failed to query parent: %s", zx_status_get_string(status));
    return zx::error(status);
  }
  zx::bti eth_bti;
  if (eth_info.features & ETHERNET_FEATURE_DMA) {
    ethernet_.GetBti(&eth_bti);
    if (!eth_bti.is_valid()) {
      FDF_LOG(ERROR, "failed to get valid bti handle");
      return zx::error(ZX_ERR_BAD_HANDLE);
    }
    opts.pin = vmo_store::PinOptions{
        .bti = eth_bti.borrow(),
        .bti_pin_options = ZX_BTI_PERM_READ | ZX_BTI_PERM_WRITE,
        .index = true,
    };
  }
  fuchsia_hardware_network::wire::PortClass port_class =
      fuchsia_hardware_network::wire::PortClass::kEthernet;
  if (eth_info.features & ETHERNET_FEATURE_SYNTH) {
    port_class = fuchsia_hardware_network::wire::PortClass::kVirtual;
  } else if (eth_info.features & ETHERNET_FEATURE_WLAN_AP) {
    // If both WLAN and WLAN_AP flags are set, WLAN_AP takes precedence
    port_class = fuchsia_hardware_network::wire::PortClass::kWlanAp;
  } else if (eth_info.features & ETHERNET_FEATURE_WLAN) {
    port_class = fuchsia_hardware_network::wire::PortClass::kWlanClient;
  }

  std::array<uint8_t, sizeof(eth_info.mac)> mac;
  std::copy_n(eth_info.mac, sizeof(eth_info.mac), mac.begin());
  if (eth_info.netbuf_size < sizeof(ethernet_netbuf_t)) {
    FDF_LOG(ERROR, "invalid buffer size %ld < min %zu", eth_info.netbuf_size,
            sizeof(ethernet_netbuf_t));
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  {
    std::lock_guard tx_lock(tx_lock_);
    eth_info.netbuf_size = ZX_ROUNDUP(eth_info.netbuf_size, 8);
    for (uint32_t i = 0; i < kFifoDepth; i++) {
      std::optional netbuf = Netbuf::Alloc(eth_info.netbuf_size);
      if (!netbuf.has_value()) {
        return zx::error(ZX_ERR_NO_MEMORY);
      }
      netbuf_pool_.push(std::move(netbuf.value()));
    }
  }

  const std::vector<fuchsia_hardware_network::FrameTypeSupport> tx_types{
      fuchsia_hardware_network::FrameTypeSupport{}
          .type(fuchsia_hardware_network::FrameType::kEthernet)
          .features(fuchsia_hardware_network::kFrameFeaturesRaw),
  };

  const std::vector<fuchsia_hardware_network::FrameType> rx_types = {
      fuchsia_hardware_network::wire::FrameType::kEthernet,
  };

  eth_bti_ = std::move(eth_bti);
  info_ = netdev::DeviceImplInfo{}
              .tx_depth(kFifoDepth)
              .rx_depth(kFifoDepth)
              .rx_threshold(kFifoDepth / 2)
              // Ensures clients do not use scatter-gather.
              .max_buffer_parts(1)
              // Per fuchsia.hardware.network.driver API:
              // "Devices that do not support scatter-gather DMA may set this to a value
              // smaller than a page size to guarantee compatibility."
              .max_buffer_length(kMaxBufferSize)
              // NetdeviceMigration has no alignment requirements.
              .buffer_alignment(1)
              // Ensures that an rx buffer will always be large enough to the ethernet MTU.
              .min_rx_buffer_length(eth_info.mtu);
  mtu_ = eth_info.mtu;
  mac_ = mac;
  port_info_ = fuchsia_hardware_network::PortBaseInfo{}
                   .port_class(port_class)
                   .rx_types(rx_types)
                   .tx_types(tx_types);
  netbuf_size_ = eth_info.netbuf_size;

  zx::result netdev_dispatcher = fdf::UnsynchronizedDispatcher::Create(
      {}, "netdev-dispatcher",
      [this](fdf_dispatcher_t*) { netdevice_dispatcher_shutdown_.Signal(); });
  if (netdev_dispatcher.is_error()) {
    FDF_LOG(ERROR, "Failed to create netdevice dispatcher: %s", netdev_dispatcher.status_string());
    return zx::error(netdev_dispatcher.status_value());
  }
  netdevice_dispatcher_ = std::move(netdev_dispatcher.value());

  {
    fbl::AutoLock vmo_lock(&vmo_lock_);
    vmo_store_ = std::make_unique<NetdeviceMigrationVmoStore>(opts);
    if (zx_status_t status = vmo_store_->Reserve(netdev::wire::kMaxVmos); status != ZX_OK) {
      FDF_LOG(ERROR, "failed to initialize vmo store: %s", zx_status_get_string(status));
      return zx::error(status);
    }
  }

  if (zx_status_t status = DeviceAdd(); status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to add device: %s", zx_status_get_string(status));
    return zx::error(status);
  }

  cleanup.cancel();

  return zx::ok();
}

zx_status_t NetdeviceMigration::DeviceAdd() {
  if (zx::result result =
          compat_server_.Initialize(incoming(), outgoing(), node_name(), kChildNodeName);
      result.is_error()) {
    FDF_LOG(ERROR, "Failed to initialize compat server: %s", result.status_string());
    return result.status_value();
  }

  // This callback will be invoked when this service is being connected.
  auto protocol = [this](fdf::ServerEnd<netdev::NetworkDeviceImpl> server_end) mutable {
    fdf::BindServer(netdevice_dispatcher_.get(), std::move(server_end), this);
  };

  // Register the callback to handler.
  netdev::Service::InstanceHandler handler({.network_device_impl = std::move(protocol)});

  auto status = outgoing()->AddService<netdev::Service>(std::move(handler));
  if (status.is_error()) {
    FDF_LOG(ERROR, "Failed to add service to outgoing directory: %s\n", status.status_string());
    return status.error_value();
  }

  fdf::Arena arena(0u);
  std::vector offers = compat_server_.CreateOffers2();
  offers.push_back(fdf::MakeOffer2<netdev::Service>());

  std::array<fuchsia_driver_framework::NodeProperty2, 0> properties{};
  zx::result netdev_child = AddChild("netdevice-migration-netdev", properties, offers);

  if (netdev_child.is_error()) {
    FDF_LOG(ERROR, "Failed to add net device child node: %s", netdev_child.status_string());
    return netdev_child.status_value();
  }
  netdev_child_ = std::move(netdev_child.value());
  return ZX_OK;
}

void NetdeviceMigration::PrepareStop(fdf::PrepareStopCompleter completer) {
  Shutdown();
  completer(zx::ok());
}

void NetdeviceMigration::Shutdown() {
  if (netdevice_dispatcher_.get()) {
    netdevice_dispatcher_.ShutdownAsync();
    netdevice_dispatcher_shutdown_.Wait();
    netdevice_dispatcher_.reset();
  }
}

void NetdeviceMigration::EthernetIfcStatus(uint32_t status) __TA_EXCLUDES(status_lock_) {
  fuchsia_hardware_network::wire::PortStatus port_status;
  fdf::Arena arena(0u);
  {
    std::lock_guard lock(status_lock_);
    port_status_flags_ = ToStatusFlags(status);
    port_status = fuchsia_hardware_network::wire::PortStatus::Builder(arena)
                      .mtu(mtu_)
                      .flags(port_status_flags_)
                      .Build();
  }
  if (fidl::OneWayStatus status = netdevice_.buffer(arena)->PortStatusChanged(kPortId, port_status);
      !status.ok()) {
    FDF_LOG(ERROR, "Failed to send port status changed: %s", status.FormatDescription().c_str());
    return;
  }
}

void NetdeviceMigration::EthernetIfcRecv(const uint8_t* data_buffer, size_t data_size,
                                         uint32_t flags) __TA_EXCLUDES(rx_lock_, vmo_lock_) {
  netdev::wire::RxSpaceBuffer space;
  // Use a closure to move logging outside of the scope of the lock.
  const zx_status_t status = [&]() {
    std::lock_guard rx_lock(rx_lock_);
    if (rx_spaces_.empty()) {
      return ZX_ERR_NO_RESOURCES;
    }
    space = rx_spaces_.front();
    rx_spaces_.pop();
    // Bounds check the incoming frame to verify that the ethernet driver respects the MTU.
    if (data_size > space.region.length) {
      DeviceRemove();
      return ZX_ERR_BUFFER_TOO_SMALL;
    }
    {
      network::SharedAutoLock vmo_lock(&vmo_lock_);
      auto* vmo = vmo_store_->GetVmo(space.region.vmo);
      if (vmo == nullptr) {
        DeviceRemove();
        return ZX_ERR_INVALID_ARGS;
      }
      cpp20::span<uint8_t> vmo_view = vmo->data();
      std::copy_n(data_buffer, data_size, vmo_view.begin() + space.region.offset);
    }
    netdev::wire::RxBufferPart part = {
        .id = space.id,
        .offset = 0,
        .length = static_cast<uint32_t>(data_size),
    };
    netdev::wire::RxBuffer buf = {
        .meta =
            {
                .port = kPortId,
                .frame_type = fuchsia_hardware_network::FrameType::kEthernet,
            },
        .data = fidl::VectorView<netdev::wire::RxBufferPart>::FromExternal(&part, 1),
    };
    fdf::Arena arena(0u);
    if (fidl::OneWayStatus status = netdevice_.buffer(arena)->CompleteRx(
            fidl::VectorView<netdev::wire::RxBuffer>::FromExternal(&buf, 1));
        !status.ok()) {
      FDF_LOG(ERROR, "Failed to complete RX: %s", status.FormatDescription().c_str());
      // This is a critical error that we can only recover from by unbinding and rebinding.
      DeviceRemove();
      return status.status();
    }
    return ZX_OK;
  }();

  switch (status) {
    case ZX_OK:
      break;
    case ZX_ERR_NO_RESOURCES: {
      constexpr size_t N = 64;
      // Assert power of 2 to avoid incorrect behavior on overflow.
      static_assert(N != 0 && (N & (N - 1)) == 0);
      // Use post-increment to ensure we log on the first dropped packet.
      const size_t v = no_rx_space_++;
      if (v % N == 0) {
        FDF_LOG(ERROR, "received ethernet frames without queued rx buffers; %zu frames dropped",
                v + 1);
      }
    } break;
    case ZX_ERR_BUFFER_TOO_SMALL:
      FDF_LOG(ERROR, "received ethernet frames larger than rx buffer length of %lu",
              space.region.length);
      break;
    case ZX_ERR_INVALID_ARGS:
      FDF_LOG(ERROR, "queued frames with unknown VMO IDs");
      break;
    default:
      ZX_PANIC("unexpected status %s", zx_status_get_string(status));
      break;
  }
}

void NetdeviceMigration::Init(netdev::wire::NetworkDeviceImplInitRequest* request,
                              fdf::Arena& arena, InitCompleter::Sync& completer) {
  if (netdevice_.is_valid()) {
    completer.buffer(arena).Reply(ZX_ERR_ALREADY_BOUND);
    return;
  }
  netdevice_.Bind(std::move(request->iface), netdevice_dispatcher_.get());

  auto [client, server] = fdf::Endpoints<fuchsia_hardware_network_driver::NetworkPort>::Create();
  fdf::BindServer(netdevice_dispatcher_.get(), std::move(server), this);

  netdevice_.buffer(arena)
      ->AddPort(kPortId, std::move(client))
      .Then([completer = completer.ToAsync()](
                fdf::WireUnownedResult<netdev::NetworkDeviceIfc::AddPort>& result) mutable {
        fdf::Arena arena(0u);
        if (!result.ok()) {
          FDF_LOG(ERROR, "failed to add port: %s", result.FormatDescription().c_str());
          completer.buffer(arena).Reply(result.status());
          return;
        }
        if (result->status != ZX_OK) {
          FDF_LOG(ERROR, "failed to add port: %s", zx_status_get_string(result->status));
          completer.buffer(arena).Reply(result->status);
          return;
        }
        completer.buffer(arena).Reply(ZX_OK);
      });
}

void NetdeviceMigration::Start(fdf::Arena& arena, StartCompleter::Sync& completer)
    __TA_EXCLUDES(rx_lock_, tx_lock_) {
  {
    std::lock_guard rx_lock(rx_lock_);
    std::lock_guard tx_lock(tx_lock_);
    if (tx_started_ || rx_started_) {
      FDF_LOG(WARNING, "device already started");
      completer.buffer(arena).Reply(ZX_ERR_ALREADY_BOUND);
      return;
    }
    rx_started_ = true;
    tx_started_ = true;
  }
  completer.buffer(arena).Reply(ZX_OK);
}

void NetdeviceMigration::Stop(fdf::Arena& arena, StopCompleter::Sync& completer)
    __TA_EXCLUDES(rx_lock_, tx_lock_) {
  ethernet_.Stop();

  std::array<netdev::wire::RxBuffer, kFifoDepth> rx_buffers;
  std::array<netdev::wire::RxBufferPart, kFifoDepth> rx_parts;
  // The size of this buffer must be smaller than or equal to the maximum number of buffers that can
  // be returned in one CompleteRx call. Otherwise we will overflow the FIDL channel.
  static_assert(std::size(rx_buffers) <= netdev::wire::kMaxRxBuffers);
  auto rx_buffer_iter = rx_buffers.begin();
  auto rx_part_iter = rx_parts.begin();
  std::array<netdev::wire::TxResult, kFifoDepth> tx_return;
  // Similarly the number of TX buffers returned must be less than or equal to the max tx results.
  static_assert(std::size(tx_return) <= netdev::wire::kMaxTxResults);
  auto tx_return_iter = tx_return.begin();
  {
    std::lock_guard rx_lock(rx_lock_);
    std::lock_guard tx_lock(tx_lock_);
    rx_started_ = false;
    tx_started_ = false;

    // On stop, we must return all the rx space buffers we hold.
    while (!rx_spaces_.empty()) {
      const netdev::wire::RxSpaceBuffer& space = rx_spaces_.front();
      netdev::wire::RxBufferPart& part = *rx_part_iter++;
      part = {
          .id = space.id,
          .offset = 0,
          .length = 0,
      };
      *rx_buffer_iter++ = netdev::wire::RxBuffer{
          .meta = {.frame_type = fuchsia_hardware_network::FrameType::kEthernet},
          .data = fidl::VectorView<netdev::wire::RxBufferPart>::FromExternal(&part, 1),
      };
      rx_spaces_.pop();
    }

    // We already stopped Ethernet, so we must also return all the tx buffers as
    // if they weren't transferred.
    for (const uint32_t& id : tx_in_flight_) {
      *tx_return_iter++ = {
          .id = id,
          .status = ZX_ERR_CANCELED,
      };
    }
    tx_in_flight_.clear();
  }
  if (size_t count = std::distance(rx_buffers.begin(), rx_buffer_iter); count != 0) {
    if (fidl::OneWayStatus status = netdevice_.buffer(arena)->CompleteRx(
            fidl::VectorView<netdev::wire::RxBuffer>::FromExternal(rx_buffers.data(), count));
        !status.ok()) {
      FDF_LOG(ERROR, "Failed to return %zu RX buffers on Stop: %s", count,
              status.FormatDescription().c_str());
      // This is a critical error that we can only recover from by unbinding and rebinding.
      DeviceRemove();
      completer.buffer(arena).Reply();
      return;
    }
  }
  if (size_t count = std::distance(tx_return.begin(), tx_return_iter); count != 0) {
    if (fidl::OneWayStatus status = netdevice_.buffer(arena)->CompleteTx(
            fidl::VectorView<netdev::wire::TxResult>::FromExternal(tx_return.data(), count));
        !status.ok()) {
      FDF_LOG(ERROR, "Failed to return %zu TX buffers on Stop: %s", count,
              status.FormatDescription().c_str());
      // This is a critical error that we can only recover from by unbinding and rebinding.
      DeviceRemove();
      completer.buffer(arena).Reply();
      return;
    }
  }
  completer.buffer(arena).Reply();
}

void NetdeviceMigration::GetInfo(
    fdf::Arena& arena,
    fdf::WireServer<netdev::NetworkDeviceImpl>::GetInfoCompleter::Sync& completer) {
  completer.buffer(arena).Reply(fidl::ToWire(arena, info_));
}

void NetdeviceMigration::QueueTx(netdev::wire::NetworkDeviceImplQueueTxRequest* request,
                                 fdf::Arena& arena, QueueTxCompleter::Sync& completer)
    __TA_EXCLUDES(tx_lock_, vmo_lock_) {
  constexpr uint32_t kQueueOpts = 0;
  std::optional<Netbuf> args[kFifoDepth];
  auto args_iter = std::begin(args);
  {
    network::SharedAutoLock vmo_lock(&vmo_lock_);
    std::lock_guard tx_lock(tx_lock_);
    const fidl::VectorView<netdev::wire::TxBuffer>& buffers = request->buffers;
    if (!tx_started_) {
      FDF_LOG(ERROR, "tx buffers queued before start call");
      static_assert(
          netdev::wire::kMaxTxBuffers <= netdev::wire::kMaxTxResults,
          "This code relies on being able to return all QueueTx buffers in one CompleteTx call");
      fidl::VectorView<netdev::wire::TxResult> results(arena, buffers.size());
      for (size_t i = 0; i < buffers.size(); ++i) {
        results[i] = {
            .id = buffers[i].id,
            .status = ZX_ERR_UNAVAILABLE,
        };
      }
      if (fidl::OneWayStatus status = netdevice_.buffer(arena)->CompleteTx(results); !status.ok()) {
        FDF_LOG(ERROR, "Failed to return %zu TX buffers: %s", buffers.size(),
                status.FormatDescription().c_str());
        // This is a critical error that we can only recover from by unbinding and rebinding.
        DeviceRemove();
      }
      return;
    }
    for (const netdev::wire::TxBuffer& buffer : buffers) {
      if (buffer.data.size() != *info_.max_buffer_parts()) {
        FDF_LOG(ERROR, "tx buffer queued with parts count %zu != max buffer parts %u",
                buffer.data.size(), *info_.max_buffer_parts());
        DeviceRemove();
        return;
      }
      const netdev::wire::BufferRegion& region = buffer.data[0];
      if (region.length > *info_.max_buffer_length()) {
        FDF_LOG(ERROR, "tx buffer queued with length %" PRIu64 " > max buffer length %u",
                region.length, *info_.max_buffer_length());
        DeviceRemove();
        return;
      }
      auto* vmo = vmo_store_->GetVmo(region.vmo);
      if (vmo == nullptr) {
        FDF_LOG(ERROR, "tx buffer %du queued with unknown vmo id %du", buffer.id, region.vmo);
        DeviceRemove();
        return;
      }
      zx_paddr_t phys_addr = 0;
      if (eth_bti_.is_valid()) {
        fzl::PinnedVmo::Region pinned_region;
        size_t regions_needed = 0;
        if (zx_status_t status = vmo->GetPinnedRegions(region.offset, region.length, &pinned_region,
                                                       1, &regions_needed);
            status != ZX_OK) {
          FDF_LOG(ERROR, "failed to get pinned regions of vmo: %s", zx_status_get_string(status));
          netdev::wire::TxResult result = {
              .id = buffer.id,
              .status = ZX_ERR_INTERNAL,
          };
          if (fidl::OneWayStatus status = netdevice_.buffer(arena)->CompleteTx(
                  fidl::VectorView<netdev::wire::TxResult>::FromExternal(&result, 1));
              !status.ok()) {
            FDF_LOG(ERROR, "Failed to return TX buffer with invalid VMO: %s",
                    status.FormatDescription().c_str());
            // This is a critical error that we can only recover from by unbinding and rebinding.
            DeviceRemove();
            return;
          }
          continue;
        }
        phys_addr = pinned_region.phys_addr;
      }
      cpp20::span vmo_view(vmo->data());
      vmo_view = vmo_view.subspan(region.offset, region.length);
      std::optional netbuf = netbuf_pool_.pop();
      if (!netbuf.has_value()) {
        FDF_LOG(ERROR, "netbuf pool exhausted");
        DeviceRemove();
        return;
      }
      *(netbuf->operation()) = {
          .data_buffer = vmo_view.data(),
          .data_size = vmo_view.size(),
          .phys = phys_addr,
      };
      *(netbuf->private_storage()) = buffer.id;
      *args_iter++ = std::move(netbuf);
      tx_in_flight_.insert(buffer.id);
    }
  }
  for (auto arg = std::begin(args); arg != args_iter; ++arg) {
    ethernet_.QueueTx(
        kQueueOpts, arg->value().take(),
        [](void* ctx, zx_status_t status, ethernet_netbuf_t* netbuf) {
          // The error semantics of fuchsia.hardware.ethernet/EthernetImpl.QueueTx are unspecified
          // other than `ZX_OK` indicating success. However, ethernet driver usages of
          // `ZX_ERR_NO_RESOURCES` and `ZX_ERR_UNAVAILABLE` map to the meanings specified by
          // fuchsia.hardware.network.driver/TxResult. Accordingly, use `ZX_ERR_INTERNAL` for any
          // other Ethernet error.
          switch (status) {
            case ZX_OK:
            case ZX_ERR_NO_RESOURCES:
            case ZX_ERR_UNAVAILABLE:
              break;
            default:
              status = ZX_ERR_INTERNAL;
          }
          netdev::wire::TxResult result = {
              .status = status,
          };
          auto* netdev = static_cast<NetdeviceMigration*>(ctx);
          Netbuf op(netbuf, netdev->netbuf_size_);
          result.id = *(op.private_storage());
          // Return the buffers to the netbuf_pool before signalling that the transaction is
          // complete. This ensures that if the netbuf_pool was empty, we can handle requests
          // that arrive immediately after.
          {
            std::lock_guard tx_lock(netdev->tx_lock_);
            netdev->netbuf_pool_.push(std::move(op));
            auto iter = netdev->tx_in_flight_.find(result.id);
            if (iter == netdev->tx_in_flight_.end()) {
              // No longer in flight, stop has been called. Don't complete the
              // transaction.
              return;
            }
            netdev->tx_in_flight_.erase(iter);
          }
          fdf::Arena arena(0u);
          if (fidl::OneWayStatus status = netdev->netdevice_.buffer(arena)->CompleteTx(
                  fidl::VectorView<netdev::wire::TxResult>::FromExternal(&result, 1));
              !status.ok()) {
            FDF_LOG(ERROR, "Failed to complete TX buffer: %s", status.FormatDescription().c_str());
            // This is a critical error that we can only recover from by unbinding and rebinding.
            netdev->DeviceRemove();
          }
        },
        this);
  }
}

void NetdeviceMigration::QueueRxSpace(netdev::wire::NetworkDeviceImplQueueRxSpaceRequest* request,
                                      fdf::Arena& arena, QueueRxSpaceCompleter::Sync& completer)
    __TA_EXCLUDES(rx_lock_) {
  bool rx_space_queued = true;
  {
    std::lock_guard rx_lock(rx_lock_);
    if (size_t total_rx_buffers = rx_spaces_.size() + request->buffers.size();
        total_rx_buffers > *info_.rx_depth()) {
      // Client has violated API contract: "The total number of outstanding rx buffers given to a
      // device will never exceed the reported [`DeviceInfo.rx_depth`] value."
      FDF_LOG(ERROR, "total received rx buffers %zu > rx_depth %u", total_rx_buffers,
              *info_.rx_depth());
      DeviceRemove();
      return;
    }
    const fidl::VectorView<netdev::wire::RxSpaceBuffer>& buffers = request->buffers;
    if (!rx_started_) {
      FDF_LOG(ERROR, "rx buffers queued before start call");
      for (const netdev::wire::RxSpaceBuffer& space : buffers) {
        netdev::wire::RxBufferPart part = {
            .id = space.id,
            .length = 0,
        };
        netdev::wire::RxBuffer buf = {
            .meta = {.frame_type = fuchsia_hardware_network::FrameType::kEthernet},
            .data = fidl::VectorView<netdev::wire::RxBufferPart>::FromExternal(&part, 1),
        };
        if (fidl::OneWayStatus status = netdevice_.buffer(arena)->CompleteRx(
                fidl::VectorView<netdev::wire::RxBuffer>::FromExternal(&buf, 1));
            !status.ok()) {
          FDF_LOG(ERROR, "Failed to complete RX buffer: %s", status.FormatDescription().c_str());
          // This is a critical error that we can only recover from by unbinding and rebinding.
          DeviceRemove();
          return;
        }
      }
      return;
    }
    for (const netdev::wire::RxSpaceBuffer& space : buffers) {
      if (space.region.length < *info_.min_rx_buffer_length() ||
          space.region.length > *info_.max_buffer_length()) {
        FDF_LOG(ERROR, "rx buffer queued with length %ld, outside valid range [%du, %du]",
                space.region.length, *info_.min_rx_buffer_length(), *info_.max_buffer_length());
        DeviceRemove();
        return;
      }
      rx_spaces_.push(space);
    }
    std::swap(rx_space_queued, rx_space_queued_);
  }
  if (!rx_space_queued) {
    // Do not hold the lock across the ethernet_.Start() call because the Netdevice contract ensures
    // that a subsequent Start() or Stop() call will not occur until after this one has returned via
    // the callback.
    if (zx_status_t status = ethernet_.Start(this, &ethernet_ifc_protocol_ops_); status != ZX_OK) {
      FDF_LOG(WARNING, "failed to start device: %s", zx_status_get_string(status));
    }
  }
}

void NetdeviceMigration::PrepareVmo(netdev::wire::NetworkDeviceImplPrepareVmoRequest* request,
                                    fdf::Arena& arena, PrepareVmoCompleter::Sync& completer)
    __TA_EXCLUDES(vmo_lock_) {
  fbl::AutoLock vmo_lock(&vmo_lock_);
  zx_status_t status = vmo_store_->RegisterWithKey(request->id, std::move(request->vmo));
  completer.buffer(arena).Reply(status);
}

void NetdeviceMigration::ReleaseVmo(netdev::wire::NetworkDeviceImplReleaseVmoRequest* request,
                                    fdf::Arena& arena, ReleaseVmoCompleter::Sync& completer)
    __TA_EXCLUDES(vmo_lock_) {
  fbl::AutoLock vmo_lock(&vmo_lock_);
  if (zx::result<zx::vmo> status = vmo_store_->Unregister(request->id); status.is_error()) {
    // A failure here may be the result of a failed call to register the vmo, in which case the
    // driver is queued for removal from device manager. Accordingly, we must not panic lest we
    // disrupt the orderly shutdown of the driver: a log statement is the best we can do.
    FDF_LOG(ERROR, "failed to release vmo id = %d: %s", request->id, status.status_string());
  }
  completer.buffer(arena).Reply();
}

void NetdeviceMigration::GetInfo(
    fdf::Arena& arena, fdf::WireServer<netdev::NetworkPort>::GetInfoCompleter::Sync& completer) {
  completer.buffer(arena).Reply(fidl::ToWire(arena, port_info_));
}

void NetdeviceMigration::GetStatus(fdf::Arena& arena, GetStatusCompleter::Sync& completer)
    __TA_EXCLUDES(status_lock_) {
  fuchsia_hardware_network::wire::PortStatus status;
  {
    std::lock_guard lock(status_lock_);
    status = fuchsia_hardware_network::wire::PortStatus::Builder(arena)
                 .flags(port_status_flags_)
                 .mtu(mtu_)
                 .Build();
  }
  completer.buffer(arena).Reply(status);
}

void NetdeviceMigration::SetActive(
    fuchsia_hardware_network_driver::wire::NetworkPortSetActiveRequest* request, fdf::Arena& arena,
    SetActiveCompleter::Sync& completer) {}

void NetdeviceMigration::GetMac(fdf::Arena& arena, GetMacCompleter::Sync& completer) {
  auto [client, server] = fdf::Endpoints<fuchsia_hardware_network_driver::MacAddr>::Create();
  fdf::BindServer(netdevice_dispatcher_.get(), std::move(server), this);
  completer.buffer(arena).Reply(std::move(client));
}

void NetdeviceMigration::Removed(fdf::Arena& arena, RemovedCompleter::Sync& completer) {
  FDF_LOG(INFO, "removed event for port %d", kPortId);
}

void NetdeviceMigration::GetAddress(fdf::Arena& arena, GetAddressCompleter::Sync& completer) {
  fuchsia_net::wire::MacAddress mac;
  static_assert(sizeof(mac_) == decltype(mac.octets)::size());
  std::copy(mac_.begin(), mac_.end(), mac.octets.begin());
  completer.buffer(arena).Reply(mac);
}

void NetdeviceMigration::GetFeatures(fdf::Arena& arena, GetFeaturesCompleter::Sync& completer) {
  netdev::wire::Features features = netdev::wire::Features::Builder(arena)
                                        .multicast_filter_count(kMulticastFilterMax)
                                        .supported_modes(kSupportedMacFilteringModes)
                                        .Build();
  completer.buffer(arena).Reply(features);
}

void NetdeviceMigration::SetMode(
    fuchsia_hardware_network_driver::wire::MacAddrSetModeRequest* request, fdf::Arena& arena,
    SetModeCompleter::Sync& completer) {
  if (request->multicast_macs.size() > kMulticastFilterMax) {
    FDF_LOG(ERROR, "multicast macs count exceeds maximum: %zu > %u", request->multicast_macs.size(),
            kMulticastFilterMax);
    DeviceRemove();
    completer.buffer(arena).Reply();
    return;
  }
  switch (request->mode) {
    case fuchsia_hardware_network::wire::MacFilterMode::kMulticastFilter:
      SetMacParam(ETHERNET_SETPARAM_MULTICAST_PROMISC, 0);
      SetMacParam(ETHERNET_SETPARAM_PROMISC, 0);
      SetMacParam(ETHERNET_SETPARAM_MULTICAST_FILTER,
                  static_cast<int32_t>(request->multicast_macs.size()), request->multicast_macs);
      break;
    case fuchsia_hardware_network::wire::MacFilterMode::kMulticastPromiscuous:
      SetMacParam(ETHERNET_SETPARAM_PROMISC, 0);
      SetMacParam(ETHERNET_SETPARAM_MULTICAST_PROMISC, 1);
      break;
    case fuchsia_hardware_network::wire::MacFilterMode::kPromiscuous:
      SetMacParam(ETHERNET_SETPARAM_PROMISC, 1);
      break;
    default:
      FDF_LOG(ERROR, "mac addr filtering mode set with unsupported mode %u",
              static_cast<uint32_t>(request->mode));
      DeviceRemove();
      completer.buffer(arena).Reply();
      return;
  }
  completer.buffer(arena).Reply();
}

void NetdeviceMigration::SetMacParam(uint32_t param, int32_t value,
                                     std::span<const fuchsia_net::wire::MacAddress> data) const {
  uint8_t macs[kMulticastFilterMax * ETH_MAC_SIZE];
  for (size_t i = 0; i < data.size() && i < kMulticastFilterMax; ++i) {
    memcpy(&macs[i * ETH_MAC_SIZE], data[i].octets.data(), ETH_MAC_SIZE);
  }

  zx_status_t status =
      ethernet_.SetParam(param, value, data.empty() ? nullptr : macs, data.size() * ETH_MAC_SIZE);
  switch (status) {
    case ZX_OK:
      break;
    // Most drivers that use netdevice migration don't support all parameters so
    // this ends up generating quite a bit of noise in logs, log this case to
    // debug instead.
    case ZX_ERR_NOT_SUPPORTED:
      FDF_LOG(DEBUG, "failed to set ethernet parameter %du to value %d: %s", param, value,
              zx_status_get_string(status));
      break;
    default:
      FDF_LOG(WARNING, "failed to set ethernet parameter %du to value %d: %s", param, value,
              zx_status_get_string(status));
      break;
  }
}

void NetdeviceMigration::DeviceRemove() {
  // Remove the driver by destroying the node channel.
  node().TakeChannel().reset();
}

}  // namespace netdevice_migration

FUCHSIA_DRIVER_EXPORT(netdevice_migration::NetdeviceMigration);
