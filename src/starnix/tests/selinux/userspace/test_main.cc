// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/cmdline/args_parser.h>
#include <lib/fit/defer.h>
#include <lib/fit/result.h>
#include <sys/mount.h>

#include <cstring>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/audit_checker.h"
#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

/// Returns the path to the policy that should be loaded for use by the test-suite.
/// This hook may also perform pre-policy-load work, e.g. creating kernel objects for later
/// validation by tests.
extern std::string DoPrePolicyLoadWork();

namespace {

struct CommandLineOptions {
  bool generate_json = false;
};

void LoadPolicy(const std::string& name) {
  // Ensure that no previous policy has been loaded.
  auto previous_policy = ReadFile("/sys/fs/selinux/policy");
  ASSERT_EQ(previous_policy, fit::error(EINVAL));

  // Load the specified policy from the policy data directory.
  auto policy_path = "data/policies/" + name;
  auto binary_policy = ReadFile(policy_path);
  ASSERT_TRUE(binary_policy.is_ok()) << "Read of policy at " << policy_path
                                     << " failed: " << strerror(binary_policy.error_value());
  auto result = WriteExistingFile("/sys/fs/selinux/load", binary_policy.value());
  ASSERT_TRUE(result.is_ok()) << "Load of policy from " << policy_path
                              << " failed: " << strerror(result.error_value());

  // Ensure that the binary policy is reported by the kernel as having been loaded.
  auto loaded_policy = ReadFile("/sys/fs/selinux/policy");
  ASSERT_TRUE(loaded_policy.is_ok());
}

// Perform one-time initialization of the test system.
void PrepareTestEnvironment() {
  // Check if selinuxfs is already mounted as a proxy for environment setup.
  auto mounts = ReadFile("/proc/mounts");
  if (mounts.is_ok() && mounts.value().find("/sys/fs/selinux selinuxfs") != std::string::npos) {
    // Environment appears to be already initialized.
    return;
  }
  ASSERT_THAT(mkdir("/proc", 0755), SyscallSucceeds());
  ASSERT_THAT(mkdir("/sys", 0755), SyscallSucceeds());
  ASSERT_THAT(mkdir("/tmp", 0755), SyscallSucceeds());
  // `/dev` already exists on Linux.
  if (test_helper::IsStarnix()) {
    ASSERT_THAT(mkdir("/dev", 0755), SyscallSucceeds());
  }
  ASSERT_THAT(mount("proc", "/proc", "proc", MS_NOEXEC | MS_NOSUID | MS_NODEV, 0),
              SyscallSucceeds());
  ASSERT_THAT(mount("sysfs", "/sys", "sysfs", MS_NOEXEC | MS_NOSUID | MS_NODEV, 0),
              SyscallSucceeds());
  ASSERT_THAT(mount("selinuxfs", "/sys/fs/selinux", "selinuxfs", MS_NOEXEC | MS_NOSUID, nullptr),
              SyscallSucceeds());
  ASSERT_THAT(mount("tmpfs", "/tmp", "tmpfs", MS_RELATIME, nullptr), SyscallSucceeds());
  ASSERT_THAT(
      mount("devtmpfs", "/dev", "devtmpfs", MS_NOEXEC | MS_NOSUID | MS_STRICTATIME, nullptr),
      SyscallSucceeds());

  auto policy_path = DoPrePolicyLoadWork();
  LoadPolicy(policy_path);
}

class UserspaceTestEnvironment : public ::testing::Environment {
 public:
  void SetUp() override {
    PrepareTestEnvironment();

    // gTest is documented as treating `Environment::SetUp` fatal failures as fatal, but does not
    // appear to actually do so, so we manually terminate the attempt on setup failures.
    if (::testing::Test::HasFailure()) {
      fprintf(stderr, "Test environment setup failed => failing all tests.\n");
      fflush(stdout);
      fflush(stderr);
      _exit(1);
    }
  }
};

}  // namespace

// Parse the arguments into valid options structure.
fit::result<std::string, CommandLineOptions> parse_args(int argc, char** argv) {
  cmdline::ArgsParser<CommandLineOptions> parser;
  CommandLineOptions options;
  parser.AddSwitch("json", 'j', "--json\tGenerate audit log JSON objects for expectations.",
                   &CommandLineOptions::generate_json);
  std::vector<std::string> params;
  if (auto status = parser.Parse(argc, const_cast<const char**>(argv), &options, &params);
      status.has_error()) {
    return fit::error("Error: " + status.error_message());
  }
  if (params.size()) {
    return fit::error("Error: arguments with parameters found.");
  }
  return fit::ok(options);
}

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);

  auto parse_res = parse_args(argc, argv);
  if (parse_res.is_error()) {
    fprintf(stderr, "%s\n", parse_res.error_value().c_str());
  }

  // Set up gTest to perform test environment setup at-most-once.
  GTEST_FLAG_SET(recreate_environments_when_repeating, false);
  ::testing::AddGlobalTestEnvironment(new UserspaceTestEnvironment);

  testing::TestEventListeners& listeners = testing::UnitTest::GetInstance()->listeners();
  // The `with_json_generation()` function can be used to get an `AuditChecker` which will
  // generate audit log JSON objects.
  if (parse_res.value().generate_json) {
    listeners.Append(AuditChecker::with_json_generation());
  } else {
    listeners.Append(new AuditChecker);
  }

  return RUN_ALL_TESTS();
}
