// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "server.h"

#include <lib/fit/defer.h>
#include <lib/sync/completion.h>
#include <unistd.h>

#include <thread>

#include <zxtest/zxtest.h>

#include "test/stub-block-device.h"

namespace {

class ServerTestFixture : public zxtest::Test {
 public:
  ServerTestFixture() : client_(blkdev_.proto()) {}

 protected:
  void SetUp() override;
  void TearDown() override;

  void CreateThread();
  void WaitForThreadStart();
  void WaitForThreadExit();
  void JoinThread();

  StubBlockDevice blkdev_;
  ddk::BlockProtocolClient client_;
  std::unique_ptr<Server> server_;

 private:
  sync_completion_t thread_started_;
  sync_completion_t thread_exited_;
  std::thread thread_;
};

void ServerTestFixture::SetUp() {
  zx::result server = Server::Create(&client_);
  ASSERT_OK(server);
  server_ = std::move(server.value());
}

void ServerTestFixture::TearDown() { ASSERT_FALSE(thread_.joinable()); }

void ServerTestFixture::CreateThread() {
  thread_ = std::thread([this]() {
    sync_completion_signal(&thread_started_);
    [[maybe_unused]] zx_status_t status = server_->Serve();
    sync_completion_signal(&thread_exited_);
    return 0;
  });
}

void ServerTestFixture::WaitForThreadStart() {
  ASSERT_OK(sync_completion_wait(&thread_started_, ZX_SEC(5)));
}

void ServerTestFixture::WaitForThreadExit() {
  ASSERT_OK(sync_completion_wait(&thread_exited_, ZX_SEC(5)));
}

void ServerTestFixture::JoinThread() { thread_.join(); }

TEST_F(ServerTestFixture, CreateServer) {}

TEST_F(ServerTestFixture, StartServer) {
  CreateThread();
  WaitForThreadStart();

  // This code is racy with Serve() being called. This is expected.
  // The server should handle shutdown commands at any time.
  server_->Shutdown();

  WaitForThreadExit();
  JoinThread();
}

TEST_F(ServerTestFixture, SplitRequestAfterFailedRequestReturnsFailure) {
  zx::result fifo_result = server_->GetFifo();
  ASSERT_OK(fifo_result);
  fzl::fifo<BlockFifoRequest, BlockFifoResponse> fifo(std::move(fifo_result.value()));
  CreateThread();
  auto cleanup = fit::defer([&] {
    server_->Shutdown();
    JoinThread();
  });
  zx::vmo vmo;
  constexpr int kTestBlockCount = 257;
  ASSERT_OK(zx::vmo::create(kTestBlockCount * kBlockSize, 0, &vmo));
  zx::result vmoid = server_->AttachVmo(std::move(vmo));
  ASSERT_OK(vmoid);

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_WRITE, .flags = BLOCK_IO_FLAG_GROUP_ITEM},
      .reqid = 100,
      .group = 5,
      .vmoid = vmoid.value(),
      .length = 4,
      .vmo_offset = 0,
      .dev_offset = 0,
  };

  blkdev_.set_callback([&](const block_op_t&) { return ZX_ERR_IO; });

  size_t actual_count = 0;
  ASSERT_OK(fifo.write(&request, 1, &actual_count));
  ASSERT_EQ(actual_count, 1);

  request = {
      .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
      .reqid = 101,
      .vmoid = vmoid.value(),
  };
  ASSERT_OK(fifo.write(&request, 1, &actual_count));
  ASSERT_EQ(actual_count, 1);

  BlockFifoResponse response;
  zx_signals_t seen;
  ASSERT_OK(fifo.wait_one(ZX_FIFO_READABLE | ZX_FIFO_PEER_CLOSED, zx::time::infinite(), &seen));
  ASSERT_OK(fifo.read_one(&response));

  // Should get the response for the read.
  EXPECT_EQ(response.reqid, 101);

  request = {
      .command = {.opcode = BLOCK_OPCODE_WRITE,
                  .flags = BLOCK_IO_FLAG_GROUP_ITEM | BLOCK_IO_FLAG_GROUP_LAST},
      .reqid = 102,
      .group = 5,
      .vmoid = vmoid.value(),
      .length = kTestBlockCount,
      .vmo_offset = 0,
      .dev_offset = 0,
  };
  ASSERT_OK(fifo.write(&request, 1, &actual_count));
  ASSERT_EQ(actual_count, 1);

  // It's the last one so it should trigger a response.
  ASSERT_OK(fifo.wait_one(ZX_FIFO_READABLE | ZX_FIFO_PEER_CLOSED, zx::time::infinite(), &seen));
  ASSERT_OK(fifo.read_one(&response));

  EXPECT_EQ(response.reqid, 102);

  // Make sure the group is correctly cleaned up and able to be used for another request.
  blkdev_.set_callback({});

  BlockFifoRequest requests[] = {
      {
          .command = {.opcode = BLOCK_OPCODE_WRITE, .flags = BLOCK_IO_FLAG_GROUP_ITEM},
          .reqid = 103,
          .group = 5,
          .vmoid = vmoid.value(),
          .length = 257,
          .vmo_offset = 0,
          .dev_offset = 0,
      },
      {
          .command = {.opcode = BLOCK_OPCODE_WRITE,
                      .flags = BLOCK_IO_FLAG_GROUP_ITEM | BLOCK_IO_FLAG_GROUP_LAST},
          .reqid = 104,
          .group = 5,
          .vmoid = vmoid.value(),
          .length = 257,
          .vmo_offset = 0,
          .dev_offset = 0,
      },
  };
  ASSERT_OK(fifo.write(requests, 2, &actual_count));
  ASSERT_EQ(actual_count, 2);

  ASSERT_OK(fifo.wait_one(ZX_FIFO_READABLE | ZX_FIFO_PEER_CLOSED, zx::time::infinite(), &seen));
  ASSERT_OK(fifo.read_one(&response));

  EXPECT_EQ(response.reqid, 104);
}

