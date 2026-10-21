// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Usage: crash_during_shutdown_bin DURATION_MS
//
// Crashes when asked to exit during shutdown, and takes DURATION_MS to handle the crash, like
// debuggerd forking crash_dump64 to write a tombstone.

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#include <print>

namespace {

useconds_t g_duration_us;

void Log(const char* msg) { write(STDOUT_FILENO, msg, strlen(msg)); }

void HandleSigsegv(int) {
  pid_t helper = fork();
  if (helper == 0) {
    usleep(g_duration_us);
    _exit(0);
  }
  if (helper < 0) {
    Log("[CRASH_HELPER_FORK_FAILED]\n");
    _exit(1);
  }
  int status;
  pid_t result;
  do {
    result = waitpid(helper, &status, 0);
  } while (result < 0 && errno == EINTR);
  if (result != helper || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    Log("[CRASH_HELPER_FAILED]\n");
    _exit(1);
  }
  Log("[CRASH_HANDLER_DONE]\n");
  _exit(0);
}

void HandleSigterm(int) { raise(SIGSEGV); }

}  // namespace

int main(int argc, char** argv) {
  if (argc != 2) {
    return 1;
  }
  g_duration_us = static_cast<useconds_t>(strtol(argv[1], nullptr, 10)) * 1000;
  signal(SIGSEGV, HandleSigsegv);
  signal(SIGTERM, HandleSigterm);
  std::println("[CRASHER_READY]");
  fflush(stdout);
  while (true) {
    pause();
  }
}
