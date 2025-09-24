// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "bootpart.h"

#include <lib/driver/compat/cpp/banjo_client.h>
#include <lib/driver/metadata/cpp/metadata_server.h>
#include <lib/driver/testing/cpp/driver_test.h>

#include <algorithm>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace bootpart {

class FakeBlockDevice : public ddk::BlockImplProtocol<FakeBlockDevice> {
 public:
  FakeBlockDevice() { memset(data_.data(), 0xff, sizeof(data_)); }

  std::span<const char, 240> data() const { return std::span<const char, 240>(data_); }
  bool flushed() const { return flushed_; }

  void BlockImplQuery(block_info_t* out_info, uint64_t* out_block_op_size) {
    out_info->block_count = 24;
    out_info->block_size = 10;
    out_info->max_transfer_size = 10;
    out_info->flags = 0;
    *out_block_op_size = sizeof(block_op_t);
  }

  void BlockImplQueue(block_op_t* txn, block_impl_queue_callback callback, void* cookie) {
    zx_status_t status = ZX_ERR_NOT_SUPPORTED;
    switch (txn->command.opcode) {
      case BLOCK_OPCODE_READ: {
        static uint64_t expected_lba = 0;
        if (txn->rw.length != 1 || txn->rw.offset_dev != expected_lba) {
          status = ZX_ERR_OUT_OF_RANGE;
        } else {
          status = zx_vmo_write(txn->rw.vmo, data_.data() + (txn->rw.offset_dev * 10),
                                txn->rw.offset_vmo * 10, 10);
          expected_lba += 12;  // Expect next read at LBA == 12.
        }
        break;
      }
      case BLOCK_OPCODE_WRITE: {
        static uint64_t expected_lba = 0;
        if (txn->rw.length != 1 || txn->rw.offset_dev != expected_lba) {
          status = ZX_ERR_OUT_OF_RANGE;
        } else {
          status = zx_vmo_read(txn->rw.vmo, data_.data() + (txn->rw.offset_dev * 10),
                               txn->rw.offset_vmo * 10, 10);
          flushed_ = status != ZX_OK && flushed_;
          expected_lba += 12;  // Expect next write at LBA == 12.
        }
        break;
      }
      case BLOCK_OPCODE_FLUSH:
        flushed_ = true;
        status = ZX_OK;
        break;
      default:
        break;
    }

    callback(cookie, status, txn);
  }

  compat::DeviceServer::BanjoConfig GetBanjoConfig() {
    compat::DeviceServer::BanjoConfig config{.default_proto_id = ZX_PROTOCOL_BLOCK_IMPL};
    config.callbacks[ZX_PROTOCOL_BLOCK_IMPL] = banjo_server_.callback();
    return config;
  }

 private:
  std::array<char, 240> data_;  // 24 blocks of 10 bytes each.
  bool flushed_ = true;
  compat::BanjoServer banjo_server_{ZX_PROTOCOL_BLOCK_IMPL, this, &block_impl_protocol_ops_};
};

const std::string kLongName = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

class BootpartTestEnvironment : public fdf_testing::Environment {
 public:
  void Init(fuchsia_boot_metadata::PartitionMap partition_map) {
    device_server_.Initialize("default", std::nullopt, block_device_.GetBanjoConfig());
    partition_map_ = std::move(partition_map);
  }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    EXPECT_OK(device_server_.Serve(dispatcher, &to_driver_vfs));
    EXPECT_OK(partition_map_metadata_server_.Serve(to_driver_vfs, dispatcher, partition_map_));

    return zx::ok();
  }

  const FakeBlockDevice& block_device() const { return block_device_; }

 private:
  FakeBlockDevice block_device_;
  compat::DeviceServer device_server_;
  fdf_metadata::MetadataServer<fuchsia_boot_metadata::PartitionMap> partition_map_metadata_server_;
  fuchsia_boot_metadata::PartitionMap partition_map_;
};

class FixtureConfig final {
 public:
  using DriverType = Driver;
  using EnvironmentType = BootpartTestEnvironment;
};

class BootPartitionTest : public ::testing::Test {
 public:
  static const fuchsia_boot_metadata::PartitionMap kPartitionMap;

  void SetUp() override {
    driver_test_.RunInEnvironmentTypeContext([&](auto& env) { env.Init(kPartitionMap); });

    ASSERT_OK(driver_test_.StartDriver());
  }

  void TearDown() override { ASSERT_OK(driver_test_.StopDriver()); }

 protected:
  Driver& driver() { return *driver_test_.driver(); }

  template <typename BanjoClient>
  BanjoClient ConnectToBanjo(size_t partition_index) {
    static const uint64_t kProcessKoid = compat::internal::GetKoid();

    const std::string instance = std::format("part-{:03}", partition_index);
    zx::result compat_client_end =
        driver_test_.Connect<fuchsia_driver_compat::Service::Device>(instance);
    EXPECT_OK(compat_client_end);
    fidl::WireClient<fuchsia_driver_compat::Device> compat(
        std::move(compat_client_end.value()),
        driver_test_.runtime().GetForegroundDispatcher()->async_dispatcher());

    zx::result<BanjoClient> banjo_client;
    compat->GetBanjoProtocol(BanjoClient::kProtocolId, kProcessKoid)
        .ThenExactlyOnce(
            [&](fidl::WireUnownedResult<fuchsia_driver_compat::Device::GetBanjoProtocol>& result) {
              ASSERT_OK(result.status());
              banjo_client = compat::internal::OnResult<BanjoClient>(result);
              driver_test_.runtime().Quit();
            });

    driver_test_.runtime().Run();
    EXPECT_OK(banjo_client);
    EXPECT_TRUE(banjo_client.value().is_valid());
    return banjo_client.value();
  }

