// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <lib/fit/function.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <linux/aio_abi.h>

#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

intptr_t io_setup(unsigned nr_events, aio_context_t* ctx_idp) {
  return syscall(SYS_io_setup, nr_events, ctx_idp);
}

intptr_t io_destroy(aio_context_t ctx_id) { return syscall(SYS_io_destroy, ctx_id); }

intptr_t io_submit(aio_context_t ctx_id, intptr_t nr, struct iocb** iocbpp) {
  return syscall(SYS_io_submit, ctx_id, nr, iocbpp);
}

intptr_t io_getevents(aio_context_t ctx_id, intptr_t min_nr, intptr_t nr, struct io_event* events,
                      struct timespec* timeout) {
  return syscall(SYS_io_getevents, ctx_id, min_nr, nr, events, timeout);
}

uint64_t ToAioBuf(const void* ptr) {
  return static_cast<uint64_t>(reinterpret_cast<uintptr_t>(ptr));
}

std::string ReadFilePrefix(int fd, size_t len) {
  std::string buf(len, '\0');
  ssize_t n = SAFE_SYSCALL(pread(fd, buf.data(), len, 0));
  buf.resize(static_cast<size_t>(n));
  return buf;
}

class AioWriteTest : public ::testing::Test {
 protected:
  static constexpr uint64_t kBlockOpData = 0x1000;

  void SetUp() override {
    page_size_ = static_cast<size_t>(SAFE_SYSCALL(sysconf(_SC_PAGESIZE)));
    ASSERT_THAT(io_setup(8, &ctx_), SyscallSucceeds());
    ASSERT_TRUE(target_fd_.is_valid());

    int pipe_fds[2] = {-1, -1};
    ASSERT_THAT(pipe2(pipe_fds, O_NONBLOCK), SyscallSucceeds());
    block_pipe_read_fd_.reset(pipe_fds[0]);
    block_pipe_write_fd_.reset(pipe_fds[1]);

    // On Starnix, AIO writes are dispatched sequentially on a background worker thread. Fill the
    // pipe to capacity so the first queued write blocks the worker until `UnblockFirstWrite()`
    // drains the pipe. (On native Linux, `io_submit` executes pipe writes synchronously on the
    // calling thread before returning, so leaving space in the pipe avoids deadlocking the single
    // test thread inside `io_submit`.)
    if (test_helper::IsStarnix()) {
      std::vector<char> fill_buf(page_size_, 'F');
      while (true) {
        ssize_t n = write(block_pipe_write_fd_.get(), fill_buf.data(), fill_buf.size());
        if (n < 0) {
          ASSERT_EQ(errno, EAGAIN);
          break;
        }
      }
    }

    int flags = SAFE_SYSCALL(fcntl(block_pipe_write_fd_.get(), F_GETFL, 0));
    ASSERT_THAT(fcntl(block_pipe_write_fd_.get(), F_SETFL, flags & ~O_NONBLOCK), SyscallSucceeds());
  }

  void TearDown() override {
    if (ctx_ != 0) {
      EXPECT_THAT(io_destroy(ctx_), SyscallSucceeds());
    }
  }

  struct iocb MakeBlockedWriteIocb(const char* byte_ptr) const {
    struct iocb cb = {};
    cb.aio_fildes = static_cast<uint32_t>(block_pipe_write_fd_.get());
    cb.aio_lio_opcode = IOCB_CMD_PWRITE;
    cb.aio_buf = ToAioBuf(byte_ptr);
    cb.aio_nbytes = 1;
    cb.aio_data = kBlockOpData;
    return cb;
  }

  void UnblockFirstWrite() {
    std::vector<char> drain_buf(page_size_);
    while (true) {
      ssize_t n = read(block_pipe_read_fd_.get(), drain_buf.data(), drain_buf.size());
      if (n < 0) {
        ASSERT_EQ(errno, EAGAIN);
        break;
      }
      if (n == 0) {
        break;
      }
    }
  }

