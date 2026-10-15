// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include <err.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

#include <print>

#include <linux/capability.h>
#include <linux/prctl.h>

namespace {

bool HasCapabilityAmbient(int cap) {
  int res = prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, cap, 0, 0);
  if (res == -1) {
    err(EXIT_FAILURE, "prctl PR_CAP_AMBIENT");
  }

  return res == 1;
}

bool HasCapabilityBounding(int cap) {
  int res = prctl(PR_CAPBSET_READ, cap, 0, 0, 0);
  if (res == -1) {
    err(EXIT_FAILURE, "prctl PR_CAPBSET_READ");
  }

  return res == 1;
}

int b2d(bool value) { return value ? 1 : 0; }

int MaxCapabilitySupported() {
  FILE* fp = fopen("/proc/sys/kernel/cap_last_cap", "r");
  if (fp == nullptr) {
    err(EXIT_FAILURE, "opening cap_last_cap");
  }
  int cap_num = -1;
  int n = fscanf(fp, "%d\n", &cap_num);
  fclose(fp);

  if (n != 1) {
    std::println(stderr, "Could not parse cap_last_cap file.");
    exit(EXIT_FAILURE);
  }
  return cap_num;
}

void PrintCapabilities() {
  __user_cap_header_struct header = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = 0};
  __user_cap_data_struct caps[_LINUX_CAPABILITY_U32S_3] = {};
  if (syscall(SYS_capget, &header, &caps) != 0) {
    err(EXIT_FAILURE, "capget");
  }

  std::println(stdout, "CAP_NUM,EFFECTIVE,PERMITTED,INHERITABLE,BOUNDING,AMBIENT");

  const int cap_last_cap = MaxCapabilitySupported();
  for (int capability = 0; capability <= cap_last_cap; capability++) {
    const auto& data = caps[CAP_TO_INDEX(capability)];
    const uint32_t mask = CAP_TO_MASK(capability);
    std::println(stdout, "{},{},{},{},{},{}", capability, b2d((data.effective & mask) != 0),
                 b2d((data.permitted & mask) != 0), b2d((data.inheritable & mask) != 0),
                 b2d(HasCapabilityBounding(capability)), b2d(HasCapabilityAmbient(capability)));
  }
}

void PrintSecurebits() {
  int res = prctl(PR_GET_SECUREBITS);
  if (res == -1) {
    err(EXIT_FAILURE, "prctl(PR_GET_SECUREBITS)");
  }

  std::println(stdout, "{:x}", res);
}

void PrintPDeathSig() {
  int pdeathsig = -1;
  if (prctl(PR_GET_PDEATHSIG, &pdeathsig) == -1) {
    err(EXIT_FAILURE, "prctl(PR_GET_PDEATHSIG)");
  }

  std::println(stdout, "{}", pdeathsig);
}

void PrintAtSecure() { std::println(stdout, "{}", getauxval(AT_SECURE)); }

void PrintUids() {
  uid_t ruid = 0, euid = 0, suid = 0;
  if (getresuid(&ruid, &euid, &suid) != 0) {
    err(EXIT_FAILURE, "getresuid");
  }
  std::println(stdout, "{},{},{}", ruid, euid, suid);
}

}  // namespace

int main(int argc, char** argv) {
  if (argc == 0) {
    exit(EXIT_FAILURE);
  }

  if (argc != 2) {
    std::println(stderr, "Usage: {} <command>", argv[0]);
    std::println(stderr, "commands:");
    std::println(stderr, "\tcapabilities: print the capability sets");
    std::println(stderr, "\tsecurebits: print the securebits flags");
    std::println(stderr, "\tpdeathsig: print the parent-death signal number");
    std::println(stderr, "\tat_secure: print the AT_SECURE auxv value");
    std::println(stderr, "\tuids: print the real, effective, and saved user IDs");
    exit(EXIT_FAILURE);
  }

  if (strcmp(argv[1], "securebits") == 0) {
    PrintSecurebits();
  } else if (strcmp(argv[1], "capabilities") == 0) {
    PrintCapabilities();
  } else if (strcmp(argv[1], "pdeathsig") == 0) {
    PrintPDeathSig();
  } else if (strcmp(argv[1], "at_secure") == 0) {
    PrintAtSecure();
  } else if (strcmp(argv[1], "uids") == 0) {
    PrintUids();
  } else {
    std::println(stderr, "Invalid command: {}", argv[1]);
    exit(EXIT_FAILURE);
  }

  return 0;
}
