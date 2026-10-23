// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/poll.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

#include <thread>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

#ifndef O_NOTIFICATION_PIPE
#define O_NOTIFICATION_PIPE O_EXCL  // As defined in <linux/watch_queue.h>.
#endif

namespace {

TEST(PipeTest, NonBlockingPartialWrite) {
  // Allocate 1M that should be bigger than the pipe buffer.
  constexpr ssize_t kBufferSize = 1024 * 1024;

  int pipefd[2];
  SAFE_SYSCALL(pipe2(pipefd, O_NONBLOCK));

  char* buffer = static_cast<char*>(malloc(kBufferSize));
  ASSERT_NE(buffer, nullptr);
  ssize_t write_result = write(pipefd[1], buffer, kBufferSize);
  free(buffer);
  ASSERT_GT(write_result, 0);
  ASSERT_LT(write_result, kBufferSize);
}

TEST(PipeTest, BlockingSmallWrites) {
  // Create a pipe with size 4096, and fill all but 128 bytes of it.
  int pipefd[2];
  SAFE_SYSCALL(pipe2(pipefd, O_NONBLOCK));
  SAFE_SYSCALL(fcntl(pipefd[1], F_SETPIPE_SZ, getpagesize()));
  const int kWriteSize = getpagesize() - 128;
  char buf[kWriteSize];
  ASSERT_EQ(write(pipefd[1], buf, kWriteSize), kWriteSize);
  // Trying to write 256 bytes must returns EAGAIN
  ASSERT_EQ(write(pipefd[1], buf, 256), -1);
  ASSERT_EQ(errno, EAGAIN);
}

TEST(PipeTest, SpliceShortRead) {
  char* tmp = getenv("TEST_TMPDIR");
  std::string path = tmp == nullptr ? "/tmp/test_file" : std::string(tmp) + "/test_file";
  fbl::unique_fd fd(open(path.c_str(), O_RDWR | O_CREAT | O_TRUNC, 0777));
  ASSERT_TRUE(fd.is_valid());
  ASSERT_EQ(write(fd.get(), "hello", 5), 5);
  int pipefd[2];
  SAFE_SYSCALL(pipe2(pipefd, 0));
  off64_t offset = 0;
  ASSERT_EQ(splice(fd.get(), &offset, pipefd[1], nullptr, 100, 0), 5);
  char buffer[100];
  ASSERT_EQ(read(pipefd[0], buffer, 10), 5);
  ASSERT_EQ(strncmp(buffer, "hello", 5), 0);
}

TEST(PipeTest, TeeFromEmptyPipe) {
  int pipe_a[2];
  SAFE_SYSCALL(pipe2(pipe_a, 0));
  // Closing the write end of pipe_a makes the read end readable even though it's empty.
  close(pipe_a[1]);
  int pipe_b[2];
  SAFE_SYSCALL(pipe2(pipe_b, 0));
  ASSERT_EQ(tee(pipe_a[0], pipe_b[1], 100, 0), 0);
  close(pipe_a[0]);
  close(pipe_b[0]);
  close(pipe_b[1]);
}

TEST(PipeTest, SpliceWrongDirection) {
  char* tmp = getenv("TEST_TMPDIR");
  std::string path = tmp == nullptr ? "/tmp/test_file" : std::string(tmp) + "/test_file";
  fbl::unique_fd file_fd(open(path.c_str(), O_RDWR | O_CREAT | O_TRUNC, 0777));
  ASSERT_TRUE(file_fd.is_valid());

  int pipe_fds[2];
  SAFE_SYSCALL(pipe2(pipe_fds, 0));
  fbl::unique_fd pipe_in(pipe_fds[0]);
  fbl::unique_fd pipe_out(pipe_fds[1]);

  // Writing into pipe[0] (which is read-only) should fail with EBADF.
  EXPECT_THAT(splice(file_fd.get(), nullptr, pipe_in.get(), nullptr, 5, 0),
              SyscallFailsWithErrno(EBADF));

  // Reading from pipe[1] (which is write-only) should fail with EBADF.
  EXPECT_THAT(splice(pipe_out.get(), nullptr, file_fd.get(), nullptr, 5, 0),
              SyscallFailsWithErrno(EBADF));

  // Pipe-to-pipe with wrong endpoints.
  int pipe_b_fds[2];
  SAFE_SYSCALL(pipe2(pipe_b_fds, 0));
  fbl::unique_fd pipe_b_in(pipe_b_fds[0]);
  fbl::unique_fd pipe_b_out(pipe_b_fds[1]);

  EXPECT_THAT(splice(pipe_out.get(), nullptr, pipe_b_out.get(), nullptr, 5, 0),
              SyscallFailsWithErrno(EBADF));
  EXPECT_THAT(splice(pipe_in.get(), nullptr, pipe_b_in.get(), nullptr, 5, 0),
              SyscallFailsWithErrno(EBADF));
}

TEST(PipeTest, TeeWrongDirection) {
  int pipe_a[2];
  SAFE_SYSCALL(pipe2(pipe_a, 0));
  fbl::unique_fd pipe_a_in(pipe_a[0]);
  fbl::unique_fd pipe_a_out(pipe_a[1]);

  int pipe_b[2];
  SAFE_SYSCALL(pipe2(pipe_b, 0));
  fbl::unique_fd pipe_b_in(pipe_b[0]);
  fbl::unique_fd pipe_b_out(pipe_b[1]);

  // tee input must be readable (e.g. pipe_a[0]), not write-only (pipe_a[1]).
  EXPECT_THAT(tee(pipe_a_out.get(), pipe_b_out.get(), 5, 0), SyscallFailsWithErrno(EBADF));

  // tee output must be writable (e.g. pipe_b[1]), not read-only (pipe_b[0]).
  EXPECT_THAT(tee(pipe_a_in.get(), pipe_b_in.get(), 5, 0), SyscallFailsWithErrno(EBADF));

  // Both wrong.
  EXPECT_THAT(tee(pipe_a_out.get(), pipe_b_in.get(), 5, 0), SyscallFailsWithErrno(EBADF));
}

TEST(PipeTest, VmspliceInvalidFd) {
  char buf[8] = "test";
  struct iovec iov = {.iov_base = buf, .iov_len = sizeof(buf)};

  // Invalid fd number.
  EXPECT_THAT(vmsplice(-1, &iov, 1, 0), SyscallFailsWithErrno(EBADF));

  // O_PATH fd of a pipe.
  int pipe_fds[2];
  SAFE_SYSCALL(pipe2(pipe_fds, 0));
  fbl::unique_fd pipe_in(pipe_fds[0]);
  fbl::unique_fd pipe_out(pipe_fds[1]);

  std::string fd_path = "/proc/self/fd/" + std::to_string(pipe_in.get());
  fbl::unique_fd path_fd(open(fd_path.c_str(), O_PATH));
  EXPECT_THAT(vmsplice(path_fd.get(), &iov, 1, 0), SyscallFailsWithErrno(EBADF));
}

TEST(PipeTest, TeeZeroLength) {
  int pipe_a[2];
  SAFE_SYSCALL(pipe2(pipe_a, 0));
  fbl::unique_fd pipe_a_in(pipe_a[0]);
  fbl::unique_fd pipe_a_out(pipe_a[1]);

  int pipe_b[2];
  SAFE_SYSCALL(pipe2(pipe_b, 0));
  fbl::unique_fd pipe_b_in(pipe_b[0]);
  fbl::unique_fd pipe_b_out(pipe_b[1]);

  EXPECT_EQ(tee(pipe_a_in.get(), pipe_b_out.get(), 0, 0), 0);
}

TEST(PipeTest, VmspliceRdwrFifoPrioritizesWrite) {
  test_helper::ScopedTempDir dir;
  std::string fifo_path = dir.path() + "/fifo";
  SAFE_SYSCALL(mkfifo(fifo_path.c_str(), 0600));

  fbl::unique_fd fifo(open(fifo_path.c_str(), O_RDWR | O_NONBLOCK));
  ASSERT_TRUE(fifo.is_valid());

  char write_buf[] = "payload";
  struct iovec iov = {.iov_base = write_buf, .iov_len = sizeof(write_buf)};

  // On an O_RDWR FIFO, vmsplice must prioritize writing into the FIFO per vmsplice(2).
  ssize_t written = vmsplice(fifo.get(), &iov, 1, 0);
  EXPECT_EQ(written, static_cast<ssize_t>(sizeof(write_buf)));

  // Verify the payload is buffered in the FIFO and can now be read.
  char read_buf[sizeof(write_buf)] = {};
  EXPECT_EQ(read(fifo.get(), read_buf, sizeof(read_buf)), static_cast<ssize_t>(sizeof(write_buf)));
  EXPECT_STREQ(read_buf, write_buf);
}

TEST(PipeTest, NotificationPipeUnsupported) {
  int pipefd[2] = {-1, -1};
  int result = pipe2(pipefd, O_NOTIFICATION_PIPE);
  if (result == 0) {
    fbl::unique_fd read_end(pipefd[0]);
    fbl::unique_fd write_end(pipefd[1]);
    // The host kernel was built with `CONFIG_WATCH_QUEUE`.
    ASSERT_FALSE(test_helper::IsStarnix()) << "Starnix must not support notification pipes.";
    GTEST_SKIP() << "Notification pipes are supported by this kernel.";
  }
  // Without `CONFIG_WATCH_QUEUE`, Linux reports ENOPKG. Starnix doesn't support notification pipes.
  EXPECT_THAT(result, SyscallFailsWithErrno(ENOPKG));
  EXPECT_THAT(pipe2(pipefd, O_NOTIFICATION_PIPE | O_CLOEXEC | O_NONBLOCK),
              SyscallFailsWithErrno(ENOPKG));
  EXPECT_THAT(pipe2(pipefd, O_NOTIFICATION_PIPE | O_DIRECT), SyscallFailsWithErrno(ENOPKG));
  // No file descriptors were returned.
  EXPECT_EQ(pipefd[0], -1);
  EXPECT_EQ(pipefd[1], -1);
}

TEST(PipeTest, NotificationPipeWithInvalidFlags) {
  int pipefd[2] = {-1, -1};
  // Invalid flags are reported with EINVAL, even when combined with `O_NOTIFICATION_PIPE`.
  EXPECT_THAT(pipe2(pipefd, O_NOTIFICATION_PIPE | O_APPEND), SyscallFailsWithErrno(EINVAL));
  EXPECT_THAT(pipe2(pipefd, O_APPEND), SyscallFailsWithErrno(EINVAL));
  // No file descriptors were returned.
  EXPECT_EQ(pipefd[0], -1);
  EXPECT_EQ(pipefd[1], -1);
}

std::string CreateNewFifo() {
  char* tmp = getenv("TEST_TMPDIR");
  std::string dir_path = tmp == nullptr ? "/tmp/dirXXXXXX" : std::string(tmp) + "/dirXXXXXX";
  mkdtemp(&dir_path[0]);
  std::string fifo_path = dir_path + "/fifo";
  EXPECT_EQ(mkfifo(fifo_path.c_str(), 0600), 0);
  return fifo_path;
}

TEST(PipeTest, OpenFifoRW) {
  std::string path = CreateNewFifo();
  // Open and close the the fifo once.
  fbl::unique_fd fifo(open(path.c_str(), O_RDWR));
  ASSERT_TRUE(fifo.is_valid());
}

TEST(PipeTest, OpenFifoRo_NotBlocking) {
  std::string path = CreateNewFifo();
  // Reopen the fifo and check it is not disconnected.
  fbl::unique_fd fifo(open(path.c_str(), O_RDONLY | O_NONBLOCK));
  ASSERT_TRUE(fifo.is_valid());
}

void on_alarm(int) {}

TEST(PipeTest, OpenFifoBlock) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    std::string path = CreateNewFifo();
    struct sigaction act;
    act.sa_handler = on_alarm;
    act.sa_flags = 0;
    sigaction(SIGALRM, &act, nullptr);
    alarm(1);
    int fd = open(path.c_str(), O_RDONLY);
    ASSERT_EQ(fd, -1);
    ASSERT_EQ(errno, EINTR);
  });
}

