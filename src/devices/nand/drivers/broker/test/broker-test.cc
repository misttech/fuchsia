// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "broker-test.h"

#include <fcntl.h>
#include <fidl/fuchsia.device/cpp/wire.h>
#include <fidl/fuchsia.nand/cpp/wire.h>
#include <lib/device-watcher/cpp/device-watcher.h>
#include <lib/fdio/cpp/caller.h>
#include <lib/fdio/watcher.h>
#include <lib/fidl/cpp/wire/channel.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/zx/vmo.h>
#include <stdio.h>
#include <stdlib.h>
#include <zircon/syscalls.h>

#include <algorithm>
#include <cstddef>

#include <fbl/algorithm.h>
#include <fbl/unique_fd.h>
#include <gmock/gmock.h>

#include "parent.h"
#include "src/lib/testing/predicates/status.h"

namespace {

constexpr uint32_t kMinOobSize = 4;
constexpr uint32_t kMinBlockSize = 4;
constexpr uint32_t kMinNumBlocks = 5;

}  // namespace

namespace nand_broker_test {

std::optional<ParentDevice> NandBrokerTest::parent_ = std::nullopt;

void NandBrokerTest::SetUp() {
  ASSERT_TRUE(parent_.has_value());
  ParentDevice& parent = parent_.value();

  MaybeOwned<fuchsia_device::Controller> controller;
  if (parent.IsBroker()) {
    controller = parent.controller().borrow();
  } else {
    static constexpr std::string_view kBroker = "nand-broker.cm";
    const fidl::WireResult result = fidl::WireCall(parent.controller().borrow())
                                        ->Rebind(fidl::StringView::FromExternal(kBroker));
    ASSERT_OK(result.status());
    ASSERT_TRUE(result.value().is_ok());

    // Get the new child.
    fbl::unique_fd dir(open(parent.Path(), O_RDONLY | O_DIRECTORY));
    zx::result channel =
        device_watcher::RecursiveWaitForFile(dir.get(), "broker/device_controller");
    ASSERT_OK(channel);
    controller = fidl::ClientEnd<fuchsia_device::Controller>(std::move(channel.value()));
  }

  auto [broker_client, broker_server] = fidl::Endpoints<fuchsia_nand::Broker>::Create();
  fidl::OneWayStatus status =
      fidl::WireCall(GetMaybeOwned(controller))->ConnectToDeviceFidl(broker_server.TakeChannel());
  ASSERT_OK(status.status());

  if (parent.IsExternal()) {
    // This looks like using code under test to setup the test, but this
    // path is for external devices, not really the broker. The issue is that
    // ParentDevice cannot query a nand device for the actual parameters.
    const fidl::WireResult result = fidl::WireCall(broker_client)->GetInfo();
    ASSERT_OK(result.status());
    ASSERT_OK(result.value().status);
    parent.SetInfo(*result.value().info);
  }

  ASSERT_GE(parent.Info().oob_size, kMinOobSize);
  ASSERT_GE(parent.Info().pages_per_block, kMinBlockSize);
  ASSERT_GE(parent.NumBlocks(), kMinNumBlocks);
  ASSERT_LE(parent.NumBlocks() + parent.FirstBlock(), parent.Info().num_blocks);

  uint32_t num_blocks = parent.NumBlocks();
  bool full_device = num_blocks == parent.Info().num_blocks;
  if (!full_device) {
    // Not using the whole device, don't need to test all limits.
    num_blocks = std::min(num_blocks, kMinNumBlocks);
  }
  device_.emplace(parent, std::move(controller), std::move(broker_client), num_blocks, full_device);
}

zx_status_t NandDevice::Read(const zx::vmo& vmo, fuchsia_nand::wire::BrokerRequestData request) {
  if (!full_device_) {
    request.offset_nand = request.offset_nand + parent_.FirstBlock() * BlockSize();
    ZX_DEBUG_ASSERT(request.offset_nand < NumPages());
    ZX_DEBUG_ASSERT(request.offset_nand + request.length <= NumPages());
  }

  zx_status_t status = vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &request.vmo);
  if (status != ZX_OK) {
    return status;
  }
  const fidl::WireResult result = fidl::WireCall(channel())->Read(std::move(request));
  if (!result.ok()) {
    return result.status();
  }
  const fidl::WireResponse response = result.value();
  return response.status;
}

