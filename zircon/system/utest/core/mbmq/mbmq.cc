// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/testonly-syscalls.h>

#include <zxtest/zxtest.h>

namespace {

zx_status_t msgqueue_create(uint32_t options, zx::handle* out) {
  return zx_msgqueue_create(options, out->reset_and_get_address());
}

zx_status_t mbo_create(uint32_t options, zx::handle* out) {
  return zx_mbo_create(options, out->reset_and_get_address());
}

zx_status_t calleesref_create(uint32_t options, zx::handle* out) {
  return zx_calleesref_create(options, out->reset_and_get_address());
}

zx_status_t msgqueue_create_channel(zx_handle_t msgqueue, uint64_t key, zx::handle* out) {
  return zx_msgqueue_create_channel(msgqueue, key, out->reset_and_get_address());
}

// Helper for creating a pair of channel endpoints.
struct Channel {
  Channel() {
    zx::handle msgqueue;
    ASSERT_OK(msgqueue_create(0, &msgqueue));
    zx::handle channel;
    uint64_t key = 123;
    ASSERT_OK(msgqueue_create_channel(msgqueue.get(), key, &channel));

    ch1 = std::move(channel);
    ch2 = std::move(msgqueue);
  }

  zx::handle ch1;
  zx::handle ch2;
};

void AssertMBONotAccessible(const zx::handle& mbo) {
  // The MBO should not be writable.
  static const char kMsg[] = "example message";
  ASSERT_EQ(zx_mbo_write(mbo.get(), 0, kMsg, sizeof(kMsg), nullptr, 0), ZX_ERR_BAD_STATE);

  // The MBO should not be readable.
  char buffer[100] = {};
  uint32_t actual_bytes = 999;
  uint32_t actual_handles = 999;
  ASSERT_EQ(
      zx_mbo_read(mbo.get(), 0, buffer, nullptr, sizeof(buffer), 0, &actual_bytes, &actual_handles),
      ZX_ERR_BAD_STATE);
}

TEST(MbmqTest, MboWriteAndRead) {
  zx::handle mbo;
  ASSERT_OK(mbo_create(0, &mbo));

  static const char kMessage[] = "example message";
  ASSERT_OK(zx_mbo_write(mbo.get(), 0, kMessage, sizeof(kMessage), nullptr, 0));

  char buffer[100] = {};
  uint32_t actual_bytes = 999;
  uint32_t actual_handles = 999;
  ASSERT_OK(zx_mbo_read(mbo.get(), 0, buffer, nullptr, sizeof(buffer), 0, &actual_bytes,
                        &actual_handles));
  ASSERT_EQ(actual_bytes, sizeof(kMessage));
  ASSERT_EQ(actual_handles, 0);
  ASSERT_EQ(memcmp(buffer, kMessage, actual_bytes), 0);

  // TODO: test read and write of handles
  // TODO: test error case where buffer is too small
  // TODO: test reading twice
  // TODO: test writing twice
}

TEST(MbmqTest, MboSend) {
  // MboAndQueue mboq;
  // zx::handle& mbo = mboq.mbo;
  zx::handle mbo;
  ASSERT_OK(mbo_create(0, &mbo));

  Channel channel;
  zx::handle calleesref;
  ASSERT_OK(calleesref_create(0, &calleesref));

  // for (int i = 0; i < 2; ++i) {

  // Send request message.
  static const char kRequest[] = "example request";
  ASSERT_OK(zx_mbo_write(mbo.get(), 0, kRequest, sizeof(kRequest), nullptr, 0));
  ASSERT_OK(zx_channel_write_mbo(channel.ch1.get(), mbo.get()));

  // TODO: Test that zx_channel_write_mbo() and zx_msgqueue_wait()
  // check handle permissions.

  // Now that the MBO is in a "sent" state, it cannot be written to or
  // read from.
  AssertMBONotAccessible(mbo);

  // TODO: test that the MBO cannot be re-sent on a channel now

  // Read the request message.
  ASSERT_OK(zx_msgqueue_wait(channel.ch2.get(), calleesref.get()));
  char buffer[100] = {};
  uint32_t actual_bytes = 999;
  uint32_t actual_handles = 999;
  ASSERT_OK(zx_mbo_read(calleesref.get(), 0, buffer, nullptr, sizeof(buffer), 0, &actual_bytes,
                        &actual_handles));
  ASSERT_EQ(actual_bytes, sizeof(kRequest));
  ASSERT_EQ(actual_handles, 0);
  ASSERT_EQ(memcmp(buffer, kRequest, actual_bytes), 0);

  // // Write the reply message.
  // static const char kReply[] = "example reply";
  // ASSERT_OK(zx_mbo_write(calleesref.get(), 0, kReply, sizeof(kReply), nullptr, 0));

  // // Before the reply is sent, the MBO should not be readable.
  // AssertMBONotAccessible(mbo);

  // // Send the reply message.
  // ASSERT_OK(zx_calleesref_send_reply(calleesref.get()));
  // // The CalleesRef no longer holds a reference to the MBO, so we can't call
  // // send_reply() on it again.
  // ASSERT_EQ(zx_calleesref_send_reply(calleesref.get()), ZX_ERR_NOT_CONNECTED);

  // // The MBO is still not accessible until it is dequeued.
  // AssertMBONotAccessible(mbo);

  // ASSERT_OK(zx_channel_read_mbo(mboq.msgqueue.get(), calleesref.get()));

  // // Read the reply message.
  // actual_bytes = 999;
  // actual_handles = 999;
  // ASSERT_OK(zx_mbo_read(mbo.get(), 0, buffer, nullptr, sizeof(buffer), 0,
  //                       &actual_bytes, &actual_handles));
  // ASSERT_EQ(actual_bytes, sizeof(kReply));
  // ASSERT_EQ(actual_handles, 0);
  // ASSERT_EQ(memcmp(buffer, kReply, actual_bytes), 0);

  // }
}

}  // namespace
