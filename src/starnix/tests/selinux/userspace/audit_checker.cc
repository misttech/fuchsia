// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/starnix/tests/selinux/userspace/audit_checker.h"

#include <sys/socket.h>

#include <algorithm>
#include <fstream>
#include <regex>
#include <sstream>
#include <string>
#include <utility>
#include <vector>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/audit.h>
#include <linux/netlink.h>
#include <rapidjson/schema.h>
#include <rapidjson/stringbuffer.h>
#include <rapidjson/writer.h>

#include "src/lib/json_parser/json_parser.h"
#include "src/starnix/tests/selinux/userspace/audit_checker.h"
#include "src/starnix/tests/selinux/userspace/audit_utils.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

AuditChecker::AuditChecker() {
  // Use `ADD_FAILURE` here to trigger an environment setup failure.
  if (!ParseExpectationsFile(kExpectationsFile)) {
    ADD_FAILURE() << "Failed to parse audit expectations file.";
  }
}

AuditChecker* AuditChecker::with_json_generation() {
  auto checker = new AuditChecker;
  checker->generate_json_ = true;
  return checker;
}

fit::result<std::string, AuditChecker::AuditLogEntry> AuditChecker::ParseAuditLogString(
    const std::string& line) {
  std::smatch matches;
  if (std::regex_search(line, matches, kAuditLogRegex) && matches.size() == 6) {
    return fit::ok(AuditChecker::AuditLogEntry{.denied = matches[1].str() == "denied",
                                               .permission = matches[2].str(),
                                               .scontext = matches[3].str(),
                                               .tcontext = matches[4].str(),
                                               .tclass = matches[5].str()});
  } else {
    return fit::error("Failed to parse audit log string: " + line);
  }
}

bool AuditChecker::ParseExpectationsFile(const std::string& file_path) {
  json_parser::JSONParser parser;
  rapidjson::Document doc = parser.ParseFromFile(file_path);

  if (parser.HasError()) {
    return false;
  }
  if (!doc.IsObject() || !doc.HasMember(kSuccessKey) || !doc[kSuccessKey].IsArray()) {
    return false;
  }

  if (doc.HasMember(kExpectedFailureKey) && !doc[kExpectedFailureKey].IsArray()) {
    return false;
  }
  for (const auto& failing_test : doc[kExpectedFailureKey].GetArray()) {
    if (!failing_test.IsString()) {
      return false;
    }
    expected_failure_tests_.push_back(failing_test.GetString());
  }

  if (doc.HasMember(kSkipKey) && !doc[kSkipKey].IsArray()) {
    return false;
  }
  for (const auto& skipped_test : doc[kSkipKey].GetArray()) {
    if (!skipped_test.IsString()) {
      return false;
    }
    skipped_tests_.push_back(skipped_test.GetString());
  }

  for (const auto& test_obj : doc[kSuccessKey].GetArray()) {
    if (!test_obj.IsObject() || !test_obj.HasMember(kTestNameKey) ||
        !test_obj[kTestNameKey].IsString() || !test_obj.HasMember(kTestAuditExpectationsKey) ||
        !test_obj[kTestAuditExpectationsKey].IsArray()) {
      return false;
    }

    std::string test_name = test_obj[kTestNameKey].GetString();
    std::vector<AuditLogEntry> expected_logs;

    for (const auto& log_str_val : test_obj[kTestAuditExpectationsKey].GetArray()) {
      if (!log_str_val.IsString()) {
        return false;
      }
      std::string log_str = log_str_val.GetString();
      // TODO: https://fxbug.dev/449714364 - Remove after denials are generated in permissive mode.
      if (test_helper::IsStarnix() && strstr(log_str.c_str(), "permissive=1")) {
        continue;
      }
      auto parse_result = ParseAuditLogString(log_str);
      if (parse_result.is_ok()) {
        expected_logs.push_back(parse_result.value());
      } else {
        return false;
      }
    }
    expectations_map_[test_name] = std::move(expected_logs);
  }
  return true;
}

