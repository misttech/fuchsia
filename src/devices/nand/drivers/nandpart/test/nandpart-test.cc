// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/nand/drivers/nandpart/nandpart.h"

#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <lib/ddk/metadata.h>
#include <lib/driver/compat/cpp/banjo_client.h>
#include <lib/driver/testing/cpp/driver_test.h>

#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace nand::testing {

class FakeNand : public ddk::NandProtocol<FakeNand> {
 public:
  compat::DeviceServer::BanjoConfig GetBanjoConfig() {
    compat::DeviceServer::BanjoConfig config{ZX_PROTOCOL_NAND};
    config.callbacks[ZX_PROTOCOL_NAND] = banjo_server_.callback();
    return config;
  }

  // Nand protocol implementation.
  void NandQuery(nand_info_t* info_out, size_t* nand_op_size_out) {
    *info_out = nand_info_t{
        .page_size = 1,
        .pages_per_block = 1,
        .num_blocks = 1,
        .ecc_bits = 0,
        .oob_size = 0,
    };
    *nand_op_size_out = 0;
  }

  void NandQueue(nand_operation_t* op, nand_queue_callback completion_cb, void* cookie) { FAIL(); }

  zx_status_t NandGetFactoryBadBlockList(uint32_t* bad_blocks, size_t bad_block_len,
                                         size_t* num_bad_blocks) {
    return ZX_ERR_NOT_SUPPORTED;
  }

 private:
  compat::BanjoServer banjo_server_{ZX_PROTOCOL_NAND, this, &nand_protocol_ops_};
};

class NandpartTestEnvironment : public fdf_testing::Environment {
 public:
  void Init(const fuchsia_hardware_nand::Config& nand_config,
            const fuchsia_boot_metadata::PartitionMap& partition_map) {
    device_server_.Initialize("default", std::nullopt, nand_.GetBanjoConfig());

    {
      fit::result persisted = fidl::Persist(nand_config);
      ASSERT_TRUE(persisted.is_ok());
      device_server_.AddMetadata(DEVICE_METADATA_PRIVATE, persisted.value().data(),
                                 persisted.value().size());
    }

    {
      fit::result persisted = fidl::Persist(partition_map);
      ASSERT_TRUE(persisted.is_ok());
      device_server_.AddMetadata(DEVICE_METADATA_PARTITION_MAP, persisted.value().data(),
                                 persisted.value().size());
    }
  }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    if (zx_status_t status = device_server_.Serve(dispatcher, &to_driver_vfs); status != ZX_OK) {
      return zx::error(status);
    }

    return zx::ok();
  }

 private:
  FakeNand nand_;
  compat::DeviceServer device_server_;
};

class FixtureConfig final {
 public:
  using DriverType = Driver;
  using EnvironmentType = NandpartTestEnvironment;
};

class NandpartTest : public ::testing::Test {
 public:
  void TearDown() override { ASSERT_OK(driver_test_.StopDriver()); }

 protected:
  void StartDriver(const fuchsia_hardware_nand::Config& nand_config,
                   const fuchsia_boot_metadata::PartitionMap& partition_map) {
    driver_test_.RunInEnvironmentTypeContext(
        [&](NandpartTestEnvironment& env) { env.Init(nand_config, partition_map); });
    ASSERT_OK(driver_test_.StartDriver());
  }

  template <typename BanjoClient>
  BanjoClient ConnectToBanjo(std::string_view partition_name) {
    static const uint64_t kProcessKoid = compat::internal::GetKoid();

    zx::result compat_client_end =
        driver_test_.Connect<fuchsia_driver_compat::Service::Device>(partition_name);
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

 private:
  fdf_testing::ForegroundDriverTest<FixtureConfig> driver_test_;
};

// Verify that the nandpart driver creates a nandpart device when given a single partition.
TEST_F(NandpartTest, OnePartition) {
  static const fuchsia_hardware_nand::Config kNandConfig(
      {.bad_block_config = fuchsia_hardware_nand::BadBlockConfig({
           .type = fuchsia_hardware_nand::BadBlockConfigType::kAmlogicUboot,
           .table_start_block = 0,
           .table_end_block = 0,
       })});

  static const fuchsia_boot_metadata::PartitionMap kPartitionMap(
      {.block_count = 1,
       .block_size = 1,
       .partitions = std::vector{{fuchsia_boot_metadata::Partition({
           .first_block = 0,
           .last_block = 0,
           .name = "partition 1",
       })}}});

  StartDriver(kNandConfig, kPartitionMap);

  // Verify that the nandpart driver created a new nandpart device that serves the nand and bad
  // block banjo protocols.
  ConnectToBanjo<ddk::NandProtocolClient>("partition 1");
  ConnectToBanjo<ddk::BadBlockProtocolClient>("partition 1");
}

}  // namespace nand::testing
