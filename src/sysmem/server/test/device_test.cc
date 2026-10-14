// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.sysmem/cpp/fidl.h>
#include <fidl/fuchsia.sysmem/cpp/natural_types.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <lib/async-loop/default.h>
#include <lib/async-loop/loop.h>
#include <lib/async/cpp/task.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/fidl/cpp/wire/arena.h>
#include <lib/sync/completion.h>
#include <lib/zx/bti.h>
#include <stdlib.h>
#include <zircon/errors.h>

#include <bind/fuchsia/sysmem/heap/cpp/bind.h>
#include <gtest/gtest.h>

#include "src/sysmem/server/allocator.h"
#include "src/sysmem/server/buffer_collection.h"
#include "src/sysmem/server/logical_buffer_collection.h"
#include "src/sysmem/server/sysmem.h"
#include "src/sysmem/server/sysmem_config.h"

namespace sysmem_service {

namespace {

class FakeDdkSysmem : public ::testing::Test {
 public:
  FakeDdkSysmem() : loop_(&kAsyncLoopConfigNeverAttachToThread) {}

  void SetUp() override {
    zx_status_t start_status = loop_.StartThread("FakeDdkSysmem");
    ZX_ASSERT_MSG(start_status == ZX_OK, "loop_.StartThread() failed: %s",
                  zx_status_get_string(start_status));

    libsync::Completion done;
    zx_status_t post_status = async::PostTask(loop_.dispatcher(), [this, &done]() mutable {
      sysmem_service::Sysmem::CreateArgs create_args;
      auto create_result = sysmem_service::Sysmem::Create(loop_.dispatcher(), create_args);
      ZX_ASSERT_MSG(create_result.is_ok(), "sysmem_service::Sysmem::Create() failed: %s",
                    create_result.status_string());
      device_ = std::move(create_result.value());
      done.Signal();
    });
    ZX_ASSERT_MSG(post_status == ZX_OK, "async::PostTask() failed: %s",
                  zx_status_get_string(post_status));
    done.Wait();
  }

  void TearDown() override {
    libsync::Completion done;
    zx_status_t post_status = async::PostTask(loop_.dispatcher(), [this, &done]() mutable {
      device_.reset();
      done.Signal();
    });
    ZX_ASSERT_MSG(post_status == ZX_OK, "async::PostTask() failed: %s",
                  zx_status_get_string(post_status));
    done.Wait();
  }

  fidl::ClientEnd<fuchsia_sysmem2::Allocator> Connect() {
    auto [client, server] = fidl::Endpoints<fuchsia_sysmem2::Allocator>::Create();
    device_->SyncCall([this, server = std::move(server)]() mutable {
      sysmem_service::Allocator::CreateOwnedV2(std::move(server), device_.get(),
                                               device_->v2_allocators());
    });
    return std::move(client);
  }

  fidl::ClientEnd<fuchsia_sysmem2::BufferCollection> AllocateNonSharedCollection() {
    fidl::SyncClient<fuchsia_sysmem2::Allocator> allocator(Connect());

    auto [collection_client_end, collection_server_end] =
        fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();

    fuchsia_sysmem2::AllocatorAllocateNonSharedCollectionRequest allocate_non_shared_request;
    allocate_non_shared_request.collection_request() = std::move(collection_server_end);
    EXPECT_TRUE(
        allocator->AllocateNonSharedCollection(std::move(allocate_non_shared_request)).is_ok());
    return std::move(collection_client_end);
  }

