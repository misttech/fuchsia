// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "third_party/android/platform/bionic/libc/kernel/uapi/linux/bpf.h"

#include <sys/mman.h>
#include <syscall.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/util.h"

namespace {

int bpf(int cmd, union bpf_attr* attr) { return (int)syscall(__NR_bpf, cmd, attr, sizeof(*attr)); }

const char kNoRightsLabel[] = "test_u:test_r:bpf_map_none_t:s0";
const char kReadLabel[] = "test_u:test_r:bpf_map_read_t:s0";
const char kWriteLabel[] = "test_u:test_r:bpf_map_write_t:s0";
const char kReadWriteLabel[] = "test_u:test_r:bpf_map_read_write_t:s0";

class BpfMapTest : public ::testing::TestWithParam<std::tuple<const char*, int>> {};

TEST_P(BpfMapTest, Map) {
  auto [label, map_flags] = GetParam();
  bool should_succeed = label == kReadWriteLabel;
  auto enforce = ScopedEnforcement::SetEnforcing();
  // Create a mappable array map.
  bpf_attr attr = {
      .map_type = BPF_MAP_TYPE_ARRAY,
      .key_size = sizeof(int),
      .value_size = static_cast<uint32_t>(getpagesize()),
      .max_entries = 1,
      .map_flags = BPF_F_MMAPABLE,
  };

  fbl::unique_fd mappable_map_fd(bpf(BPF_MAP_CREATE, &attr));
  ASSERT_TRUE(mappable_map_fd.is_valid()) << strerror(errno);

  ASSERT_TRUE(RunSubprocessAs(label, [&] {
    if (should_succeed) {
      EXPECT_THAT(reinterpret_cast<intptr_t>(mmap(nullptr, getpagesize(), map_flags, MAP_SHARED,
                                                  mappable_map_fd.get(), 0)),
                  SyscallSucceeds());
    } else {
      EXPECT_THAT(reinterpret_cast<uintptr_t>(mmap(nullptr, getpagesize(), map_flags, MAP_SHARED,
                                                   mappable_map_fd.get(), 0)),
                  SyscallFailsWithErrno(EACCES));
    }
  }));
}

INSTANTIATE_TEST_SUITE_P(BpfMapTestSuite, BpfMapTest,
                         ::testing::Combine(::testing::Values(kNoRightsLabel, kReadLabel,
                                                              kWriteLabel, kReadWriteLabel),
                                            ::testing::Values(PROT_NONE, PROT_READ, PROT_WRITE,
                                                              PROT_READ | PROT_WRITE)));

}  // namespace

extern std::string DoPrePolicyLoadWork() { return "bpf_policy.pp"; }
