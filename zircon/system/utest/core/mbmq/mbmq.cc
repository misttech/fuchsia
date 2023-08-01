// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zx/thread.h>
#include <zircon/process.h>
#include <zircon/testonly-syscalls.h>

#include <thread>

#include <zxtest/zxtest.h>

// XXX: This hack overrides zxtest.h.  It allows ASSERT_OK() to be used
// from functions with non-void return types.
#undef LIB_ZXTEST_RETURN_IF_FATAL_true
#define LIB_ZXTEST_RETURN_IF_FATAL_true \
  do {                                  \
    abort();                            \
  } while (0)

namespace {

zx_status_t msgqueue_create(uint32_t options, zx::handle* out) {
  return zx_msgqueue_create(options, out->reset_and_get_address());
}

zx_status_t mbo_create(uint32_t options, zx_handle_t msgqueue, zx::handle* out) {
  uint64_t key = 123;
  return zx_mbo_create(options, msgqueue, key, out->reset_and_get_address());
}

zx_status_t mbo_create(uint32_t options, zx::handle* out) {
  zx::handle msgqueue;
  ASSERT_OK(msgqueue_create(0, &msgqueue));
  uint64_t key = 123;
  return zx_mbo_create(options, msgqueue.get(), key, out->reset_and_get_address());
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

struct MboAndQueue {
  MboAndQueue() {
    ASSERT_OK(msgqueue_create(0, &msgqueue));
    ASSERT_OK(mbo_create(0, msgqueue.get(), &mbo));
  }

  zx::handle msgqueue;
  zx::handle mbo;
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
  MboAndQueue mboq;
  zx::handle& mbo = mboq.mbo;
  Channel channel;
  zx::handle calleesref;
  ASSERT_OK(calleesref_create(0, &calleesref));

  for (int i = 0; i < 2; ++i) {
    // Send request message.
    static const char kRequest[] = "example request";
    ASSERT_OK(zx_mbo_write(mbo.get(), 0, kRequest, sizeof(kRequest), nullptr, 0));
    ASSERT_OK(zx_channel_write_mbo(channel.ch1.get(), mbo.get()));

    // TODO: Test that zx_channel_write_mbo() and zx_msgqueue_wait()
    // check handle permissions.

    // Now that the MBO is in a "sent" state, it cannot be written to
    // or read from.
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

    // Write the reply message.
    static const char kReply[] = "example reply";
    ASSERT_OK(zx_mbo_write(calleesref.get(), 0, kReply, sizeof(kReply), nullptr, 0));

    // Before the reply is sent, the MBO should not be readable.
    AssertMBONotAccessible(mbo);

    // Send the reply message.
    ASSERT_OK(zx_calleesref_send_reply(calleesref.get()));
    // The CalleesRef no longer holds a reference to the MBO, so we
    // can't call send_reply() on it again.
    ASSERT_EQ(zx_calleesref_send_reply(calleesref.get()), ZX_ERR_NOT_CONNECTED);

    // The MBO is still not accessible until it is dequeued.
    AssertMBONotAccessible(mbo);

    ASSERT_OK(zx_msgqueue_wait(mboq.msgqueue.get(), calleesref.get()));

    // Read the reply message.
    actual_bytes = 999;
    actual_handles = 999;
    ASSERT_OK(zx_mbo_read(mbo.get(), 0, buffer, nullptr, sizeof(buffer), 0, &actual_bytes,
                          &actual_handles));
    ASSERT_EQ(actual_bytes, sizeof(kReply));
    ASSERT_EQ(actual_handles, 0);
    ASSERT_EQ(memcmp(buffer, kReply, actual_bytes), 0);
  }
}

TEST(MbmqTest, WaitWakeup) {
  zx::handle mbo;
  ASSERT_OK(mbo_create(0, &mbo));
  Channel channel;
  zx::handle calleesref;
  ASSERT_OK(calleesref_create(0, &calleesref));

  std::thread thread([&] {
    // Wait for and read the request message.
    ASSERT_OK(zx_msgqueue_wait(channel.ch2.get(), calleesref.get()));
  });
  // Sleep to give the thread time to block.
  // TODO: Change to poll until we confirm the thread has blocked.
  ASSERT_OK(zx_nanosleep(zx_deadline_after(ZX_MSEC(10))));

  // Send request message.
  static const char kRequest[] = "example request";
  ASSERT_OK(zx_mbo_write(mbo.get(), 0, kRequest, sizeof(kRequest), nullptr, 0));
  ASSERT_OK(zx_channel_write_mbo(channel.ch1.get(), mbo.get()));

  // Wait for the request to be received by the other thread.
  thread.join();

  // Read the request message from the CalleesRef.
  char buffer[100] = {};
  uint32_t actual_bytes = 999;
  uint32_t actual_handles = 999;
  ASSERT_OK(zx_mbo_read(calleesref.get(), 0, buffer, nullptr, sizeof(buffer), 0, &actual_bytes,
                        &actual_handles));
  ASSERT_EQ(actual_bytes, sizeof(kRequest));
  ASSERT_EQ(actual_handles, 0);
  ASSERT_EQ(memcmp(buffer, kRequest, actual_bytes), 0);
}

// Test suspending a thread that is blocked in zx_msgqueue_wait().
TEST(MbmqTest, SuspendMsgQueueWait) {
  Channel channel;
  zx::handle calleesref;
  ASSERT_OK(calleesref_create(0, &calleesref));

  std::atomic<zx_handle_t> thread_handle(ZX_HANDLE_INVALID);
  std::thread thread([&] {
    // We can get a pthread_t from a std::thread using native_handle(), and
    // we can get a thread zx_handle_t from a thrd_t using
    // thrd_get_zx_handle(), but we can't get a thrd_t from a pthread_t.
    // So instead we resort to having the child thread provide its own
    // thread zx_handle_t to the parent as follows.
    thread_handle.store(_zx_thread_self());

    // TODO: We should mark the syscall as "[blocking]" so that the VDSO
    // wrapper retries instead of getting ZX_ERR_INTERNAL_INTR_RETRY
    // returned here.
    ASSERT_EQ(zx_msgqueue_wait(channel.ch2.get(), calleesref.get()), ZX_ERR_INTERNAL_INTR_RETRY);
  });
  // Sleep to give the thread time to block.
  // TODO: Change to poll until we confirm the thread has blocked.
  ASSERT_OK(zx_nanosleep(zx_deadline_after(ZX_MSEC(10))));

  zx::unowned_thread thread_h(thread_handle.load());
  zx::suspend_token suspend_token;
  ASSERT_OK(thread_h->suspend(&suspend_token));
  // Wait for the thread to suspend.
  ASSERT_OK(thread_h->wait_one(ZX_THREAD_SUSPENDED, zx::time::infinite(), nullptr));
  // Resume the thread.
  suspend_token.reset();
  thread.join();
}

TEST(MbmqTest, UnconnectedCalleesRef) {
  zx::handle calleesref;
  ASSERT_OK(calleesref_create(0, &calleesref));

  // When a CalleesRef is unconnected, zx_calleesref_send_reply() should
  // return an error.
  ASSERT_EQ(zx_calleesref_send_reply(calleesref.get()), ZX_ERR_NOT_CONNECTED);

  // When a CalleesRef is unconnected, you should not be able to write to
  // it.
  static const char kReply[] = "example reply";
  ASSERT_EQ(zx_mbo_write(calleesref.get(), 0, kReply, sizeof(kReply), nullptr, 0),
            ZX_ERR_NOT_CONNECTED);

  // When a CalleesRef is unconnected, you should not be able to read from
  // it.
  char buffer[100] = {};
  uint32_t actual_bytes = 999;
  uint32_t actual_handles = 999;
  ASSERT_EQ(zx_mbo_read(calleesref.get(), 0, buffer, nullptr, sizeof(buffer), 0, &actual_bytes,
                        &actual_handles),
            ZX_ERR_NOT_CONNECTED);
}

TEST(MbmqTest, SendEmptyMbo) {
  zx::handle mbo;
  ASSERT_OK(mbo_create(0, &mbo));
  Channel channel;

  ASSERT_EQ(zx_channel_write_mbo(channel.ch1.get(), mbo.get()), ZX_ERR_BAD_STATE);
}

TEST(MbmqTest, SendEmptyCalleesRef) {
  zx::handle mbo;
  ASSERT_OK(mbo_create(0, &mbo));
  Channel channel;
  zx::handle calleesref;
  ASSERT_OK(calleesref_create(0, &calleesref));

  // Send request message.
  static const char kRequest[] = "example request";
  ASSERT_OK(zx_mbo_write(mbo.get(), 0, kRequest, sizeof(kRequest), nullptr, 0));
  ASSERT_OK(zx_channel_write_mbo(channel.ch1.get(), mbo.get()));
  // Read the request message.
  ASSERT_OK(zx_msgqueue_wait(channel.ch2.get(), calleesref.get()));
  char buffer[100] = {};
  uint32_t actual_bytes = 999;
  uint32_t actual_handles = 999;
  ASSERT_OK(zx_mbo_read(calleesref.get(), 0, buffer, nullptr, sizeof(buffer), 0, &actual_bytes,
                        &actual_handles));

  // zx_mbo_read() cleared the message from the CalleesRef.  That means
  // that if we try to send a reply from the CalleesRef now, we should get
  // an error.
  ASSERT_EQ(zx_calleesref_send_reply(calleesref.get()), ZX_ERR_BAD_STATE);
}

// Check that the given MBO was sent an empty reply message, which is what
// is expected when the MBO is sent an auto-reply.
void AssertMBOReceivedAutoReply(MboAndQueue* mboq) {
  zx::handle calleesref;
  ASSERT_OK(calleesref_create(0, &calleesref));

  // The MBO should be enqueued on the MsgQueue now.
  ASSERT_OK(zx_msgqueue_wait(mboq->msgqueue.get(), calleesref.get()));

  // Check the message that was returned.
  char buffer[100] = {};
  uint32_t actual_bytes = 999;
  uint32_t actual_handles = 999;
  ASSERT_OK(zx_mbo_read(mboq->mbo.get(), 0, buffer, nullptr, sizeof(buffer), 0, &actual_bytes,
                        &actual_handles));
  ASSERT_EQ(actual_bytes, 0);
  ASSERT_EQ(actual_handles, 0);
}

TEST(MbmqTest, AutoReplyWhenMessageDropped) {
  MboAndQueue mboq;
  zx::handle& mbo = mboq.mbo;
  Channel channel;

  // Send request message.
  static const char kRequest[] = "example request";
  ASSERT_OK(zx_mbo_write(mbo.get(), 0, kRequest, sizeof(kRequest), nullptr, 0));
  ASSERT_OK(zx_channel_write_mbo(channel.ch1.get(), mbo.get()));

  // MBO should not be readable.
  AssertMBONotAccessible(mbo);

  // Drop the channel and hence the message contained in its queue.
  channel.ch2.reset();

  // Currently channel.ch1 keeps channel.ch2's message queue alive, so we
  // have to also drop the former to drop the latter.
  // TODO: Implement an on_zero_handles() handler so that this is not
  // necessary.
  channel.ch1.reset();

  AssertMBOReceivedAutoReply(&mboq);
}

TEST(MbmqTest, AutoReplyWhenCalleesRefDropped) {
  MboAndQueue mboq;
  zx::handle& mbo = mboq.mbo;
  Channel channel;
  zx::handle calleesref;
  ASSERT_OK(calleesref_create(0, &calleesref));

  // Send request message.
  static const char kRequest[] = "example request";
  ASSERT_OK(zx_mbo_write(mbo.get(), 0, kRequest, sizeof(kRequest), nullptr, 0));
  ASSERT_OK(zx_channel_write_mbo(channel.ch1.get(), mbo.get()));
  // Read the request message into a CalleesRef.
  ASSERT_OK(zx_msgqueue_wait(channel.ch2.get(), calleesref.get()));

  // MBO should not be readable.
  AssertMBONotAccessible(mbo);

  // Drop the CalleesRef and hence its reference to the MBO.
  calleesref.reset();

  AssertMBOReceivedAutoReply(&mboq);
}

}  // namespace
