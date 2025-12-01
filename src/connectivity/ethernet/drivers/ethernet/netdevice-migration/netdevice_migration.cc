// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "netdevice_migration.h"

#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/zircon-internal/align.h>
#include <zircon/system/public/zircon/assert.h>

#include <algorithm>

#include <fbl/alloc_checker.h>

namespace {

fuchsia_hardware_network::wire::StatusFlags ToStatusFlags(uint32_t ethernet_status) {
  fuchsia_hardware_network::wire::StatusFlags flags;
  if (ethernet_status & ETHERNET_STATUS_ONLINE) {
    flags |= fuchsia_hardware_network::wire::StatusFlags::kOnline;
  }
  return flags;
}

constexpr uint8_t kRxTypes[] = {
    static_cast<uint8_t>(fuchsia_hardware_network::wire::FrameType::kEthernet)};
constexpr frame_type_support_t kTxTypes[] = {frame_type_support_t{
    .type = static_cast<uint8_t>(fuchsia_hardware_network::wire::FrameType::kEthernet),
    .features = fuchsia_hardware_network::wire::kFrameFeaturesRaw}};

}  // namespace

namespace netdevice_migration {

NetdeviceMigration::NetdeviceMigration(fdf::DriverStartArgs start_args,
                                       fdf::UnownedSynchronizedDispatcher driver_dispatcher)
    : fdf::DriverBase("netdevice-migration", std::move(start_args), std::move(driver_dispatcher)),
      mac_addr_proto_({&mac_addr_protocol_ops_, this}),
      ethernet_ifc_proto_({&ethernet_ifc_protocol_ops_, this}) {}

zx::result<> NetdeviceMigration::Start() {
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
  eth_bti_ = std::move(eth_bti);
  info_ = {
      .tx_depth = kFifoDepth,
      .rx_depth = kFifoDepth,
      .rx_threshold = kFifoDepth / 2,
      // Ensures clients do not use scatter-gather.
      .max_buffer_parts = 1,
      // Per fuchsia.hardware.network.driver API:
      // "Devices that do not support scatter-gather DMA may set this to a value smaller than
      // a page size to guarantee compatibility."
      .max_buffer_length = kMaxBufferSize,
      // NetdeviceMigration has no alignment requirements.
      .buffer_alignment = 1,
      // Ensures that an rx buffer will always be large enough to the ethernet MTU.
      .min_rx_buffer_length = eth_info.mtu,
  };
  mtu_ = eth_info.mtu;
  mac_ = mac;
  port_info_ = {
      .port_class = static_cast<uint16_t>(port_class),
      .rx_types_list = kRxTypes,
      .rx_types_count = std::size(kRxTypes),
      .tx_types_list = kTxTypes,
      .tx_types_count = std::size(kTxTypes),
  };
  netbuf_size_ = eth_info.netbuf_size;

  {
    fbl::AutoLock vmo_lock(&vmo_lock_);
    vmo_store_ = std::make_unique<NetdeviceMigrationVmoStore>(opts);
    if (zx_status_t status = vmo_store_->Reserve(MAX_VMOS); status != ZX_OK) {
      FDF_LOG(ERROR, "failed to initialize vmo store: %s", zx_status_get_string(status));
      return zx::error(status);
    }
  }

  if (zx_status_t status = DeviceAdd(); status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to add device: %s", zx_status_get_string(status));
    return zx::error(status);
  }

  return zx::ok();
}

zx_status_t NetdeviceMigration::DeviceAdd() {
  compat::DeviceServer::BanjoConfig banjo_config;
  banjo_config.callbacks[ZX_PROTOCOL_NETWORK_DEVICE_IMPL] = net_device_server_.callback();

  if (zx::result result =
          compat_server_.Initialize(incoming(), outgoing(), node_name(), kChildNodeName,
                                    compat::ForwardMetadata::None(), std::move(banjo_config));
      result.is_error()) {
    FDF_LOG(ERROR, "Failed to initialize compat server: %s", result.status_string());
    return result.status_value();
  }

  zx::result netdev_child =
      AddChild("netdevice-migration-netdev", {{net_device_server_.property()}},
               compat_server_.CreateOffers2());
  if (netdev_child.is_error()) {
    FDF_LOG(ERROR, "Failed to add net device child node: %s", netdev_child.status_string());
    return netdev_child.status_value();
  }
  netdev_child_ = std::move(netdev_child.value());
  return ZX_OK;
}

void NetdeviceMigration::EthernetIfcStatus(uint32_t status) __TA_EXCLUDES(status_lock_) {
  port_status_t port_status = {
      .mtu = mtu_,
  };
  {
    std::lock_guard lock(status_lock_);
    port_status_flags_ = ToStatusFlags(status);
    port_status.flags = status;
  }
  netdevice_.PortStatusChanged(kPortId, &port_status);
}

void NetdeviceMigration::EthernetIfcRecv(const uint8_t* data_buffer, size_t data_size,
                                         uint32_t flags) __TA_EXCLUDES(rx_lock_, vmo_lock_) {
  rx_space_buffer_t space;
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
    rx_buffer_part_t part = {
        .id = space.id,
        .offset = 0,
        .length = static_cast<uint32_t>(data_size),
    };
    rx_buffer_t buf = {
        .meta =
            {
                .port = kPortId,
                .frame_type = static_cast<uint8_t>(fuchsia_hardware_network::FrameType::kEthernet),
            },
        .data_list = &part,
        .data_count = 1,
    };
    netdevice_.CompleteRx(&buf, 1);
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

void NetdeviceMigration::NetworkDeviceImplInit(const network_device_ifc_protocol_t* iface,
                                               network_device_impl_init_callback callback,
                                               void* cookie) {
  if (netdevice_.is_valid()) {
    callback(cookie, ZX_ERR_ALREADY_BOUND);
    return;
  }
  netdevice_ = ddk::NetworkDeviceIfcProtocolClient(iface);

  using Context = std::pair<network_device_impl_init_callback, void*>;

  std::unique_ptr context = std::make_unique<Context>(callback, cookie);
  netdevice_.AddPort(
      kPortId, this, &network_port_protocol_ops_,
      [](void* ctx, zx_status_t status) {
        std::unique_ptr<Context> context(static_cast<Context*>(ctx));
        auto [callback, cookie] = *context;
        if (status != ZX_OK) {
          FDF_LOG(ERROR, "failed to add port: %s", zx_status_get_string(status));
          callback(cookie, status);
          return;
        }
        callback(cookie, ZX_OK);
      },
      context.release());
}

void NetdeviceMigration::NetworkDeviceImplStart(network_device_impl_start_callback callback,
                                                void* cookie) __TA_EXCLUDES(rx_lock_, tx_lock_) {
  {
    std::lock_guard rx_lock(rx_lock_);
    std::lock_guard tx_lock(tx_lock_);
    if (tx_started_ || rx_started_) {
      FDF_LOG(WARNING, "device already started");
      callback(cookie, ZX_ERR_ALREADY_BOUND);
      return;
    }
    rx_started_ = true;
    tx_started_ = true;
  }
  callback(cookie, ZX_OK);
}

void NetdeviceMigration::NetworkDeviceImplStop(network_device_impl_stop_callback callback,
                                               void* cookie) __TA_EXCLUDES(rx_lock_, tx_lock_) {
  ethernet_.Stop();

  std::array<rx_buffer_t, kFifoDepth> rx_buffers;
  std::array<rx_buffer_part_t, kFifoDepth> rx_parts;
  auto rx_buffer_iter = rx_buffers.begin();
  auto rx_part_iter = rx_parts.begin();
  std::array<tx_result_t, kFifoDepth> tx_return;
  auto tx_return_iter = tx_return.begin();
  {
    std::lock_guard rx_lock(rx_lock_);
    std::lock_guard tx_lock(tx_lock_);
    rx_started_ = false;
    tx_started_ = false;

    // On stop, we must return all the rx space buffers we hold.
    while (!rx_spaces_.empty()) {
      const rx_space_buffer_t& space = rx_spaces_.front();
      rx_buffer_part_t& part = *rx_part_iter++;
      part = {
          .id = space.id,
          .offset = 0,
          .length = 0,
      };
      *rx_buffer_iter++ = {
          .meta = {.frame_type =
                       static_cast<uint8_t>(fuchsia_hardware_network::FrameType::kEthernet)},
          .data_list = &part,
          .data_count = 1,
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
    netdevice_.CompleteRx(rx_buffers.data(), count);
  }
  if (size_t count = std::distance(tx_return.begin(), tx_return_iter); count != 0) {
    netdevice_.CompleteTx(tx_return.data(), count);
  }
  callback(cookie);
}

void NetdeviceMigration::NetworkDeviceImplGetInfo(device_impl_info_t* out_info) {
  *out_info = info_;
}

void NetdeviceMigration::NetworkDeviceImplQueueTx(const tx_buffer_t* buffers_list,
                                                  size_t buffers_count)
    __TA_EXCLUDES(tx_lock_, vmo_lock_) {
  constexpr uint32_t kQueueOpts = 0;
  std::optional<Netbuf> args[kFifoDepth];
  auto args_iter = std::begin(args);
  {
    network::SharedAutoLock vmo_lock(&vmo_lock_);
    std::lock_guard tx_lock(tx_lock_);
    cpp20::span buffers(buffers_list, buffers_count);
    if (!tx_started_) {
      FDF_LOG(ERROR, "tx buffers queued before start call");
      tx_result_t results[buffers.size()];
      for (size_t i = 0; i < buffers.size(); ++i) {
        results[i] = {
            .id = buffers[i].id,
            .status = ZX_ERR_UNAVAILABLE,
        };
      }
      netdevice_.CompleteTx(results, buffers.size());
      return;
    }
    for (const tx_buffer_t& buffer : buffers) {
      if (buffer.data_count > info_.max_buffer_parts) {
        FDF_LOG(ERROR, "tx buffer queued with parts count %ld > max buffer parts %du",
                buffer.data_count, info_.max_buffer_parts);
        DeviceRemove();
        return;
      }
      if (buffer.data_list->length > info_.max_buffer_length) {
        FDF_LOG(ERROR, "tx buffer queued with length %ld > max buffer length %du",
                buffer.data_list->length, info_.max_buffer_length);
        DeviceRemove();
        return;
      }
      auto* vmo = vmo_store_->GetVmo(buffer.data_list->vmo);
      if (vmo == nullptr) {
        FDF_LOG(ERROR, "tx buffer %du queued with unknown vmo id %du", buffer.id,
                buffer.data_list->vmo);
        DeviceRemove();
        return;
      }
      zx_paddr_t phys_addr = 0;
      if (eth_bti_.is_valid()) {
        fzl::PinnedVmo::Region region;
        size_t regions_needed = 0;
        if (zx_status_t status = vmo->GetPinnedRegions(
                buffer.data_list->offset, buffer.data_list->length, &region, 1, &regions_needed);
            status != ZX_OK) {
          FDF_LOG(ERROR, "failed to get pinned regions of vmo: %s", zx_status_get_string(status));
          tx_result_t result = {
              .id = buffer.id,
              .status = ZX_ERR_INTERNAL,
          };
          netdevice_.CompleteTx(&result, 1);
          continue;
        }
        phys_addr = region.phys_addr;
      }
      cpp20::span vmo_view(vmo->data());
      vmo_view = vmo_view.subspan(buffer.data_list->offset, buffer.data_list->length);
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
          tx_result_t result = {
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
          netdev->netdevice_.CompleteTx(&result, 1);
        },
        this);
  }
}

void NetdeviceMigration::NetworkDeviceImplQueueRxSpace(const rx_space_buffer_t* buffers_list,
                                                       size_t buffers_count)
    __TA_EXCLUDES(rx_lock_) {
  bool rx_space_queued = true;
  {
    std::lock_guard rx_lock(rx_lock_);
    if (size_t total_rx_buffers = rx_spaces_.size() + buffers_count;
        total_rx_buffers > info_.rx_depth) {
      // Client has violated API contract: "The total number of outstanding rx buffers given to a
      // device will never exceed the reported [`DeviceInfo.rx_depth`] value."
      FDF_LOG(ERROR, "total received rx buffers %ld > rx_depth %d", total_rx_buffers,
              info_.rx_depth);
      DeviceRemove();
      return;
    }
    cpp20::span buffers(buffers_list, buffers_count);
    if (!rx_started_) {
      FDF_LOG(ERROR, "rx buffers queued before start call");
      for (const rx_space_buffer_t& space : buffers) {
        rx_buffer_part_t part = {
            .id = space.id,
            .length = 0,
        };
        rx_buffer_t buf = {
            .meta = {.frame_type =
                         static_cast<uint8_t>(fuchsia_hardware_network::FrameType::kEthernet)},
            .data_list = &part,
            .data_count = 1,
        };
        netdevice_.CompleteRx(&buf, 1);
      }
      return;
    }
    for (const rx_space_buffer_t& space : buffers) {
      if (space.region.length < info_.min_rx_buffer_length ||
          space.region.length > info_.max_buffer_length) {
        FDF_LOG(ERROR, "rx buffer queued with length %ld, outside valid range [%du, %du]",
                space.region.length, info_.min_rx_buffer_length, info_.max_buffer_length);
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

void NetdeviceMigration::NetworkDeviceImplPrepareVmo(
    uint8_t id, zx::vmo vmo, network_device_impl_prepare_vmo_callback callback, void* cookie)
    __TA_EXCLUDES(vmo_lock_) {
  fbl::AutoLock vmo_lock(&vmo_lock_);
  zx_status_t status = vmo_store_->RegisterWithKey(id, std::move(vmo));
  callback(cookie, status);
}

void NetdeviceMigration::NetworkDeviceImplReleaseVmo(uint8_t id) __TA_EXCLUDES(vmo_lock_) {
  fbl::AutoLock vmo_lock(&vmo_lock_);
  if (zx::result<zx::vmo> status = vmo_store_->Unregister(id); status.status_value() != ZX_OK) {
    // A failure here may be the result of a failed call to register the vmo, in which case the
    // driver is queued for removal from device manager. Accordingly, we must not panic lest we
    // disrupt the orderly shutdown of the driver: a log statement is the best we can do.
    FDF_LOG(ERROR, "failed to release vmo id = %d: %s", id, status.status_string());
  }
}

void NetdeviceMigration::NetworkPortGetInfo(port_base_info_t* out_info) { *out_info = port_info_; }

void NetdeviceMigration::NetworkPortGetStatus(port_status_t* out_status)
    __TA_EXCLUDES(status_lock_) {
  std::lock_guard lock(status_lock_);
  *out_status = {
      .flags = static_cast<uint32_t>(port_status_flags_),
      .mtu = mtu_,
  };
}

void NetdeviceMigration::NetworkPortSetActive(bool active) {}

void NetdeviceMigration::NetworkPortGetMac(mac_addr_protocol_t** out_mac_ifc) {
  if (out_mac_ifc) {
    *out_mac_ifc = &mac_addr_proto_;
  }
}

void NetdeviceMigration::NetworkPortRemoved() {
  FDF_LOG(INFO, "removed event for port %d", kPortId);
}

void NetdeviceMigration::MacAddrGetAddress(mac_address_t* out_mac) {
  static_assert(sizeof(mac_) == sizeof(out_mac->octets));
  std::copy(mac_.begin(), mac_.end(), out_mac->octets);
}

void NetdeviceMigration::MacAddrGetFeatures(features_t* out_features) {
  *out_features = {
      .multicast_filter_count = kMulticastFilterMax,
      .supported_modes = kSupportedMacFilteringModes,
  };
}

void NetdeviceMigration::MacAddrSetMode(mac_filter_mode_t mode,
                                        const mac_address_t* multicast_macs_list,
                                        size_t multicast_macs_count) {
  if (multicast_macs_count > kMulticastFilterMax) {
    FDF_LOG(ERROR, "multicast macs count exceeds maximum: %zu > %du", multicast_macs_count,
            kMulticastFilterMax);
    DeviceRemove();
    return;
  }
  switch (mode) {
    case MAC_FILTER_MODE_MULTICAST_FILTER:
      SetMacParam(ETHERNET_SETPARAM_MULTICAST_PROMISC, 0, nullptr, 0);
      SetMacParam(ETHERNET_SETPARAM_PROMISC, 0, nullptr, 0);
      SetMacParam(ETHERNET_SETPARAM_MULTICAST_FILTER, static_cast<int32_t>(multicast_macs_count),
                  multicast_macs_list, multicast_macs_count);
      break;
    case MAC_FILTER_MODE_MULTICAST_PROMISCUOUS:
      SetMacParam(ETHERNET_SETPARAM_PROMISC, 0, nullptr, 0);
      SetMacParam(ETHERNET_SETPARAM_MULTICAST_PROMISC, 1, nullptr, 0);
      break;
    case MAC_FILTER_MODE_PROMISCUOUS:
      SetMacParam(ETHERNET_SETPARAM_PROMISC, 1, nullptr, 0);
      break;
    default:
      FDF_LOG(ERROR, "mac addr filtering mode set with unsupported mode %du", mode);
      DeviceRemove();
      return;
  }
}

void NetdeviceMigration::SetMacParam(uint32_t param, int32_t value,
                                     const mac_address_t* data_buffer, size_t data_size) const {
  uint8_t macs[kMulticastFilterMax * MAC_SIZE];
  for (size_t i = 0; i < data_size && i < kMulticastFilterMax; ++i) {
    memcpy(&macs[i * MAC_SIZE], data_buffer[i].octets, MAC_SIZE);
  }

  zx_status_t status =
      ethernet_.SetParam(param, value, data_buffer ? macs : nullptr, data_size * MAC_SIZE);
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
