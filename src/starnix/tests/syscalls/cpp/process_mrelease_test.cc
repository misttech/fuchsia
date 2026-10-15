// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <format>
#include <fstream>
#include <thread>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/files/file.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

// Define SYS_process_mrelease if not available in the toolchain's headers.
// This is syscall 448 on x86_64 and arm64.
#ifndef SYS_process_mrelease
#define SYS_process_mrelease 448
#endif

namespace {
const size_t kMemSizeBytes = 1 * 1024 * 1024;  // 1MB
constexpr char RSS_PREFIX[] = "VmRSS:";

int DoPidFdOpen(pid_t pid) { return static_cast<int>(syscall(SYS_pidfd_open, pid, 0u)); }
int DoProcessMRelease(int pidfd, int flags = 0) {
  return static_cast<int>(syscall(SYS_process_mrelease, pidfd, flags));
}

int GetProcessRSSMemorySize(pid_t pid) {
  auto path = std::format("/proc/{}/status", pid);
  std::ifstream file(path);
  if (!file.is_open()) {
    return testing::AssertionFailure() << "Unable to open " << path;
  }

  std::string line;
  while (std::getline(file, line)) {
    if (line.starts_with(RSS_PREFIX)) {
      std::string value_str = line.substr(strlen(RSS_PREFIX));
      // Trim leading whitespace
      value_str.erase(0, value_str.find_first_not_of(" \t\n\r\f\v"));
      // Remove " kB" suffix
      size_t kb_pos = value_str.rfind(" kB");
      if (kb_pos != std::string::npos) {
        value_str.erase(kb_pos);
      }
      return std::atoi(value_str.c_str());
    }
  }

  return testing::AssertionFailure() << "Did not find rss value";
}

std::string GetMapsString(pid_t pid) {
  auto path = std::format("/proc/{}/maps", pid);
  std::string res;
  files::ReadFileToString(path, &res);
  return res;
}

}  // namespace

class ProcessMemoryReleaseTest : public ::testing::Test {
 public:
  void SetUp() override {
    if (!test_helper::IsStarnix() && !test_helper::IsKernelVersionAtLeast(5, 15)) {
      GTEST_SKIP()
          << "process_mrelease isn't supported on Linux with kernel version older than 5.15,"
          << "skipping.";
    }
  }
};

TEST_F(ProcessMemoryReleaseTest, InvalidPidfd) {
  int invalid_pidfd = -1;
  EXPECT_THAT(DoProcessMRelease(invalid_pidfd), SyscallFailsWithErrno(EBADF));
}

TEST_F(ProcessMemoryReleaseTest, InvalidFlags) {
  test_helper::ForkHelper fork_helper;
  pid_t child_pid = fork_helper.RunInForkedProcess([]() {
    // Wait to be killed by the parent.
    pause();
  });
  int child_pidfd = DoPidFdOpen(child_pid);
  ASSERT_THAT(child_pidfd, SyscallSucceeds());
  ASSERT_THAT(kill(child_pid, SIGKILL), SyscallSucceeds());
  EXPECT_THAT(DoProcessMRelease(child_pidfd, -1), SyscallFailsWithErrno(EINVAL));
  ASSERT_FALSE(fork_helper.WaitForChildren());
}

TEST_F(ProcessMemoryReleaseTest, NoPendingSIGKILL) {
  test_helper::ForkHelper fork_helper;
  pid_t child_pid = fork_helper.RunInForkedProcess([]() {
    // Wait to be killed by the parent.
    pause();
  });
  int child_pidfd = DoPidFdOpen(child_pid);
  ASSERT_THAT(child_pidfd, SyscallSucceeds());
  // Child process is still active when reaping.
  EXPECT_THAT(DoProcessMRelease(child_pidfd), SyscallFailsWithErrno(EINVAL));
  ASSERT_THAT(kill(child_pid, SIGKILL), SyscallSucceeds());
  ASSERT_FALSE(fork_helper.WaitForChildren());
}

