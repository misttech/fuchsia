// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/pci/drivers/pciroot-banjo-adapter/adapter.h"

#include <fidl/fuchsia.hardware.pci.root/cpp/fidl.h>
#include <fidl/fuchsia.hardware.pci/cpp/fidl.h>
#include <lib/driver/compat/cpp/banjo_server.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/metadata/cpp/metadata.h>
#include <lib/driver/metadata/cpp/metadata_server.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fake-msi/msi.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/resource.h>
#include <lib/zx/vmo.h>

#include <memory>
#include <optional>

#include <bind/fuchsia/cpp/bind.h>
#include <gtest/gtest.h>

#include "src/devices/pci/drivers/pci/test/fakes/fake_pciroot.h"
#include "src/lib/testing/predicates/status.h"

namespace pciroot_banjo_adapter::testing {

class PcirootBanjoAdapterTestEnvironment : public fdf_testing::Environment {
 public:
  PcirootBanjoAdapterTestEnvironment() = default;

  void InitFake(bool is_extended) {
    fake_pciroot_ = std::make_unique<FakePciroot>(0, 1, is_extended);
  }

  FakePciroot& fake() { return *fake_pciroot_; }

  void SetMetadata(fuchsia_hardware_pci::BoardConfiguration metadata) {
    metadata_ = std::move(metadata);
  }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    compat::DeviceServer::BanjoConfig banjo_config{.default_proto_id = ZX_PROTOCOL_PCIROOT};
    banjo_server_.emplace(ZX_PROTOCOL_PCIROOT, fake_pciroot_.get(), fake_pciroot_->proto()->ops);
    banjo_config.callbacks[ZX_PROTOCOL_PCIROOT] = banjo_server_->callback();

    device_server_.Initialize("default", std::nullopt, std::move(banjo_config));
    if (zx_status_t status = device_server_.Serve(dispatcher, &to_driver_vfs); status != ZX_OK) {
      return zx::error(status);
    }

    if (metadata_.has_value()) {
      if (zx::result result =
              metadata_server_.Serve(to_driver_vfs, dispatcher, std::move(*metadata_));
          result.is_error()) {
        return result.take_error();
      }
    }

    return zx::ok();
  }

 private:
  std::unique_ptr<FakePciroot> fake_pciroot_ = std::make_unique<FakePciroot>();
  std::optional<compat::BanjoServer> banjo_server_;
  compat::DeviceServer device_server_;
  fdf_metadata::MetadataServer<fuchsia_hardware_pci::BoardConfiguration> metadata_server_;
  std::optional<fuchsia_hardware_pci::BoardConfiguration> metadata_;
};

class FixtureConfig final {
 public:
  using DriverType = Adapter;
  using EnvironmentType = PcirootBanjoAdapterTestEnvironment;
};

class PcirootBanjoAdapterTest : public ::testing::Test {
 protected:
  void TearDown() override {
    if (driver_started_) {
      ASSERT_OK(driver_test_.StopDriver());
    }
  }

  struct Clients {
    fidl::SyncClient<fuchsia_hardware_pci_root::Root> root;
  };

  Clients StartDriver() {
    EXPECT_EQ(driver_test_.StartDriver().status_value(), ZX_OK);
    driver_started_ = true;
    auto root_client_end = driver_test_.Connect<fuchsia_hardware_pci_root::RootService::Root>();
    EXPECT_TRUE(root_client_end.is_ok());
    return Clients{
        .root =
            fidl::SyncClient<fuchsia_hardware_pci_root::Root>(std::move(root_client_end.value())),
    };
  }

  void StopDriver() {
    driver_started_ = false;
    ASSERT_OK(driver_test_.StopDriver());
  }

  bool driver_started_ = false;
  fdf_testing::BackgroundDriverTest<FixtureConfig> driver_test_;
};

TEST_F(PcirootBanjoAdapterTest, StartFailsWhenGetPciPlatformInfoFails) {
  driver_test_.RunInEnvironmentTypeContext(
      [](auto& env) { env.fake().enable_get_pci_platform_info(false); });
  EXPECT_STATUS(driver_test_.StartDriver().status_value(), ZX_ERR_NOT_SUPPORTED);
}