zx_status_t NandDevice::ReadBytes(const zx::vmo& vmo,
                                  fuchsia_nand::wire::BrokerRequestDataBytes request) {
  if (!full_device_) {
    request.offset_nand = request.offset_nand +
                          static_cast<uint64_t>(parent_.FirstBlock()) * BlockSize() * PageSize();
    ZX_DEBUG_ASSERT(request.offset_nand < NumPages());
    ZX_DEBUG_ASSERT(request.offset_nand + request.length <= NumPages());
  }

  zx_status_t status = vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &request.vmo);
  if (status != ZX_OK) {
    return status;
  }
  const fidl::WireResult result = fidl::WireCall(channel())->ReadBytes(std::move(request));
  if (!result.ok()) {
    return result.status();
  }
  const fidl::WireResponse response = result.value();
  return response.status;
}

zx_status_t NandDevice::Write(const zx::vmo& vmo, fuchsia_nand::wire::BrokerRequestData request) {
  if (!full_device_) {
    request.offset_nand = request.offset_nand + parent_.FirstBlock() * BlockSize();
    ZX_DEBUG_ASSERT(request.offset_nand < static_cast<uint64_t>(NumPages()) * PageSize());
    ZX_DEBUG_ASSERT(request.offset_nand + request.length <=
                    static_cast<uint64_t>(NumPages()) * PageSize());
  }

  zx_status_t status = vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &request.vmo);
  if (status != ZX_OK) {
    return status;
  }
  const fidl::WireResult result = fidl::WireCall(channel())->Write(std::move(request));
  if (!result.ok()) {
    return result.status();
  }
  const fidl::WireResponse response = result.value();
  return response.status;
}

zx_status_t NandDevice::WriteBytes(const zx::vmo& vmo,
                                   fuchsia_nand::wire::BrokerRequestDataBytes request) {
  if (!full_device_) {
    request.offset_nand = request.offset_nand +
                          static_cast<uint64_t>(parent_.FirstBlock()) * BlockSize() * PageSize();
    ZX_DEBUG_ASSERT(request.offset_nand < static_cast<uint64_t>(NumPages()) * PageSize());
    ZX_DEBUG_ASSERT(request.offset_nand + request.length <=
                    static_cast<uint64_t>(NumPages()) * PageSize());
  }

  zx_status_t status = vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &request.vmo);
  if (status != ZX_OK) {
    return status;
  }
  const fidl::WireResult result = fidl::WireCall(channel())->WriteBytes(std::move(request));
  if (!result.ok()) {
    return result.status();
  }
  const fidl::WireResponse response = result.value();
  return response.status;
}

zx_status_t NandDevice::Erase(fuchsia_nand::wire::BrokerRequestData request) {
  if (!full_device_) {
    request.offset_nand = request.offset_nand + parent_.FirstBlock();
    ZX_DEBUG_ASSERT(request.offset_nand < NumBlocks());
    ZX_DEBUG_ASSERT(request.offset_nand + request.length <= NumBlocks());
  }

  const fidl::WireResult result = fidl::WireCall(channel())->Erase(std::move(request));
  if (!result.ok()) {
    return result.status();
  }
  const fidl::WireResponse response = result.value();
  return response.status;
}

zx_status_t NandDevice::EraseBlock(uint32_t block_num) {
  return Erase({
      .length = 1,
      .offset_nand = block_num,
  });
}

bool NandDevice::CheckPattern(uint8_t expected, int start, int num_pages,
                              const void* memory) const {
  const uint8_t* buffer = reinterpret_cast<const uint8_t*>(memory) + PageSize() * start;
  for (uint32_t i = 0; i < PageSize() * num_pages; i++) {
    if (buffer[i] != expected) {
      return false;
    }
  }
  return true;
}

TEST_F(NandBrokerTest, TrivialLifetime) {}

TEST_F(NandBrokerTest, Query) {
  {
    const fidl::WireResult result = fidl::WireCall(device().channel())->GetInfo();
    ASSERT_OK(result.status());
    const fidl::WireResponse response = result.value();
    ASSERT_OK(response.status);
    const fuchsia_hardware_nand::wire::Info& info = *response.info;

    EXPECT_EQ(device().Info().page_size, info.page_size);
    EXPECT_EQ(device().Info().oob_size, info.oob_size);
    EXPECT_EQ(device().Info().pages_per_block, info.pages_per_block);
    EXPECT_EQ(device().Info().num_blocks, info.num_blocks);
    EXPECT_EQ(device().Info().ecc_bits, info.ecc_bits);
    EXPECT_EQ(device().Info().nand_class, info.nand_class);
  }
}

