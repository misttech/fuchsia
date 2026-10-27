// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/zx/result.h>
#include <zircon/limits.h>
#include <zircon/syscalls.h>

#include <memory>
#include <optional>

#include <gtest/gtest.h>

#include "src/devices/pci/drivers/pci/allocation.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_allocator.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_root_service.h"
#include "src/lib/testing/predicates/status.h"

namespace pci {
namespace {

class PciAllocationTests : public ::testing::Test {
 protected:
  void SetUp() override {
    ASSERT_OK(loop_.StartThread("allocation-fidl-thread"));
    auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_pci_root::Root>();
    ASSERT_OK(endpoints);
    root_server_.Bind(loop_.dispatcher(), std::move(endpoints->server));
    root_client_ =
        fidl::WireSyncClient<fuchsia_hardware_pci_root::Root>(std::move(endpoints->client));
  }

  void TearDown() override { loop_.Shutdown(); }

  FakeRootServer& root_server() { return root_server_; }
  const fidl::WireSyncClient<fuchsia_hardware_pci_root::Root>& root_client() {
    return root_client_;
  }

  void AllocationTypeHelper(fuchsia_hardware_pci_root::AddressSpace type) {
    PciRootAllocator root_allocator(root_client(), type, false);
    EXPECT_EQ(root_allocator.type(), type);

    size_t page_size = zx_system_get_page_size();
    auto root_result = root_allocator.Allocate(std::nullopt, page_size * 4);
    ASSERT_OK(root_result.status_value());
    ASSERT_EQ(root_result->type(), type);

    PciRegionAllocator region_allocator;
    ASSERT_OK(region_allocator.SetParentAllocation(std::move(root_result.value())));
    ASSERT_EQ(region_allocator.type(), type);

    auto region_allocator_result = region_allocator.Allocate(std::nullopt, page_size);
    ASSERT_OK(region_allocator_result.status_value());
    ASSERT_EQ(region_allocator_result->type(), type);
  }

