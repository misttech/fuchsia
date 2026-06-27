// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fit/defer.h>
#include <signal.h>
#include <sys/poll.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <thread>

#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <linux/sched.h>

#include "src/lib/files/file.h"
#include "src/lib/fxl/strings/string_number_conversions.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

// As of this writing, our sysroot's syscall.h lacks the SYS_clone3 definition.
#ifndef SYS_clone3
#if defined(__aarch64__) || defined(__x86_64__) || defined(__riscv) || defined(__arm__)
#define SYS_clone3 435
#else
#error SYS_clone3 needs a definition for this architecture.
#endif
#endif

#ifndef SYS_pidfd_send_signal
#if defined(__aarch64__) || defined(__x86_64__) || defined(__riscv) || defined(__arm__)
#define SYS_pidfd_send_signal 424
#else
#error SYS_pidfd_send_signal needs a definition for this architecture.
#endif
#endif

pid_t ForkUsingClone3(const clone_args* cl_args, size_t size) {
  return static_cast<pid_t>(syscall(SYS_clone3, cl_args, size));
}

// Our Linux sysroot doesn't seem to have pidfd_open() and gettid().
fbl::unique_fd DoPidFdOpen(pid_t pid) {
  return fbl::unique_fd(static_cast<int>(syscall(SYS_pidfd_open, pid, 0u)));
}
pid_t DoGetTid() { return static_cast<pid_t>(syscall(SYS_gettid)); }

// Returns (readable_end, writable_end).
std::pair<fbl::unique_fd, fbl::unique_fd> CreatePipe() {
  int pipe_fds[2];
  SAFE_SYSCALL(pipe(pipe_fds));
  return {fbl::unique_fd(pipe_fds[0]), fbl::unique_fd(pipe_fds[1])};
}

TEST(PidFdTest, ProcessCanBeOpened) {
  auto pid_fd = DoPidFdOpen(getpid());
  ASSERT_TRUE(pid_fd.is_valid()) << strerror(errno);
}

TEST(PidFdTest, ThreadCannotBeOpened) {
  std::thread([] {
    auto pid_fd = DoPidFdOpen(DoGetTid());
    ASSERT_FALSE(pid_fd.is_valid());
    EXPECT_EQ(errno, EINVAL);
  }).join();
}

TEST(PidFdTest, CanPollProcessExit) {
  // Create a pipe that will be used to ask the child process to exit.
  auto [r_fd, w_fd] = CreatePipe();

  test_helper::ForkHelper helper;
  pid_t pid = helper.RunInForkedProcess([&r_fd, &w_fd] {
    w_fd.reset();

    // Wait for the readable end to signal the end of the stream.
    char buf;
    read(r_fd.get(), &buf, 1);

    _exit(0);
  });

  r_fd.reset();

  auto pid_fd = DoPidFdOpen(pid);
  ASSERT_TRUE(pid_fd.is_valid()) << strerror(errno);

  // Verify that poll does not return POLLIN while the process is running.
  pollfd pfd = {.fd = pid_fd.get(), .events = POLLIN};
  ASSERT_EQ(poll(&pfd, 1, 0), 0);

  // Verify that poll returns POLLIN when the process stops running.
  {
    // Do not let SIGCHLD interrupt our poll() call.
    test_helper::SignalMaskHelper signal_mask_helper;
    signal_mask_helper.blockSignal(SIGCHLD);
    auto restorer = fit::defer([&]() { signal_mask_helper.restoreSigmask(); });

    w_fd.reset();
    ASSERT_EQ(poll(&pfd, 1, -1), 1);
    EXPECT_EQ(pfd.revents, POLLIN);
  }

  // Verify that poll returns POLLIN even if the wait starts after the process has exited.
  ASSERT_EQ(HANDLE_EINTR(poll(&pfd, 1, -1)), 1);
  EXPECT_EQ(pfd.revents, POLLIN);

  pid_fd.reset();
  ASSERT_TRUE(helper.WaitForChildren());
}

TEST(PidFdTest, PollWaitsForSecondaryThreadsToo) {
  // Create a pipe that will be used to ask the child process to exit.
  auto [r_fd, w_fd] = CreatePipe();

  test_helper::ForkHelper helper;
  pid_t pid = helper.RunInForkedProcess([&r_fd, &w_fd] {
    w_fd.reset();

    std::thread([&r_fd] {
      // Wait for the readable end to signal the end of the stream.
      char buf;
      read(r_fd.get(), &buf, 1);
    }).detach();

    // Immediately exit the main thread, leaving the secondary thread running.
    syscall(SYS_exit, 0);
  });

  r_fd.reset();

  // Wait for the main thread to exit.
  ASSERT_TRUE(test_helper::WaitUntilZombie(pid));

  // Open a pidfd using the main thread's pid.
  auto pid_fd = DoPidFdOpen(pid);
  ASSERT_TRUE(pid_fd.is_valid()) << strerror(errno);

  // Verify that poll does not return POLLIN even after the main thread exited,
  // if a secondary thread is still running.
  pollfd pfd = {.fd = pid_fd.get(), .events = POLLIN};
  ASSERT_EQ(poll(&pfd, 1, 500), 0);

  // Verify that poll returns POLLIN when the secondary thread stops running.
  w_fd.reset();
  ASSERT_EQ(HANDLE_EINTR(poll(&pfd, 1, -1)), 1);
  EXPECT_EQ(pfd.revents, POLLIN);

  pid_fd.reset();
  ASSERT_TRUE(helper.WaitForChildren());
}