  // Queues 2 write operations where the first operation blocks on `block_pipe_write_fd_`, mutates
  // the user buffer(s) of the second operation while the first operation is blocked, unblocks the
  // first operation, and verifies that the second operation wrote the submit-time data.
  void VerifySecondWriteUsesSubmitTimeBuffer(struct iocb second_write_cb,
                                             fit::function<void()> mutate_buffers,
                                             std::string_view expected_data) {
    char block_byte = 'B';
    struct iocb block_cb = MakeBlockedWriteIocb(&block_byte);
    struct iocb* ptrs[2] = {&block_cb, &second_write_cb};

    ASSERT_THAT(io_submit(ctx_, 2, ptrs), SyscallSucceedsWithValue(2));

    mutate_buffers();
    UnblockFirstWrite();

    struct io_event events[2] = {};
    ASSERT_THAT(io_getevents(ctx_, 2, 2, events, nullptr), SyscallSucceedsWithValue(2));

    int64_t block_res = -1;
    int64_t target_res = -1;
    for (const auto& ev : events) {
      if (ev.data == kBlockOpData) {
        block_res = ev.res;
      } else if (ev.data == second_write_cb.aio_data) {
        target_res = ev.res;
      }
    }
    EXPECT_EQ(block_res, 1);
    EXPECT_EQ(target_res, static_cast<int64_t>(expected_data.size()));
    EXPECT_EQ(ReadFilePrefix(target_fd_.fd(), expected_data.size()), expected_data);
  }

  size_t page_size_ = 0;
  aio_context_t ctx_ = 0;
  test_helper::ScopedTempFD target_fd_;
  fbl::unique_fd block_pipe_read_fd_;
  fbl::unique_fd block_pipe_write_fd_;
};

// Validates that `IOCB_CMD_PWRITE` snapshots the caller's user buffer synchronously during
// `io_submit` rather than reading from user virtual addresses when the background worker executes.
TEST_F(AioWriteTest, PWriteSnapshotsUserBufferAtSubmitTime) {
  std::string buf = "aio-pwrite-synchronous-snapshot";
  struct iocb write_cb = {};
  write_cb.aio_fildes = static_cast<uint32_t>(target_fd_.fd());
  write_cb.aio_lio_opcode = IOCB_CMD_PWRITE;
  write_cb.aio_buf = ToAioBuf(buf.data());
  write_cb.aio_nbytes = buf.size();
  write_cb.aio_offset = 0;
  write_cb.aio_data = 0x1001;

  VerifySecondWriteUsesSubmitTimeBuffer(
      write_cb, [&] { std::fill(buf.begin(), buf.end(), 'X'); }, "aio-pwrite-synchronous-snapshot");
}

// Validates that `IOCB_CMD_PWRITEV` snapshots all vectored user buffers synchronously during
// `io_submit` rather than reading from user virtual addresses when the background worker executes.
TEST_F(AioWriteTest, PWriteVSnapshotsVectoredBuffersAtSubmitTime) {
  std::string part1 = "aio-pwritev-header:";
  std::string part2 = "vectored-payload-body";
  struct iovec iovs[2] = {
      {.iov_base = part1.data(), .iov_len = part1.size()},
      {.iov_base = part2.data(), .iov_len = part2.size()},
  };

  struct iocb writev_cb = {};
  writev_cb.aio_fildes = static_cast<uint32_t>(target_fd_.fd());
  writev_cb.aio_lio_opcode = IOCB_CMD_PWRITEV;
  writev_cb.aio_buf = ToAioBuf(iovs);
  writev_cb.aio_nbytes = 2;
  writev_cb.aio_offset = 0;
  writev_cb.aio_data = 0x1002;

  VerifySecondWriteUsesSubmitTimeBuffer(
      writev_cb,
      [&] {
        std::fill(part1.begin(), part1.end(), 'X');
        std::fill(part2.begin(), part2.end(), 'Y');
      },
      "aio-pwritev-header:vectored-payload-body");
}

struct ChildSubmitArgs {
  aio_context_t ctx;
  int block_pipe_write_fd;
  int target_fd;
  size_t page_size;
};

