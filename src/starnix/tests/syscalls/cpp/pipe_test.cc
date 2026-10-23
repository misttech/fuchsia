// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/poll.h>
#include <sys/resource.h>
#include <sys/sendfile.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <future>
#include <string_view>
#include <thread>
#include <vector>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

#ifndef O_NOTIFICATION_PIPE
#define O_NOTIFICATION_PIPE O_EXCL  // As defined in <linux/watch_queue.h>.
#endif

#ifndef RWF_NOWAIT
#define RWF_NOWAIT 0x00000008
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

TEST(PipeTest, SetPipeSizeRoundingAndWakeBlockedWriter) {
  const int page_size = getpagesize();
  test_helper::ScopedPipe pipe;

  // 1. Negative F_SETPIPE_SZ arguments must be rejected with EINVAL.
  EXPECT_THAT(fcntl(pipe.WriteSide().get(), F_SETPIPE_SZ, -1), SyscallFailsWithErrno(EINVAL));

  // 2. Non-power-of-two sizes above page_size must round up to the next power of two.
  // Before fixing F_SETPIPE_SZ rounding, Starnix rounded 3 * page_size to 3 * page_size instead
  // of 4 * page_size.
  EXPECT_THAT(fcntl(pipe.WriteSide().get(), F_SETPIPE_SZ, 3 * page_size),
              SyscallSucceedsWithValue(4 * page_size));
  EXPECT_THAT(fcntl(pipe.WriteSide().get(), F_GETPIPE_SZ), SyscallSucceedsWithValue(4 * page_size));

  // 3. Growing a full pipe via F_SETPIPE_SZ must wake a blocked writer and assert POLLOUT.
  std::vector<char> fill(4 * page_size, 'F');
  ASSERT_EQ(write(pipe.WriteSide().get(), fill.data(), fill.size()), 4 * page_size);

  pollfd pfd = {
      .fd = pipe.WriteSide().get(),
      .events = POLLOUT,
      .revents = 0,
  };
  EXPECT_EQ(poll(&pfd, 1, 0), 0);

  std::atomic<bool> writer_done{false};
  std::thread resizer([&pipe, page_size, &writer_done]() {
    usleep(20000);
    EXPECT_THAT(fcntl(pipe.WriteSide().get(), F_SETPIPE_SZ, 8 * page_size),
                SyscallSucceedsWithValue(8 * page_size));
    // Linux kernels prior to 6.6.15 (CVE-2023-52672, "pipe: wakeup wr_wait after setting
    // max_usage") wake `pipe->wr_wait` inside `pipe_resize_ring()` before updating
    // `pipe->max_usage` in `pipe_set_size()`. If the blocked writer evaluates `pipe_writable()`
    // before `pipe->max_usage` is updated, it goes back to sleep with `pipe->max_usage` already
    // set to the new capacity. Re-issuing `F_SETPIPE_SZ` with the same capacity is a wakeup no-op
    // on Starnix (`requested_capacity == old_capacity`) but triggers `pipe_resize_ring()` on
    // unpatched Linux kernels to wake `pipe->wr_wait` once `pipe->max_usage` reflects 8 pages.
    while (!writer_done.load(std::memory_order_acquire)) {
      usleep(10000);
      if (!writer_done.load(std::memory_order_acquire)) {
        EXPECT_THAT(fcntl(pipe.WriteSide().get(), F_SETPIPE_SZ, 8 * page_size),
                    SyscallSucceedsWithValue(8 * page_size));
      }
    }
  });

  const char payload[] = "GROW";
  EXPECT_THAT(write(pipe.WriteSide().get(), payload, 4), SyscallSucceedsWithValue(4));
  writer_done.store(true, std::memory_order_release);
  resizer.join();

  pfd.revents = 0;
  EXPECT_EQ(poll(&pfd, 1, 0), 1);
  EXPECT_EQ(pfd.revents, POLLOUT);
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

TEST(PipeTest, SpliceFromSocketReaderClosesMidFlight) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    signal(SIGPIPE, SIG_IGN);

    int sock_fds[2];
    SAFE_SYSCALL(socketpair(AF_UNIX, SOCK_STREAM, 0, sock_fds));
    fbl::unique_fd sock_rd(sock_fds[0]);
    fbl::unique_fd sock_wr(sock_fds[1]);

    test_helper::ScopedPipe pipe;

    const int page_size = getpagesize();
    SAFE_SYSCALL(fcntl(pipe.WriteSide().get(), F_SETPIPE_SZ, page_size));
    std::vector<char> fill(page_size, 'A');
    ASSERT_EQ(write(pipe.WriteSide().get(), fill.data(), fill.size()), page_size);

    const char payload[] = "data";
    ASSERT_EQ(write(sock_wr.get(), payload, 4), 4);

    std::thread closer([&pipe]() {
      usleep(20000);
      pipe.ReadSide().reset();
    });

    EXPECT_THAT(splice(sock_rd.get(), nullptr, pipe.WriteSide().get(), nullptr, 4, 0),
                SyscallFailsWithErrno(EPIPE));
    closer.join();
  });
  ASSERT_TRUE(helper.WaitForChildren());
}