bool AuditChecker::AuditLogEntry::operator==(const AuditChecker::AuditLogEntry& other) const {
  // If the permission can not be found in the other structure or the other structure's
  // permission can not be found in permission, return false because of total difference.
  if (!strstr(other.permission.c_str(), permission.c_str()) &&
      !strstr(permission.c_str(), other.permission.c_str())) {
    return false;
  }
  // If the permission field contains multiple permissions (on Linux), just print the
  // difference.
  if (permission != other.permission) {
    printf("Notice: 'permission' field differs: '%s' vs '%s'\n", permission.c_str(),
           other.permission.c_str());
  }
  return scontext == other.scontext && tcontext == other.tcontext && tclass == other.tclass;
}

fit::result<std::string, std::vector<AuditChecker::AuditLogEntry>> AuditChecker::ReadAuditLogs(
    const std::string& test_name) {
  std::vector<std::string> raw_logs;
  std::vector<AuditChecker::AuditLogEntry> parsed_logs;
  fbl::unique_fd fd = OpenNetlinkAuditSocket();
  if (!fd.is_valid()) {
    return fit::error("Failed to open NETLINK_AUDIT socket");
  }
  auto register_result = RegisterAsAuditDaemon(fd.get(), getpid());
  if (register_result.is_error()) {
    return fit::error("Failed to register as daemon: " +
                      std::to_string(register_result.error_value()));
  }

  bool checked_start = false;
  char buf[kNetlinkBufSize];
  // Drain the audit backlog until there is no other message.
  while (true) {
    auto recv_res = ReceiveNetlinkMessage(fd.get(), buf, sizeof(buf));
    if (recv_res.is_error() && recv_res.error_value() == EINTR) {
      continue;
    }
    if (recv_res.value() < NLMSG_LENGTH(0)) {
      fprintf(stderr, "audit_listener: message too short\n");
      continue;
    }

    struct nlmsghdr* header = (struct nlmsghdr*)buf;
    if (header->nlmsg_type != AUDIT_AVC && header->nlmsg_type != AUDIT_USER_AVC) {
      continue;
    }
    std::string message((char*)NLMSG_DATA(header),
                        (char*)NLMSG_DATA(header) + (recv_res.value() - sizeof(*header)));
    if (header->nlmsg_type == AUDIT_USER_AVC) {
      if (!checked_start && strstr(message.c_str(), "SENTINEL_START")) {
        checked_start = true;
        continue;
      }
      if (!strstr(message.c_str(), "SENTINEL_END")) {
        continue;
      }
      break;
    }
    // Parse the audit string
    auto parse_result = ParseAuditLogString(message);
    if (parse_result.is_ok()) {
      auto entry = parse_result.value();
      if (!entry.denied) {
        continue;
      }
      // If there is a AUDIT_AVC log before the start sentinel, it means
      // there is a policy violation outside of the current test.
      // The message should be checked, it can be from another test that
      // should include an audit expectation.
      if (!checked_start) {
        fprintf(stderr, "Found AUDIT_AVC log before start sentinel: %s\n", message.c_str());
        continue;
      }
      if (generate_json_) {
        raw_logs.push_back(message);
      }
      fprintf(stderr, "%s\n", message.c_str());
      parsed_logs.push_back(entry);
    } else {
      ADD_FAILURE() << "Failed to parse AUDIT_AVC message: " << parse_result.error_value()
                    << "\nMessage: " << message;
    }
  }
  if (UnregisterAuditDaemon(fd.get()).is_error()) {
    fprintf(stderr, "Failed to unregister\n");
    return fit::ok(parsed_logs);
  }
  if (!checked_start) {
    fprintf(stderr, "Did not find start sentinel\n");
  }
  if (generate_json_) {
    current_test_suite_raw_logs_.push_back(std::make_tuple(raw_logs, std::string(test_name)));
  }
  return fit::ok(parsed_logs);
}

void AuditChecker::SendStartSentinel() {
  auto fork_helper = test_helper::ForkHelper();
  pid_t child_pid = fork_helper.RunInForkedProcess([&] {
    fbl::unique_fd fd = OpenNetlinkAuditSocket();
    if (!fd.is_valid()) {
      exit(2);
    }
    auto send_res = SendUserAuditMessage(fd.get(), 1, "SENTINEL_START");
    if (send_res.is_error()) {
      fprintf(stderr, "Start sentinel send error %s\n", strerror(send_res.error_value()));
      exit(3);
    }
    exit(0);
  });
  if (child_pid < 0 || !fork_helper.WaitForChildren()) {
    fprintf(stderr, "Start sentinel child error\n");
  }
}

