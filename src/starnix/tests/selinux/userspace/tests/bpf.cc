// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "third_party/android/platform/bionic/libc/kernel/uapi/linux/bpf.h"

#include <fcntl.h>
#include <sys/mman.h>
#include <syscall.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

namespace {

int bpf(int cmd, union bpf_attr* attr) { return (int)syscall(__NR_bpf, cmd, attr, sizeof(*attr)); }

fbl::unique_fd CreateArrayMap() {
  bpf_attr attr = {
      .map_type = BPF_MAP_TYPE_ARRAY,
      .key_size = sizeof(int),
      .value_size = static_cast<uint32_t>(getpagesize()),
      .max_entries = 1,
      .map_flags = BPF_F_MMAPABLE,
  };

  return fbl::unique_fd(bpf(BPF_MAP_CREATE, &attr));
}

#define BPF_MOV_IMM(reg, value)                                                    \
  bpf_insn {                                                                       \
    .code = BPF_ALU64 | BPF_MOV | BPF_IMM, .dst_reg = reg, .src_reg = 0, .off = 0, \
    .imm = static_cast<int32_t>(value),                                            \
  }
#define BPF_RETURN() \
  bpf_insn { .code = BPF_JMP | BPF_EXIT, .dst_reg = 0, .src_reg = 0, .off = 0, .imm = 0, }

fbl::unique_fd LoadProgram() {
  bpf_insn program[] = {
      // r0 <- 1
      BPF_MOV_IMM(0, 1),
      // exit
      BPF_RETURN(),
  };

  char buffer[4096];
  union bpf_attr attr;
  memset(&attr, 0, sizeof(attr));
  attr = {
      .prog_type = BPF_PROG_TYPE_SOCKET_FILTER,
      .expected_attach_type = 0,
      .insns = reinterpret_cast<uint64_t>(program),
      .insn_cnt = static_cast<uint32_t>(sizeof(program) / sizeof(program[0])),
      .license = reinterpret_cast<uint64_t>("N/A"),
      .log_buf = reinterpret_cast<uint64_t>(buffer),
      .log_size = 4096,
      .log_level = 1,
  };

  return fbl::unique_fd(bpf(BPF_PROG_LOAD, &attr));
}

struct BpfMapTestParam {
  const char* label;
  int map_flags;
  bool should_succeed;

  BpfMapTestParam(const char* label, int map_flags, bool should_succeed)
      : label(label), map_flags(map_flags), should_succeed(should_succeed) {}
};

class BpfMapTest : public ::testing::TestWithParam<BpfMapTestParam> {};

TEST_P(BpfMapTest, Map) {
  auto [label, map_flags, should_succeed] = GetParam();
  auto enforce = ScopedEnforcement::SetEnforcing();
  // Create a mappable array map.
  fbl::unique_fd mappable_map_fd = CreateArrayMap();
  ASSERT_TRUE(mappable_map_fd.is_valid()) << strerror(errno);

  ASSERT_TRUE(RunSubprocessAs(label, [&] {
    intptr_t result = reinterpret_cast<intptr_t>(
        mmap(nullptr, getpagesize(), map_flags, MAP_SHARED, mappable_map_fd.get(), 0));
    if (should_succeed) {
      EXPECT_THAT(result, SyscallSucceeds());
    } else {
      EXPECT_THAT(result, SyscallFailsWithErrno(EACCES));
    }
  }));
}

INSTANTIATE_TEST_SUITE_P(
    BpfMapTestSuite, BpfMapTest,
    ::testing::Values(
        BpfMapTestParam("test_u:test_r:bpf_map_none_t:s0", PROT_NONE, false),
        BpfMapTestParam("test_u:test_r:bpf_map_none_t:s0", PROT_READ, false),
        BpfMapTestParam("test_u:test_r:bpf_map_none_t:s0", PROT_WRITE, false),
        BpfMapTestParam("test_u:test_r:bpf_map_none_t:s0", PROT_READ | PROT_WRITE, false),
        BpfMapTestParam("test_u:test_r:bpf_map_read_t:s0", PROT_NONE, false),
        BpfMapTestParam("test_u:test_r:bpf_map_read_t:s0", PROT_READ, false),
        BpfMapTestParam("test_u:test_r:bpf_map_read_t:s0", PROT_WRITE, false),
        BpfMapTestParam("test_u:test_r:bpf_map_read_t:s0", PROT_READ | PROT_WRITE, false),
        BpfMapTestParam("test_u:test_r:bpf_map_write_t:s0", PROT_NONE, false),
        BpfMapTestParam("test_u:test_r:bpf_map_write_t:s0", PROT_READ, false),
        BpfMapTestParam("test_u:test_r:bpf_map_write_t:s0", PROT_WRITE, false),
        BpfMapTestParam("test_u:test_r:bpf_map_write_t:s0", PROT_READ | PROT_WRITE, false),
        BpfMapTestParam("test_u:test_r:bpf_map_read_write_t:s0", PROT_NONE, true),
        BpfMapTestParam("test_u:test_r:bpf_map_read_write_t:s0", PROT_READ, true),
        BpfMapTestParam("test_u:test_r:bpf_map_read_write_t:s0", PROT_WRITE, true),
        BpfMapTestParam("test_u:test_r:bpf_map_read_write_t:s0", PROT_READ | PROT_WRITE, true)));

struct BpfPinTestParam {
  const char* label;
  uint32_t flags;
  bool map_should_succeed;
  bool prog_should_succeed;