TEST_F(ProcessMemoryReleaseTest, SuccessfulReleaseImmediateSIGKILL) {
  // Sending SIGKILL transitions the target process into an exiting state synchronously, allowing
  // process_mrelease(2) to succeed immediately after kill(2) returns. If the child finishes
  // tearing down its address space before process_mrelease(2) runs, process_mrelease(2) fails with
  // ESRCH; retry on ESRCH to tolerate scheduling races.
  constexpr int kMaxAttempts = 10;
  for (int attempt = 0; attempt < kMaxAttempts; ++attempt) {
    test_helper::ForkHelper fork_helper;
    fork_helper.ExpectSignal(SIGKILL);
    pid_t child_pid = fork_helper.RunInForkedProcess([]() {
      for (;;) {
        std::this_thread::yield();
      }
    });

    fbl::unique_fd child_pidfd(DoPidFdOpen(child_pid));
    ASSERT_THAT(child_pidfd.get(), SyscallSucceeds());

    ASSERT_THAT(kill(child_pid, SIGKILL), SyscallSucceeds());
    int result = DoProcessMRelease(child_pidfd.get());
    int release_errno = errno;
    ASSERT_TRUE(fork_helper.WaitForChildren());

    if (result == -1 && release_errno == ESRCH) {
      continue;
    }
    errno = release_errno;
    ASSERT_THAT(result, SyscallSucceeds());
    return;
  }
  FAIL() << "process_mrelease(2) repeatedly lost the race with child exit (ESRCH)";
}

TEST_F(ProcessMemoryReleaseTest, ZombieProcessFailsWithEsrch) {
  test_helper::ForkHelper fork_helper;
  pid_t child_pid = fork_helper.RunInForkedProcess([]() { _exit(0); });
  fbl::unique_fd child_pidfd(DoPidFdOpen(child_pid));
  ASSERT_THAT(child_pidfd.get(), SyscallSucceeds());

  // Wait for the child to become a zombie without reaping it.
  siginfo_t info = {};
  ASSERT_THAT(HANDLE_EINTR(waitid(P_PID, child_pid, &info, WEXITED | WNOWAIT)), SyscallSucceeds());

  EXPECT_THAT(DoProcessMRelease(child_pidfd.get()), SyscallFailsWithErrno(ESRCH));
  ASSERT_TRUE(fork_helper.WaitForChildren());
}

TEST_F(ProcessMemoryReleaseTest, ExitedLeaderWithSiblingThreadSucceeds) {
  test_helper::ForkHelper fork_helper;
  pid_t child_pid = fork_helper.RunInForkedProcess([]() {
    test_helper::Rendezvous sibling_attached = test_helper::MakeRendezvous();
    std::thread sibling([attached = std::move(sibling_attached.poker)]() mutable {
      ASSERT_THAT(ptrace(PTRACE_TRACEME, 0, nullptr, nullptr), SyscallSucceeds());
      attached.poke();
      raise(SIGSTOP);
      _exit(0);
    });
    sibling.detach();
    sibling_attached.holder.hold();

    // Exit only the thread-group leader while the sibling thread remains alive.
    syscall(SYS_exit, 0);
  });

  fbl::unique_fd child_pidfd(DoPidFdOpen(child_pid));
  ASSERT_THAT(child_pidfd.get(), SyscallSucceeds());

  int status = 0;
  pid_t sibling_tid = waitpid(-1, &status, __WALL);
  ASSERT_THAT(sibling_tid, SyscallSucceeds());
  ASSERT_NE(sibling_tid, child_pid);
  ASSERT_TRUE(WIFSTOPPED(status) && WSTOPSIG(status) == SIGSTOP);
  ASSERT_THAT(ptrace(PTRACE_SETOPTIONS, sibling_tid, nullptr, PTRACE_O_TRACEEXIT),
              SyscallSucceeds());

  // Wait for the leader thread to finish exiting.
  if (test_helper::IsStarnix()) {
    // TODO(https://fxbug.dev/365540067): Use `WaitUntilZombie` once Starnix retains the exited
    // leader's `/proc/<pid>/task/<pid>` entry until the thread group exits.
    auto leader_stat_path = std::format("/proc/{}/task/{}/stat", child_pid, child_pid);
    while (files::IsFile(leader_stat_path)) {
      usleep(1000);
    }
  } else {
    ASSERT_TRUE(test_helper::WaitUntilZombie(child_pid));
  }

  // Resume the sibling thread so it initiates group exit and stops at PTRACE_EVENT_EXIT before
  // tearing down the shared address space.
  ASSERT_THAT(ptrace(PTRACE_CONT, sibling_tid, nullptr, 0), SyscallSucceeds());
  ASSERT_THAT(waitpid(sibling_tid, &status, __WALL), SyscallSucceedsWithValue(sibling_tid));
  ASSERT_TRUE(WIFSTOPPED(status));
  ASSERT_EQ(status >> 8, (SIGTRAP | (PTRACE_EVENT_EXIT << 8)));

  EXPECT_THAT(DoProcessMRelease(child_pidfd.get()), SyscallSucceeds());

  ASSERT_THAT(ptrace(PTRACE_DETACH, sibling_tid, nullptr, 0), SyscallSucceeds());
  ASSERT_TRUE(fork_helper.WaitForChildren());
}