TEST(OffsetMap, InvalidMapping) {
  // Empty mappings
  ASSERT_EQ(OffsetMap::Create(std::vector<fuchsia_storage_block::wire::BlockOffsetMapping>{}, 10000)
                .status_value(),
            ZX_ERR_INVALID_ARGS);

  // Zero-length
  ASSERT_EQ(OffsetMap::Create(
                std::vector<fuchsia_storage_block::wire::BlockOffsetMapping>{
                    {
                        .target_block_offset = 1000,
                        .length = 0,
                    },
                },
                10000)
                .status_value(),
            ZX_ERR_INVALID_ARGS);

  // Target overflow
  ASSERT_EQ(OffsetMap::Create(
                std::vector<fuchsia_storage_block::wire::BlockOffsetMapping>{
                    {
                        .target_block_offset = std::numeric_limits<uint64_t>::max(),
                        .length = 100,
                    },
                },
                10000)
                .status_value(),
            ZX_ERR_OUT_OF_RANGE);

  // Target extends beyond device block_count
  ASSERT_EQ(OffsetMap::Create(
                std::vector<fuchsia_storage_block::wire::BlockOffsetMapping>{
                    {
                        .target_block_offset = 500,
                        .length = 600,
                    },
                },
                1000)
                .status_value(),
            ZX_ERR_OUT_OF_RANGE);

  // Device with block_count == 0 rejects any mapping
  ASSERT_EQ(OffsetMap::Create(
                std::vector<fuchsia_storage_block::wire::BlockOffsetMapping>{
                    {
                        .target_block_offset = 0,
                        .length = 1,
                    },
                },
                0)
                .status_value(),
            ZX_ERR_OUT_OF_RANGE);
}

TEST(OffsetMap, RemapRequests) {
  zx::result map = OffsetMap::Create(
      std::vector<fuchsia_storage_block::wire::BlockOffsetMapping>{
          {
              .target_block_offset = 1000,
              .length = 100,
          },
      },
      10000);
  ASSERT_OK(map);
  BlockFifoRequest request{
      .command = {.opcode = BLOCK_OPCODE_WRITE},
      .reqid = 1,
      .group = 2,
      .vmoid = 3,
      .length = 10,
      .vmo_offset = 0x2000,
  };
  const BlockFifoRequest orig_request = request;

  auto AssertUnchangedExceptOffset = [&orig_request](BlockFifoRequest& request) {
    request.dev_offset = orig_request.dev_offset;
    ASSERT_BYTES_EQ(&request, &orig_request, sizeof(request));
  };

  request.dev_offset = 0;
  ASSERT_TRUE(map->AdjustRequest(request));
  ASSERT_EQ(request.dev_offset, 1000);
  AssertUnchangedExceptOffset(request);

  request.dev_offset = 30;
  ASSERT_TRUE(map->AdjustRequest(request));
  ASSERT_EQ(request.dev_offset, 1030);
  AssertUnchangedExceptOffset(request);

  request.dev_offset = 90;
  ASSERT_TRUE(map->AdjustRequest(request));
  ASSERT_EQ(request.dev_offset, 1090);
  AssertUnchangedExceptOffset(request);

  // Past end of map
  request.dev_offset = 91;
  ASSERT_FALSE(map->AdjustRequest(request));
  AssertUnchangedExceptOffset(request);
}