TEST_F(NandBrokerTest, ReadWriteLimits) {
  fzl::VmoMapper mapper;
  zx::vmo vmo;
  ASSERT_OK(mapper.CreateAndMap(device().MaxBufferSize(), ZX_VM_PERM_READ | ZX_VM_PERM_WRITE,
                                nullptr, &vmo));

  EXPECT_EQ(ZX_ERR_OUT_OF_RANGE, device().Read(vmo, {}));
  EXPECT_EQ(ZX_ERR_OUT_OF_RANGE, device().Write(vmo, {}));

  if (device().IsFullDevice()) {
    {
      auto request = [this]() -> fuchsia_nand::wire::BrokerRequestData {
        return {
            .length = 1,
            .offset_nand = device().NumPages(),
        };
      };

      EXPECT_EQ(ZX_ERR_OUT_OF_RANGE, device().Read(vmo, request()));
      EXPECT_EQ(ZX_ERR_OUT_OF_RANGE, device().Write(vmo, request()));
    }

    {
      auto request = [this]() -> fuchsia_nand::wire::BrokerRequestData {
        return {
            .length = 2,
            .offset_nand = device().NumPages() - 1,
        };
      };

      EXPECT_EQ(ZX_ERR_OUT_OF_RANGE, device().Read(vmo, request()));
      EXPECT_EQ(ZX_ERR_OUT_OF_RANGE, device().Write(vmo, request()));
    }
  }

  auto request = [this]() -> fuchsia_nand::wire::BrokerRequestData {
    return {
        .length = 1,
        .offset_nand = device().NumPages() - 1,
    };
  };

  EXPECT_EQ(ZX_ERR_BAD_HANDLE, device().Read(vmo, request()));
  EXPECT_EQ(ZX_ERR_BAD_HANDLE, device().Write(vmo, request()));

  auto request_with_data_vmo = [request]() {
    fuchsia_nand::wire::BrokerRequestData base = request();
    base.data_vmo = true;
    return base;
  };

  EXPECT_OK(device().Read(vmo, request_with_data_vmo()));
  EXPECT_OK(device().Write(vmo, request_with_data_vmo()));
}

TEST_F(NandBrokerTest, EraseLimits) {
  EXPECT_EQ(ZX_ERR_OUT_OF_RANGE, device().Erase({}));

  if (device().IsFullDevice()) {
    EXPECT_EQ(ZX_ERR_OUT_OF_RANGE, device().Erase({
                                       .length = 1,
                                       .offset_nand = device().NumBlocks(),
                                   }));

    EXPECT_EQ(ZX_ERR_OUT_OF_RANGE, device().Erase({
                                       .length = 2,
                                       .offset_nand = device().NumBlocks() - 1,
                                   }));
  }

  EXPECT_OK(device().Erase({
      .length = 1,
      .offset_nand = device().NumBlocks() - 1,
  }));
}

TEST_F(NandBrokerTest, ReadWrite) {
  ASSERT_OK(device().EraseBlock(0));

  fzl::VmoMapper mapper;
  zx::vmo vmo;
  ASSERT_OK(mapper.CreateAndMap(device().MaxBufferSize(), ZX_VM_PERM_READ | ZX_VM_PERM_WRITE,
                                nullptr, &vmo));
  memset(mapper.start(), 0x55, mapper.size());

  auto request = []() -> fuchsia_nand::wire::BrokerRequestData {
    return {
        .length = 4,
        .offset_nand = 4,
        .data_vmo = true,
    };
  };

  ASSERT_OK(device().Write(vmo, request()));

  memset(mapper.start(), 0, mapper.size());

  ASSERT_OK(device().Read(vmo, request()));
  ASSERT_TRUE(device().CheckPattern(0x55, 0, 4, mapper.start()));
}

TEST_F(NandBrokerTest, ReadWriteOob) {
  ASSERT_OK(device().EraseBlock(0));

  fzl::VmoMapper mapper;
  zx::vmo vmo;
  ASSERT_OK(mapper.CreateAndMap(device().MaxBufferSize(), ZX_VM_PERM_READ | ZX_VM_PERM_WRITE,
                                nullptr, &vmo));
  const char desired[] = {'a', 'b', 'c', 'd'};
  memcpy(mapper.start(), desired, sizeof(desired));

  auto request = []() -> fuchsia_nand::wire::BrokerRequestData {
    return {
        .length = 1,
        .offset_nand = 2,
        .oob_vmo = true,
    };
  };

  ASSERT_OK(device().Write(vmo, request()));

  memset(mapper.start(), 0, device().OobSize() * 2);

  ASSERT_OK(device().Read(vmo, [request]() {
    fuchsia_nand::wire::BrokerRequestData base = request();
    base.length = 2;
    base.offset_nand = 1;
    return base;
  }()));

  // The "second page" has the data of interest.
  ASSERT_EQ(0, memcmp(reinterpret_cast<char*>(mapper.start()) + device().OobSize(), desired,
                      sizeof(desired)));
}

