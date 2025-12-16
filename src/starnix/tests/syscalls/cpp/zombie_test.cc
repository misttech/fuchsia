// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/syscall.h>
#include <unistd.h>

#include <thread>

#include <gtest/gtest.h>

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

  test_helper::ForkHelper fork_helper_;
  test_helper::Poker complete_;
  pid_t leader_pid_;
};

// Check the status fields of a zombie task.
TEST_F(ZombieProcTest, ZombieStatus) {
  std::string status;
  ASSERT_TRUE(ReadZombieProcFile("status", status));
  ASSERT_THAT(status, testing::ContainsRegex("\nState:[[:space:]]+Z \\(zombie\\)\n"));
}

// Check that the memory map of a zombie task is empty.
TEST_F(ZombieProcTest, ZombieMapsEmpty) {
  std::string maps;
  ASSERT_TRUE(ReadZombieProcFile("maps", maps));
  ASSERT_TRUE(maps.empty());
}

}  // namespace
