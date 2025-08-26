// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_NAND_DRIVERS_BROKER_TEST_BROKER_TEST_H_
#define SRC_DEVICES_NAND_DRIVERS_BROKER_TEST_BROKER_TEST_H_

#include <fidl/fuchsia.nand/cpp/wire.h>

#include <gtest/gtest.h>

#include "parent.h"

namespace nand_broker_test {

template <typename Protocol>
using MaybeOwned = std::variant<fidl::ClientEnd<Protocol>, fidl::UnownedClientEnd<Protocol>>;

template <typename Protocol>
static fidl::UnownedClientEnd<Protocol> GetMaybeOwned(const MaybeOwned<Protocol>& variant) {
  return std::visit(
      [](auto&& arg) -> fidl::UnownedClientEnd<Protocol> {
        using T = std::decay_t<decltype(arg)>;
        if constexpr (std::is_same_v<T, fidl::ClientEnd<Protocol>>) {
          return arg;
        } else if constexpr (std::is_same_v<T, fidl::UnownedClientEnd<Protocol>>) {
          return arg;
        }
      },
      variant);
}

// The device under test.
class NandDevice {
 public:
  static constexpr uint32_t kInMemoryPages = 20;

  NandDevice(ParentDevice& parent, MaybeOwned<fuchsia_device::Controller> controller,
             fidl::ClientEnd<fuchsia_nand::Broker> broker, uint32_t num_blocks, bool full_device)
      : parent_(parent),
        controller_(std::move(controller)),
        broker_(std::move(broker)),
        num_blocks_(num_blocks),
        full_device_(full_device) {}

  fidl::UnownedClientEnd<fuchsia_device::Controller> controller() {
    return GetMaybeOwned(controller_);
  }

  // Provides a channel to issue fidl calls.
  fidl::UnownedClientEnd<fuchsia_nand::Broker> channel() { return broker_.borrow(); }

  // Wrappers for "queue" operations that take care of preserving the vmo's handle
  // and translating the request to the desired block range on the actual device.
  zx_status_t Read(const zx::vmo& vmo, fuchsia_nand::wire::BrokerRequestData request);
  zx_status_t Write(const zx::vmo& vmo, fuchsia_nand::wire::BrokerRequestData request);
  zx_status_t ReadBytes(const zx::vmo& vmo, fuchsia_nand::wire::BrokerRequestDataBytes request);
  zx_status_t WriteBytes(const zx::vmo& vmo, fuchsia_nand::wire::BrokerRequestDataBytes request);
  zx_status_t Erase(fuchsia_nand::wire::BrokerRequestData request);

  // Erases a given block number.
  zx_status_t EraseBlock(uint32_t block_num);

  // Verifies that the buffer pointed to by the operation's vmo contains the given
  // pattern for the desired number of pages, skipping the pages before start.
  bool CheckPattern(uint8_t expected, int start, int num_pages, const void* memory) const;

  const fuchsia_hardware_nand::wire::Info& Info() const { return parent_.Info(); }

  uint32_t PageSize() const { return parent_.Info().page_size; }
  uint32_t OobSize() const { return parent_.Info().oob_size; }
  uint32_t BlockSize() const { return parent_.Info().pages_per_block; }
  uint32_t NumBlocks() const { return num_blocks_; }
  uint32_t NumPages() const { return NumBlocks() * BlockSize(); }
  uint32_t MaxBufferSize() const { return kInMemoryPages * (PageSize() + OobSize()); }

  // True when the whole device under test can be modified.
  bool IsFullDevice() const { return full_device_; }

 private:
  ParentDevice& parent_;
  MaybeOwned<fuchsia_device::Controller> controller_;
  fidl::ClientEnd<fuchsia_nand::Broker> broker_;
  const uint32_t num_blocks_;
  const bool full_device_;
};

class NandBrokerTest : public ::testing::Test {
 public:
  static void SetParent(ParentDevice parent) { parent_ = std::move(parent); }

  void SetUp() override;

 protected:
  NandDevice& device() {
    EXPECT_TRUE(device_.has_value());
    return device_.value();
  }

 private:
  static std::optional<ParentDevice> parent_;

  std::optional<NandDevice> device_;
};

}  // namespace nand_broker_test

#endif  // SRC_DEVICES_NAND_DRIVERS_BROKER_TEST_BROKER_TEST_H_
