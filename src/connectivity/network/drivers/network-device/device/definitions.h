// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_DEFINITIONS_H_
#define SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_DEFINITIONS_H_

#include <fidl/fuchsia.hardware.network.driver/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.network/cpp/wire.h>
#include <zircon/types.h>

#include <array>
#include <iterator>
#include <type_traits>

#include <fbl/intrusive_double_list.h>
#include <fbl/intrusive_single_list.h>

#include "src/lib/vmo_store/vmo_store.h"

namespace network {
namespace netdev = fuchsia_hardware_network;
namespace netdriver = fuchsia_hardware_network_driver;
constexpr uint16_t kMaxFifoDepth = ZX_FIFO_MAX_SIZE_BYTES / sizeof(uint16_t);

namespace internal {
template <typename T>
using BufferParts = std::array<T, netdriver::kMaxBufferParts>;
using netdev::wire::VmoId;
struct AllVmosTag;
struct PreparedVmosTag;
struct DataVmoMeta
    : public fbl::ContainableBaseClasses<
          fbl::SinglyLinkedListable<DataVmoMeta*, fbl::NodeOptions::AllowMove, AllVmosTag>,
          fbl::DoublyLinkedListable<DataVmoMeta*, fbl::NodeOptions::AllowMove, PreparedVmosTag>> {
  VmoId id;
  uint16_t num_rx_buffers;
};
using DataVmoStore = vmo_store::VmoStore<vmo_store::SlabStorage<uint8_t, DataVmoMeta>>;
using AllDataVmos = fbl::SinglyLinkedList<DataVmoStore::Meta*, AllVmosTag>;
using PreparedDataVmos = fbl::DoublyLinkedList<DataVmoStore::Meta*, PreparedVmosTag>;

template <typename Iter>
concept DataVmoIter = std::forward_iterator<Iter> &&
                      std::is_same_v<typename std::iterator_traits<Iter>::value_type, DataVmoMeta>;
}  // namespace internal

}  // namespace network

#endif  // SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_DEFINITIONS_H_