TEST(PipeTest, PolloutFragmentation) {
  const size_t page_size = sysconf(_SC_PAGE_SIZE);
  std::vector<std::byte> page_sized_buffer(page_size, std::byte{0xAB});
  int pipe_fds[2];
  SAFE_SYSCALL(pipe2(pipe_fds, O_NONBLOCK));
  fbl::unique_fd pipe_rd(pipe_fds[0]);
  fbl::unique_fd pipe_wr(pipe_fds[1]);

  // Set the pipe buffer capacity to 2 pages.
  SAFE_SYSCALL(fcntl(pipe_wr.get(), F_SETPIPE_SZ, 2 * page_size));

  // Fill the first page of the pipe completely.
  ASSERT_EQ(static_cast<ssize_t>(page_size),
            write(pipe_wr.get(), page_sized_buffer.data(), page_sized_buffer.size()));

  // Write one more byte to occupy the start of the second page.
  ASSERT_EQ(1, write(pipe_wr.get(), page_sized_buffer.data(), 1));

  // Since there is not a full page size buffer available, POLLOUT is not asserted.
  pollfd p = {
      .fd = pipe_wr.get(),
      .events = POLLOUT,
  };
  EXPECT_EQ(poll(&p, 1, 0), 0);

  // Despite POLLOUT not being ready, writes that fit into the remaining portion of the second page
  // do not block.
  ASSERT_EQ(1, write(pipe_wr.get(), page_sized_buffer.data(), 1));

  // Attempts to write more data than will fit into the remaining portion would block.
  ASSERT_EQ(-1, write(pipe_wr.get(), page_sized_buffer.data(), page_size - 1));
  EXPECT_EQ(errno, EAGAIN);

  // Read out all but 1 byte from the first page of the pipe
  ASSERT_EQ(static_cast<ssize_t>(page_size - 1),
            read(pipe_rd.get(), page_sized_buffer.data(), page_size - 1));

  // Even though only 3 bytes are currently in the pipe, POLLOUT is still not ready.
  EXPECT_EQ(poll(&p, 1, 0), 0);

  // Read one more byte so that the first page is no longer occupied.
  ASSERT_EQ(1, read(pipe_rd.get(), page_sized_buffer.data(), 1));

  // Now POLLOUT will finally be asserted.
  EXPECT_EQ(poll(&p, 1, 0), 1);
  EXPECT_EQ(p.revents, POLLOUT);
}