void AuditChecker::SendEndSentinel() {
  auto fork_helper = test_helper::ForkHelper();
  pid_t child_pid = fork_helper.RunInForkedProcess([&] {
    fbl::unique_fd fd = OpenNetlinkAuditSocket();
    if (!fd.is_valid()) {
      exit(2);
    }
    auto send_res = SendUserAuditMessage(fd.get(), 1, "SENTINEL_END");
    if (send_res.is_error()) {
      fprintf(stderr, "End sentinel send error %s\n", strerror(send_res.error_value()));
      exit(3);
    }
    exit(0);
  });
  if (child_pid < 0 || !fork_helper.WaitForChildren()) {
    fprintf(stderr, "End sentinel end child error\n");
  }
}

bool AuditChecker::HasAuditExpectations(const std::string& test_name) {
  return expectations_map_.count(test_name) > 0;
}

bool AuditChecker::ShouldOnlyDrainAudits(const std::string& test_name) {
  auto found = std::find(skipped_tests_.begin(), skipped_tests_.end(), test_name);
  if (found != skipped_tests_.end()) {
    return true;
  }
  return false;
}

bool AuditChecker::IsExpectedToFail(const std::string& test_name) {
  if (!test_helper::IsStarnix()) {
    return false;
  }

  auto found = std::find(expected_failure_tests_.begin(), expected_failure_tests_.end(), test_name);
  if (found != expected_failure_tests_.end()) {
    return true;
  }
  return false;
}

void AuditChecker::DrainAuditLog() {
  SendEndSentinel();
  fbl::unique_fd fd = OpenNetlinkAuditSocket();
  if (!fd.is_valid() || RegisterAsAuditDaemon(fd.get(), getpid()).is_error()) {
    return;
  }

  char buf[kNetlinkBufSize];
  // Drain the audit backlog until there is no other message.
  while (true) {
    auto recv_res = ReceiveNetlinkMessage(fd.get(), buf, sizeof(buf));
    if (recv_res.is_error() && recv_res.error_value() == EINTR) {
      continue;
    }
    if (recv_res.value() < NLMSG_LENGTH(0)) {
      fprintf(stderr, "audit_drainer: message too short\n");
      continue;
    }
    struct nlmsghdr* header = (struct nlmsghdr*)buf;
    if (header->nlmsg_type != AUDIT_USER_AVC) {
      continue;
    }
    std::string message((char*)NLMSG_DATA(header),
                        (char*)NLMSG_DATA(header) + (recv_res.value() - sizeof(*header)));
    if (header->nlmsg_type == AUDIT_USER_AVC) {
      if (!strstr(message.c_str(), "SENTINEL_END")) {
        continue;
      }
      break;
    }
  }
  if (UnregisterAuditDaemon(fd.get()).is_error()) {
    printf("Failed to unregister\n");
    return;
  }
}

void AuditChecker::OnTestSuiteStart(const testing::TestSuite& test_suite) {
  current_test_suite_name_ = std::string(test_suite.name());
}

void AuditChecker::OnTestSuiteEnd(const testing::TestSuite& test_suite) {
  if (generate_json_) {
    // Add a sleep to allow the Linux kernel to print syslog messages before
    // printing any audit JSON objects.
    sleep(1);
    for (auto test_logs : current_test_suite_raw_logs_) {
      ExpectationsToJSON(std::get<0>(test_logs), std::get<1>(test_logs));
    }
    current_test_suite_raw_logs_.clear();
  }
}

void AuditChecker::OnTestStart(const testing::TestInfo& test_info) {
  std::string test_name(current_test_suite_name_ + "/" + test_info.name());
  if (ShouldOnlyDrainAudits(test_name)) {
    return;
  }
  SendStartSentinel();
}