TEST_F(PcirootBanjoAdapterTest, PlatformInfoNonEmpty) {
  driver_test_.RunInEnvironmentTypeContext([](auto& env) {
    env.fake().routing_entries().push_back(pci_irq_routing_entry_t{
        .port_device_id = 0,
        .port_function_id = 0,
        .device_id = 1,
        .pins = {10, 11, 12, 13},
    });
    env.fake().acpi_devices().push_back(pci_bdf_t{.bus_id = 0, .device_id = 1, .function_id = 0});
  });

  Clients clients = StartDriver();
  auto result = clients.root->GetPlatformInfo();
  ASSERT_TRUE(result.is_ok());
  const auto& info = result->info();
  EXPECT_EQ(info.name(), "fakroot");
  EXPECT_EQ(info.start_bus_num(), 0u);
  EXPECT_EQ(info.end_bus_num(), 1u);
  EXPECT_EQ(info.segment_group(), 0u);

  ASSERT_EQ(info.irq_routing().size(), 1u);
  EXPECT_EQ(info.irq_routing()[0].device_id(), 1u);
  EXPECT_EQ(info.irq_routing()[0].pins()[0], 10u);
  EXPECT_EQ(info.irq_routing()[0].pins()[1], 11u);
  EXPECT_EQ(info.irq_routing()[0].pins()[2], 12u);
  EXPECT_EQ(info.irq_routing()[0].pins()[3], 13u);

  ASSERT_TRUE(info.parent_bdfs().has_value());
  EXPECT_EQ(info.parent_bdfs()->Which(), fuchsia_hardware_pci_root::ParentBdfs::Tag::kAcpi);
  ASSERT_TRUE(info.parent_bdfs()->acpi().has_value());
  ASSERT_EQ(info.parent_bdfs()->acpi()->size(), 1u);
  EXPECT_EQ(info.parent_bdfs()->acpi()->at(0).bus_id(), 0u);
  EXPECT_EQ(info.parent_bdfs()->acpi()->at(0).device_id(), 1u);
  EXPECT_EQ(info.parent_bdfs()->acpi()->at(0).function_id(), 0u);

  // Stop driver to test devicetree BDFs in a second call.
  StopDriver();
  driver_test_.ShutdownAndDestroyDriver();

  driver_test_.RunInEnvironmentTypeContext([](auto& env) {
    env.fake().acpi_devices().clear();
    env.fake().devicetree_devices().push_back(
        pci_bdf_t{.bus_id = 0, .device_id = 2, .function_id = 0});
  });

  clients = StartDriver();
  auto result2 = clients.root->GetPlatformInfo();
  ASSERT_TRUE(result2.is_ok());
  const auto& info2 = result2->info();
  ASSERT_TRUE(info2.parent_bdfs().has_value());
  EXPECT_EQ(info2.parent_bdfs()->Which(), fuchsia_hardware_pci_root::ParentBdfs::Tag::kDevicetree);
  ASSERT_TRUE(info2.parent_bdfs()->devicetree().has_value());
  ASSERT_EQ(info2.parent_bdfs()->devicetree()->size(), 1u);
  EXPECT_EQ(info2.parent_bdfs()->devicetree()->at(0).bus_id(), 0u);
  EXPECT_EQ(info2.parent_bdfs()->devicetree()->at(0).device_id(), 2u);
  EXPECT_EQ(info2.parent_bdfs()->devicetree()->at(0).function_id(), 0u);
}

TEST_F(PcirootBanjoAdapterTest, PlatformInfoAllEmpty) {
  driver_test_.RunInEnvironmentTypeContext([](auto& env) {
    env.fake().routing_entries().clear();
    env.fake().acpi_devices().clear();
    env.fake().devicetree_devices().clear();
  });

  Clients clients = StartDriver();
  auto result = clients.root->GetPlatformInfo();
  ASSERT_TRUE(result.is_ok());
  const auto& info = result->info();
  EXPECT_EQ(info.name(), "fakroot");
  EXPECT_TRUE(info.irq_routing().empty());
  EXPECT_FALSE(info.parent_bdfs().has_value());
}

