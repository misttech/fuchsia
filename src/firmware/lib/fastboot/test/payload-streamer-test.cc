// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "payload-streamer.h"

#include <fidl/fuchsia.paver/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>

#include <gtest/gtest.h>

namespace fastboot {

namespace {
TEST(PayloadStreamerTest, RegisterVmo) {
  const char data[] = "payload streamer data";
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_paver::PayloadStream>::Create();

  fidl::WireSyncClient<fuchsia_paver::PayloadStream> client =
      fidl::WireSyncClient(std::move(client_end));

  // Launch thread which implements interface.
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  internal::PayloadStreamer streamer(std::move(server_end), data, sizeof(data));
  loop.StartThread("fastboot-payload-stream");

  zx::vmo vmo;
  ASSERT_EQ(zx::vmo::create(1, 0, &vmo), ZX_OK);
  auto result = client->RegisterVmo(std::move(vmo));
  ASSERT_EQ(result.status(), ZX_OK);
  ASSERT_EQ(result.value().status, ZX_OK);
}

TEST(PayloadStreamerTest, RegisterVmoAgainErrorsOut) {
  const char data[] = "payload streamer data";
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_paver::PayloadStream>::Create();

  fidl::WireSyncClient<fuchsia_paver::PayloadStream> client =
      fidl::WireSyncClient(std::move(client_end));

  // Launch thread which implements interface.
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  internal::PayloadStreamer streamer(std::move(server_end), data, sizeof(data));
  loop.StartThread("fastboot-payload-stream");

  {
    zx::vmo vmo;
    ASSERT_EQ(zx::vmo::create(1, 0, &vmo), ZX_OK);
    auto result = client->RegisterVmo(std::move(vmo));
    ASSERT_EQ(result.status(), ZX_OK);
    ASSERT_EQ(result.value().status, ZX_OK);
  }

  {
    zx::vmo vmo;
    ASSERT_EQ(zx::vmo::create(1, 0, &vmo), ZX_OK);
    auto result = client->RegisterVmo(std::move(vmo));
    ASSERT_EQ(result.status(), ZX_OK);
    EXPECT_EQ(result.value().status, ZX_ERR_ALREADY_BOUND);
  }
}

TEST(PayloadStreamerTest, ReadData) {
  const char data[] = "payload streamer data";
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_paver::PayloadStream>::Create();

  fidl::WireSyncClient<fuchsia_paver::PayloadStream> client =
      fidl::WireSyncClient(std::move(client_end));

  // Launch thread which implements interface.
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  internal::PayloadStreamer streamer(std::move(server_end), data, sizeof(data));
  loop.StartThread("fastboot-payload-stream");

  zx::vmo vmo, dup;
  ASSERT_EQ(zx::vmo::create(sizeof(data), 0, &vmo), ZX_OK);
  ASSERT_EQ(vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &dup), ZX_OK);
  auto register_result = client->RegisterVmo(std::move(dup));
  ASSERT_EQ(register_result.status(), ZX_OK);
  ASSERT_EQ(register_result.value().status, ZX_OK);

  auto read_result = client->ReadData();
  ASSERT_EQ(read_result.status(), ZX_OK);
  ASSERT_TRUE(read_result.value().result.is_info());

  char buffer[sizeof(data)] = {};
  ASSERT_EQ(read_result.value().result.info().size, sizeof(buffer));
  ASSERT_EQ(vmo.read(buffer, read_result.value().result.info().offset,
                     read_result.value().result.info().size),
            ZX_OK);
  ASSERT_EQ(memcmp(data, buffer, sizeof(data)), 0);

  // eof is returned if continue to read.
  auto eof_result = client->ReadData();
  ASSERT_EQ(eof_result.status(), ZX_OK);
  ASSERT_TRUE(eof_result.value().result.is_eof());
}

}  // namespace

}  // namespace fastboot
