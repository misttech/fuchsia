// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fit/defer.h>
#include <lib/zx/channel.h>
#include <lib/zx/job.h>
#include <lib/zx/thread.h>
#include <lib/zx/vmar.h>
#include <zircon/threads.h>
#include <zircon/types.h>

#include <thread>

#include <zxtest/zxtest.h>

#include "utils.h"

// SYSCALL_zx_channel_call_finish is an internal system call used in the
// vDSO's implementation of zx_channel_call.  It's not part of the ABI and
// so it's not exported from the vDSO.  It's hard to test the kernel's
// invariants without calling this directly.  So use some chicanery to
// find its address in the vDSO despite it not being public.
//
// The vdso-code.h header file is generated from the vDSO binary.  It gives
// the offsets of the internal functions.  So take a public vDSO function,
// subtract its offset to discover the vDSO base (could do this other ways,
// but this is the simplest), and then add the offset of the internal
// SYSCALL_zx_channel_call_finish function we want to call.
#include "vdso-code.h"

namespace channel {
namespace {

zx_status_t zx_channel_call_finish(zx_instant_mono_t deadline, const zx_channel_call_args_t* args,
                                   uint32_t* actual_bytes, uint32_t* actual_handles) {
  uintptr_t vdso_base = (uintptr_t)&zx_handle_close - VDSO_SYSCALL_zx_handle_close;
  uintptr_t fnptr = vdso_base + VDSO_SYSCALL_zx_channel_call_finish;
  return (*(__typeof(zx_channel_call_finish)*)fnptr)(deadline, args, actual_bytes, actual_handles);
}

TEST(ChannelInternalTest, CallFinishWithoutPreviouslyCallingCallReturnsBadState) {
  char msg[8] = {
      0,
  };

  zx_channel_call_args_t args = {
      .wr_bytes = &msg,
      .wr_handles = nullptr,
      .rd_bytes = nullptr,
      .rd_handles = nullptr,
      .wr_num_bytes = sizeof(msg),
      .wr_num_handles = 0,
      .rd_num_bytes = 0,
      .rd_num_handles = 0,
  };

  uint32_t act_bytes = 0xffffffff;
  uint32_t act_handles = 0xffffffff;

  // Call channel_call_finish without having had a channel call interrupted
  ASSERT_EQ(ZX_ERR_BAD_STATE, zx_channel_call_finish(zx_deadline_after(ZX_MSEC(1000)), &args,
                                                     &act_bytes, &act_handles));
}

void WaitForThreadState(zx_handle_t thread_handle, zx_thread_state_t state) {
  // Make sure the original thread is still blocked.
  // It is safe to read from caller_thread_handle since this is set before
  // the read happends, and we waited until the remote endpoint became readable.
  zx_info_thread_t info = {};
  while (info.state != state) {
    uint64_t actual = 0;
    uint64_t actual_2 = 0;
    ASSERT_OK(
        zx_object_get_info(thread_handle, ZX_INFO_THREAD, &info, sizeof(info), &actual, &actual_2));
  }
  return;
}

// Verify pending channel_calls are canceled when the handle is transferred.
TEST(ChannelInternalTest, TransferChannelWithPendingCall) {
  constexpr uint32_t kRequestPayload = 0xc0ffee;

  zx::channel local;
  zx::channel remote;

  ASSERT_OK(zx::channel::create(0, &local, &remote));

  struct Message {
    zx_txid_t id;
    uint32_t payload;
  };
  std::atomic<const char*> caller_error = nullptr;

  {
    std::atomic<zx_handle_t> caller_thread_handle = ZX_HANDLE_INVALID;
    AutoJoinThread caller_thread([&local, &caller_error, &caller_thread_handle] {
      Message request;
      request.payload = kRequestPayload;
      Message reply;
      caller_thread_handle.store(zx::thread::self()->get());
      zx_channel_call_args_t args = {
          .wr_bytes = &request,
          .wr_handles = nullptr,
          .rd_bytes = &reply,
          .rd_handles = nullptr,
          .wr_num_bytes = sizeof(Message),
          .wr_num_handles = 0,
          .rd_num_bytes = sizeof(Message),
          .rd_num_handles = 0,
      };
      uint32_t actual_bytes = 0;
      uint32_t actual_handles = 0;

      const zx_status_t status =
          local.call(0, zx::time::infinite(), &args, &actual_bytes, &actual_handles);

      if (status != ZX_ERR_CANCELED) {
        caller_error = "channel::call was not canceled as unexpected";
        return;
      }
    });

    ASSERT_OK(remote.wait_one(ZX_CHANNEL_READABLE, zx::time::infinite(), nullptr));

    // Read the message from the test thread.
    Message request = {};
    uint32_t actual_bytes = 0;
    uint32_t actual_handles = 0;

    ASSERT_OK(
        remote.read(0, &request, nullptr, sizeof(Message), 0, &actual_bytes, &actual_handles));
    ASSERT_EQ(actual_bytes, sizeof(Message));
    ASSERT_EQ(kRequestPayload, request.payload);

    // See that the original thread is still blocked.
    ASSERT_NO_FATAL_FAILURE(
        WaitForThreadState(caller_thread_handle.load(), ZX_THREAD_STATE_BLOCKED_CHANNEL));

    {
      // Transfer the local endpoint in a channel message.
      zx::channel a;
      zx::channel b;
      ASSERT_OK(zx::channel::create(0, &a, &b));

      Message transfer_msg;
      zx_handle_t raw_handle = local.release();
      ASSERT_OK(a.write(0, &transfer_msg, sizeof(transfer_msg), &raw_handle, 1));
      raw_handle = ZX_HANDLE_INVALID;

      // See that the original thread is still blocked.
      ASSERT_NO_FATAL_FAILURE(
          WaitForThreadState(caller_thread_handle.load(), ZX_THREAD_STATE_BLOCKED_CHANNEL));

      // Reading this channel message will unblock the caller_thread.
      ASSERT_OK(b.read(0, &transfer_msg, local.reset_and_get_address(), sizeof(transfer_msg), 1,
                       &actual_bytes, &actual_handles));
      ASSERT_EQ(actual_bytes, sizeof(transfer_msg));
      ASSERT_EQ(actual_handles, 1);
    }

    caller_thread.Join();
  }

  if (caller_error.load() != nullptr) {
    FAIL("caller_thread encountered an error on channel::call: %s", caller_error.load());
  }
}

}  // namespace
}  // namespace channel