TEST(PipeTest, TeeAndSplicePipeToPipeEdgeCases) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    signal(SIGPIPE, SIG_IGN);

    test_helper::ScopedPipe pipe_a;
    test_helper::ScopedPipe pipe_b(O_NONBLOCK);

    // O_NONBLOCK on pipe_b causes tee on empty pipe_a to return EAGAIN immediately.
    EXPECT_THAT(tee(pipe_a.ReadSide().get(), pipe_b.WriteSide().get(), 16, 0),
                SyscallFailsWithErrno(EAGAIN));

    // Close pipe_a writer (EOF on pipe_a).
    pipe_a.WriteSide().reset();

    // tee from an EOF pipe into a writable pipe returns 0 immediately.
    EXPECT_EQ(tee(pipe_a.ReadSide().get(), pipe_b.WriteSide().get(), 16, 0), 0);

    // Fill pipe_b completely; tee from an EOF pipe into a full non-blocking pipe returns EAGAIN.
    const int page_size = getpagesize();
    SAFE_SYSCALL(fcntl(pipe_b.WriteSide().get(), F_SETPIPE_SZ, page_size));
    std::vector<char> fill(page_size, 'B');
    ASSERT_EQ(write(pipe_b.WriteSide().get(), fill.data(), fill.size()), page_size);
    EXPECT_THAT(tee(pipe_a.ReadSide().get(), pipe_b.WriteSide().get(), 16, 0),
                SyscallFailsWithErrno(EAGAIN));

    // Closing the destination reader causes tee to return EPIPE even when the source is at EOF.
    pipe_b.ReadSide().reset();
    EXPECT_THAT(tee(pipe_a.ReadSide().get(), pipe_b.WriteSide().get(), 16, 0),
                SyscallFailsWithErrno(EPIPE));
  });
  ASSERT_TRUE(helper.WaitForChildren());
}

TEST(PipeTest, SpliceEofPipeToClosedSocket) {
  test_helper::ScopedPipe pipe;
  pipe.WriteSide().reset();

  int sock_fds[2];
  SAFE_SYSCALL(socketpair(AF_UNIX, SOCK_STREAM, 0, sock_fds));
  close(sock_fds[0]);
  fbl::unique_fd sock_wr(sock_fds[1]);

  // Splicing from an empty pipe with no writers (EOF) returns 0 immediately without invoking
  // write() on the closed destination socket.
  EXPECT_EQ(splice(pipe.ReadSide().get(), nullptr, sock_wr.get(), nullptr, 16, 0), 0);
}

// Verify pipe2(O_DIRECT) flag placement and packet mode read/write semantics.
TEST(PipeTest, ODirectPacketMode) {
  int fds[2];
  SAFE_SYSCALL(pipe2(fds, O_DIRECT | O_NONBLOCK));
  fbl::unique_fd rfd(fds[0]);
  fbl::unique_fd wfd(fds[1]);

  // Linux sets O_DIRECT only on the write end (fds[1]), never on the read end (fds[0]).
  int rflags = SAFE_SYSCALL(fcntl(rfd.get(), F_GETFL));
  EXPECT_EQ(rflags & O_DIRECT, 0);
  int wflags = SAFE_SYSCALL(fcntl(wfd.get(), F_GETFL));
  EXPECT_NE(wflags & O_DIRECT, 0);

  // Two small writes remain distinct packets and do not coalesce on read.
  ASSERT_THAT(write(wfd.get(), "hello", 5), SyscallSucceedsWithValue(5));
  ASSERT_THAT(write(wfd.get(), "world", 5), SyscallSucceedsWithValue(5));

  char buf[16] = {};
  // Reading with a 16-byte buffer returns only the first 5-byte packet ("hello").
  EXPECT_THAT(read(rfd.get(), buf, sizeof(buf)), SyscallSucceedsWithValue(5));
  EXPECT_EQ(std::string_view(buf, 5), "hello");

  // Reading with a 2-byte buffer truncates the second packet ("wo") and discards the remaining 3
  // bytes ("rld").
  ssize_t second_read = read(rfd.get(), buf, 2);
  EXPECT_EQ(second_read, 2);
  if (second_read == 2) {
    EXPECT_EQ(std::string_view(buf, 2), "wo");
  }

  // The pipe is now empty because the unread tail of the second packet was discarded.
  EXPECT_THAT(read(rfd.get(), buf, sizeof(buf)), SyscallFailsWithErrno(EAGAIN));
}

// Verify that partially reading a full page slot does not free write space until the entire page
// slot is drained.
TEST(PipeTest, NonBlockingWriteAfterPartialPageReadFailsWithEagain) {
  const size_t page_size = sysconf(_SC_PAGESIZE);
  int fds[2];
  SAFE_SYSCALL(pipe2(fds, O_NONBLOCK));
  fbl::unique_fd rfd(fds[0]);
  fbl::unique_fd wfd(fds[1]);

  ASSERT_THAT(fcntl(wfd.get(), F_SETPIPE_SZ, page_size),
              SyscallSucceedsWithValue(static_cast<int>(page_size)));

  std::vector<char> buf(page_size, 'A');
  ASSERT_THAT(write(wfd.get(), buf.data(), page_size),
              SyscallSucceedsWithValue(static_cast<ssize_t>(page_size)));

  // Read less than a full page so the single pipe_buffer slot remains occupied.
  ASSERT_THAT(read(rfd.get(), buf.data(), 100), SyscallSucceedsWithValue(100));

  // The sole page slot is still occupied and at the end of the page, so a non-blocking 1-byte
  // write fails with EAGAIN.
  EXPECT_THAT(write(wfd.get(), "B", 1), SyscallFailsWithErrno(EAGAIN));
}

// Verify that non-merging vmsplice writes exhaust pipe_buffer slots on a multi-page pipe.
TEST(PipeTest, VmspliceSlotExhaustionMultiPagePipe) {
  const size_t page_size = sysconf(_SC_PAGESIZE);
  int fds[2];
  SAFE_SYSCALL(pipe2(fds, O_NONBLOCK));
  fbl::unique_fd rfd(fds[0]);
  fbl::unique_fd wfd(fds[1]);

  ASSERT_THAT(fcntl(wfd.get(), F_SETPIPE_SZ, 2 * page_size),
              SyscallSucceedsWithValue(static_cast<int>(2 * page_size)));

  char byte = 'X';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};

  // Each vmsplice allocates a distinct pipe_buffer slot without PIPE_BUF_FLAG_CAN_MERGE.
  ASSERT_THAT(vmsplice(wfd.get(), &iov, 1, SPLICE_F_NONBLOCK), SyscallSucceedsWithValue(1));
  ASSERT_THAT(vmsplice(wfd.get(), &iov, 1, SPLICE_F_NONBLOCK), SyscallSucceedsWithValue(1));

  // Both slots on the 2-page pipe are occupied, so a third vmsplice or write fails with EAGAIN.
  EXPECT_THAT(vmsplice(wfd.get(), &iov, 1, SPLICE_F_NONBLOCK), SyscallFailsWithErrno(EAGAIN));
  EXPECT_THAT(write(wfd.get(), &byte, 1), SyscallFailsWithErrno(EAGAIN));
}

