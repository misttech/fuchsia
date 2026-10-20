// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Usage: exit_on_sigterm_bin NAME DELAY_MS
//
// Prints [NAME_READY], then waits for SIGTERM. Exits DELAY_MS after it, printing [NAME_EXITING].

#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#include <print>

int main(int argc, char** argv) {
  if (argc != 3) {
    return 1;
  }
  sigset_t sigterm;
  sigemptyset(&sigterm);
  sigaddset(&sigterm, SIGTERM);
  sigprocmask(SIG_BLOCK, &sigterm, nullptr);
  std::println("[{}_READY]", argv[1]);
  fflush(stdout);

  int signal;
  sigwait(&sigterm, &signal);
  usleep(static_cast<useconds_t>(strtol(argv[2], nullptr, 10)) * 1000);
  std::println("[{}_EXITING]", argv[1]);
  fflush(stdout);
  return 0;
}
