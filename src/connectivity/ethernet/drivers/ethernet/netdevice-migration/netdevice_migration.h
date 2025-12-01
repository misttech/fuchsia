// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_ETHERNET_DRIVERS_ETHERNET_NETDEVICE_MIGRATION_NETDEVICE_MIGRATION_H_
#define SRC_CONNECTIVITY_ETHERNET_DRIVERS_ETHERNET_NETDEVICE_MIGRATION_NETDEVICE_MIGRATION_H_

#include <fidl/fuchsia.hardware.network/cpp/wire.h>
#include <fuchsia/hardware/ethernet/cpp/banjo.h>
#include <fuchsia/hardware/network/driver/cpp/banjo.h>
#include <lib/driver/compat/cpp/banjo_server.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <zircon/system/public/zircon/compiler.h>

#include <queue>
#include <unordered_set>
#include <utility>

#include <ddktl/device.h>
#include <fbl/auto_lock.h>
#include <fbl/mutex.h>

#include "src/connectivity/network/drivers/network-device/device/public/locks.h"
#include "src/devices/lib/dev-operation/include/lib/operation/ethernet.h"
#include "src/lib/vmo_store/vmo_store.h"

namespace netdevice_migration {

using NetdeviceMigrationVmoStore = vmo_store::VmoStore<vmo_store::SlabStorage<uint32_t>>;
using Netbuf = eth::Operation<uint32_t>;
using NetbufPool = eth::OperationPool<uint32_t>;

class NetdeviceMigration;
class NetdeviceMigration
    : public fdf::DriverBase,
      public ddk::EthernetIfcProtocol<NetdeviceMigration>,
      public ddk::NetworkDeviceImplProtocol<NetdeviceMigration, ddk::base_protocol>,
      public ddk::NetworkPortProtocol<NetdeviceMigration>,
      public ddk::MacAddrProtocol<NetdeviceMigration> {
 public:
  static constexpr uint8_t kPortId = 13;
  // Equivalent to old ethernet driver FIFO depth; see
  // https://cs.opensource.google/fuchsia/fuchsia/+/main:src/connectivity/ethernet/drivers/ethernet/ethernet.h;l=169;drc=bd653b0d513ea6cc0d2ec85d38ae31bf084f0651
  static constexpr uint32_t kFifoDepth = 256;
  static constexpr uint32_t kMaxBufferSize = 2048;
  static constexpr supported_mac_filter_mode_t kSupportedMacFilteringModes =
      SUPPORTED_MAC_FILTER_MODE_MULTICAST_FILTER | SUPPORTED_MAC_FILTER_MODE_MULTICAST_PROMISCUOUS |
      SUPPORTED_MAC_FILTER_MODE_PROMISCUOUS;
  static constexpr uint32_t kMulticastFilterMax = MAX_MAC_FILTER;
  static constexpr const char kChildNodeName[] = "netdevice-migration-compat";

  NetdeviceMigration(fdf::DriverStartArgs start_args,
                     fdf::UnownedSynchronizedDispatcher driver_dispatcher);

  // DriverBase implementation.
  zx::result<> Start() override;

  // For EthernetIfcProtocol.
  void EthernetIfcStatus(uint32_t status) __TA_EXCLUDES(status_lock_);
  void EthernetIfcRecv(const uint8_t* data_buffer, size_t data_size, uint32_t flags)
      __TA_EXCLUDES(rx_lock_) __TA_EXCLUDES(vmo_lock_);

  // For NetworkDeviceImplProtocol.
  void NetworkDeviceImplInit(const network_device_ifc_protocol_t* iface,
                             network_device_impl_init_callback callback, void* cookie);
  void NetworkDeviceImplStart(network_device_impl_start_callback callback, void* cookie)
      __TA_EXCLUDES(tx_lock_) __TA_EXCLUDES(rx_lock_);
  void NetworkDeviceImplStop(network_device_impl_stop_callback callback, void* cookie)
      __TA_EXCLUDES(tx_lock_) __TA_EXCLUDES(rx_lock_);
  void NetworkDeviceImplGetInfo(device_impl_info_t* out_info);
  void NetworkDeviceImplQueueTx(const tx_buffer_t* buffers_list, size_t buffers_count);
  void NetworkDeviceImplQueueRxSpace(const rx_space_buffer_t* buffers_list, size_t buffers_count)
      __TA_EXCLUDES(rx_lock_);
  void NetworkDeviceImplPrepareVmo(uint8_t id, zx::vmo vmo,
                                   network_device_impl_prepare_vmo_callback callback, void* cookie)
      __TA_EXCLUDES(vmo_lock_);
  void NetworkDeviceImplReleaseVmo(uint8_t id) __TA_EXCLUDES(vmo_lock_);

  // For NetworkPortProtocol.
  void NetworkPortGetInfo(port_base_info_t* out_info);
  void NetworkPortGetStatus(port_status_t* out_status) __TA_EXCLUDES(status_lock_);
  void NetworkPortSetActive(bool active);
  void NetworkPortGetMac(mac_addr_protocol_t** out_mac_ifc);
  void NetworkPortRemoved();

  // For MacAddrProtocol.
  void MacAddrGetAddress(mac_address_t* out_mac);
  void MacAddrGetFeatures(features_t* out_features);
  void MacAddrSetMode(mac_filter_mode_t mode, const mac_address_t* multicast_macs_list,
                      size_t multicast_macs_count);

 private:
  void SetMacParam(uint32_t param, int32_t value, const mac_address_t* data_buffer,
                   size_t data_size) const;

  zx_status_t DeviceAdd();
  void DeviceRemove();

  compat::SyncInitializedDeviceServer compat_server_;
  compat::BanjoServer net_device_server_{ZX_PROTOCOL_NETWORK_DEVICE_IMPL, this,
                                         &network_device_impl_protocol_ops_};
  fidl::ClientEnd<fuchsia_driver_framework::NodeController> netdev_child_;

  std::atomic<size_t> no_rx_space_ = 0;

  ddk::NetworkDeviceIfcProtocolClient netdevice_;
  mac_addr_protocol_t mac_addr_proto_;
  const ethernet_ifc_protocol_t ethernet_ifc_proto_;
  ddk::EthernetImplProtocolClient ethernet_;

  zx::bti eth_bti_;
  device_impl_info_t info_;
  uint32_t mtu_;
  std::array<uint8_t, MAC_SIZE> mac_;
  port_base_info_t port_info_;
  size_t netbuf_size_;

  std::mutex status_lock_;
  fuchsia_hardware_network::wire::StatusFlags port_status_flags_ __TA_GUARDED(status_lock_);

  std::mutex tx_lock_ __TA_ACQUIRED_AFTER(rx_lock_, vmo_lock_);
  bool tx_started_ __TA_GUARDED(tx_lock_) = false;
  NetbufPool netbuf_pool_ __TA_GUARDED(tx_lock_);
  std::unordered_set<uint32_t> tx_in_flight_ __TA_GUARDED(tx_lock_);

  std::mutex rx_lock_ __TA_ACQUIRED_BEFORE(tx_lock_, vmo_lock_);
  bool rx_started_ __TA_GUARDED(rx_lock_) = false;
  // Use a queue to enforce FIFO ordering. With LIFO ordering, some buffers will sit unused unless
  // the driver hits buffer starvation, which could obscure bugs related to malformed buffers.
  std::queue<rx_space_buffer_t> rx_spaces_ __TA_GUARDED(rx_lock_);
  bool rx_space_queued_ __TA_GUARDED(rx_lock_) = false;

  network::SharedLock vmo_lock_ __TA_ACQUIRED_BEFORE(tx_lock_) __TA_ACQUIRED_AFTER(rx_lock_);
  std::unique_ptr<NetdeviceMigrationVmoStore> vmo_store_ __TA_GUARDED(vmo_lock_);

  friend class NetdeviceMigrationTestHelper;
};

}  // namespace netdevice_migration

#endif  // SRC_CONNECTIVITY_ETHERNET_DRIVERS_ETHERNET_NETDEVICE_MIGRATION_NETDEVICE_MIGRATION_H_