TEST(OffsetMap, MultipleMappings) {
  fuchsia_storage_block::wire::BlockOffsetMapping mappings[] = {
      {.target_block_offset = 100, .length = 50},
      {.target_block_offset = 150, .length = 30},
      {.target_block_offset = 250, .length = 20},
  };
  zx::result map = OffsetMap::Create(mappings, 1000);
  ASSERT_OK(map);

  BlockFifoRequest request{
      .command = {.opcode = BLOCK_OPCODE_WRITE},
      .reqid = 1,
      .group = 2,
      .vmoid = 3,
      .length = 10,
      .vmo_offset = 0x2000,
  };
  const BlockFifoRequest orig_request = request;

  auto TestRemap = [&orig_request, &map](uint64_t dev_offset, uint32_t length,
                                         bool expected_success, uint64_t expected_dev_offset) {
    BlockFifoRequest request = orig_request;
    request.dev_offset = dev_offset;
    request.length = length;
    ASSERT_EQ(map->AdjustRequest(request), expected_success);
    if (expected_success) {
      ASSERT_EQ(request.dev_offset, expected_dev_offset);
    }
    request.dev_offset = orig_request.dev_offset;
    request.length = orig_request.length;
    ASSERT_BYTES_EQ(&request, &orig_request, sizeof(request));
  };

  // Falls in first extent (logical 0..50 -> physical 100..150)
  TestRemap(10, 20, true, 110);

  // Falls in second extent which was coalesced with the first (logical 50..80 -> physical 150..180)
  TestRemap(55, 25, true, 155);

  // Spans across the boundary of mapping 1 and mapping 2 (which are not contiguous)
  TestRemap(70, 15, false, 0);

  // Falls in third extent (logical 80..100 -> physical 250..270)
  TestRemap(85, 10, true, 255);

  // Past the end of all mappings
  TestRemap(95, 10, false, 0);
}

TEST(OffsetMap, MultipleMappingsInvalid) {
  // One mapping has 0 length
  {
    fuchsia_storage_block::wire::BlockOffsetMapping mappings[] = {
        {.target_block_offset = 100, .length = 50},
        {.target_block_offset = 150, .length = 0},
    };
    ASSERT_EQ(OffsetMap::Create(mappings, 1000).status_value(), ZX_ERR_INVALID_ARGS);
  }
  // Sum of lengths overflows uint64_t
  {
    fuchsia_storage_block::wire::BlockOffsetMapping mappings[] = {
        {.target_block_offset = 0, .length = std::numeric_limits<uint64_t>::max() - 10},
        {.target_block_offset = 0, .length = 20},
    };
    ASSERT_EQ(OffsetMap::Create(mappings, std::numeric_limits<uint64_t>::max()).status_value(),
              ZX_ERR_OUT_OF_RANGE);
  }
}