  fdf_testing::ForegroundDriverTest<FixtureConfig>& driver_test() { return driver_test_; }

 private:
  fdf_testing::ForegroundDriverTest<FixtureConfig> driver_test_;
};

const fuchsia_boot_metadata::PartitionMap BootPartitionTest::kPartitionMap = {
    {.partitions = std::vector<fuchsia_boot_metadata::Partition>{
         {{
             .type_guid = {'T', 'T', 'T', 'T', 'T', 'T', 'T', 'T', 'T', 'T', 'T', 'T', 'T', 'T',
                           'T', 'T'},
             .unique_guid = {'I', 'I', 'I', 'I', 'I', 'I', 'I', 'I', 'I', 'I', 'I', 'I', 'I', 'I',
                             'I', 'I'},
             .first_block = 0,
             .last_block = 11,
             .name = "This is partition 0",
         }},
         {{
             .type_guid = {'U', 'U', 'U', 'U', 'U', 'U', 'U', 'U', 'U', 'U', 'U', 'U', 'U', 'U',
                           'U', 'U'},
             .unique_guid = {'J', 'J', 'J', 'J', 'J', 'J', 'J', 'J', 'J', 'J', 'J', 'J', 'J', 'J',
                             'J', 'J'},
             .first_block = 12,
             .last_block = 23,
             .name = kLongName,
         }},
     }}};

TEST_F(BootPartitionTest, BlockPartitionOps) {
  std::span<const fuchsia_boot_metadata::Partition> partitions = kPartitionMap.partitions().value();
  for (size_t i = 0; i < partitions.size(); ++i) {
    const fuchsia_boot_metadata::Partition& partition = partitions[i];
    ddk::BlockPartitionProtocolClient partition_client =
        ConnectToBanjo<ddk::BlockPartitionProtocolClient>(i);

    guid_t guid_type{};
    EXPECT_OK(partition_client.GetGuid(GUIDTYPE_TYPE, &guid_type));
    for (size_t j = 0; j < GUID_LENGTH; j++) {
      EXPECT_EQ(reinterpret_cast<char*>(&guid_type)[j], partition.type_guid()[j]);
    }

    guid_t guid_instance{};
    EXPECT_OK(partition_client.GetGuid(GUIDTYPE_INSTANCE, &guid_instance));
    for (uint32_t j = 0; j < GUID_LENGTH; j++) {
      EXPECT_EQ(reinterpret_cast<char*>(&guid_instance)[j], partition.unique_guid()[j]);
    }

    char name[MAX_PARTITION_NAME_LENGTH];
    EXPECT_OK(partition_client.GetName(name, sizeof(name)));
    EXPECT_EQ(std::string(name), partition.name());

    char name_short[33];
    EXPECT_OK(partition_client.GetName(name_short, 33));
    EXPECT_EQ(std::string(name_short), partition.name());

    EXPECT_NE(partition_client.GetName(name_short, 32), ZX_OK);
  }
}

TEST_F(BootPartitionTest, BlockImplOpsPassedThrough) {
  std::span<const fuchsia_boot_metadata::Partition> partitions = kPartitionMap.partitions().value();
  for (size_t i = 0; i < partitions.size(); ++i) {
    ddk::BlockImplProtocolClient block_client = ConnectToBanjo<ddk::BlockImplProtocolClient>(i);

    block_info_t info{};
    uint64_t block_op_size = 0;
    block_client.Query(&info, &block_op_size);

    EXPECT_EQ(info.block_count, 12u);
    EXPECT_EQ(info.block_size, 10u);
    EXPECT_EQ(info.max_transfer_size, 10u);
    EXPECT_EQ(block_op_size, sizeof(block_op_t));

    auto block_callback = [](void*, zx_status_t status, block_op_t*) { EXPECT_OK(status); };

    zx::vmo vmo;
    ASSERT_OK(zx::vmo::create(10, 0, &vmo));

    char buffer[10];
    strncpy(buffer, "Test data", sizeof(buffer));
    EXPECT_OK(vmo.write(buffer, 0, sizeof(buffer)));

    block_op_t txn{
        .rw =
            {
                .command = {.opcode = BLOCK_OPCODE_WRITE, .flags = 0},
                .vmo = vmo.get(),
                .length = 1,
                .offset_dev = 0,
                .offset_vmo = 0,
            },
    };
    block_client.Queue(&txn, block_callback, nullptr);
    driver_test().RunInEnvironmentTypeContext([](BootpartTestEnvironment& env) {
      EXPECT_FALSE(env.block_device().flushed());  // FakeBlockDevice operates synchronously.
    });

    txn = {
        .command = {.opcode = BLOCK_OPCODE_FLUSH, .flags = 0},
    };
    block_client.Queue(&txn, block_callback, nullptr);
    driver_test().RunInEnvironmentTypeContext([](BootpartTestEnvironment& env) {
      EXPECT_TRUE(env.block_device().flushed());  // FakeBlockDevice operates synchronously.
      std::span data = env.block_device().data();
      auto end = std::ranges::find(data, '\0');
      ASSERT_NE(end, data.end());
      EXPECT_EQ(std::string(data.begin(), end), "Test data");
    });

    txn = {
        .rw =
            {
                .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
                .vmo = vmo.get(),
                .length = 1,
                .offset_dev = 0,
                .offset_vmo = 0,
            },
    };
    block_client.Queue(&txn, block_callback, nullptr);

    EXPECT_OK(vmo.read(buffer, 0, sizeof(buffer)));
    EXPECT_STREQ(buffer, "Test data");
  }
}

}  // namespace bootpart