 protected:
  async::Loop loop_;
  std::unique_ptr<sysmem_service::Sysmem> device_;
};

TEST_F(FakeDdkSysmem, Lifecycle) {
  // Queue up something that would be processed on the FIDL thread, so we can try to detect a
  // use-after-free if the FidlServer outlives the sysmem device.
  AllocateNonSharedCollection();
}

// Test that creating and tearing down a SecureMem connection works correctly.
TEST_F(FakeDdkSysmem, DummySecureMem) {
  auto [client, server] = fidl::Endpoints<fuchsia_sysmem2::SecureMem>::Create();

  device_->RunSyncOnClientDispatcher([this, client = std::move(client)]() mutable {
    std::lock_guard lock(device_->client_checker_);
    zx_status_t register_status = device_->RegisterSecureMemInternal(std::move(client));
    ZX_ASSERT_MSG(register_status == ZX_OK, "device_->RegisterSecureMemInternal() failed: %s",
                  zx_status_get_string(register_status));
  });

  device_->RunSyncOnClientDispatcher([this]() mutable {
    std::lock_guard lock(device_->client_checker_);
    // This shouldn't deadlock waiting for a message on the channel.
    zx_status_t unregister_status = device_->UnregisterSecureMemInternal();
    ZX_ASSERT_MSG(unregister_status == ZX_OK, "device_->UnregisterSecureMemInternal() failed: %s",
                  zx_status_get_string(unregister_status));
  });

  // This shouldn't cause a panic due to receiving peer closed.
  server.reset();
}

TEST_F(FakeDdkSysmem, NamedToken) {
  fidl::SyncClient<fuchsia_sysmem2::Allocator> allocator(Connect());

  auto [token_client_end, token_server_end] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();

  fuchsia_sysmem2::AllocatorAllocateSharedCollectionRequest allocate_shared_request;
  allocate_shared_request.token_request() = std::move(token_server_end);
  EXPECT_TRUE(allocator->AllocateSharedCollection(std::move(allocate_shared_request)).is_ok());

  fidl::SyncClient<fuchsia_sysmem2::BufferCollectionToken> token(std::move(token_client_end));

  // The buffer collection should end up with a name of "a" because that's the highest priority.
  {
    fuchsia_sysmem2::NodeSetNameRequest set_name_request;
    set_name_request.priority() = 5u;
    set_name_request.name() = "c";
    EXPECT_TRUE(token->SetName(std::move(set_name_request)).is_ok());
  }
  {
    fuchsia_sysmem2::NodeSetNameRequest set_name_request;
    set_name_request.priority() = 100u;
    set_name_request.name() = "a";
    EXPECT_TRUE(token->SetName(std::move(set_name_request)).is_ok());
  }
  {
    fuchsia_sysmem2::NodeSetNameRequest set_name_request;
    set_name_request.priority() = 6u;
    set_name_request.name() = "b";
    EXPECT_TRUE(token->SetName(std::move(set_name_request)).is_ok());
  }

  auto [collection_client_end, collection_server_end] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();

  fuchsia_sysmem2::AllocatorBindSharedCollectionRequest bind_shared_request;
  bind_shared_request.token() = token.TakeClientEnd();
  bind_shared_request.buffer_collection_request() = std::move(collection_server_end);
  EXPECT_TRUE(allocator->BindSharedCollection(std::move(bind_shared_request)).is_ok());

  // Poll until a matching buffer collection is found.
  while (true) {
    bool found_collection = device_->SyncCall([&]() {
      if (device_->logical_buffer_collections().size() == 1) {
        const auto* logical_collection = *device_->logical_buffer_collections().begin();
        auto collection_views = logical_collection->collection_views();
        if (collection_views.size() == 1) {
          auto name = logical_collection->name();
          EXPECT_TRUE(name);
          EXPECT_EQ("a", *name);
          return true;
        }
      }
      return false;
    });
    if (found_collection) {
      break;
    }
  }
}

TEST_F(FakeDdkSysmem, NamedClient) {
  auto collection_client_end = AllocateNonSharedCollection();

  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> collection(std::move(collection_client_end));
  fuchsia_sysmem2::NodeSetDebugClientInfoRequest set_debug_request;
  set_debug_request.name() = "a";
  set_debug_request.id() = 5;
  EXPECT_TRUE(collection->SetDebugClientInfo(std::move(set_debug_request)).is_ok());

  // Poll until a matching buffer collection is found.
  while (true) {
    bool found_collection = device_->SyncCall([&]() mutable {
      if (device_->logical_buffer_collections().size() == 1) {
        const auto* logical_collection = *device_->logical_buffer_collections().begin();
        auto collection_views = logical_collection->collection_views();
        if (collection_views.size() == 1) {
          const BufferCollection* collection = logical_collection->collection_views().front();
          if (collection->node_properties().client_debug_info().name == "a") {
            EXPECT_EQ(5u, collection->node_properties().client_debug_info().id);
            return true;
          }
        }
      }
      return false;
    });
    if (found_collection) {
      break;
    }
  }
}

// Check that the allocator name overrides the collection name.
TEST_F(FakeDdkSysmem, NamedAllocatorToken) {
  fidl::SyncClient<fuchsia_sysmem2::Allocator> allocator(Connect());

  auto [token_client_end, token_server_end] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();

  fuchsia_sysmem2::AllocatorAllocateSharedCollectionRequest allocate_shared_request;
  allocate_shared_request.token_request() = std::move(token_server_end);
  EXPECT_TRUE(allocator->AllocateSharedCollection(std::move(allocate_shared_request)).is_ok());

  fidl::SyncClient<fuchsia_sysmem2::BufferCollectionToken> token(std::move(token_client_end));

  const char kAlphabetString[] = "abcdefghijklmnopqrstuvwxyz";
  {
    fuchsia_sysmem2::AllocatorSetDebugClientInfoRequest set_debug_request;
    set_debug_request.name() = kAlphabetString;
    set_debug_request.id() = 5;
    EXPECT_TRUE(allocator->SetDebugClientInfo(std::move(set_debug_request)).is_ok());
  }
  // Despite this message being sent after the above message, this message is not the "final word"
  // on the debug info, because the allocator will fence all token messages before transferring
  // the allocator's debug info to the BufferColllection.
  {
    fuchsia_sysmem2::NodeSetDebugClientInfoRequest set_debug_request;
    set_debug_request.name() = "bad";
    set_debug_request.id() = 6;
    EXPECT_TRUE(token->SetDebugClientInfo(std::move(set_debug_request)).is_ok());
  }

  auto [collection_client_end, collection_server_end] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();

  fuchsia_sysmem2::AllocatorBindSharedCollectionRequest bind_shared_request;
  bind_shared_request.token() = token.TakeClientEnd();
  bind_shared_request.buffer_collection_request() = std::move(collection_server_end);
  EXPECT_TRUE(allocator->BindSharedCollection(std::move(bind_shared_request)).is_ok());

  // Poll until a matching buffer collection is found. If this gets stuck, sysmem may be failing
  // to ensure that the allocator's debug info is the "last word" - may be failing to fence the
  // token messages before applyign the allocator's debug info to the token.
  while (true) {
    bool found_collection = device_->SyncCall([&]() {
      if (device_->logical_buffer_collections().size() == 1) {
        const auto* logical_collection = *device_->logical_buffer_collections().begin();
        auto collection_views = logical_collection->collection_views();
        if (collection_views.size() == 1) {
          const auto& collection = collection_views.front();
          // This needs to tell the difference between "abcdefghijklmnopqrstuvwxyz" and
          // "bad (was abcdefghijklmnopqrstuvwxyz)".
          if (collection->node_properties().client_debug_info().name.find(kAlphabetString) == 0) {
            EXPECT_EQ(5u, collection->node_properties().client_debug_info().id);
            return true;
          }
        }
      }
      return false;
    });
    if (found_collection) {
      break;
    }
  }
}

TEST_F(FakeDdkSysmem, MaxSize) {
  device_->set_settings(sysmem_service::Settings{.max_allocation_size = zx_system_get_page_size()});

  auto collection_client = AllocateNonSharedCollection();

  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  constraints.min_buffer_count() = 1;
  auto& bmc = constraints.buffer_memory_constraints().emplace();
  bmc.min_size_bytes() = zx_system_get_page_size() * 2;
  bmc.cpu_domain_supported() = true;
  constraints.usage().emplace().cpu() = fuchsia_sysmem::kCpuUsageRead;

  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> collection(std::move(collection_client));
  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.constraints() = std::move(constraints);
  EXPECT_TRUE(collection->SetConstraints(std::move(set_constraints_request)).is_ok());

  // Sysmem should fail the collection and return an error.
  auto wait_result = collection->WaitForAllBuffersAllocated();
  EXPECT_TRUE(!wait_result.is_ok());
}

// Check that teardown doesn't leak any memory (detected through LSAN).
TEST_F(FakeDdkSysmem, TeardownLeak) {
  auto collection_client = AllocateNonSharedCollection();

  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  constraints.min_buffer_count() = 1;
  auto& bmc = constraints.buffer_memory_constraints().emplace();
  bmc.min_size_bytes() = zx_system_get_page_size();
  bmc.cpu_domain_supported() = true;
  constraints.usage().emplace().cpu() = fuchsia_sysmem::kCpuUsageRead;

  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> collection(std::move(collection_client));
  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.constraints() = std::move(constraints);
  EXPECT_TRUE(collection->SetConstraints(std::move(set_constraints_request)).is_ok());

  auto wait_result = collection->WaitForAllBuffersAllocated();
  EXPECT_TRUE(wait_result.is_ok());
  auto info = std::move(wait_result->buffer_collection_info());

  for (uint32_t i = 0; i < info->buffers()->size(); i++) {
    info->buffers()->at(i).vmo().reset();
  }
  collection = {};
}

// Check that there are no circular references from a VMO to the logical buffer collection.
TEST_F(FakeDdkSysmem, BufferLeak) {
  auto collection_client = AllocateNonSharedCollection();

  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  constraints.min_buffer_count() = 1;
  auto& bmc = constraints.buffer_memory_constraints().emplace();
  bmc.min_size_bytes() = zx_system_get_page_size();
  bmc.cpu_domain_supported() = true;
  constraints.usage().emplace().cpu() = fuchsia_sysmem::kCpuUsageRead;

  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> collection(std::move(collection_client));
  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.constraints() = std::move(constraints);
  EXPECT_TRUE(collection->SetConstraints(std::move(set_constraints_request)).is_ok());

  auto wait_result = collection->WaitForAllBuffersAllocated();
  EXPECT_TRUE(wait_result.is_ok());
  auto info = std::move(wait_result->buffer_collection_info());

  for (uint32_t i = 0; i < info->buffers()->size(); i++) {
    info->buffers()->at(i).vmo().reset();
  }

  collection = {};

  // Poll until all buffer collections are deleted.
  while (true) {
    bool no_collections = device_->SyncCall([&]() {
      no_collections = device_->logical_buffer_collections().empty();
      return no_collections;
    });
    if (no_collections) {
      break;
    }
  }
}

TEST_F(FakeDdkSysmem, BufferCountOverflow_SingleParticipant_Failure) {
  auto collection_client = AllocateNonSharedCollection();

  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  // We pick 0x80000000 (2^31) and 0x80000001 (2^31 + 1) so that when summed together
  // in uint32_t arithmetic without overflow checking, they wrap around to 1
  // (0x80000000 + 0x80000001 = 0x100000001 -> 1).
  constraints.min_buffer_count_for_camping() = 0x80000000;
  constraints.min_buffer_count_for_dedicated_slack() = 0x80000001;
  auto& bmc = constraints.buffer_memory_constraints().emplace();
  bmc.min_size_bytes() = zx_system_get_page_size();
  bmc.cpu_domain_supported() = true;
  constraints.usage().emplace().cpu() = fuchsia_sysmem::kCpuUsageRead;

  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> collection(std::move(collection_client));
  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.constraints() = std::move(constraints);
  EXPECT_TRUE(collection->SetConstraints(std::move(set_constraints_request)).is_ok());

  auto wait_result = collection->WaitForAllBuffersAllocated();
  EXPECT_FALSE(wait_result.is_ok());
}

TEST_F(FakeDdkSysmem, BufferCountOverflow_MultiParticipant_Failure) {
  fidl::SyncClient<fuchsia_sysmem2::Allocator> allocator(Connect());

  auto [token_client_1, token_server_1] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
  fuchsia_sysmem2::AllocatorAllocateSharedCollectionRequest allocate_shared_request;
  allocate_shared_request.token_request() = std::move(token_server_1);
  EXPECT_TRUE(allocator->AllocateSharedCollection(std::move(allocate_shared_request)).is_ok());

  auto [token_client_2, token_server_2] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
  fidl::SyncClient token_1{std::move(token_client_1)};
  fuchsia_sysmem2::BufferCollectionTokenDuplicateRequest duplicate_request;
  duplicate_request.rights_attenuation_mask() = ZX_RIGHT_SAME_RIGHTS;
  duplicate_request.token_request() = std::move(token_server_2);
  EXPECT_TRUE(token_1->Duplicate(std::move(duplicate_request)).is_ok());
  EXPECT_TRUE(token_1->Sync().is_ok());

  auto [collection_client_1, collection_server_1] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();
  fidl::SyncClient collection_1{std::move(collection_client_1)};
  fuchsia_sysmem2::AllocatorBindSharedCollectionRequest bind_shared_request_1;
  bind_shared_request_1.token() = token_1.TakeClientEnd();
  bind_shared_request_1.buffer_collection_request() = std::move(collection_server_1);
  EXPECT_TRUE(allocator->BindSharedCollection(std::move(bind_shared_request_1)).is_ok());

  auto [collection_client_2, collection_server_2] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();
  fidl::SyncClient collection_2{std::move(collection_client_2)};
  fuchsia_sysmem2::AllocatorBindSharedCollectionRequest bind_shared_request_2;
  bind_shared_request_2.token() = std::move(token_client_2);
  bind_shared_request_2.buffer_collection_request() = std::move(collection_server_2);
  EXPECT_TRUE(allocator->BindSharedCollection(std::move(bind_shared_request_2)).is_ok());

  // We pick 0x80000000 (2^31) for participant 1 and 0x80000001 (2^31 + 1) for participant 2
  // for min_buffer_count_for_camping. In uint32_t arithmetic without overflow checking,
  // accumulating these two camping counts would wrap around to 1 (0x80000000 + 0x80000001 -> 1)
  // during constraint aggregation, causing sysmem to incorrectly allocate 1 buffer.
  fuchsia_sysmem2::BufferCollectionConstraints constraints_1;
  constraints_1.min_buffer_count_for_camping() = 0x80000000;
  auto& bmc_1 = constraints_1.buffer_memory_constraints().emplace();
  bmc_1.min_size_bytes() = zx_system_get_page_size();
  bmc_1.cpu_domain_supported() = true;
  constraints_1.usage().emplace().cpu() = fuchsia_sysmem::kCpuUsageRead;

  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request_1;
  set_constraints_request_1.constraints() = std::move(constraints_1);
  EXPECT_TRUE(collection_1->SetConstraints(std::move(set_constraints_request_1)).is_ok());

  fuchsia_sysmem2::BufferCollectionConstraints constraints_2;
  constraints_2.min_buffer_count_for_camping() = 0x80000001;
  auto& bmc_2 = constraints_2.buffer_memory_constraints().emplace();
  bmc_2.min_size_bytes() = zx_system_get_page_size();
  bmc_2.cpu_domain_supported() = true;
  constraints_2.usage().emplace().cpu() = fuchsia_sysmem::kCpuUsageRead;

  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request_2;
  set_constraints_request_2.constraints() = std::move(constraints_2);
  EXPECT_TRUE(collection_2->SetConstraints(std::move(set_constraints_request_2)).is_ok());

  auto wait_result_1 = collection_1->WaitForAllBuffersAllocated();
  EXPECT_FALSE(wait_result_1.is_ok());
  auto wait_result_2 = collection_2->WaitForAllBuffersAllocated();
  EXPECT_FALSE(wait_result_2.is_ok());
}

TEST_F(FakeDdkSysmem, RegisterHeap_SystemRam_Denied) {
  auto heap = sysmem::MakeHeap(bind_fuchsia_sysmem_heap::HEAP_TYPE_SYSTEM_RAM, 0);
  auto [client, server] = fidl::Endpoints<fuchsia_hardware_sysmem::Heap>::Create();

  device_->RunSyncOnClientDispatcher([this, heap = heap, client = std::move(client)]() mutable {
    std::lock_guard lock(device_->client_checker_);
    zx_status_t status = device_->RegisterHeapInternal(heap, std::move(client));
    EXPECT_EQ(status, ZX_ERR_ACCESS_DENIED);
  });

  device_->RunSyncOnLoop([this, heap = std::move(heap)]() {
    std::lock_guard lock(*device_->loop_checker_);
    EXPECT_TRUE(device_->is_allocator_present_for_testing(heap));
    EXPECT_FALSE(device_->is_secure_allocator_present_for_testing(heap));
  });
}

TEST_F(FakeDdkSysmem, RegisterHeap_SecureHeap_Denied) {
  auto heap = sysmem::MakeHeap("amlogic_secure", 0);
  auto [client, server] = fidl::Endpoints<fuchsia_hardware_sysmem::Heap>::Create();

  device_->RunSyncOnLoop([this, heap = heap]() {
    std::lock_guard lock(*device_->loop_checker_);
    device_->add_secure_allocator_id_for_testing(heap);
  });

  device_->RunSyncOnClientDispatcher([this, heap = heap, client = std::move(client)]() mutable {
    std::lock_guard lock(device_->client_checker_);
    zx_status_t status = device_->RegisterHeapInternal(heap, std::move(client));
    EXPECT_EQ(status, ZX_OK);
  });

  fidl::Arena arena;
  auto coherency = fuchsia_hardware_sysmem::wire::CoherencyDomainSupport::Builder(arena)
                       .cpu_supported(false)
                       .ram_supported(false)
                       .inaccessible_supported(true)
                       .Build();

  auto properties = fuchsia_hardware_sysmem::wire::HeapProperties::Builder(arena)
                        .coherency_domain_support(coherency)
                        .need_clear(false)
                        .Build();

  EXPECT_TRUE(fidl::WireSendEvent(server)->OnRegister(properties).ok());
  EXPECT_EQ(server.channel().wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite(), nullptr),
            ZX_OK);

