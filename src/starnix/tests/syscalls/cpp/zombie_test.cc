// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <thread>

#include <gtest/gtest.h>

#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/fxl/strings/string_printf.h"
#include "src/starnix/tests/syscalls/cpp/proc_test_base.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

std::string ProcFilePath(pid_t pid, const char* name) {
  return fxl::StringPrintf("/proc/%d/%s", pid, name);
}

// Waits until the given task enters the zombie state.
void WaitUntilTaskIsZombie(pid_t pid) {
  std::string stat_path = ProcFilePath(pid, "stat");

  while (true) {
    std::string contents;
    ASSERT_TRUE(files::ReadFileToString(stat_path, &contents));

    char state;
    ASSERT_EQ(sscanf(contents.c_str(), "%*d %*s %c", &state), 1) << contents;
    if (state == 'Z') {
      return;  // Thread is a zombie.
    }

    usleep(10000);  // Check again in 10 ms.
  }
}

class ZombieProcTest : public ProcTestBase {
 protected:
  void SetUp() override {
    ProcTestBase::SetUp();
    SpawnZombie();
  }

  void TearDown() override {
    ReapZombie();
    ProcTestBase::TearDown();
  }

  void SpawnZombie() {
    test_helper::Rendezvous ready = test_helper::MakeRendezvous();
    test_helper::Rendezvous complete = test_helper::MakeRendezvous();
    leader_pid_ = fork_helper_.RunInForkedProcess(
        [ready = std::move(ready.poker), complete = std::move(complete.holder)]() mutable {
          // Spawn a control thread. This thread will block until the test signals that it is done
          // inspecting the zombie task. At that point, the control thread will exit, causing the
          // entire forked thread group to exit.
          std::thread([&] { complete.hold(); }).detach();

          // Signal to the test that the leader is entering the zombie state. Then, exit the leader
          // thread while the control thread blocks, keeping the forked thread group alive.
          ready.poke();
          syscall(SYS_exit, 0);
        });

    // Wait for the leader thread to exit, entering the zombie state.
    complete_ = std::move(complete.poker);
    ready.holder.hold();
    ASSERT_NO_FATAL_FAILURE(WaitUntilTaskIsZombie(leader_pid_));
  }

  void ReapZombie() {
    // Signal for the forked thread group to exit.
    complete_.poke();
    ASSERT_TRUE(fork_helper_.WaitForChildren());
  }

  bool ReadZombieProcFile(const char* name, std::string& contents) const {
    std::string path = ProcFilePath(leader_pid_, name);
    return files::ReadFileToString(path, &contents);
  }

  void AssertZombieProcFileOpenError(const char* name, int expected_errno) const {
    std::string path = ProcFilePath(leader_pid_, name);
    fbl::unique_fd fd(open(path.c_str(), O_RDONLY));
    // Being able to open the file is an error. For debugging purposes, log the contents of the file
    // if it can be opened.
    if (fd) {
      std::string contents;
      if (files::ReadFileDescriptorToString(fd.get(), &contents)) {
        GTEST_FAIL() << path << " could be opened, contents: " << contents;
      }
      GTEST_FAIL() << path << " could be opened, but could not be read";
    }
    ASSERT_FALSE(fd);
    ASSERT_EQ(errno, expected_errno);
  }

  void AssertZombieProcFileReadError(const char* name, int expected_errno) const {
    std::string path = ProcFilePath(leader_pid_, name);
    fbl::unique_fd fd(open(path.c_str(), O_RDONLY));
    ASSERT_TRUE(fd);

    char buf[1];
    ssize_t res = read(fd.get(), buf, sizeof(buf));
    ASSERT_LT(res, 0);
    ASSERT_EQ(errno, expected_errno);
  }

  void AssertZombieProcFileEmpty(const char* name) const {
    std::string contents;
    ASSERT_TRUE(ReadZombieProcFile(name, contents));
    ASSERT_EQ(contents, "");
  }

  void AssertZombieProcFileNotEmpty(const char* name) const {
    std::string contents;
    ASSERT_TRUE(ReadZombieProcFile(name, contents));
    ASSERT_FALSE(contents.empty());
  }

  void AssertZombieProcDirEmpty(const char* name) const {
    std::string path = ProcFilePath(leader_pid_, name);
    std::vector<std::string> contents;
    ASSERT_TRUE(files::ReadDirContents(path, &contents));
    ASSERT_EQ(contents, (std::vector<std::string>{".", ".."}));
  }

  void AssertZombieProcDirNotEmpty(const char* name) const {
    std::string path = ProcFilePath(leader_pid_, name);
    std::vector<std::string> contents;
    ASSERT_TRUE(files::ReadDirContents(path, &contents));
    ASSERT_NE(contents, (std::vector<std::string>{".", ".."}));
  }

  test_helper::ForkHelper fork_helper_;
  test_helper::Poker complete_;
  pid_t leader_pid_;
};

// Nodes that fail with ENOENT on open()
TEST_F(ZombieProcTest, Cwd) { AssertZombieProcFileOpenError("cwd", ENOENT); }
TEST_F(ZombieProcTest, Exe) { AssertZombieProcFileOpenError("exe", ENOENT); }
TEST_F(ZombieProcTest, Root) { AssertZombieProcFileOpenError("root", ENOENT); }