TEST(PipeTest, PartialEfaultRollbackAndShortCounts) {
  const size_t page_size = sysconf(_SC_PAGE_SIZE);
  auto mapping = test_helper::ScopedMMap::MMap(nullptr, 2 * page_size, PROT_READ | PROT_WRITE,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_TRUE(mapping.is_ok()) << mapping.error_value();
  char* valid_page = static_cast<char*>(mapping->mapping());
  char* bad_page = valid_page + page_size;
  ASSERT_EQ(mprotect(bad_page, page_size, PROT_NONE), 0);
  memset(valid_page, 'X', page_size);

  // 1. Sub-PIPE_BUF writev with a trailing faulting iovec must fail atomically with EFAULT and
  // leave 0 bytes in the pipe.
  {
    test_helper::ScopedPipe pipe(O_NONBLOCK);

    iovec iov[2] = {
        {.iov_base = valid_page, .iov_len = 100},
        {.iov_base = bad_page, .iov_len = 100},
    };
    EXPECT_THAT(writev(pipe.WriteSide().get(), iov, 2), SyscallFailsWithErrno(EFAULT));
    int avail = -1;
    SAFE_SYSCALL(ioctl(pipe.ReadSide().get(), FIONREAD, &avail));
    EXPECT_EQ(avail, 0);
  }

  // 2. Multi-page write and writev spanning [valid_page, bad_page] must commit the first page and
  // return a short write count of page_size rather than failing with EFAULT.
  {
    test_helper::ScopedPipe pipe(O_NONBLOCK);

    EXPECT_THAT(write(pipe.WriteSide().get(), valid_page, 2 * page_size),
                SyscallSucceedsWithValue(static_cast<ssize_t>(page_size)));
    int avail = -1;
    SAFE_SYSCALL(ioctl(pipe.ReadSide().get(), FIONREAD, &avail));
    EXPECT_EQ(avail, static_cast<int>(page_size));

    std::vector<char> drain(page_size);
    EXPECT_THAT(read(pipe.ReadSide().get(), drain.data(), page_size),
                SyscallSucceedsWithValue(static_cast<ssize_t>(page_size)));

    iovec iov[2] = {
        {.iov_base = valid_page, .iov_len = page_size},
        {.iov_base = bad_page, .iov_len = page_size},
    };
    EXPECT_THAT(writev(pipe.WriteSide().get(), iov, 2),
                SyscallSucceedsWithValue(static_cast<ssize_t>(page_size)));
    SAFE_SYSCALL(ioctl(pipe.ReadSide().get(), FIONREAD, &avail));
    EXPECT_EQ(avail, static_cast<int>(page_size));
  }

  // 3. vmsplice from a PROT_NONE page fails with EFAULT, while vmsplice across
  // [valid_page, bad_page] returns a short count of page_size.
  {
    test_helper::ScopedPipe pipe(O_NONBLOCK);

    iovec bad_iov = {.iov_base = bad_page, .iov_len = page_size};
    EXPECT_THAT(vmsplice(pipe.WriteSide().get(), &bad_iov, 1, SPLICE_F_NONBLOCK),
                SyscallFailsWithErrno(EFAULT));
    int avail = -1;
    SAFE_SYSCALL(ioctl(pipe.ReadSide().get(), FIONREAD, &avail));
    EXPECT_EQ(avail, 0);

    iovec mixed_iov[2] = {
        {.iov_base = valid_page, .iov_len = page_size},
        {.iov_base = bad_page, .iov_len = page_size},
    };
    EXPECT_THAT(vmsplice(pipe.WriteSide().get(), mixed_iov, 2, SPLICE_F_NONBLOCK),
                SyscallSucceedsWithValue(static_cast<ssize_t>(page_size)));
    SAFE_SYSCALL(ioctl(pipe.ReadSide().get(), FIONREAD, &avail));
    EXPECT_EQ(avail, static_cast<int>(page_size));
  }

  // 4. read, readv, and vmsplice from a pipe into a faulting buffer mid-message must roll back the
  // partial copy, return EFAULT, and preserve the unread message in the pipe.
  {
    test_helper::ScopedPipe pipe(O_NONBLOCK);

    ASSERT_EQ(write(pipe.WriteSide().get(), "AAAABBBB", 8), 8);

    EXPECT_THAT(read(pipe.ReadSide().get(), bad_page, 8), SyscallFailsWithErrno(EFAULT));
    int avail = -1;
    SAFE_SYSCALL(ioctl(pipe.ReadSide().get(), FIONREAD, &avail));
    EXPECT_EQ(avail, 8);

    iovec fault_iov[2] = {
        {.iov_base = valid_page, .iov_len = 4},
        {.iov_base = bad_page, .iov_len = 4},
    };
    EXPECT_THAT(readv(pipe.ReadSide().get(), fault_iov, 2), SyscallFailsWithErrno(EFAULT));
    SAFE_SYSCALL(ioctl(pipe.ReadSide().get(), FIONREAD, &avail));
    EXPECT_EQ(avail, 8);

    EXPECT_THAT(vmsplice(pipe.ReadSide().get(), fault_iov, 2, SPLICE_F_NONBLOCK),
                SyscallFailsWithErrno(EFAULT));
    SAFE_SYSCALL(ioctl(pipe.ReadSide().get(), FIONREAD, &avail));
    EXPECT_EQ(avail, 8);

    char verify_buf[8] = {};
    ASSERT_EQ(read(pipe.ReadSide().get(), verify_buf, 8), 8);
    EXPECT_EQ(std::string(verify_buf, 8), "AAAABBBB");
  }

  // 5. readv and vmsplice across two separate 4-byte pipe buffers (enqueued via vmsplice so Linux
  // does not merge them with PIPE_BUF_FLAG_CAN_MERGE) with [4B valid, 4B PROT_NONE] must consume
  // the first buffer, preserve the second buffer on EFAULT, and return 4.
  {
    test_helper::ScopedPipe pipe(O_NONBLOCK);

    char msg1[4] = {'A', 'A', 'A', 'A'};
    char msg2[4] = {'B', 'B', 'B', 'B'};
    iovec init_iov[2] = {
        {.iov_base = msg1, .iov_len = 4},
        {.iov_base = msg2, .iov_len = 4},
    };
    ASSERT_EQ(vmsplice(pipe.WriteSide().get(), init_iov, 2, 0), 8);

    iovec fault_iov[2] = {
        {.iov_base = valid_page, .iov_len = 4},
        {.iov_base = bad_page, .iov_len = 4},
    };
    EXPECT_THAT(readv(pipe.ReadSide().get(), fault_iov, 2), SyscallSucceedsWithValue(4));
    EXPECT_EQ(std::string(valid_page, 4), "AAAA");

    int avail = -1;
    SAFE_SYSCALL(ioctl(pipe.ReadSide().get(), FIONREAD, &avail));
    EXPECT_EQ(avail, 4);

    char rem_buf[4] = {};
    ASSERT_EQ(read(pipe.ReadSide().get(), rem_buf, 4), 4);
    EXPECT_EQ(std::string(rem_buf, 4), "BBBB");

    char msg3[4] = {'C', 'C', 'C', 'C'};
    char msg4[4] = {'D', 'D', 'D', 'D'};
    iovec init_iov2[2] = {
        {.iov_base = msg3, .iov_len = 4},
        {.iov_base = msg4, .iov_len = 4},
    };
    ASSERT_EQ(vmsplice(pipe.WriteSide().get(), init_iov2, 2, 0), 8);
    EXPECT_THAT(vmsplice(pipe.ReadSide().get(), fault_iov, 2, SPLICE_F_NONBLOCK),
                SyscallSucceedsWithValue(4));
    EXPECT_EQ(std::string(valid_page, 4), "CCCC");
    SAFE_SYSCALL(ioctl(pipe.ReadSide().get(), FIONREAD, &avail));
    EXPECT_EQ(avail, 4);
    ASSERT_EQ(read(pipe.ReadSide().get(), rem_buf, 4), 4);
    EXPECT_EQ(std::string(rem_buf, 4), "DDDD");
  }
}

TEST(PipeTest, SubPipeBufReadAndPartialSpliceWakeBlockedWriter) {
  const size_t page_size = sysconf(_SC_PAGE_SIZE);

  // 1. Reading < PIPE_BUF (100 bytes) from a full pipe must wake a blocked small writer.
  // Before fixing sub-PIPE_BUF writer wakeups, Starnix only notified writers when free capacity
  // reached >= PIPE_BUF (4096 bytes), so freeing 100 bytes left the blocked writer hung.
  {
    test_helper::ScopedPipe pipe;
    SAFE_SYSCALL(fcntl(pipe.WriteSide().get(), F_SETPIPE_SZ, 2 * page_size));

    // Enqueue a 100-byte vmsplice segment at the head of the pipe so Slot 0 on Linux is a
    // non-mergeable 100-byte buffer, then fill the remaining capacity of the 2-page pipe.
    std::vector<char> head_buf(100, 'H');
    iovec head_iov = {.iov_base = head_buf.data(), .iov_len = head_buf.size()};
    ASSERT_EQ(vmsplice(pipe.WriteSide().get(), &head_iov, 1, 0), 100);

    std::vector<char> page_buf(page_size, 'A');
    ASSERT_EQ(write(pipe.WriteSide().get(), page_buf.data(), page_size),
              static_cast<ssize_t>(page_size));

    // On Starnix, pipe capacity is tracked in bytes (2 * page_size), so fill the remaining
    // `page_size - 100` bytes; on Linux, both pipe_buffer slots are already occupied so a
    // non-blocking write returns EAGAIN.
    SAFE_SYSCALL(fcntl(pipe.WriteSide().get(), F_SETFL, O_NONBLOCK));
    ssize_t rem = write(pipe.WriteSide().get(), page_buf.data(), page_size - 100);
    if (rem == -1) {
      ASSERT_EQ(errno, EAGAIN);
    } else {
      ASSERT_EQ(rem, static_cast<ssize_t>(page_size - 100));
    }
    SAFE_SYSCALL(fcntl(pipe.WriteSide().get(), F_SETFL, 0));

    std::thread reader([&pipe]() {
      usleep(20000);
      char small_buf[100];
      EXPECT_EQ(read(pipe.ReadSide().get(), small_buf, sizeof(small_buf)), 100);
    });

    const char payload[] = "ABCD";
    EXPECT_THAT(write(pipe.WriteSide().get(), payload, 4), SyscallSucceedsWithValue(4));
    reader.join();
  }

  // 2. Splicing a single page out of a full 2-page pipe (leaving the source pipe non-empty) must
  // also wake a writer blocked on the source pipe.
  {
    test_helper::ScopedPipe pipe_a;
    test_helper::ScopedPipe pipe_b;

    SAFE_SYSCALL(fcntl(pipe_a.WriteSide().get(), F_SETPIPE_SZ, 2 * page_size));
    std::vector<char> fill(2 * page_size, 'A');
    ASSERT_EQ(write(pipe_a.WriteSide().get(), fill.data(), fill.size()),
              static_cast<ssize_t>(2 * page_size));

    std::thread splicer([&pipe_a, &pipe_b, page_size]() {
      usleep(20000);
      EXPECT_EQ(
          splice(pipe_a.ReadSide().get(), nullptr, pipe_b.WriteSide().get(), nullptr, page_size, 0),
          static_cast<ssize_t>(page_size));
    });

    const char payload[] = "EFGH";
    EXPECT_THAT(write(pipe_a.WriteSide().get(), payload, 4), SyscallSucceedsWithValue(4));
    splicer.join();
  }
}

TEST(PipeTest, SpliceValidationAndRlimitFsize) {
  // 1. Non-null offset pointers on non-pipe non-seekable descriptors (such as sockets) must return
  // EFAULT for unmapped pointers and EINVAL for valid pointers, rather than ESPIPE.
  {
    int sock_fds[2];
    SAFE_SYSCALL(socketpair(AF_UNIX, SOCK_STREAM, 0, sock_fds));
    fbl::unique_fd sock_rd(sock_fds[0]);
    fbl::unique_fd sock_wr(sock_fds[1]);
    test_helper::ScopedPipe pipe(O_NONBLOCK);

    loff_t valid_offset = 0;
    loff_t* bad_offset = reinterpret_cast<loff_t*>(0x1);

    EXPECT_THAT(splice(sock_rd.get(), &valid_offset, pipe.WriteSide().get(), nullptr, 4, 0),
                SyscallFailsWithErrno(EINVAL));
    EXPECT_THAT(splice(sock_rd.get(), bad_offset, pipe.WriteSide().get(), nullptr, 4, 0),
                SyscallFailsWithErrno(EFAULT));
    EXPECT_THAT(splice(pipe.ReadSide().get(), nullptr, sock_wr.get(), &valid_offset, 4, 0),
                SyscallFailsWithErrno(EINVAL));
    EXPECT_THAT(splice(pipe.ReadSide().get(), nullptr, sock_wr.get(), bad_offset, 4, 0),
                SyscallFailsWithErrno(EFAULT));
  }

  // 2. vmsplice must validate iovec user addresses and short-circuit zero-length requests before
  // downcasting the target file descriptor to a pipe.
  {
    fbl::unique_fd dev_null(open("/dev/null", O_RDWR));
    ASSERT_TRUE(dev_null.is_valid());

    EXPECT_THAT(vmsplice(dev_null.get(), nullptr, 0, 0), SyscallSucceedsWithValue(0));

    iovec empty_iov = {.iov_base = nullptr, .iov_len = 0};
    EXPECT_THAT(vmsplice(dev_null.get(), &empty_iov, 1, 0), SyscallSucceedsWithValue(0));

    iovec bad_iov = {.iov_base = reinterpret_cast<void*>(~0ull), .iov_len = 4096};
    EXPECT_THAT(vmsplice(dev_null.get(), &bad_iov, 1, 0), SyscallFailsWithErrno(EFAULT));
  }

  // 3. splice from a pipe to a regular file under RLIMIT_FSIZE must check RLIMIT_FSIZE against
  // actual bytes available in the pipe (after EOF/EAGAIN checks) rather than the requested length.
  {
    test_helper::ForkHelper helper;
    helper.RunInForkedProcess([] {
      static volatile sig_atomic_t sigxfsz_count = 0;
      struct sigaction sa = {};
      sa.sa_handler = [](int) { sigxfsz_count = sigxfsz_count + 1; };
      SAFE_SYSCALL(sigaction(SIGXFSZ, &sa, nullptr));

      test_helper::ScopedTempFD temp_file;
      ASSERT_TRUE(temp_file.is_valid());

      const rlimit rl = {.rlim_cur = 4, .rlim_max = 4};
      SAFE_SYSCALL(setrlimit(RLIMIT_FSIZE, &rl));

      // 3a. Requesting len = 8 (> RLIMIT_FSIZE) when the pipe only has 4 bytes must transfer 4
      // bytes and not raise SIGXFSZ.
      {
        test_helper::ScopedPipe pipe(O_NONBLOCK);
        ASSERT_EQ(write(pipe.WriteSide().get(), "AAAA", 4), 4);
        loff_t off = 0;
        sigxfsz_count = 0;
        EXPECT_THAT(
            splice(pipe.ReadSide().get(), nullptr, temp_file.fd(), &off, 8, SPLICE_F_NONBLOCK),
            SyscallSucceedsWithValue(4));
        EXPECT_EQ(off, 4);
        EXPECT_EQ(sigxfsz_count, 0);
      }

      // 3b. Splicing from an empty non-blocking pipe or an EOF pipe when off >= RLIMIT_FSIZE must
      // return EAGAIN or 0 without raising SIGXFSZ.
      {
        test_helper::ScopedPipe pipe(O_NONBLOCK);
        loff_t off = 4;
        sigxfsz_count = 0;
        EXPECT_THAT(
            splice(pipe.ReadSide().get(), nullptr, temp_file.fd(), &off, 4, SPLICE_F_NONBLOCK),
            SyscallFailsWithErrno(EAGAIN));
        EXPECT_EQ(sigxfsz_count, 0);

        pipe.WriteSide().reset();
        EXPECT_THAT(splice(pipe.ReadSide().get(), nullptr, temp_file.fd(), &off, 4, 0),
                    SyscallSucceedsWithValue(0));
        EXPECT_EQ(sigxfsz_count, 0);
      }

      // 3c. Splicing 8 bytes in the pipe at off = 0 with RLIMIT_FSIZE = 4 clamps the transfer to 4
      // bytes, leaves the remaining 4 bytes in the pipe, and raises SIGXFSZ; splicing again at
      // off = 4 fails with EFBIG, raises SIGXFSZ, and preserves the remaining 4 bytes in the pipe.
      {
        test_helper::ScopedPipe pipe(O_NONBLOCK);
        ASSERT_EQ(write(pipe.WriteSide().get(), "AAAABBBB", 8), 8);
        loff_t off = 0;
        sigxfsz_count = 0;
        EXPECT_THAT(
            splice(pipe.ReadSide().get(), nullptr, temp_file.fd(), &off, 8, SPLICE_F_NONBLOCK),
            SyscallSucceedsWithValue(4));
        EXPECT_EQ(off, 4);
        EXPECT_EQ(sigxfsz_count, 1);

        sigxfsz_count = 0;
        EXPECT_THAT(
            splice(pipe.ReadSide().get(), nullptr, temp_file.fd(), &off, 4, SPLICE_F_NONBLOCK),
            SyscallFailsWithErrno(EFBIG));
        EXPECT_EQ(sigxfsz_count, 1);

        char rem[4] = {};
        ASSERT_EQ(read(pipe.ReadSide().get(), rem, sizeof(rem)), 4);
        EXPECT_EQ(std::string(rem, 4), "BBBB");
      }
    });
    ASSERT_TRUE(helper.WaitForChildren());
  }
}

// Verify that when the only writer (a child process) exits, the pipe is immediately closed even if
// the child is not yet reaped.
TEST(PipeTest, ZombieWriter) {
  int pipe_fds[2];
  // Create a non-blocking pipe so read won't block if the writer is still alive.
  SAFE_SYSCALL(pipe2(pipe_fds, O_NONBLOCK));
  fbl::unique_fd pipe_rd(pipe_fds[0]);
  fbl::unique_fd pipe_wr(pipe_fds[1]);

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([read_fd = pipe_fds[0]] {
    // Close read end so it is only a writer, then exit immediately.
    // This should also close the write end.
    close(read_fd);
  });

  // Close the parent's write end so the child is the only writer.
  // Then wait for the child to exit and enter the zombie state.
  pipe_wr.reset();
  ASSERT_TRUE(helper.WaitForChildren());

  // Read from the pipe.
  //
  // If the uniquely-owned file descriptor table was cleared correctly on child exit, the write-end
  // is closed, and read() should return 0 immediately.
  char buf[1];
  EXPECT_THAT(read(pipe_rd.get(), buf, sizeof(buf)), SyscallSucceedsWithValue(0));
}

// Verify that when a multi-threaded child process exits, the pipe is immediately closed when the
// last thread (the main thread, in this case) exits, even if spawned threads exited earlier.
TEST(PipeTest, ZombieMultiThreadedWriter) {
  int pipe_fds[2];
  SAFE_SYSCALL(pipe2(pipe_fds, O_NONBLOCK));
  fbl::unique_fd pipe_rd(pipe_fds[0]);
  fbl::unique_fd pipe_wr(pipe_fds[1]);

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([read_fd = pipe_fds[0]] {
    close(read_fd);

    // Spawn a thread that also shares the FD table.
    std::thread t([] {
      // Just exit immediately.
    });

    // Wait for the spawned thread to exit.
    // This decrements file descriptor table's share count to 1.
    t.join();

    // Now the main thread (the last thread) exits, decrementing the share count to 0. This should
    // immediately clear FDs.
  });

  pipe_wr.reset();
  ASSERT_TRUE(helper.WaitForChildren());

  char buf[1];
  EXPECT_THAT(read(pipe_rd.get(), buf, sizeof(buf)), SyscallSucceedsWithValue(0));
}

TEST(PipeTest, PartialReadWakesBlockedWriter) {
  int pipe_fds[2];
  SAFE_SYSCALL(pipe2(pipe_fds, 0));
  fbl::unique_fd pipe_rd(pipe_fds[0]);
  fbl::unique_fd pipe_wr(pipe_fds[1]);

  const int page_size = getpagesize();
  SAFE_SYSCALL(fcntl(pipe_wr.get(), F_SETPIPE_SZ, 2 * page_size));
  std::vector<char> fill(2 * page_size, 'A');
  ASSERT_EQ(write(pipe_wr.get(), fill.data(), fill.size()), 2 * page_size);

  std::thread reader([&pipe_rd, page_size]() {
    usleep(20000);
    // Read one page, leaving the pipe non-empty while freeing space for the blocked writer.
    std::vector<char> buf(page_size);
    EXPECT_EQ(read(pipe_rd.get(), buf.data(), buf.size()), page_size);
  });

  const char extra[] = "WXYZ";
  EXPECT_EQ(write(pipe_wr.get(), extra, 4), 4);
  reader.join();
}

TEST(PipeTest, PollHupWithoutPollinOnEmptyClosedPipe) {
  int pipe_fds[2];
  SAFE_SYSCALL(pipe2(pipe_fds, 0));
  fbl::unique_fd pipe_rd(pipe_fds[0]);
  fbl::unique_fd pipe_wr(pipe_fds[1]);

  ASSERT_EQ(write(pipe_wr.get(), "data", 4), 4);
  pipe_wr.reset();

  pollfd pfd = {
      .fd = pipe_rd.get(),
      .events = POLLIN,
      .revents = 0,
  };
  ASSERT_EQ(poll(&pfd, 1, 0), 1);
  EXPECT_EQ(pfd.revents, POLLIN | POLLHUP);

  char buf[4];
  ASSERT_EQ(read(pipe_rd.get(), buf, 4), 4);

  pfd.revents = 0;
  ASSERT_EQ(poll(&pfd, 1, 0), 1);
  EXPECT_EQ(pfd.revents, POLLHUP);
}

TEST(PipeTest, ZeroLengthIoAndSplicePrecedence) {
  int pipe_a[2];
  int pipe_b[2];
  int blocking_pipe[2];
  SAFE_SYSCALL(pipe2(pipe_a, O_NONBLOCK));
  SAFE_SYSCALL(pipe2(pipe_b, O_NONBLOCK));
  SAFE_SYSCALL(pipe2(blocking_pipe, 0));
  fbl::unique_fd a_rd(pipe_a[0]);
  fbl::unique_fd a_wr(pipe_a[1]);
  fbl::unique_fd b_rd(pipe_b[0]);
  fbl::unique_fd b_wr(pipe_b[1]);
  fbl::unique_fd blk_rd(blocking_pipe[0]);
  fbl::unique_fd blk_wr(blocking_pipe[1]);

  // Zero-length read and readv on empty non-blocking and blocking pipes return 0 rather than
  // EAGAIN or blocking.
  char buf[1];
  iovec empty_iov = {.iov_base = buf, .iov_len = 0};
  EXPECT_THAT(read(a_rd.get(), buf, 0), SyscallSucceedsWithValue(0));
  EXPECT_THAT(readv(a_rd.get(), &empty_iov, 1), SyscallSucceedsWithValue(0));
  EXPECT_THAT(read(blk_rd.get(), buf, 0), SyscallSucceedsWithValue(0));
  EXPECT_THAT(readv(blk_rd.get(), &empty_iov, 1), SyscallSucceedsWithValue(0));

  // Passing a non-null invalid offset pointer for a pipe fd in splice fails with ESPIPE before
  // dereferencing the pointer (EFAULT).
  loff_t* bad_offset = reinterpret_cast<loff_t*>(0xdeadbeef);
  EXPECT_THAT(splice(a_rd.get(), bad_offset, b_wr.get(), nullptr, 4, 0),
              SyscallFailsWithErrno(ESPIPE));

  // Zero-length splice returns 0 before validating flags or file descriptors.
  EXPECT_THAT(splice(-1, nullptr, -1, nullptr, 0, ~0u), SyscallSucceedsWithValue(0));

  // Zero-length write and writev on broken non-blocking and blocking pipes return 0 without EPIPE
  // or raising SIGPIPE.
  b_rd.reset();
  blk_rd.reset();
  EXPECT_THAT(write(b_wr.get(), "", 0), SyscallSucceedsWithValue(0));
  EXPECT_THAT(writev(b_wr.get(), &empty_iov, 1), SyscallSucceedsWithValue(0));
  EXPECT_THAT(write(blk_wr.get(), "", 0), SyscallSucceedsWithValue(0));
  EXPECT_THAT(writev(blk_wr.get(), &empty_iov, 1), SyscallSucceedsWithValue(0));
}

TEST(PipeTest, SpliceNonblockSocketEndpoint) {
  int sock_fds[2];
  SAFE_SYSCALL(socketpair(AF_UNIX, SOCK_STREAM, 0, sock_fds));
  fbl::unique_fd sock_rd(sock_fds[0]);
  fbl::unique_fd sock_wr(sock_fds[1]);

  test_helper::ScopedPipe pipe;

  // SPLICE_F_NONBLOCK must make splice from an empty blocking socket return EAGAIN immediately
  // without mutating the socket's O_NONBLOCK flag.
  EXPECT_THAT(splice(sock_rd.get(), nullptr, pipe.WriteSide().get(), nullptr, 4, SPLICE_F_NONBLOCK),
              SyscallFailsWithErrno(EAGAIN));
  EXPECT_EQ(fcntl(sock_rd.get(), F_GETFL) & O_NONBLOCK, 0);

  // O_NONBLOCK on the pipe write end must also make splice from an empty blocking socket return
  // EAGAIN immediately even when SPLICE_F_NONBLOCK is not passed, without mutating the socket's
  // O_NONBLOCK flag.
  test_helper::ScopedPipe nb_pipe(O_NONBLOCK);
  EXPECT_THAT(splice(sock_rd.get(), nullptr, nb_pipe.WriteSide().get(), nullptr, 4, 0),
              SyscallFailsWithErrno(EAGAIN));
  EXPECT_EQ(fcntl(sock_rd.get(), F_GETFL) & O_NONBLOCK, 0);
}

}  // namespace