TEST_F(PcirootBanjoAdapterTest, PlatformInfoBothAcpiAndDevicetreeFails) {
  driver_test_.RunInEnvironmentTypeContext([](auto& env) {
    env.fake().acpi_devices().push_back(pci_bdf_t{.bus_id = 0, .device_id = 1, .function_id = 0});
    env.fake().devicetree_devices().push_back(
        pci_bdf_t{.bus_id = 0, .device_id = 2, .function_id = 0});
  });

  EXPECT_STATUS(driver_test_.StartDriver().status_value(), ZX_ERR_INVALID_ARGS);
}

TEST_F(PcirootBanjoAdapterTest, GetCamEcamPresent) {
  driver_test_.RunInEnvironmentTypeContext([](auto& env) { env.InitFake(true); });

  Clients clients = StartDriver();
  auto result = clients.root->GetCam();
  ASSERT_TRUE(result.is_ok());
  EXPECT_TRUE(result->is_extended());
  EXPECT_TRUE(result->vmo().is_valid());

  // Calling GetCam a second time proves the cached handle survives.
  auto result2 = clients.root->GetCam();
  ASSERT_TRUE(result2.is_ok());
  EXPECT_TRUE(result2->is_extended());
  EXPECT_TRUE(result2->vmo().is_valid());
}

TEST_F(PcirootBanjoAdapterTest, GetCamLegacyCamPresent) {
  driver_test_.RunInEnvironmentTypeContext([](auto& env) { env.InitFake(false); });

  Clients clients = StartDriver();
  auto result = clients.root->GetCam();
  ASSERT_TRUE(result.is_ok());
  EXPECT_FALSE(result->is_extended());
  EXPECT_TRUE(result->vmo().is_valid());
}

TEST_F(PcirootBanjoAdapterTest, GetCamAbsent) {
  driver_test_.RunInEnvironmentTypeContext([](auto& env) { env.fake().enable_cam(false); });

  Clients clients = StartDriver();
  auto result = clients.root->GetCam();
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_NOT_SUPPORTED);
}

TEST_F(PcirootBanjoAdapterTest, GetLegacyInterrupt) {
  auto get_koid = [](const zx::object_base& handle) {
    zx_info_handle_basic_t basic_info;
    EXPECT_OK(
        handle.get_info(ZX_INFO_HANDLE_BASIC, &basic_info, sizeof(basic_info), nullptr, nullptr));
    return basic_info.koid;
  };

  zx_koid_t expected_koid = 0;
  driver_test_.RunInEnvironmentTypeContext([&expected_koid, &get_koid](auto& env) {
    zx::interrupt irq;
    ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &irq));
    expected_koid = get_koid(irq);
    env.fake().legacy_irqs().push_back(pci_legacy_irq_t{
        .interrupt = irq.release(),
        .vector = 42,
    });
  });

  Clients clients = StartDriver();
  auto result = clients.root->GetLegacyInterrupt({{.vector = 42}});
  ASSERT_TRUE(result.is_ok());
  ASSERT_TRUE(result->interrupt().is_valid());
  EXPECT_EQ(get_koid(result->interrupt()), expected_koid);

  // Calling GetLegacyInterrupt a second time proves the cached handle survives.
  auto result2 = clients.root->GetLegacyInterrupt({{.vector = 42}});
  ASSERT_TRUE(result2.is_ok());
  ASSERT_TRUE(result2->interrupt().is_valid());
  EXPECT_EQ(get_koid(result2->interrupt()), expected_koid);

  auto not_found_result = clients.root->GetLegacyInterrupt({{.vector = 99}});
  ASSERT_TRUE(not_found_result.is_error());
  EXPECT_EQ(not_found_result.error_value().domain_error(), ZX_ERR_NOT_FOUND);
}

TEST_F(PcirootBanjoAdapterTest, ChildNodeAndProperties) {
  Clients clients = StartDriver();

  driver_test_.RunInNodeContext([](fdf_testing::TestNode& node) {
    ASSERT_EQ(node.children().count("pci-root"), 1u);
    auto& child = node.children().at("pci-root");
    auto properties = child.GetProperties();
    ASSERT_EQ(properties.size(), 1u);
    EXPECT_EQ(properties[0].key(), bind_fuchsia::SERVICE);
    ASSERT_TRUE(properties[0].value().string_value().has_value());
    EXPECT_EQ(properties[0].value().string_value().value(),
              fuchsia_hardware_pci_root::RootService::Name);
  });
}