// Nodes that fail with EINVAL on open()
TEST_F(ZombieProcTest, MountInfo) { AssertZombieProcFileOpenError("mountinfo", EINVAL); }
TEST_F(ZombieProcTest, Mounts) { AssertZombieProcFileOpenError("mounts", EINVAL); }

// Nodes that succeed on open() but fail with EINVAL on read()
TEST_F(ZombieProcTest, AttrExec) { AssertZombieProcFileReadError("attr/exec", EINVAL); }
TEST_F(ZombieProcTest, AttrFscreate) { AssertZombieProcFileReadError("attr/fscreate", EINVAL); }
TEST_F(ZombieProcTest, AttrKeycreate) { AssertZombieProcFileReadError("attr/keycreate", EINVAL); }
TEST_F(ZombieProcTest, AttrPrev) { AssertZombieProcFileReadError("attr/prev", EINVAL); }
TEST_F(ZombieProcTest, AttrSockcreate) { AssertZombieProcFileReadError("attr/sockcreate", EINVAL); }
TEST_F(ZombieProcTest, ClearRefs) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Requires CAP_SYS_ADMIN";
  }
  AssertZombieProcFileReadError("clear_refs", EINVAL);
}

// Nodes that are readable and contain no data
TEST_F(ZombieProcTest, Auxv) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Requires CAP_SYS_ADMIN";
  }
  AssertZombieProcFileEmpty("auxv");
}
TEST_F(ZombieProcTest, Cmdline) { AssertZombieProcFileEmpty("cmdline"); }
TEST_F(ZombieProcTest, Environ) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Requires CAP_SYS_ADMIN";
  }
  AssertZombieProcFileEmpty("environ");
}
TEST_F(ZombieProcTest, Fd) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Requires CAP_SYS_ADMIN";
  }
  AssertZombieProcDirEmpty("fd");
}
TEST_F(ZombieProcTest, FdInfo) { AssertZombieProcDirEmpty("fdinfo"); }
TEST_F(ZombieProcTest, Maps) { AssertZombieProcFileEmpty("maps"); }
TEST_F(ZombieProcTest, Mem) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Requires CAP_SYS_ADMIN";
  }
  AssertZombieProcFileEmpty("mem");
}
TEST_F(ZombieProcTest, PageMap) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Requires CAP_SYS_ADMIN";
  }
  AssertZombieProcFileEmpty("pagemap");
}
TEST_F(ZombieProcTest, Smaps) { AssertZombieProcFileEmpty("smaps"); }

// Nodes that are readable and contain general data
TEST_F(ZombieProcTest, AttrCurrent) { AssertZombieProcFileNotEmpty("attr/current"); }
TEST_F(ZombieProcTest, Cgroup) { AssertZombieProcFileNotEmpty("cgroup"); }
TEST_F(ZombieProcTest, Comm) { AssertZombieProcFileNotEmpty("comm"); }
TEST_F(ZombieProcTest, Io) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Requires CAP_SYS_ADMIN";
  }
  AssertZombieProcFileNotEmpty("io");
}
TEST_F(ZombieProcTest, Limits) { AssertZombieProcFileNotEmpty("limits"); }
TEST_F(ZombieProcTest, Ns) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Requires CAP_SYS_ADMIN";
  }
  AssertZombieProcDirNotEmpty("ns");
}
TEST_F(ZombieProcTest, OomAdj) { AssertZombieProcFileNotEmpty("oom_adj"); }
TEST_F(ZombieProcTest, OomScore) { AssertZombieProcFileNotEmpty("oom_score"); }
TEST_F(ZombieProcTest, OomScoreAdj) { AssertZombieProcFileNotEmpty("oom_score_adj"); }
TEST_F(ZombieProcTest, Sched) { AssertZombieProcFileNotEmpty("sched"); }
TEST_F(ZombieProcTest, Schedstat) { AssertZombieProcFileNotEmpty("schedstat"); }
TEST_F(ZombieProcTest, Statm) { AssertZombieProcFileNotEmpty("statm"); }
TEST_F(ZombieProcTest, TimerslackNs) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Requires CAP_SYS_ADMIN";
  }
  AssertZombieProcFileNotEmpty("timerslack_ns");
}
TEST_F(ZombieProcTest, Wchan) { AssertZombieProcFileNotEmpty("wchan"); }

// Nodes that are readable and contain specific data
TEST_F(ZombieProcTest, Stat) {
  std::string stat;
  ASSERT_TRUE(ReadZombieProcFile("stat", stat));
  char state;
  ASSERT_EQ(sscanf(stat.c_str(), "%*d %*s %c", &state), 1) << stat;
  ASSERT_EQ(state, 'Z');
}

TEST_F(ZombieProcTest, Status) {
  std::string status;
  ASSERT_TRUE(ReadZombieProcFile("status", status));
  ASSERT_THAT(status, testing::ContainsRegex("\nState:[[:space:]]+Z \\(zombie\\)\n"));
}

}  // namespace