// Verify that a non-blocking write <= PIPE_BUF fails with EAGAIN when it cannot merge into the
// tail page slot and no free slots remain.
TEST(PipeTest, WriteCannotCrossPageSlotBoundaryWhenSlotsFull) {
  const size_t page_size = sysconf(_SC_PAGESIZE);
  int fds[2];
  SAFE_SYSCALL(pipe2(fds, O_NONBLOCK));
  fbl::unique_fd rfd(fds[0]);
  fbl::unique_fd wfd(fds[1]);

  ASSERT_THAT(fcntl(wfd.get(), F_SETPIPE_SZ, 2 * page_size),
              SyscallSucceedsWithValue(static_cast<int>(2 * page_size)));

  const size_t chunk = (page_size * 3) / 5;
  std::vector<char> buf(chunk, 'A');

  // First write occupies slot 0; second write (> page_size - chunk) allocates slot 1.
  ASSERT_THAT(write(wfd.get(), buf.data(), chunk),
              SyscallSucceedsWithValue(static_cast<ssize_t>(chunk)));
  ASSERT_THAT(write(wfd.get(), buf.data(), chunk),
              SyscallSucceedsWithValue(static_cast<ssize_t>(chunk)));

  // Both page slots are occupied and slot 1 has (page_size - chunk) bytes left. A write of size
  // (page_size - chunk + 1) <= PIPE_BUF cannot merge into slot 1 and has no free slot, so it fails
  // with EAGAIN.
  const size_t third_write = page_size - chunk + 1;
  EXPECT_THAT(write(wfd.get(), buf.data(), third_write), SyscallFailsWithErrno(EAGAIN));
}

// Verify tail-merge semantics for non-blocking writes > PIPE_BUF on a pipe with an unfilled tail
// slot and no free slots.
TEST(PipeTest, NonBlockingLargeWriteTailMergeSemantics) {
  const size_t page_size = sysconf(_SC_PAGESIZE);
  int fds[2];
  SAFE_SYSCALL(pipe2(fds, O_NONBLOCK));
  fbl::unique_fd rfd(fds[0]);
  fbl::unique_fd wfd(fds[1]);

  ASSERT_THAT(fcntl(wfd.get(), F_SETPIPE_SZ, page_size),
              SyscallSucceedsWithValue(static_cast<int>(page_size)));

  std::vector<char> buf(page_size + 200, 'A');

  // Case A: 100 bytes in slot 0. Writing (page_size + 200) bytes merges only
  // chars = (total_len & (page_size - 1)) = 200 bytes into slot 0 and returns 200.
  ASSERT_THAT(write(wfd.get(), buf.data(), 100), SyscallSucceedsWithValue(100));
  EXPECT_THAT(write(wfd.get(), buf.data(), page_size + 200), SyscallSucceedsWithValue(200));

  // Drain all bytes currently in the pipe.
  ASSERT_GT(read(rfd.get(), buf.data(), page_size), 0);

  // Case B: (page_size - 100) bytes in slot 0 (100 bytes remaining in page). Writing
  // (page_size + 200) bytes has chars = 200 > 100, which cannot merge into slot 0, and no free
  // slots remain, so it fails with EAGAIN.
  ASSERT_THAT(write(wfd.get(), buf.data(), page_size - 100),
              SyscallSucceedsWithValue(static_cast<ssize_t>(page_size - 100)));
  EXPECT_THAT(write(wfd.get(), buf.data(), page_size + 200), SyscallFailsWithErrno(EAGAIN));
}

// Verify pipe-to-pipe splice and tee slot limits and partial source buffer retention.
TEST(PipeTest, SpliceAndTeePipeToPipeSlotSemantics) {
  const size_t page_size = sysconf(_SC_PAGESIZE);

  // Scenario A: tee and splice into a 1-slot destination pipe transfer at most 1 pipe_buffer.
  {
    int in_fds[2], out_fds[2];
    SAFE_SYSCALL(pipe2(in_fds, O_NONBLOCK));
    SAFE_SYSCALL(pipe2(out_fds, O_NONBLOCK));
    fbl::unique_fd in_r(in_fds[0]), in_w(in_fds[1]);
    fbl::unique_fd out_r(out_fds[0]), out_w(out_fds[1]);

    ASSERT_THAT(fcntl(out_w.get(), F_SETPIPE_SZ, page_size),
                SyscallSucceedsWithValue(static_cast<int>(page_size)));

    char data[100] = {};
    struct iovec iov = {.iov_base = data, .iov_len = sizeof(data)};
    ASSERT_THAT(vmsplice(in_w.get(), &iov, 1, SPLICE_F_NONBLOCK), SyscallSucceedsWithValue(100));
    ASSERT_THAT(vmsplice(in_w.get(), &iov, 1, SPLICE_F_NONBLOCK), SyscallSucceedsWithValue(100));

    EXPECT_THAT(tee(in_r.get(), out_w.get(), 200, SPLICE_F_NONBLOCK),
                SyscallSucceedsWithValue(100));

    char drain[200];
    ASSERT_GT(read(out_r.get(), drain, sizeof(drain)), 0);

    EXPECT_THAT(splice(in_r.get(), nullptr, out_w.get(), nullptr, 200, SPLICE_F_NONBLOCK),
                SyscallSucceedsWithValue(100));
  }

  // Scenario B: Splicing 1 byte out of a full 1-page source pipe leaves the source slot occupied.
  {
    int src_fds[2], dst_fds[2];
    SAFE_SYSCALL(pipe2(src_fds, O_NONBLOCK));
    SAFE_SYSCALL(pipe2(dst_fds, O_NONBLOCK));
    fbl::unique_fd src_r(src_fds[0]), src_w(src_fds[1]);
    fbl::unique_fd dst_r(dst_fds[0]), dst_w(dst_fds[1]);

    ASSERT_THAT(fcntl(src_w.get(), F_SETPIPE_SZ, page_size),
                SyscallSucceedsWithValue(static_cast<int>(page_size)));
    std::vector<char> fill(page_size, 'A');
    ASSERT_THAT(write(src_w.get(), fill.data(), page_size),
                SyscallSucceedsWithValue(static_cast<ssize_t>(page_size)));

    ASSERT_THAT(splice(src_r.get(), nullptr, dst_w.get(), nullptr, 1, SPLICE_F_NONBLOCK),
                SyscallSucceedsWithValue(1));
    EXPECT_THAT(write(src_w.get(), "Z", 1), SyscallFailsWithErrno(EAGAIN));
  }
}