TEST_F(ServerTestFixture, InvalidGroupId) {
  zx::result fifo_result = server_->GetFifo();
  ASSERT_OK(fifo_result);
  fzl::fifo<BlockFifoRequest, BlockFifoResponse> fifo(std::move(fifo_result.value()));
  CreateThread();
  auto cleanup = fit::defer([&] {
    server_->Shutdown();
    JoinThread();
  });

  groupid_t invalid_group = MAX_TXN_GROUP_COUNT + 1;
  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_WRITE,
                  .flags = BLOCK_IO_FLAG_GROUP_ITEM | BLOCK_IO_FLAG_GROUP_LAST},
      .reqid = 200,
      .group = invalid_group,
      .vmoid = BLOCK_VMOID_INVALID,
  };

  size_t actual_count = 0;
  ASSERT_OK(fifo.write(&request, 1, &actual_count));
  ASSERT_EQ(actual_count, 1);

  BlockFifoResponse response;
  zx_signals_t seen;
  ASSERT_OK(fifo.wait_one(ZX_FIFO_READABLE | ZX_FIFO_PEER_CLOSED, zx::time::infinite(), &seen));
  ASSERT_OK(fifo.read_one(&response));

  EXPECT_EQ(response.status, ZX_ERR_IO);
  EXPECT_EQ(response.reqid, 200);
  EXPECT_EQ(response.group, invalid_group);
}

TEST_F(ServerTestFixture, CreateServerWithMultipleMappings) {
  fuchsia_storage_block::wire::BlockOffsetMapping mappings[] = {
      {.target_block_offset = 0, .length = 10},
      {.target_block_offset = 20, .length = 10},
  };
  zx::result server = Server::Create(&client_, mappings);
  ASSERT_OK(server);
}

TEST_F(ServerTestFixture, CreateServerWithEmptyMappingsFails) {
  std::vector<fuchsia_storage_block::wire::BlockOffsetMapping> mappings;
  zx::result server = Server::Create(&client_, mappings);
  ASSERT_EQ(server.status_value(), ZX_ERR_INVALID_ARGS);
}

TEST(OffsetMap, MapMethod) {
  fuchsia_storage_block::wire::BlockOffsetMapping mappings[] = {
      {.target_block_offset = 100, .length = 50},
      {.target_block_offset = 200, .length = 30},
  };
  zx::result map = OffsetMap::Create(mappings, 1000);
  ASSERT_OK(map);

  auto res1 = map->Map(40, 20);
  ASSERT_OK(res1);
  EXPECT_EQ(res1->first, 140u);
  EXPECT_EQ(res1->second, 10u);

  auto res2 = map->Map(50, 20);
  ASSERT_OK(res2);
  EXPECT_EQ(res2->first, 200u);
  EXPECT_EQ(res2->second, 20u);

  auto res3 = map->Map(80, 10);
  EXPECT_TRUE(res3.is_error());
}

TEST_F(ServerTestFixture, RequestSpanningMultipleMappings) {
  fuchsia_storage_block::wire::BlockOffsetMapping mappings[] = {
      {.target_block_offset = 100, .length = 10},
      {.target_block_offset = 500, .length = 10},
  };
  zx::result server = Server::Create(&client_, mappings);
  ASSERT_OK(server);
  server_ = std::move(server.value());

  zx::result fifo_result = server_->GetFifo();
  ASSERT_OK(fifo_result);
  fzl::fifo<BlockFifoRequest, BlockFifoResponse> fifo(std::move(fifo_result.value()));

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(4096 * 10, 0, &vmo));
  zx::result vmoid = server_->AttachVmo(std::move(vmo));
  ASSERT_OK(vmoid);

  CreateThread();
  auto cleanup = fit::defer([&] {
    server_->Shutdown();
    JoinThread();
  });

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_WRITE, .flags = 0},
      .reqid = 300,
      .group = 0,
      .vmoid = vmoid.value(),
      .length = 15,
      .vmo_offset = 0,
      .dev_offset = 5,
  };

  size_t actual_count = 0;
  ASSERT_OK(fifo.write(&request, 1, &actual_count));
  ASSERT_EQ(actual_count, 1);

  BlockFifoResponse response;
  zx_signals_t seen;
  ASSERT_OK(fifo.wait_one(ZX_FIFO_READABLE | ZX_FIFO_PEER_CLOSED, zx::time::infinite(), &seen));
  ASSERT_OK(fifo.read_one(&response));

  EXPECT_OK(response.status);
  EXPECT_EQ(response.reqid, 300);

  const auto& ops = blkdev_.GetOperationSequence();
  ASSERT_EQ(ops.size(), 2u);
  EXPECT_EQ(ops[0].rw.offset_dev, 105u);
  EXPECT_EQ(ops[0].rw.length, 5u);
  EXPECT_EQ(ops[1].rw.offset_dev, 500u);
  EXPECT_EQ(ops[1].rw.length, 10u);
}

}  // namespace