TEST(PidFdTest, PidFdOpenAfterZombification) {
  struct clone_args ca;
  bzero(&ca, sizeof(ca));

  ca.flags = CLONE_PIDFD;
  ca.exit_signal = SIGCHLD;  // Needed in order to wait on the child.

  // Ask for a PID FD through which the child process can be observed.
  fbl::unique_fd pid_fd;
  ca.pidfd = reinterpret_cast<uint64_t>(pid_fd.reset_and_get_address());

  auto child_pid = ForkUsingClone3(&ca, sizeof(ca));
  ASSERT_NE(child_pid, -1);
  if (child_pid == 0) {
    exit(0);
  } else {
    ASSERT_TRUE(pid_fd.is_valid());

    // Use the `pid_fd` to wait for the child to exit, becoming a zombie.
    pollfd pfd{.fd = pid_fd.get(), .events = POLLIN};
    ASSERT_THAT(HANDLE_EINTR(poll(&pfd, 1, -1)), SyscallSucceedsWithValue(1));

    // Connect a new PID-FD, which should be immediately in the signalled state.
    auto new_pid_fd = DoPidFdOpen(child_pid);
    ASSERT_TRUE(new_pid_fd.is_valid()) << strerror(errno);
    pfd = {.fd = new_pid_fd.get(), .events = POLLIN};
    EXPECT_THAT(HANDLE_EINTR(poll(&pfd, 1, 0)), SyscallSucceedsWithValue(1));

    // Now reap the zombie child process.
    int wait_status = 0;
    pid_t wait_result = HANDLE_EINTR(waitpid(child_pid, &wait_status, 0));
    EXPECT_THAT(wait_result, SyscallSucceedsWithValue(child_pid));
  }
}

TEST(PidFdTest, PidFdSendSignal) {
  fbl::unique_fd pid_fd(DoPidFdOpen(getpid()));
  ASSERT_TRUE(pid_fd.is_valid()) << strerror(errno);

  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGTERM);
  ASSERT_EQ(sigprocmask(SIG_BLOCK, &mask, nullptr), 0);

  ASSERT_THAT(syscall(SYS_pidfd_send_signal, pid_fd.get(), SIGTERM, nullptr, 0), SyscallSucceeds());

  siginfo_t info;
  int sig = sigwaitinfo(&mask, &info);
  ASSERT_EQ(sig, SIGTERM);
  ASSERT_EQ(info.si_signo, SIGTERM);
  ASSERT_EQ(info.si_code, SI_USER);

  sigprocmask(SIG_UNBLOCK, &mask, nullptr);
}

TEST(PidFdTest, PidFdSendSiginfo) {
  fbl::unique_fd pid_fd(DoPidFdOpen(getpid()));
  ASSERT_TRUE(pid_fd.is_valid()) << strerror(errno);

  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGTERM);
  ASSERT_EQ(sigprocmask(SIG_BLOCK, &mask, nullptr), 0);

  siginfo_t info_send = {};
  info_send.si_signo = SIGTERM;
  info_send.si_code = SI_USER;
  ASSERT_THAT(syscall(SYS_pidfd_send_signal, pid_fd.get(), SIGTERM, &info_send, 0),
              SyscallSucceeds());

  siginfo_t info_recv;
  int sig = sigwaitinfo(&mask, &info_recv);
  ASSERT_EQ(sig, SIGTERM);
  ASSERT_EQ(info_recv.si_signo, SIGTERM);
  ASSERT_EQ(info_recv.si_code, SI_USER);

  sigprocmask(SIG_UNBLOCK, &mask, nullptr);
}

TEST(PidFdTest, PidFdSendSiginfoMismatchedSignoFails) {
  fbl::unique_fd pid_fd(DoPidFdOpen(getpid()));
  ASSERT_TRUE(pid_fd.is_valid()) << strerror(errno);

  siginfo_t info_send = {};
  info_send.si_signo = SIGUSR1;  // signo is different from the signal number passed to the syscall
  info_send.si_code = SI_USER;
  ASSERT_THAT(syscall(SYS_pidfd_send_signal, pid_fd.get(), SIGTERM, &info_send, 0),
              SyscallFailsWithErrno(EINVAL));

  info_send.si_signo = 0;
  ASSERT_THAT(syscall(SYS_pidfd_send_signal, pid_fd.get(), SIGTERM, &info_send, 0),
              SyscallFailsWithErrno(EINVAL));
}

}  // namespace