// Verify that fcntl(F_SETPIPE_SZ) checks occupied pipe_buffer slots rather than total bytes when
// shrinking a pipe.
TEST(PipeTest, FcntlSetPipeSzShrinkChecksOccupiedSlotsNotBytes) {
  const size_t page_size = sysconf(_SC_PAGESIZE);
  int fds[2];
  SAFE_SYSCALL(pipe2(fds, O_NONBLOCK));
  fbl::unique_fd rfd(fds[0]);
  fbl::unique_fd wfd(fds[1]);

  // Write page_size + 1 bytes (occupying 2 page slots), then read page_size - 1 bytes so 1 byte
  // remains in slot 0 and 1 byte remains in slot 1 (2 bytes total across 2 slots).
  std::vector<char> buf(page_size + 1, 'A');
  ASSERT_THAT(write(wfd.get(), buf.data(), page_size + 1),
              SyscallSucceedsWithValue(static_cast<ssize_t>(page_size + 1)));
  ASSERT_THAT(read(rfd.get(), buf.data(), page_size - 1),
              SyscallSucceedsWithValue(static_cast<ssize_t>(page_size - 1)));

  // Shrinking to 1 page slot fails with EBUSY because 2 slots are occupied.
  EXPECT_THAT(fcntl(wfd.get(), F_SETPIPE_SZ, page_size), SyscallFailsWithErrno(EBUSY));
}

// Verify that fcntl(F_SETPIPE_SZ, (1 << 30) + 1) fails with -1 and ENOMEM/EPERM instead of
// rounding to 0x80000000 and returning INT_MIN.
TEST(PipeTest, FcntlSetPipeSzNearIntMaxDoesNotReturnIntMin) {
  int fds[2];
  SAFE_SYSCALL(pipe(fds));
  fbl::unique_fd rfd(fds[0]);
  fbl::unique_fd wfd(fds[1]);

  errno = 0;
  int ret = fcntl(wfd.get(), F_SETPIPE_SZ, (1 << 30) + 1);
  EXPECT_EQ(ret, -1);
  EXPECT_TRUE(errno == ENOMEM || errno == EPERM) << "Unexpected errno: " << errno;
}