TEST_F(ProcessMemoryReleaseTest, ProcDirectoryFdRejected) {
  test_helper::ForkHelper fork_helper;
  fork_helper.ExpectSignal(SIGKILL);
  pid_t child_pid = fork_helper.RunInForkedProcess([]() { pause(); });
  auto proc_path = std::format("/proc/{}", child_pid);
  fbl::unique_fd proc_fd(open(proc_path.c_str(), O_RDONLY | O_DIRECTORY));
  ASSERT_THAT(proc_fd.get(), SyscallSucceeds());

  ASSERT_THAT(kill(child_pid, SIGKILL), SyscallSucceeds());
  EXPECT_THAT(DoProcessMRelease(proc_fd.get()), SyscallFailsWithErrno(EBADF));

  ASSERT_TRUE(fork_helper.WaitForChildren());
}

TEST_F(ProcessMemoryReleaseTest, VmsplicePreservesPipePayload) {
  test_helper::ScopedPipe data_pipe;

  constexpr char kPayload = 'V';
  const size_t page_size = sysconf(_SC_PAGE_SIZE);
  ASSERT_GT(page_size, 0u);

  test_helper::ForkHelper fork_helper;
  pid_t child_pid = fork_helper.RunInForkedProcess([&data_pipe, page_size]() {
    data_pipe.ReadSide().reset();
    auto mapping = ASSERT_RESULT_SUCCESS_AND_RETURN(test_helper::ScopedMMap::MMap(
        nullptr, page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    memset(mapping.mapping(), kPayload, page_size);
    iovec iov = {
        .iov_base = mapping.mapping(),
        .iov_len = page_size,
    };
    ASSERT_THAT(vmsplice(data_pipe.WriteSide().get(), &iov, 1, 0),
                SyscallSucceedsWithValue(static_cast<ssize_t>(page_size)));
    ASSERT_THAT(ptrace(PTRACE_TRACEME, 0, nullptr, nullptr), SyscallSucceeds());
    raise(SIGSTOP);
    _exit(0);
  });
  data_pipe.WriteSide().reset();

  int status = 0;
  ASSERT_THAT(waitpid(child_pid, &status, 0), SyscallSucceedsWithValue(child_pid));
  ASSERT_TRUE(WIFSTOPPED(status) && WSTOPSIG(status) == SIGSTOP);
  ASSERT_THAT(ptrace(PTRACE_SETOPTIONS, child_pid, nullptr, PTRACE_O_TRACEEXIT), SyscallSucceeds());

  fbl::unique_fd child_pidfd(DoPidFdOpen(child_pid));
  ASSERT_THAT(child_pidfd.get(), SyscallSucceeds());

  // Hold the child in PTRACE_EVENT_EXIT so process_mrelease(2) deterministically reaps the
  // address space while the vmspliced mapping is still present.
  ASSERT_THAT(ptrace(PTRACE_CONT, child_pid, nullptr, 0), SyscallSucceeds());
  ASSERT_THAT(waitpid(child_pid, &status, 0), SyscallSucceedsWithValue(child_pid));
  ASSERT_TRUE(WIFSTOPPED(status));
  ASSERT_EQ(status >> 8, (SIGTRAP | (PTRACE_EVENT_EXIT << 8)));

  EXPECT_THAT(DoProcessMRelease(child_pidfd.get()), SyscallSucceeds());

  std::vector<char> read_buf(page_size);
  ASSERT_THAT(read(data_pipe.ReadSide().get(), read_buf.data(), read_buf.size()),
              SyscallSucceedsWithValue(static_cast<ssize_t>(page_size)));
  EXPECT_THAT(read_buf, testing::Each(kPayload));

  ASSERT_THAT(ptrace(PTRACE_CONT, child_pid, nullptr, 0), SyscallSucceeds());
  ASSERT_TRUE(fork_helper.WaitForChildren());
}

TEST_F(ProcessMemoryReleaseTest, ReleaseDuringPtraceEventExit) {
  test_helper::ForkHelper fork_helper;
  pid_t child_pid = fork_helper.RunInForkedProcess([]() {
    test_helper::Rendezvous thread_ready = test_helper::MakeRendezvous();
    test_helper::Rendezvous hold_sibling = test_helper::MakeRendezvous();
    std::thread sibling([ready = std::move(thread_ready.poker),
                         sibling_holder = std::move(hold_sibling.holder)]() mutable {
      ready.poke();
      sibling_holder.hold();
    });
    sibling.detach();
    thread_ready.holder.hold();

    ASSERT_THAT(ptrace(PTRACE_TRACEME, 0, nullptr, nullptr), SyscallSucceeds());
    raise(SIGSTOP);
    _exit(0);
  });

  int status = 0;
  ASSERT_THAT(waitpid(child_pid, &status, 0), SyscallSucceedsWithValue(child_pid));
  ASSERT_TRUE(WIFSTOPPED(status) && WSTOPSIG(status) == SIGSTOP);

  ASSERT_THAT(ptrace(PTRACE_SETOPTIONS, child_pid, nullptr, PTRACE_O_TRACEEXIT), SyscallSucceeds());
  fbl::unique_fd child_pidfd(DoPidFdOpen(child_pid));
  ASSERT_THAT(child_pidfd.get(), SyscallSucceeds());

  ASSERT_THAT(ptrace(PTRACE_CONT, child_pid, nullptr, 0), SyscallSucceeds());
  ASSERT_THAT(waitpid(child_pid, &status, 0), SyscallSucceedsWithValue(child_pid));
  ASSERT_TRUE(WIFSTOPPED(status));
  ASSERT_EQ(status >> 8, (SIGTRAP | (PTRACE_EVENT_EXIT << 8)));

  EXPECT_THAT(DoProcessMRelease(child_pidfd.get()), SyscallSucceeds());

  ASSERT_THAT(ptrace(PTRACE_CONT, child_pid, nullptr, 0), SyscallSucceeds());
  ASSERT_TRUE(fork_helper.WaitForChildren());
}

TEST_F(ProcessMemoryReleaseTest, SuccessfulRelease) {
  size_t page_size = sysconf(_SC_PAGE_SIZE);
  ASSERT_GT(page_size, static_cast<size_t>(0))
      << "sysconf(_SC_PAGE_SIZE) failed or returned invalid size";
  size_t num_pages = kMemSizeBytes / page_size;

  // Create a pipe to share memory pointer
  int pipe_fds[2];
  ASSERT_THAT(pipe(pipe_fds), SyscallSucceeds());
  int read_fd = pipe_fds[0];
  int write_fd = pipe_fds[1];

  // Using fork() instead of test_helper::ForkHelper to check details on the wait.
  pid_t child_pid = fork();

  if (child_pid == 0) {
    // Child process
    close(read_fd);
    // Allocate memory
    auto mem = test_helper::ScopedMMap::MMap(nullptr, kMemSizeBytes, PROT_READ | PROT_WRITE,
                                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mem.is_error()) {
      perror("child: mmap failed");
      close(write_fd);  // Ensure pipe is closed before exiting.
      _exit(EXIT_FAILURE);
    }

    // Touch memory to ensure pages are faulted in
    volatile char* p = static_cast<volatile char*>(mem->mapping());
    for (size_t i = 0; i < num_pages; ++i) {
      p[i * page_size] = static_cast<char>(1 + (i % 255));
    }
    ASSERT_THAT(dprintf(write_fd, "%p", (void*)p), SyscallSucceeds());
    close(write_fd);

    // Wait the parent catches the WSTOPSIG and sends back SIGCONT
    raise(SIGSTOP);

    _exit(EXIT_SUCCESS);  // Should ideally not be reached if killed.
  }

  // Parent process
  close(write_fd);

  // Attach to the child with ptrace and wait for PTRACE_O_TRACEEXIT events.
  ASSERT_THAT(ptrace(PTRACE_SEIZE, child_pid, 0, PTRACE_O_TRACEEXIT), SyscallSucceeds())
      << "ptrace PTRACE_SEIZE failed: " << strerror(errno);

  // Open a pidfd for the child process.
  int child_pidfd = DoPidFdOpen(child_pid);
  ASSERT_THAT(child_pidfd, SyscallSucceeds());

  int status;
  // Sync with child when the child has completed the memory allocation
  ASSERT_EQ(child_pid, waitpid(child_pid, &status, 0));
  ASSERT_TRUE(WIFSTOPPED(status) && WSTOPSIG(status) == SIGSTOP)
      << "status = " << status << " WIFSTOPPED = " << WIFSTOPPED(status)
      << " WSTOPSIG = " << WSTOPSIG(status);
  ASSERT_EQ(0, ptrace(PTRACE_CONT, child_pid, 0, 0));

  // Read the memory pointer
  char pipe_buffer[64];
  void* restored_ptr = nullptr;
  ssize_t bytes_read = read(read_fd, pipe_buffer, sizeof(pipe_buffer));
  close(read_fd);
  ASSERT_GT(bytes_read, 0);
  std::stringstream ss_read;
  ss_read << std::hex;      // Set stream to interpret input as hexadecimal
  ss_read << pipe_buffer;   // Put the string buffer into the stringstream
  ss_read >> restored_ptr;  // Read the pointer directly
  std::cout << "parent ptr: " << restored_ptr << "\n";

  // Use process_vm_readv to check the memory before release.
  std::vector<char> buffer(kMemSizeBytes);
  struct iovec local_iov = {.iov_base = buffer.data(), .iov_len = kMemSizeBytes};
  struct iovec remote_iov = {.iov_base = restored_ptr, .iov_len = kMemSizeBytes};
  ASSERT_THAT(process_vm_readv(child_pid, &local_iov, 1, &remote_iov, 1, 0),
              SyscallSucceedsWithValue(kMemSizeBytes));
  for (size_t i = 0; i < num_pages; ++i) {
    for (size_t j = 0; j < page_size; ++j) {
      char expected = (j == 0) ? static_cast<char>(1 + (i % 255)) : 0;
      ASSERT_EQ(buffer[i * page_size + j], expected)
          << "Memory at offset " << (i * page_size + j) << " does not match expected value";
    }
  }

  // Wait for the PTRACE_EVENT_EXIT
  ASSERT_THAT(waitpid(child_pid, &status, 0), SyscallSucceedsWithValue(child_pid))
      << "waitpid for PTRACE_EVENT_EXIT failed: " << strerror(errno);
  ASSERT_FALSE(WIFEXITED(status)) << "Child not stopped for PTRACE_EVENT_EXIT";
  ASSERT_EQ(status >> 8, (SIGTRAP | (PTRACE_EVENT_EXIT << 8)))
      << "Child not stopped with PTRACE_EVENT_EXIT";

  // Verify the RSS size decreases 1MB and `maps` info has no change after memory is released.
  auto msize = GetProcessRSSMemorySize(child_pid);
  auto maps_str = GetMapsString(child_pid);
  EXPECT_THAT(DoProcessMRelease(child_pidfd), SyscallSucceeds());
  EXPECT_GT(msize, GetProcessRSSMemorySize(child_pid) + 1024);
  EXPECT_EQ(maps_str, GetMapsString(child_pid));

  // Wait for the child process to terminate and check its status.
  ASSERT_THAT(ptrace(PTRACE_CONT, child_pid, 0, 0), SyscallSucceeds());
  ASSERT_THAT(waitpid(child_pid, &status, 0), SyscallSucceeds());
  ASSERT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}
