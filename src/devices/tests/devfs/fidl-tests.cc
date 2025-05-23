// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.io/cpp/wire.h>
#include <fidl/fuchsia.io/cpp/wire_test_base.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/namespace.h>
#include <lib/zx/channel.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <utility>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace {

namespace fio = fuchsia_io;

void FidlOpenValidator(const fidl::ClientEnd<fio::Directory>& directory, const char* path,
                       zx::result<fio::wire::Representation::Tag> expected) {
  auto [client, server] = fidl::Endpoints<fio::Node>::Create();
  const fidl::Status result = fidl::WireCall(directory)->Open(
      fidl::StringView::FromExternal(path),
      fio::wire::kPermReadable | fio::wire::Flags::kFlagSendRepresentation, {},
      server.TakeChannel());
  ASSERT_OK(result.status());

  class EventHandler : public fidl::testing::WireSyncEventHandlerTestBase<fio::Node> {
   public:
    fio::wire::Representation::Tag tag() const { return tag_; }

    void OnRepresentation(fidl::WireEvent<fio::Node::OnRepresentation>* event) override {
      tag_ = event->Which();
    }

    void NotImplemented_(const std::string& name) override {
      FAIL() << "Unexpected " << name.c_str();
    }

   private:
    fio::wire::Representation::Tag tag_;
  };

  EventHandler event_handler;
  zx_status_t status = event_handler.HandleOneEvent(client).status();
  ASSERT_EQ(status, expected.status_value()) << zx_status_get_string(status);
  if (expected.is_ok()) {
    ASSERT_EQ(event_handler.tag(), *expected);
  }
}

// Ensure that our hand-rolled FIDL messages within devfs and memfs are acting correctly
// for open event messages (on both success and error).
TEST(FidlTestCase, OpenDev) {
  auto endpoints = fidl::Endpoints<fio::Directory>::Create();
  ASSERT_OK(fdio_open3("/dev", static_cast<uint64_t>(fio::wire::kPermReadable),
                       endpoints.server.channel().release()));

  FidlOpenValidator(endpoints.client, "zero", zx::ok(fio::wire::Representation::Tag::kFile));
  FidlOpenValidator(endpoints.client, "this-path-better-not-actually-exist",
                    zx::error(ZX_ERR_NOT_FOUND));
  FidlOpenValidator(endpoints.client, "zero/this-path-better-not-actually-exist",
                    zx::error(ZX_ERR_NOT_SUPPORTED));
}

TEST(FidlTestCase, OpenPkg) {
  auto endpoints = fidl::Endpoints<fio::Directory>::Create();
  ASSERT_OK(fdio_open3("/pkg", static_cast<uint64_t>(fio::wire::kPermReadable),
                       endpoints.server.channel().release()));

  FidlOpenValidator(endpoints.client, "bin", zx::ok(fio::wire::Representation::Tag::kDirectory));
  FidlOpenValidator(endpoints.client, "this-path-better-not-actually-exist",
                    zx::error(ZX_ERR_NOT_FOUND));
}

TEST(FidlTestCase, BasicDevClass) {
  auto endpoints = fidl::Endpoints<fio::Node>::Create();
  ASSERT_OK(fdio_open3("/dev/class", uint64_t{fio::wire::kPermReadable},
                       endpoints.server.channel().release()));
  const fidl::WireResult result = fidl::WireCall(endpoints.client)->Query();
  ASSERT_OK(result.status());
  const auto& response = result.value();
  const cpp20::span data = response.protocol.get();
  const std::string_view protocol{reinterpret_cast<const char*>(data.data()), data.size_bytes()};
  ASSERT_EQ(protocol, fio::wire::kDirectoryProtocolName);
}

TEST(FidlTestCase, BasicDevZero) {
  auto endpoints = fidl::Endpoints<fio::Node>::Create();
  ASSERT_OK(fdio_open3("/dev/zero", uint64_t{fio::wire::kPermReadable},
                       endpoints.server.channel().release()));
  const fidl::WireResult result = fidl::WireCall(endpoints.client)->Query();
  ASSERT_OK(result.status());
  const auto& response = result.value();
  const cpp20::span data = response.protocol.get();
  const std::string_view protocol{reinterpret_cast<const char*>(data.data()), data.size_bytes()};
  ASSERT_EQ(protocol, fio::wire::kFileProtocolName);
}

using watch_buffer_t = struct {
  // Buffer containing cached messages
  uint8_t buf[fio::wire::kMaxBuf];
  uint8_t name_buf[fio::wire::kMaxNameLength + 1];
  // Offset into 'buf' of next message
  uint8_t* ptr;
  // Maximum size of buffer
  size_t size;
};