TEST_F(PcirootBanjoAdapterTest, AllocateMsiErrorForwarded) {
  driver_test_.RunInEnvironmentTypeContext(
      [](auto& env) { env.fake().enable_allocate_msi(false); });

  Clients clients = StartDriver();
  auto result = clients.root->AllocateMsi({{.msi_count = 1, .can_target_64bit = true}});
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_NOT_SUPPORTED);
}

TEST_F(PcirootBanjoAdapterTest, GetMsiHandleErrorForwarded) {
  driver_test_.RunInEnvironmentTypeContext(
      [](auto& env) { env.fake().enable_get_msi_handle(false); });

  Clients clients = StartDriver();
  auto result = clients.root->GetMsiHandle({{.options = 0, .msi_id = 0, .cfg_offset = 0}});
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_NOT_SUPPORTED);
}

TEST_F(PcirootBanjoAdapterTest, AllocateMsiAndGetMsiHandle) {
  fake_object::Msi::disable_ids_in_use_assert(true);

  Clients clients = StartDriver();
  auto alloc_result = clients.root->AllocateMsi({{.msi_count = 2, .can_target_64bit = true}});
  ASSERT_TRUE(alloc_result.is_ok());
  ASSERT_TRUE(alloc_result->allocation().is_valid());
  EXPECT_EQ(alloc_result->info().target_addr(), 0xCAFEu);
  EXPECT_EQ(alloc_result->info().target_data(), 0xC0FEu);
  EXPECT_EQ(alloc_result->info().irq_count(), 2u);

  zx::handle msi_keepalive;
  ASSERT_OK(alloc_result->allocation().duplicate(ZX_RIGHT_SAME_RIGHTS, &msi_keepalive));

  zx::vmo cfg_vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &cfg_vmo));
  ASSERT_OK(cfg_vmo.set_cache_policy(ZX_CACHE_POLICY_UNCACHED_DEVICE));
  auto irq_result = clients.root->GetMsiHandle({{
      .allocation = std::move(alloc_result->allocation()),
      .options = 0,
      .msi_id = 0,
      .cfg_vmo = std::move(cfg_vmo),
      .cfg_offset = 0,
  }});
  ASSERT_TRUE(irq_result.is_ok());
  EXPECT_TRUE(irq_result->interrupt().is_valid());
}

TEST_F(PcirootBanjoAdapterTest, GetAddressSpace) {
  Clients clients = StartDriver();
  auto res = clients.root->GetAddressSpace({{
      .in_base = 0x2000,
      .size = 0x1000,
      .type = fuchsia_hardware_pci_root::AddressSpace::kMemory,
      .low = true,
  }});
  ASSERT_TRUE(res.is_ok());
  EXPECT_EQ(res->base(), 0x2000u);
  EXPECT_TRUE(res->resource().is_valid());
  EXPECT_TRUE(res->token().is_valid());

  {
    auto res_low_mem = clients.root->GetAddressSpace({{
        .in_base = 0,
        .size = 0x1000,
        .type = fuchsia_hardware_pci_root::AddressSpace::kMemory,
        .low = true,
    }});
    ASSERT_TRUE(res_low_mem.is_ok());
    EXPECT_EQ(res_low_mem->base(), FakePciroot::kDefaultLowMemoryAddress);
    EXPECT_TRUE(res_low_mem->resource().is_valid());
    EXPECT_TRUE(res_low_mem->token().is_valid());
    zx_info_resource_t rinfo;
    ASSERT_OK(res_low_mem->resource().get_info(ZX_INFO_RESOURCE, &rinfo, sizeof(rinfo), nullptr,
                                               nullptr));
    EXPECT_EQ(rinfo.kind, ZX_RSRC_KIND_MMIO);
  }

  {
    auto res_high_mem = clients.root->GetAddressSpace({{
        .in_base = 0,
        .size = 0x1000,
        .type = fuchsia_hardware_pci_root::AddressSpace::kMemory,
        .low = false,
    }});
    ASSERT_TRUE(res_high_mem.is_ok());
    EXPECT_EQ(res_high_mem->base(), FakePciroot::kDefaultHighMemoryAddress);
    EXPECT_TRUE(res_high_mem->resource().is_valid());
    EXPECT_TRUE(res_high_mem->token().is_valid());
  }

  {
    auto res_io = clients.root->GetAddressSpace({{
        .in_base = 0,
        .size = 0x1000,
        .type = fuchsia_hardware_pci_root::AddressSpace::kIo,
        .low = false,
    }});
    ASSERT_TRUE(res_io.is_ok());
    EXPECT_EQ(res_io->base(), FakePciroot::kDefaultIoAddress);
    EXPECT_TRUE(res_io->resource().is_valid());
    EXPECT_TRUE(res_io->token().is_valid());
    zx_info_resource_t rinfo;
    ASSERT_OK(
        res_io->resource().get_info(ZX_INFO_RESOURCE, &rinfo, sizeof(rinfo), nullptr, nullptr));
    EXPECT_EQ(rinfo.kind, ZX_RSRC_KIND_IOPORT);
  }
}