TEST_F(NandBrokerTest, ReadWriteDataAndOob) {
  ASSERT_OK(device().EraseBlock(0));

  fzl::VmoMapper mapper;
  zx::vmo vmo;
  ASSERT_OK(mapper.CreateAndMap(device().MaxBufferSize(), ZX_VM_PERM_READ | ZX_VM_PERM_WRITE,
                                nullptr, &vmo));

  char* buffer = reinterpret_cast<char*>(mapper.start());
  memset(buffer, 0x55, device().PageSize() * 2);
  memset(buffer + device().PageSize() * 2, 0xaa, device().OobSize() * 2);

  auto request = []() -> fuchsia_nand::wire::BrokerRequestData {
    return {
        .length = 2,
        .offset_nand = 2,
        .offset_oob_vmo = 2,  // OOB is right after data.
        .data_vmo = true,
        .oob_vmo = true,
    };
  };

  ASSERT_OK(device().Write(vmo, request()));

  memset(buffer, 0, device().PageSize() * 4);
  ASSERT_OK(device().Read(vmo, request()));

  // Verify data.
  ASSERT_TRUE(device().CheckPattern(0x55, 0, 2, buffer));

  // Verify OOB.
  memset(buffer, 0xaa, device().PageSize());
  ASSERT_EQ(0, memcmp(buffer + device().PageSize() * 2, buffer, device().OobSize() * 2));
}

TEST_F(NandBrokerTest, Erase) {
  fzl::VmoMapper mapper;
  zx::vmo vmo;
  ASSERT_OK(mapper.CreateAndMap(device().MaxBufferSize(), ZX_VM_PERM_READ | ZX_VM_PERM_WRITE,
                                nullptr, &vmo));

  memset(mapper.start(), 0x55, mapper.size());

  auto request = [this]() -> fuchsia_nand::wire::BrokerRequestData {
    return {
        .length = kMinBlockSize,
        .offset_nand = device().BlockSize(),
        .data_vmo = true,
    };
  };
  ASSERT_OK(device().Write(vmo, request()));

  auto request_with_double_offset = [request]() {
    fuchsia_nand::wire::BrokerRequestData base = request();
    base.offset_nand *= 2;
    return base;
  };
  ASSERT_OK(device().Write(vmo, request_with_double_offset()));

  ASSERT_OK(device().EraseBlock(1));
  ASSERT_OK(device().EraseBlock(2));

  ASSERT_OK(device().Read(vmo, request_with_double_offset()));
  ASSERT_TRUE(device().CheckPattern(0xff, 0, kMinBlockSize, mapper.start()));

  ASSERT_OK(device().Read(vmo, request()));
  ASSERT_TRUE(device().CheckPattern(0xff, 0, kMinBlockSize, mapper.start()));
}

TEST_F(NandBrokerTest, ReadWriteDataBytes) {
  ASSERT_OK(device().EraseBlock(0));

  fzl::VmoMapper mapper;
  zx::vmo vmo;
  ASSERT_OK(mapper.CreateAndMap(device().MaxBufferSize(), ZX_VM_PERM_READ | ZX_VM_PERM_WRITE,
                                nullptr, &vmo));

  std::span<uint8_t> buffer(reinterpret_cast<uint8_t*>(mapper.start()), mapper.size());
  memset(buffer.data(), 0x55, 2);

  auto request = []() -> fuchsia_nand::wire::BrokerRequestDataBytes {
    return {
        .length = 2,
        .offset_nand = 2,
    };
  };

  ASSERT_OK(device().WriteBytes(vmo, request()));

  memset(buffer.data(), 0, 4);
  ASSERT_OK(device().ReadBytes(vmo, request()));

  // Verify data.
  ASSERT_THAT(buffer.subspan(0, 2),
              ::testing::ElementsAre(static_cast<uint8_t>(0x55), static_cast<uint8_t>(0x55)));
}

}  // namespace nand_broker_test