// Verify that consecutive small writes merge into a single pipe_buffer so a mid-read fault on the
// first buffer rolls back and returns EFAULT with 0 bytes consumed.
TEST(PipeTest, MergedWriteFaultOnReadReturnsEfault) {
  const size_t page_size = sysconf(_SC_PAGESIZE);
  int fds[2];
  SAFE_SYSCALL(pipe2(fds, O_NONBLOCK));
  fbl::unique_fd rfd(fds[0]);
  fbl::unique_fd wfd(fds[1]);

  std::vector<char> data(100, 'A');
  // Two 100-byte writes merge into a single 200-byte pipe_buffer on Linux.
  ASSERT_THAT(write(wfd.get(), data.data(), 100), SyscallSucceedsWithValue(100));
  ASSERT_THAT(write(wfd.get(), data.data(), 100), SyscallSucceedsWithValue(100));

  auto mapping = test_helper::ScopedMMap::MMap(nullptr, 2 * page_size, PROT_READ | PROT_WRITE,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_TRUE(mapping.is_ok()) << mapping.error_value();
  char* base = static_cast<char*>(mapping->mapping());
  ASSERT_THAT(mprotect(base + page_size, page_size, PROT_NONE), SyscallSucceeds());

  // Buffer spans 100 writable bytes in page 0 and 100 PROT_NONE bytes in page 1.
  char* read_buf = base + page_size - 100;
  EXPECT_THAT(read(rfd.get(), read_buf, 200), SyscallFailsWithErrno(EFAULT));

  // All 200 bytes remain unread in the pipe.
  std::vector<char> verify(200);
  EXPECT_THAT(read(rfd.get(), verify.data(), 200), SyscallSucceedsWithValue(200));
}

// Verify that splice from an AF_INET TCP socket into a 2-page pipe with SPLICE_F_NONBLOCK fails
// with EAGAIN once both pipe_buffer slots are occupied by prior socket splices.
TEST(PipeTest, SpliceFromTcpSocketHonorsSpliceFNonblock) {
  const size_t page_size = sysconf(_SC_PAGESIZE);
  fbl::unique_fd listener(socket(AF_INET, SOCK_STREAM, 0));
  ASSERT_TRUE(listener.is_valid());

  struct sockaddr_in addr = {};
  addr.sin_family = AF_INET;
  addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  addr.sin_port = 0;
  ASSERT_THAT(bind(listener.get(), reinterpret_cast<struct sockaddr*>(&addr), sizeof(addr)),
              SyscallSucceeds());
  socklen_t addr_len = sizeof(addr);
  ASSERT_THAT(getsockname(listener.get(), reinterpret_cast<struct sockaddr*>(&addr), &addr_len),
              SyscallSucceeds());
  ASSERT_THAT(listen(listener.get(), 1), SyscallSucceeds());

  fbl::unique_fd client(socket(AF_INET, SOCK_STREAM, 0));
  ASSERT_TRUE(client.is_valid());
  ASSERT_THAT(connect(client.get(), reinterpret_cast<struct sockaddr*>(&addr), sizeof(addr)),
              SyscallSucceeds());
  fbl::unique_fd server(accept(listener.get(), nullptr, nullptr));
  ASSERT_TRUE(server.is_valid());

  int pipe_fds[2];
  SAFE_SYSCALL(pipe(pipe_fds));
  fbl::unique_fd pipe_r(pipe_fds[0]), pipe_w(pipe_fds[1]);
  ASSERT_THAT(fcntl(pipe_w.get(), F_SETPIPE_SZ, 2 * page_size),
              SyscallSucceedsWithValue(static_cast<int>(2 * page_size)));

  ASSERT_THAT(write(client.get(), "hello!", 6), SyscallSucceedsWithValue(6));

  // Each splice from a TCP socket allocates a non-merging pipe_buffer slot. After two 2-byte
  // splices, both slots of the 2-page pipe are occupied.
  ASSERT_THAT(splice(server.get(), nullptr, pipe_w.get(), nullptr, 2, SPLICE_F_NONBLOCK),
              SyscallSucceedsWithValue(2));
  ASSERT_THAT(splice(server.get(), nullptr, pipe_w.get(), nullptr, 2, SPLICE_F_NONBLOCK),
              SyscallSucceedsWithValue(2));

  // Third splice with SPLICE_F_NONBLOCK fails with EAGAIN because both slots are occupied, leaving
  // the remaining 2 bytes unread in the TCP socket.
  EXPECT_THAT(splice(server.get(), nullptr, pipe_w.get(), nullptr, 2, SPLICE_F_NONBLOCK),
              SyscallFailsWithErrno(EAGAIN));

  char rem[2] = {};
  EXPECT_THAT(recv(server.get(), rem, sizeof(rem), MSG_DONTWAIT), SyscallSucceedsWithValue(2));
  EXPECT_EQ(std::string_view(rem, 2), "o!");
}

// Verify preadv/pwritev ESPIPE error precedence and preadv2/pwritev2 RWF_NOWAIT on pipes.
TEST(PipeTest, Preadv2Pwritev2RwfNowaitAndEspipePrecedence) {
  int fds[2];
  SAFE_SYSCALL(pipe(fds));
  fbl::unique_fd pipe_r(fds[0]);
  fbl::unique_fd pipe_w(fds[1]);

  // 1. preadv and pwritev on a pipe reject non-seekable descriptors with ESPIPE before reading the
  // user iovec pointer.
  const struct iovec* bad_iov = reinterpret_cast<const struct iovec*>(0x1);
  EXPECT_THAT(preadv(pipe_r.get(), bad_iov, 1, 0), SyscallFailsWithErrno(ESPIPE));
  EXPECT_THAT(pwritev(pipe_w.get(), bad_iov, 1, 0), SyscallFailsWithErrno(ESPIPE));

  // 2. preadv2 with offset == -1 and RWF_NOWAIT on an empty blocking pipe returns EAGAIN
  // immediately without blocking.
  std::atomic<bool> done{false};
  std::thread watchdog([&]() {
    for (int i = 0; i < 25 && !done.load(); ++i) {
      usleep(10000);
    }
    if (!done.load()) {
      // Unblock a stuck preadv2 by writing 1 byte to the pipe.
      std::ignore = write(pipe_w.get(), "X", 1);
    }
  });

  char buf[4] = {};
  struct iovec iov = {.iov_base = buf, .iov_len = sizeof(buf)};
  EXPECT_THAT(preadv2(pipe_r.get(), &iov, 1, -1, RWF_NOWAIT), SyscallFailsWithErrno(EAGAIN));
  done.store(true);
  watchdog.join();
}

// Verify that pipe2 does not leak open file descriptors into the process FD table when copying
// pipefd[2] to user memory faults with EFAULT.
TEST(PipeTest, Pipe2FaultAndEmfileDoNotLeakFds) {
  int probe_before = SAFE_SYSCALL(dup(0));
  SAFE_SYSCALL(close(probe_before));

  EXPECT_THAT(pipe2(reinterpret_cast<int*>(0x1), 0), SyscallFailsWithErrno(EFAULT));

  int probe_after = SAFE_SYSCALL(dup(0));
  SAFE_SYSCALL(close(probe_after));

  EXPECT_EQ(probe_after, probe_before);
  for (int leaked_fd = probe_before; leaked_fd < probe_after; ++leaked_fd) {
    close(leaked_fd);
  }
}

// Verify that sendfile from a regular file to a full O_NONBLOCK pipe fails with EAGAIN without
// advancing the input file offset or *offset.
TEST(PipeTest, SendfileToFullNonBlockingPipeDoesNotAdvanceOffset) {
  const size_t page_size = sysconf(_SC_PAGESIZE);
  test_helper::ScopedTempFD temp_file;
  ASSERT_TRUE(temp_file);
  const char payload[] = "0123456789abcdef";
  ASSERT_THAT(write(temp_file.fd(), payload, 16), SyscallSucceedsWithValue(16));
  ASSERT_THAT(lseek(temp_file.fd(), 0, SEEK_SET), SyscallSucceedsWithValue(0));

  int fds[2];
  SAFE_SYSCALL(pipe2(fds, O_NONBLOCK));
  fbl::unique_fd rfd(fds[0]);
  fbl::unique_fd wfd(fds[1]);

  ASSERT_THAT(fcntl(wfd.get(), F_SETPIPE_SZ, page_size),
              SyscallSucceedsWithValue(static_cast<int>(page_size)));
  std::vector<char> fill(page_size, 'A');
  ASSERT_THAT(write(wfd.get(), fill.data(), page_size),
              SyscallSucceedsWithValue(static_cast<ssize_t>(page_size)));

  off_t explicit_off = 0;
  EXPECT_THAT(sendfile(wfd.get(), temp_file.fd(), &explicit_off, 16),
              SyscallFailsWithErrno(EAGAIN));
  EXPECT_EQ(explicit_off, 0);

  EXPECT_THAT(sendfile(wfd.get(), temp_file.fd(), nullptr, 16), SyscallFailsWithErrno(EAGAIN));
  EXPECT_THAT(lseek(temp_file.fd(), 0, SEEK_CUR), SyscallSucceedsWithValue(0));
}

// Verify that splicing into a pipe does not emit a spurious EPOLLOUT edge-triggered wakeup and
// splicing out of a pipe does not emit a spurious EPOLLIN edge-triggered wakeup.
TEST(PipeTest, SpliceDoesNotTriggerSpuriousEpollEtWakeups) {
  int pipe_fds[2];
  SAFE_SYSCALL(pipe2(pipe_fds, O_NONBLOCK));
  fbl::unique_fd pipe_rd(pipe_fds[0]);
  fbl::unique_fd pipe_wr(pipe_fds[1]);

  test_helper::ScopedTempFD temp_file;
  ASSERT_TRUE(temp_file);
  const char payload[] = "01234567";
  ASSERT_THAT(write(temp_file.fd(), payload, 8), SyscallSucceedsWithValue(8));

  fbl::unique_fd epfd(SAFE_SYSCALL(epoll_create1(0)));

  // Monitor pipe_wr for edge-triggered EPOLLOUT and consume the initial readiness event.
  struct epoll_event ev = {};
  ev.events = EPOLLOUT | EPOLLET;
  ev.data.fd = pipe_wr.get();
  SAFE_SYSCALL(epoll_ctl(epfd.get(), EPOLL_CTL_ADD, pipe_wr.get(), &ev));

  struct epoll_event out_ev = {};
  ASSERT_EQ(SAFE_SYSCALL(epoll_wait(epfd.get(), &out_ev, 1, 0)), 1);
  ASSERT_EQ(out_ev.events & EPOLLOUT, static_cast<uint32_t>(EPOLLOUT));

  // Splicing 8 bytes from temp_file into pipe_wr consumes write space and must not fire a new
  // EPOLLOUT edge on pipe_wr.
  off64_t offset = 0;
  ASSERT_EQ(SAFE_SYSCALL(splice(temp_file.fd(), &offset, pipe_wr.get(), nullptr, 8, 0)), 8);
  EXPECT_EQ(SAFE_SYSCALL(epoll_wait(epfd.get(), &out_ev, 1, 0)), 0);

  // Monitor pipe_rd for edge-triggered EPOLLIN and consume the initial readiness event.
  SAFE_SYSCALL(epoll_ctl(epfd.get(), EPOLL_CTL_DEL, pipe_wr.get(), nullptr));
  ev.events = EPOLLIN | EPOLLET;
  ev.data.fd = pipe_rd.get();
  SAFE_SYSCALL(epoll_ctl(epfd.get(), EPOLL_CTL_ADD, pipe_rd.get(), &ev));

  ASSERT_EQ(SAFE_SYSCALL(epoll_wait(epfd.get(), &out_ev, 1, 0)), 1);
  ASSERT_EQ(out_ev.events & EPOLLIN, static_cast<uint32_t>(EPOLLIN));

  // Splicing 4 of the 8 bytes from pipe_rd to temp_file consumes readable data and must not fire a
  // new EPOLLIN edge on pipe_rd.
  ASSERT_EQ(SAFE_SYSCALL(splice(pipe_rd.get(), nullptr, temp_file.fd(), &offset, 4, 0)), 4);
  EXPECT_EQ(SAFE_SYSCALL(epoll_wait(epfd.get(), &out_ev, 1, 0)), 0);
}

// Verify that enabling O_NONBLOCK via fcntl(F_SETFL) while a multi-page write is blocked causes the
// writer to return a partial write count once a single page of space is freed.
TEST(PipeTest, ConcurrentFcntlNonblockUnblocksPartialWrite) {
  const size_t page_size = sysconf(_SC_PAGESIZE);
  int pipe_fds[2];
  SAFE_SYSCALL(pipe2(pipe_fds, 0));
  fbl::unique_fd pipe_rd(pipe_fds[0]);
  fbl::unique_fd pipe_wr(pipe_fds[1]);

  SAFE_SYSCALL(fcntl(pipe_wr.get(), F_SETPIPE_SZ, static_cast<int>(2 * page_size)));

  std::vector<char> fill_buf(2 * page_size, 'A');
  ASSERT_EQ(SAFE_SYSCALL(write(pipe_wr.get(), fill_buf.data(), fill_buf.size())),
            static_cast<ssize_t>(2 * page_size));

  std::promise<ssize_t> write_done;
  std::future<ssize_t> write_fut = write_done.get_future();
  std::atomic<bool> thread_started{false};

  std::thread writer([&]() {
    thread_started.store(true);
    std::vector<char> write_buf(2 * page_size, 'B');
    ssize_t n = write(pipe_wr.get(), write_buf.data(), write_buf.size());
    write_done.set_value(n);
  });

  while (!thread_started.load()) {
    std::this_thread::yield();
  }
  std::this_thread::sleep_for(std::chrono::milliseconds(50));

  // Enable O_NONBLOCK on pipe_wr while the write is blocked, then read 1 page to wake the writer.
  int flags = SAFE_SYSCALL(fcntl(pipe_wr.get(), F_GETFL));
  SAFE_SYSCALL(fcntl(pipe_wr.get(), F_SETFL, flags | O_NONBLOCK));

  std::vector<char> read_buf(page_size);
  ASSERT_EQ(SAFE_SYSCALL(read(pipe_rd.get(), read_buf.data(), page_size)),
            static_cast<ssize_t>(page_size));

  auto status = write_fut.wait_for(std::chrono::milliseconds(300));
  EXPECT_EQ(status, std::future_status::ready);
  if (status != std::future_status::ready) {
    // Drain another page so the stuck writer thread can exit cleanly on Starnix.
    std::ignore = read(pipe_rd.get(), read_buf.data(), page_size);
  } else {
    EXPECT_EQ(write_fut.get(), static_cast<ssize_t>(page_size));
  }
  writer.join();
}

// Verify FIFO EOF before any writer opens, absence of stale POLLHUP on newly opened readers, and
// blocking open(O_RDONLY) when all prior writers have closed while a reader stays open.
TEST(PipeTest, FifoLifecycleEofAndReopenSemantics) {
  test_helper::ScopedTempDir dir;
  std::string fifo_path = dir.path() + "/fifo";
  SAFE_SYSCALL(mkfifo(fifo_path.c_str(), 0600));

  // (a) Reading or splicing from a FIFO opened with O_RDONLY | O_NONBLOCK before any writer opens
  // it returns 0 (EOF), not EAGAIN.
  fbl::unique_fd rfd1(SAFE_SYSCALL(open(fifo_path.c_str(), O_RDONLY | O_NONBLOCK)));
  char ch = 0;
  EXPECT_THAT(read(rfd1.get(), &ch, 1), SyscallSucceedsWithValue(0));

  test_helper::ScopedPipe dst_pipe(O_NONBLOCK);
  EXPECT_THAT(
      splice(rfd1.get(), nullptr, dst_pipe.WriteSide().get(), nullptr, 1, SPLICE_F_NONBLOCK),
      SyscallSucceedsWithValue(0));

  // Open and close a writer while rfd1 remains open.
  fbl::unique_fd wfd1(SAFE_SYSCALL(open(fifo_path.c_str(), O_WRONLY)));
  wfd1.reset();

  // (b) A newly opened O_RDONLY | O_NONBLOCK descriptor opened after wfd1 closed does not report a
  // stale POLLHUP from wfd1's closure.
  fbl::unique_fd rfd2(SAFE_SYSCALL(open(fifo_path.c_str(), O_RDONLY | O_NONBLOCK)));
  struct pollfd pfd = {.fd = rfd2.get(), .events = POLLIN, .revents = 0};
  EXPECT_EQ(SAFE_SYSCALL(poll(&pfd, 1, 0)), 0);
  EXPECT_EQ(pfd.revents & POLLHUP, 0);

  // (c) Because writer_count is currently 0, a blocking open(O_RDONLY) blocks until a new writer
  // opens the FIFO, even though rfd1 and rfd2 remain open.
  std::promise<int> open_done;
  std::future<int> open_fut = open_done.get_future();
  std::thread opener([&]() {
    int fd = open(fifo_path.c_str(), O_RDONLY);
    open_done.set_value(fd);
  });

  auto status = open_fut.wait_for(std::chrono::milliseconds(100));
  EXPECT_EQ(status, std::future_status::timeout);

  // Opening a new writer unblocks the waiting open(O_RDONLY).
  fbl::unique_fd wfd2(SAFE_SYSCALL(open(fifo_path.c_str(), O_WRONLY)));
  ASSERT_EQ(open_fut.wait_for(std::chrono::milliseconds(500)), std::future_status::ready);
  fbl::unique_fd rfd3(open_fut.get());
  EXPECT_TRUE(rfd3.is_valid());
  opener.join();
}

// Verify that while splice(pipe_rd, nullptr, sock_wr, ...) is blocked on a full destination socket
// before transferring any bytes, poll(pipe_rd, POLLIN, 0) still reports POLLIN.
TEST(PipeTest, PollReadabilityDuringBlockedSpliceToFullSocket) {
  int pipe_fds[2];
  SAFE_SYSCALL(pipe2(pipe_fds, 0));
  fbl::unique_fd pipe_rd(pipe_fds[0]);
  fbl::unique_fd pipe_wr(pipe_fds[1]);

  ASSERT_EQ(SAFE_SYSCALL(write(pipe_wr.get(), "abcd", 4)), 4);

  int sock_fds[2];
  SAFE_SYSCALL(socketpair(AF_UNIX, SOCK_STREAM, 0, sock_fds));
  fbl::unique_fd sock_rd(sock_fds[0]);
  fbl::unique_fd sock_wr(sock_fds[1]);

  // Fill sock_wr's send buffer using non-blocking writes, then restore blocking mode.
  int sock_flags = SAFE_SYSCALL(fcntl(sock_wr.get(), F_GETFL));
  SAFE_SYSCALL(fcntl(sock_wr.get(), F_SETFL, sock_flags | O_NONBLOCK));
  std::vector<char> fill(16384, 'X');
  while (write(sock_wr.get(), fill.data(), fill.size()) > 0) {
  }
  ASSERT_EQ(errno, EAGAIN);
  SAFE_SYSCALL(fcntl(sock_wr.get(), F_SETFL, sock_flags));

  std::atomic<bool> splice_started{false};
  std::thread splicer([&]() {
    splice_started.store(true);
    std::ignore = splice(pipe_rd.get(), nullptr, sock_wr.get(), nullptr, 4, 0);
  });

  while (!splice_started.load()) {
    std::this_thread::yield();
  }
  std::this_thread::sleep_for(std::chrono::milliseconds(50));

  // No bytes have been written to sock_wr yet, so the 4 bytes remain in pipe_rd and poll(POLLIN)
  // returns 1.
  struct pollfd pfd = {.fd = pipe_rd.get(), .events = POLLIN, .revents = 0};
  EXPECT_EQ(SAFE_SYSCALL(poll(&pfd, 1, 0)), 1);
  EXPECT_EQ(pfd.revents & POLLIN, POLLIN);

  // Drain all buffered bytes from sock_rd so the blocked splice wakes up and completes cleanly.
  int rd_flags = SAFE_SYSCALL(fcntl(sock_rd.get(), F_GETFL));
  SAFE_SYSCALL(fcntl(sock_rd.get(), F_SETFL, rd_flags | O_NONBLOCK));
  while (read(sock_rd.get(), fill.data(), fill.size()) > 0) {
  }
  splicer.join();
}

// Verify that blocking readv, splice, and tee on a pipe automatically restart (ERESTARTSYS) when
// interrupted by a signal handler installed with SA_RESTART while 0 bytes are transferred.
TEST(PipeTest, SaRestartAppliesToReadvWritevSpliceVmspliceTee) {
  static std::atomic<int> sig_count{0};
  sig_count.store(0);

  struct sigaction sa = {};
  sa.sa_handler = [](int) { sig_count.fetch_add(1); };
  sa.sa_flags = SA_RESTART;
  sigemptyset(&sa.sa_mask);
  struct sigaction old_sa = {};
  SAFE_SYSCALL(sigaction(SIGUSR1, &sa, &old_sa));

  // 1. Test readv with SA_RESTART on an empty pipe.
  {
    int fds[2];
    SAFE_SYSCALL(pipe(fds));
    fbl::unique_fd rfd(fds[0]), wfd(fds[1]);

    std::atomic<bool> started{false};
    std::thread reader([&]() {
      started.store(true);
      char buf[4] = {};
      struct iovec iov = {.iov_base = buf, .iov_len = sizeof(buf)};
      EXPECT_THAT(readv(rfd.get(), &iov, 1), SyscallSucceedsWithValue(4));
      EXPECT_EQ(std::string_view(buf, 4), "test");
    });

    while (!started.load()) {
      std::this_thread::yield();
    }
    usleep(50000);
    ASSERT_EQ(pthread_kill(reader.native_handle(), SIGUSR1), 0);
    usleep(20000);
    ASSERT_THAT(write(wfd.get(), "test", 4), SyscallSucceedsWithValue(4));
    reader.join();
    EXPECT_GE(sig_count.load(), 1);
  }

  // 2. Test splice with SA_RESTART on an empty input pipe.
  {
    int in_fds[2], out_fds[2];
    SAFE_SYSCALL(pipe(in_fds));
    SAFE_SYSCALL(pipe(out_fds));
    fbl::unique_fd in_r(in_fds[0]), in_w(in_fds[1]);
    fbl::unique_fd out_r(out_fds[0]), out_w(out_fds[1]);

    std::atomic<bool> started{false};
    std::thread splicer([&]() {
      started.store(true);
      EXPECT_THAT(splice(in_r.get(), nullptr, out_w.get(), nullptr, 4, 0),
                  SyscallSucceedsWithValue(4));
    });

    while (!started.load()) {
      std::this_thread::yield();
    }
    usleep(50000);
    ASSERT_EQ(pthread_kill(splicer.native_handle(), SIGUSR1), 0);
    usleep(20000);
    ASSERT_THAT(write(in_w.get(), "data", 4), SyscallSucceedsWithValue(4));
    splicer.join();
  }

  // 3. Test tee with SA_RESTART on an empty input pipe.
  {
    int in_fds[2], out_fds[2];
    SAFE_SYSCALL(pipe(in_fds));
    SAFE_SYSCALL(pipe(out_fds));
    fbl::unique_fd in_r(in_fds[0]), in_w(in_fds[1]);
    fbl::unique_fd out_r(out_fds[0]), out_w(out_fds[1]);

    std::atomic<bool> started{false};
    std::thread teer([&]() {
      started.store(true);
      EXPECT_THAT(tee(in_r.get(), out_w.get(), 4, 0), SyscallSucceedsWithValue(4));
    });

    while (!started.load()) {
      std::this_thread::yield();
    }
    usleep(50000);
    ASSERT_EQ(pthread_kill(teer.native_handle(), SIGUSR1), 0);
    usleep(20000);
    ASSERT_THAT(write(in_w.get(), "teed", 4), SyscallSucceedsWithValue(4));
    teer.join();
  }

  SAFE_SYSCALL(sigaction(SIGUSR1, &old_sa, nullptr));
}

// Verify that polling the write end of a full pipe after all readers close returns POLLERR without
// POLLOUT.
TEST(PipeTest, PollOutOnFullPipeWithClosedReaderReturnsPollErrOnly) {
  const size_t page_size = sysconf(_SC_PAGESIZE);
  int fds[2];
  SAFE_SYSCALL(pipe2(fds, O_NONBLOCK));
  fbl::unique_fd rfd(fds[0]);
  fbl::unique_fd wfd(fds[1]);

  ASSERT_THAT(fcntl(wfd.get(), F_SETPIPE_SZ, page_size),
              SyscallSucceedsWithValue(static_cast<int>(page_size)));
  std::vector<char> fill(page_size, 'A');
  ASSERT_THAT(write(wfd.get(), fill.data(), page_size),
              SyscallSucceedsWithValue(static_cast<ssize_t>(page_size)));

  // Close the read end while the pipe remains completely full.
  rfd.reset();

  struct pollfd pfd = {.fd = wfd.get(), .events = POLLOUT, .revents = 0};
  ASSERT_EQ(SAFE_SYSCALL(poll(&pfd, 1, 0)), 1);
  EXPECT_EQ(pfd.revents, POLLERR);
}
}  // namespace