 private:
  async::Loop loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
  FakeRootServer root_server_;
  fidl::WireSyncClient<fuchsia_hardware_pci_root::Root> root_client_;
};

// Tests that GetAddressSpace allocations are tracked through
// PciRootAllocation and PciRegionAllocation dtors.
TEST_F(PciAllocationTests, BalancedAllocation) {
  PciRootAllocator root_alloc(root_client(), fuchsia_hardware_pci_root::AddressSpace::kMemory,
                              false);
  {
    auto alloc1 = root_alloc.Allocate(std::nullopt, zx_system_get_page_size());
    EXPECT_TRUE(alloc1.is_ok());
    EXPECT_EQ(1u, root_server().allocation_eps().size());
    auto alloc2 = root_alloc.Allocate(1024u, zx_system_get_page_size());
    EXPECT_TRUE(alloc2.is_ok());
    EXPECT_EQ(2u, root_server().allocation_eps().size());
  }
}

TEST_F(PciAllocationTests, RootNaturalAlignment) {
  PciRootAllocator root_alloc(root_client(), fuchsia_hardware_pci_root::AddressSpace::kMemory,
                              false);

  // Allocate the front 1024 bytes.
  auto alloc1 = root_alloc.Allocate(std::nullopt, 0x400);
  ASSERT_TRUE(alloc1.is_ok());

  // Attempt to allocate 8192, which needs to be naturally aligned to the same
  // size. It cannot start at 1024 where the previous allocation ended.
  size_t alloc2_sz = 8192;
  auto alloc2 = root_alloc.Allocate(std::nullopt, alloc2_sz);
  ASSERT_TRUE(alloc2.is_ok());
  EXPECT_TRUE(alloc2->base() % alloc2_sz == 0);
  EXPECT_EQ(alloc2->size(), alloc2_sz);
}

TEST_F(PciAllocationTests, RegionNaturalAlignment) {
  PciRootAllocator root_alloc(root_client(), fuchsia_hardware_pci_root::AddressSpace::kMemory,
                              false);

  auto result = root_alloc.Allocate(std::nullopt, 0x1000000);
  std::unique_ptr<PciAllocation> root_allocation = std::move(result.value());
  PciRegionAllocator pci_allocator;
  pci_allocator.SetParentAllocation(std::move(root_allocation));

  // Allocate the front 1024 bytes.
  auto alloc1 = pci_allocator.Allocate(std::nullopt, 0x400);
  ASSERT_TRUE(alloc1.is_ok());

  // Attempt to allocate 16K, which needs to be naturally aligned to the same
  // size. It cannot start at 1024 where the previous allocation ended.
  size_t alloc2_sz = 0x10000;
  auto alloc2 = pci_allocator.Allocate(std::nullopt, alloc2_sz);
  ASSERT_TRUE(alloc2.is_ok());
  EXPECT_EQ(alloc2->size(), alloc2_sz);
  EXPECT_EQ(alloc2->base() & (alloc2_sz - 1), 0u);
}

TEST_F(PciAllocationTests, ZeroSize) {
  PciRootAllocator root_alloc(root_client(), fuchsia_hardware_pci_root::AddressSpace::kMemory,
                              false);

  ASSERT_TRUE(root_alloc.Allocate(0, 0).is_error());
}

// Since test allocations lack a valid resource they should fail when
// CreateVMObject is called
TEST_F(PciAllocationTests, VmoCreationFailure) {
  zx::vmo vmo;
  PciRootAllocator root(root_client(), fuchsia_hardware_pci_root::AddressSpace::kMemory, false);
  PciAllocator* root_ptr = &root;
  auto alloc = root_ptr->Allocate(std::nullopt, zx_system_get_page_size());
  EXPECT_TRUE(alloc.is_ok());
  EXPECT_OK(alloc->CreateVmo().status_value());
}

// Ensure that all allocator and allocation types report the correct address
// space type even as they're passed to downstream allocators.
TEST_F(PciAllocationTests, IoType) {
  EXPECT_NO_FATAL_FAILURE(AllocationTypeHelper(fuchsia_hardware_pci_root::AddressSpace::kIo));
}

TEST_F(PciAllocationTests, MmioType) {
  EXPECT_NO_FATAL_FAILURE(AllocationTypeHelper(fuchsia_hardware_pci_root::AddressSpace::kMemory));
}

TEST_F(PciAllocationTests, MmioBackedIoType) {
  root_server().set_io_type(fuchsia_hardware_pci_root::AddressSpace::kMemory);

  PciRootAllocator root_allocator(root_client(), fuchsia_hardware_pci_root::AddressSpace::kIo,
                                  false);
  EXPECT_EQ(root_allocator.type(), fuchsia_hardware_pci_root::AddressSpace::kIo);

  size_t page_size = zx_system_get_page_size();
  auto root_result = root_allocator.Allocate(std::nullopt, page_size * 4);
  ASSERT_OK(root_result.status_value());
  ASSERT_EQ(root_result->type(), fuchsia_hardware_pci_root::AddressSpace::kMemory);

  PciRegionAllocator region_allocator;
  ASSERT_OK(region_allocator.SetParentAllocation(std::move(root_result.value())));
  ASSERT_EQ(region_allocator.type(), fuchsia_hardware_pci_root::AddressSpace::kMemory);

  auto region_allocator_result = region_allocator.Allocate(std::nullopt, page_size);
  ASSERT_OK(region_allocator_result.status_value());
  ASSERT_EQ(region_allocator_result->type(), fuchsia_hardware_pci_root::AddressSpace::kMemory);
}

// A PciRegionAllocator has no type until it is given a backing allocation and should assert.
TEST(PciAllocationTest, RegionTypeNone) {
  PciRegionAllocator allocator;
  ASSERT_DEATH(allocator.type(), "");

  fuchsia_hardware_pci_root::AddressSpace type = fuchsia_hardware_pci_root::AddressSpace::kMemory;
  auto allocation = std::make_unique<FakeAllocation>(type, std::nullopt, 1024);
  ASSERT_OK(allocator.SetParentAllocation(std::move(allocation)));
  EXPECT_EQ(allocator.type(), type);
}

}  // namespace
}  // namespace pci
