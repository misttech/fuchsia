// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_DEFINITIONS_H_
#define SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_DEFINITIONS_H_

#include <fidl/fuchsia.hardware.network.driver/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.network/cpp/wire.h>
#include <zircon/types.h>

#include <array>

#include <fbl/intrusive_double_list.h>

#include "src/lib/vmo_store/vmo_store.h"

namespace network {
namespace netdev = fuchsia_hardware_network;
namespace netdriver = fuchsia_hardware_network_driver;
constexpr uint16_t kMaxFifoDepth = ZX_FIFO_MAX_SIZE_BYTES / sizeof(uint16_t);

namespace internal {
template <typename T>
using BufferParts = std::array<T, netdriver::kMaxBufferParts>;
using netdev::wire::VmoId;

struct DataVmoMeta : public fbl::DoublyLinkedListable<DataVmoMeta*, fbl::NodeOptions::AllowMove> {
  const VmoId id;
  const uint16_t num_rx_buffers;
  bool tx_registered;
  bool prepared;
};
using DataVmoStore = vmo_store::VmoStore<vmo_store::SlabStorage<uint8_t, DataVmoMeta>>;
using DataVmoList = fbl::DoublyLinkedList<DataVmoMeta*>;
}  // namespace internal

}  // namespace network

#endif  // SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_DEFINITIONS_H_