  BpfPinTestParam(const char* label, uint32_t flags, bool map_should_succeed,
                  bool prog_should_succeed)
      : label(label),
        flags(flags),
        map_should_succeed(map_should_succeed),
        prog_should_succeed(prog_should_succeed) {}
};

const char PIN_PATH[] = "/sys/fs/bpf/foo";
class BpfPinTest : public ::testing::TestWithParam<BpfPinTestParam> {};

TEST_P(BpfPinTest, PinnedMap) {
  auto [label, flags, map_should_succeed, prog_should_succeed] = GetParam();

  {
    auto enforce = ScopedEnforcement::SetEnforcing();

    // Create a mappable array map.
    fbl::unique_fd mappable_map_fd = CreateArrayMap();
    ASSERT_TRUE(mappable_map_fd.is_valid()) << strerror(errno);

    bpf_attr attr = {
        .pathname = reinterpret_cast<uintptr_t>(PIN_PATH),
        .bpf_fd = static_cast<unsigned>(mappable_map_fd.get()),
    };
    EXPECT_THAT(bpf(BPF_OBJ_PIN, &attr), SyscallSucceeds());

    EXPECT_TRUE(RunSubprocessAs(label, [&] {
      bpf_attr attr = {
          .pathname = reinterpret_cast<uintptr_t>(PIN_PATH),
          .file_flags = flags,
      };
      int result = bpf(BPF_OBJ_GET, &attr);
      if (map_should_succeed) {
        EXPECT_THAT(result, SyscallSucceeds());
      } else {
        EXPECT_THAT(result, SyscallFailsWithErrno(EACCES));
      }
      if (result >= 0) {
        close(result);
      }
    }));
  }

  EXPECT_THAT(unlink(PIN_PATH), SyscallSucceeds());
}

TEST_P(BpfPinTest, PinnedProg) {
  auto [label, flags, map_should_succeed, prog_should_succeed] = GetParam();

  {
    auto enforce = ScopedEnforcement::SetEnforcing();

    // Create a mappable array map.
    fbl::unique_fd prog_fd = LoadProgram();
    ASSERT_TRUE(prog_fd.is_valid()) << strerror(errno);

    bpf_attr attr = {
        .pathname = reinterpret_cast<uintptr_t>(PIN_PATH),
        .bpf_fd = static_cast<unsigned>(prog_fd.get()),
    };
    EXPECT_THAT(bpf(BPF_OBJ_PIN, &attr), SyscallSucceeds());

    EXPECT_TRUE(RunSubprocessAs(label, [&] {
      bpf_attr attr = {
          .pathname = reinterpret_cast<uintptr_t>(PIN_PATH),
          .file_flags = flags,
      };
      int result = bpf(BPF_OBJ_GET, &attr);
      if (prog_should_succeed) {
        EXPECT_THAT(result, SyscallSucceeds());
      } else {
        EXPECT_THAT(result, SyscallFailsWithErrno(EACCES));
      }
      if (result >= 0) {
        close(result);
      }
    }));
  }

  EXPECT_THAT(unlink(PIN_PATH), SyscallSucceeds());
}

// Pinned maps need map_read and read for readable access, map_write and
// write for writable access. prog_run is not needed.
// Opening a pinned program always requires prog_run. File read and file write are
// needed unless the access is readonly or writeonly.
INSTANTIATE_TEST_SUITE_P(
    BpfPinTestSuite, BpfPinTest,
    ::testing::Values(
        BpfPinTestParam("test_u:test_r:bpf_pin_no_read_t:s0", BPF_F_RDONLY, false, false),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_read_t:s0", BPF_F_WRONLY, true, true),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_read_t:s0", 0, false, false),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_write_t:s0", BPF_F_RDONLY, true, true),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_write_t:s0", BPF_F_WRONLY, false, false),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_write_t:s0", 0, false, false),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_map_read_t:s0", BPF_F_RDONLY, false, true),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_map_read_t:s0", BPF_F_WRONLY, true, true),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_map_read_t:s0", 0, false, true),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_map_write_t:s0", BPF_F_RDONLY, true, true),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_map_write_t:s0", BPF_F_WRONLY, false, true),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_map_write_t:s0", 0, false, true),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_prog_run_t:s0", BPF_F_RDONLY, true, false),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_prog_run_t:s0", BPF_F_WRONLY, true, false),
        BpfPinTestParam("test_u:test_r:bpf_pin_no_prog_run_t:s0", 0, true, false),
        BpfPinTestParam("test_u:test_r:bpf_pin_all_rights_t:s0", BPF_F_RDONLY, true, true),
        BpfPinTestParam("test_u:test_r:bpf_pin_all_rights_t:s0", BPF_F_WRONLY, true, true),
        BpfPinTestParam("test_u:test_r:bpf_pin_all_rights_t:s0", 0, true, true)));

}  // namespace

extern std::string DoPrePolicyLoadWork() { return "bpf_policy.pp"; }