int ChildSubmitAndExitImmediately(void* arg) {
  auto* args = static_cast<ChildSubmitArgs*>(arg);

  constexpr std::string_view kHeader = "A_WRTE_HEADER:";
  constexpr std::string_view kBodyPart1 = "restarting adbd ";
  constexpr std::string_view kBodyPart2 = "as root\n";

  char* page = static_cast<char*>(
      mmap(nullptr, args->page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
  if (page == MAP_FAILED) {
    return 1;
  }
  page[0] = 'B';
  char* payload_ptr = page + 1;
  std::memcpy(payload_ptr, kHeader.data(), kHeader.size());
  char* part1_ptr = payload_ptr + kHeader.size();
  std::memcpy(part1_ptr, kBodyPart1.data(), kBodyPart1.size());
  char* part2_ptr = part1_ptr + kBodyPart1.size();
  std::memcpy(part2_ptr, kBodyPart2.data(), kBodyPart2.size());

  struct iovec iovs[2] = {
      {.iov_base = part1_ptr, .iov_len = kBodyPart1.size()},
      {.iov_base = part2_ptr, .iov_len = kBodyPart2.size()},
  };

  struct iocb cb_block = {};
  cb_block.aio_fildes = static_cast<uint32_t>(args->block_pipe_write_fd);
  cb_block.aio_lio_opcode = IOCB_CMD_PWRITE;
  cb_block.aio_buf = ToAioBuf(page);
  cb_block.aio_nbytes = 1;
  cb_block.aio_data = 0x2000;

  struct iocb cb_pwrite = {};
  cb_pwrite.aio_fildes = static_cast<uint32_t>(args->target_fd);
  cb_pwrite.aio_lio_opcode = IOCB_CMD_PWRITE;
  cb_pwrite.aio_buf = ToAioBuf(payload_ptr);
  cb_pwrite.aio_nbytes = kHeader.size();
  cb_pwrite.aio_offset = 0;
  cb_pwrite.aio_data = 0x2001;

  struct iocb cb_pwritev = {};
  cb_pwritev.aio_fildes = static_cast<uint32_t>(args->target_fd);
  cb_pwritev.aio_lio_opcode = IOCB_CMD_PWRITEV;
  cb_pwritev.aio_buf = ToAioBuf(iovs);
  cb_pwritev.aio_nbytes = 2;
  cb_pwritev.aio_offset = static_cast<int64_t>(kHeader.size());
  cb_pwritev.aio_data = 0x2002;

  struct iocb* ptrs[3] = {&cb_block, &cb_pwrite, &cb_pwritev};
  if (io_submit(args->ctx, 3, ptrs) != 3) {
    munmap(page, args->page_size);
    return 1;
  }
  std::memset(page, 'X', args->page_size);
  if (munmap(page, args->page_size) < 0) {
    return 1;
  }
  if (close(args->block_pipe_write_fd) < 0) {
    return 1;
  }
  if (close(args->target_fd) < 0) {
    return 1;
  }
  return 0;
}

// Validates that async `IOCB_CMD_PWRITE` and `IOCB_CMD_PWRITEV` writes complete reliably even when
// the submitting process unmaps its buffers, closes its file descriptors, and exits before the
// background AIO worker unblocks from an earlier write.
TEST_F(AioWriteTest, WriteCompletesAfterSubmittingProcessExits) {
  ChildSubmitArgs args = {
      .ctx = ctx_,
      .block_pipe_write_fd = block_pipe_write_fd_.get(),
      .target_fd = target_fd_.fd(),
      .page_size = page_size_,
  };

  constexpr size_t kStackSize = 64ULL * 1024;
  std::vector<char> child_stack(kStackSize);
  pid_t child_pid =
      SAFE_SYSCALL(clone(&ChildSubmitAndExitImmediately, child_stack.data() + child_stack.size(),
                         CLONE_VM | SIGCHLD, &args));

  int status = 0;
  ASSERT_THAT(waitpid(child_pid, &status, 0), SyscallSucceedsWithValue(child_pid));
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(WEXITSTATUS(status), 0);

  // Unblock the first write only AFTER the submitting child process has unmapped its memory,
  // closed its FDs, and exited.
  UnblockFirstWrite();

  struct io_event events[3] = {};
  ASSERT_THAT(io_getevents(ctx_, 3, 3, events, nullptr), SyscallSucceedsWithValue(3));

  constexpr std::string_view kHeader = "A_WRTE_HEADER:";
  constexpr std::string_view kBody = "restarting adbd as root\n";

  int64_t block_res = -1;
  int64_t pwrite_res = -1;
  int64_t pwritev_res = -1;
  for (const auto& ev : events) {
    if (ev.data == 0x2000) {
      block_res = ev.res;
    } else if (ev.data == 0x2001) {
      pwrite_res = ev.res;
    } else if (ev.data == 0x2002) {
      pwritev_res = ev.res;
    }
  }
  EXPECT_EQ(block_res, 1);
  EXPECT_EQ(pwrite_res, static_cast<int64_t>(kHeader.size()));
  EXPECT_EQ(pwritev_res, static_cast<int64_t>(kBody.size()));

  const std::string expected = std::string(kHeader) + std::string(kBody);
  EXPECT_EQ(ReadFilePrefix(target_fd_.fd(), expected.size()), expected);
}

}  // namespace