void CheckLocalEvent(watch_buffer_t* wb, const char** name, fio::WatchEvent* event) {
  ASSERT_NE(wb->ptr, nullptr);

  // Used a cached event
  *event = static_cast<fio::WatchEvent>(wb->ptr[0]);
  ASSERT_LT(static_cast<size_t>(wb->ptr[1]), sizeof(wb->name_buf));
  memcpy(wb->name_buf, wb->ptr + 2, wb->ptr[1]);
  wb->name_buf[wb->ptr[1]] = 0;
  *name = reinterpret_cast<const char*>(wb->name_buf);
  wb->ptr += wb->ptr[1] + 2;
  ASSERT_LE((uintptr_t)wb->ptr, (uintptr_t)wb->buf + wb->size);
  if (wb->ptr == wb->buf + wb->size) {
    wb->ptr = nullptr;
  }
}

// Read the next event off the channel.  Storage for |*name| will be reused
// between calls.
void ReadEvent(watch_buffer_t* wb, const fidl::ClientEnd<fio::DirectoryWatcher>& client_end,
               const char** name, fio::WatchEvent* event) {
  if (wb->ptr == nullptr) {
    zx_signals_t observed;
    ASSERT_OK(client_end.channel().wait_one(ZX_CHANNEL_READABLE, zx::time::infinite(), &observed));
    ASSERT_EQ(observed & ZX_CHANNEL_READABLE, ZX_CHANNEL_READABLE);
    uint32_t actual;
    ASSERT_OK(client_end.channel().read(0, wb->buf, nullptr, sizeof(wb->buf), 0, &actual, nullptr));
    wb->size = actual;
    wb->ptr = wb->buf;
  }
  CheckLocalEvent(wb, name, event);
}

TEST(FidlTestCase, DirectoryWatcherExisting) {
  auto endpoints = fidl::Endpoints<fio::Directory>::Create();

  zx::result watcher_endpoints = fidl::CreateEndpoints<fio::DirectoryWatcher>();
  ASSERT_OK(watcher_endpoints.status_value());

  ASSERT_OK(fdio_open3("/dev/class", uint64_t{fio::wire::kPermReadable},
                       endpoints.server.channel().release()));

  const fidl::WireResult result =
      fidl::WireCall(endpoints.client)
          ->Watch(fio::wire::WatchMask::kMask, 0, std::move(watcher_endpoints->server));
  ASSERT_OK(result.status());
  const auto& response = result.value();
  ASSERT_OK(response.s);

  watch_buffer_t wb = {};
  // We should see nothing but EXISTING events until we see an IDLE event
  while (true) {
    const char* name = nullptr;
    fio::wire::WatchEvent event;
    ReadEvent(&wb, watcher_endpoints->client, &name, &event);
    if (event == fio::wire::WatchEvent::kIdle) {
      ASSERT_STREQ(name, "");
      break;
    }
    ASSERT_EQ(event, fio::wire::WatchEvent::kExisting);
    ASSERT_STRNE(name, "");
  }
}

TEST(FidlTestCase, DirectoryWatcherWithClosedHalf) {
  auto endpoints = fidl::Endpoints<fio::Directory>::Create();

  ASSERT_OK(fdio_open3("/dev/class", uint64_t{fio::wire::kPermReadable},
                       endpoints.server.channel().release()));

  {
    zx::result watcher_endpoints = fidl::CreateEndpoints<fio::DirectoryWatcher>();
    ASSERT_OK(watcher_endpoints.status_value());

    // Close our end of the watcher before devmgr gets its end.
    watcher_endpoints->client.reset();

    const fidl::WireResult result =
        fidl::WireCall(endpoints.client)
            ->Watch(fio::wire::WatchMask::kMask, 0, std::move(watcher_endpoints->server));
    ASSERT_OK(result.status());
    const auto& response = result.value();
    ASSERT_OK(response.s);
    // If we're here and usermode didn't crash, we didn't hit the bug.
  }

  {
    // Create a new watcher, and see if it's functional at all
    auto watcher_endpoints = fidl::Endpoints<fio::DirectoryWatcher>::Create();

    const fidl::WireResult result =
        fidl::WireCall(endpoints.client)
            ->Watch(fio::wire::WatchMask::kMask, 0, std::move(watcher_endpoints.server));
    ASSERT_OK(result.status());
    const auto& response = result.value();
    ASSERT_OK(response.s);

    watch_buffer_t wb = {};
    const char* name = nullptr;
    fio::wire::WatchEvent event;
    ReadEvent(&wb, watcher_endpoints.client, &name, &event);
    ASSERT_EQ(event, fio::wire::WatchEvent::kExisting);
  }
}

}  // namespace