TEST_F(PcirootBanjoAdapterTest, GetBtiPacksBdf) {
  Clients clients = StartDriver();
  fuchsia_hardware_pci::Bdf bdf{{.bus_id = 1, .device_id = 2, .function_id = 3}};
  auto result = clients.root->GetBti({{.bdf = bdf, .index = 4}});
  ASSERT_TRUE(result.is_ok());
  EXPECT_TRUE(result->bti().is_valid());
  driver_test_.RunInEnvironmentTypeContext([](auto& env) {
    const auto& req = env.fake().last_bti_request();
    ASSERT_TRUE(req.has_value());
    EXPECT_EQ(req->bdf, (1u << 8) | (2u << 3) | 3u);
    EXPECT_EQ(req->index, 4u);
  });
}

TEST_F(PcirootBanjoAdapterTest, GetBtiErrorForwarded) {
  driver_test_.RunInEnvironmentTypeContext([](auto& env) { env.fake().enable_get_bti(false); });

  Clients clients = StartDriver();
  fuchsia_hardware_pci::Bdf bdf{{.bus_id = 1, .device_id = 2, .function_id = 3}};
  auto result = clients.root->GetBti({{.bdf = bdf, .index = 0}});
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_NOT_SUPPORTED);
}

TEST_F(PcirootBanjoAdapterTest, GetBtiOutOfRangeBdfFails) {
  Clients clients = StartDriver();
  {
    fuchsia_hardware_pci::Bdf bdf{{.bus_id = 1, .device_id = 32, .function_id = 0}};
    auto result = clients.root->GetBti({{.bdf = bdf, .index = 0}});
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_INVALID_ARGS);
  }
  {
    fuchsia_hardware_pci::Bdf bdf{{.bus_id = 1, .device_id = 0, .function_id = 8}};
    auto result = clients.root->GetBti({{.bdf = bdf, .index = 0}});
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_INVALID_ARGS);
  }
}

TEST_F(PcirootBanjoAdapterTest, MetadataForwarded) {
  driver_test_.RunInEnvironmentTypeContext([](auto& env) {
    fuchsia_hardware_pci::BoardConfiguration config;
    config.use_intx_workaround() = fuchsia_hardware_pci::UseIntxWorkaroundType{};
    env.SetMetadata(std::move(config));
  });

  Clients clients = StartDriver();
  zx::result<fuchsia_hardware_pci::BoardConfiguration> metadata =
      fdf_metadata::GetMetadata<fuchsia_hardware_pci::BoardConfiguration>(
          driver_test_.ConnectToDriverSvcDir());
  ASSERT_TRUE(metadata.is_ok());
  EXPECT_TRUE(metadata->use_intx_workaround().has_value());
}

TEST_F(PcirootBanjoAdapterTest, GetAddressSpaceErrorForwarded) {
  driver_test_.RunInEnvironmentTypeContext(
      [](auto& env) { env.fake().enable_get_address_space(false); });

  Clients clients = StartDriver();
  auto result = clients.root->GetAddressSpace({{
      .in_base = 0x2000,
      .size = 0x1000,
      .type = fuchsia_hardware_pci_root::AddressSpace::kMemory,
      .low = true,
  }});
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_NOT_SUPPORTED);
}

}  // namespace pciroot_banjo_adapter::testing