void AuditChecker::OnTestEnd(const testing::TestInfo& test_info) {
  std::string test_name(current_test_suite_name_ + "/" + test_info.name());
  // If the audit log checking should be skipped, drain the audit logs.
  // This can be used for tests that fail on Starnix or do not produce any
  // useful logs.
  if (ShouldOnlyDrainAudits(test_name)) {
    DrainAuditLog();
    return;
  }
  SendEndSentinel();
  CheckAuditExpectations(test_name);
}

void AuditChecker::EscapeAuditLog(std::string& audit_log) {
  size_t i = 0;
  while (i < audit_log.size()) {
    if (audit_log.at(i) == '"') {
      audit_log.insert(i, 1, '\\');
      i += 2;
      continue;
    }
    i += 1;
  }
}

void AuditChecker::PrintWithTab(int multiplier, const char* format, ...) {
  printf("%*s", multiplier * kTabSize, "");
  va_list args;
  va_start(args, format);
  vprintf(format, args);
  va_end(args);
}

void AuditChecker::ExpectationsToJSON(std::vector<std::string> logs, const std::string& test_name) {
  if (!logs.size()) {
    return;
  }

  printf("\n{\n");
  PrintWithTab(1, "\"%s\": \"%s\",\n", kTestNameKey, test_name.c_str());
  PrintWithTab(1, "\"%s\": [\n", kTestAuditExpectationsKey);
  for (int i = 0; i < (int)logs.size(); i++) {
    EscapeAuditLog(logs[i]);
    PrintWithTab(2, "\"%s\"", logs[i].c_str());
    if (i != (int)logs.size() - 1) {
      printf(",");
    }
    printf("\n");
  }
  PrintWithTab(1, "]\n");
  printf("}\n");
}

void AuditChecker::AddAuditFailure(const std::string& failure, bool expected) {
  if (expected) {
    fprintf(stderr, "%s\n", failure.c_str());
    return;
  }
  ADD_FAILURE() << failure;
}

std::string AuditChecker::StringifyAudit(const AuditChecker::AuditLogEntry entry) {
  return "permission: " + entry.permission + " | " + "scontext: " + entry.scontext + " | " +
         "tcontext: " + entry.tcontext + " | " + "tclass: " + entry.tclass;
}

void AuditChecker::CheckAuditExpectations(const std::string& test_name) {
  auto read_result = ReadAuditLogs(test_name);
  bool expect_fail = IsExpectedToFail(test_name);
  if (read_result.is_error()) {
    ADD_FAILURE() << read_result.error_value();
    return;
  }
  std::vector<AuditChecker::AuditLogEntry> actual_logs = read_result.value();
  if (generate_json_) {
    return;
  }

  if (!HasAuditExpectations(test_name)) {
    for (auto audit_log : actual_logs) {
      std::string failure_string("Unexpected audit log: " + StringifyAudit(audit_log));
      AddAuditFailure(failure_string, expect_fail);
    }
    return;
  }

  std::vector<AuditChecker::AuditLogEntry> expected_logs = expectations_map_.at(test_name.data());
  if (expected_logs.size() != actual_logs.size()) {
    std::string failure_string(
        "Audit log count mismatch. Expected: " + std::to_string(expected_logs.size()) +
        ", Actual: " + std::to_string(actual_logs.size()));
    AddAuditFailure(failure_string, expect_fail);
  }

  bool found_mismatch = false;
  size_t min_size = std::min(expected_logs.size(), actual_logs.size());
  // Check each audit log against the expectation
  for (size_t i = 0; i < min_size; ++i) {
    const auto actual = actual_logs[i];
    const auto expected = expected_logs[i];
    if (actual != expected) {
      if (expect_fail) {
        found_mismatch = true;
      }
      std::string failure_string("Audit log mismatch at index " + std::to_string(i) +
                                 ":\nExpected: " + StringifyAudit(expected) +
                                 "\nActual: " + StringifyAudit(actual));
      AddAuditFailure(failure_string, expect_fail);
    }
  }
  if (expect_fail && expected_logs.size() == actual_logs.size() && !found_mismatch) {
    std::string failure_string("Expected failure in test. All audit logs match.");
    AddAuditFailure(failure_string, false);
  }
}