  // Verify the OnRegister event was rejected on loop_.
  device_->RunSyncOnLoop([this, heap = std::move(heap)]() {
    std::lock_guard lock(*device_->loop_checker_);
    EXPECT_FALSE(device_->is_allocator_present_for_testing(heap));
    EXPECT_TRUE(device_->is_secure_allocator_present_for_testing(heap));
  });
}

TEST_F(FakeDdkSysmem, PadBeyondImageSizeBytesOverflowFailure) {
  auto collection_client = AllocateNonSharedCollection();

  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  constraints.min_buffer_count() = 1;
  auto& bmc = constraints.buffer_memory_constraints().emplace();
  bmc.min_size_bytes() = 0x80000000;
  bmc.cpu_domain_supported() = true;
  constraints.usage().emplace().cpu() = fuchsia_sysmem::kCpuUsageRead;

  auto& ifc = constraints.image_format_constraints().emplace();
  ifc.emplace_back();
  ifc.back().pixel_format() = fuchsia_images2::PixelFormat::kR8G8B8A8;
  ifc.back().color_spaces() = {fuchsia_images2::ColorSpace::kSrgb};
  ifc.back().min_size() = {1, 1};
  ifc.back().max_size() = {100, 100};
  ifc.back().size_alignment() = {1, 1};
  ifc.back().min_bytes_per_row() = 4;
  ifc.back().max_bytes_per_row() = 400;
  ifc.back().bytes_per_row_divisor() = 1;
  ifc.back().max_width_times_height() = 10000;
  // 0x80000000 + 0x80000000 = 0x100000000 which overflows uint32.
  ifc.back().pad_beyond_image_size_bytes() = 0x80000000;

  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> collection(std::move(collection_client));
  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.constraints() = std::move(constraints);
  EXPECT_TRUE(collection->SetConstraints(std::move(set_constraints_request)).is_ok());

  auto wait_result = collection->WaitForAllBuffersAllocated();
  EXPECT_FALSE(wait_result.is_ok());
}

TEST_F(FakeDdkSysmem, TotalSizeBytesOverflowFailure) {
  auto collection_client = AllocateNonSharedCollection();

  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  constraints.min_buffer_count() = 2;
  auto& bmc = constraints.buffer_memory_constraints().emplace();
  // 0x8000000000000000 * 2 = 0 in 64-bit arithmetic
  bmc.min_size_bytes() = 0x8000000000000000ULL;
  bmc.cpu_domain_supported() = true;
  constraints.usage().emplace().cpu() = fuchsia_sysmem::kCpuUsageRead;

  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> collection(std::move(collection_client));
  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.constraints() = std::move(constraints);
  EXPECT_TRUE(collection->SetConstraints(std::move(set_constraints_request)).is_ok());

  auto wait_result = collection->WaitForAllBuffersAllocated();
  EXPECT_FALSE(wait_result.is_ok());
}

TEST_F(FakeDdkSysmem, ZeroBytesPerRowDivisor) {
  auto collection_client = AllocateNonSharedCollection();

  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  constraints.min_buffer_count() = 1;
  auto& bmc = constraints.buffer_memory_constraints().emplace();
  bmc.min_size_bytes() = zx_system_get_page_size();
  bmc.cpu_domain_supported() = true;
  constraints.usage().emplace().cpu() = fuchsia_sysmem::kCpuUsageRead;

  auto& ifc = constraints.image_format_constraints().emplace();
  ifc.emplace_back();
  ifc[0].pixel_format() = fuchsia_images2::PixelFormat::kR8G8B8A8;
  ifc[0].pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  ifc[0].bytes_per_row_divisor() = 0;

  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> collection(std::move(collection_client));
  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.constraints() = std::move(constraints);
  EXPECT_TRUE(collection->SetConstraints(std::move(set_constraints_request)).is_ok());

  // Should fail allocation cleanly without crashing sysmem.
  auto wait_result = collection->WaitForAllBuffersAllocated();
  EXPECT_TRUE(!wait_result.is_ok());
}

TEST_F(FakeDdkSysmem, OverflowPadForBlockSize) {
  auto collection_client = AllocateNonSharedCollection();

  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  constraints.min_buffer_count() = 1;
  auto& bmc = constraints.buffer_memory_constraints().emplace();
  bmc.min_size_bytes() = zx_system_get_page_size();
  bmc.cpu_domain_supported() = true;
  constraints.usage().emplace().cpu() = fuchsia_sysmem::kCpuUsageRead;

  auto& ifc = constraints.image_format_constraints().emplace();
  ifc.emplace_back();
  ifc[0].pixel_format() = fuchsia_images2::PixelFormat::kR8G8B8A8;
  ifc[0].pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  ifc[0].pad_for_block_size() = fuchsia_math::SizeU{0x40000000, 1};

  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> collection(std::move(collection_client));
  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.constraints() = std::move(constraints);
  EXPECT_TRUE(collection->SetConstraints(std::move(set_constraints_request)).is_ok());

  // Should fail allocation cleanly without crashing sysmem.
  auto wait_result = collection->WaitForAllBuffersAllocated();
  EXPECT_TRUE(!wait_result.is_ok());
}

TEST_F(FakeDdkSysmem, OverflowMinSizeWidthRoundUp) {
  auto collection_client = AllocateNonSharedCollection();

  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  constraints.min_buffer_count() = 1;
  auto& bmc = constraints.buffer_memory_constraints().emplace();
  bmc.min_size_bytes() = zx_system_get_page_size();
  bmc.cpu_domain_supported() = true;
  constraints.usage().emplace().cpu() = fuchsia_sysmem::kCpuUsageRead;

  auto& ifc = constraints.image_format_constraints().emplace();
  ifc.emplace_back();
  ifc[0].pixel_format() = fuchsia_images2::PixelFormat::kR8G8B8A8;
  ifc[0].pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  ifc[0].min_size() = fuchsia_math::SizeU{0xFFFFFFFF, 100};
  ifc[0].size_alignment() = fuchsia_math::SizeU{2, 1};

  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> collection(std::move(collection_client));
  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.constraints() = std::move(constraints);
  EXPECT_TRUE(collection->SetConstraints(std::move(set_constraints_request)).is_ok());

  // Should fail allocation cleanly without crashing sysmem.
  auto wait_result = collection->WaitForAllBuffersAllocated();
  EXPECT_TRUE(!wait_result.is_ok());
}

TEST_F(FakeDdkSysmem, OverflowBytesPerRowDivisorRoundUp) {
  auto collection_client = AllocateNonSharedCollection();

  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  constraints.min_buffer_count() = 1;
  auto& bmc = constraints.buffer_memory_constraints().emplace();
  bmc.min_size_bytes() = zx_system_get_page_size();
  bmc.cpu_domain_supported() = true;
  constraints.usage().emplace().cpu() = fuchsia_sysmem::kCpuUsageRead;

  auto& ifc = constraints.image_format_constraints().emplace();
  ifc.emplace_back();
  ifc[0].pixel_format() = fuchsia_images2::PixelFormat::kR8G8B8A8;
  ifc[0].pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  ifc[0].min_bytes_per_row() = 0xFFFFFF00;
  ifc[0].bytes_per_row_divisor() = 256;

  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> collection(std::move(collection_client));
  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.constraints() = std::move(constraints);
  EXPECT_TRUE(collection->SetConstraints(std::move(set_constraints_request)).is_ok());

  // Should fail allocation cleanly without crashing sysmem.
  auto wait_result = collection->WaitForAllBuffersAllocated();
  EXPECT_TRUE(!wait_result.is_ok());
}

}  // namespace
}  // namespace sysmem_service
