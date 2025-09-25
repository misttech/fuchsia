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
#include <string_view>
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

AuditChecker* AuditChecker::for_debug() {
  auto debug_checker = new AuditChecker;
  debug_checker->debug_ = true;
  return debug_checker;
}

std::optional<AuditChecker::AuditLogEntry> AuditChecker::ParseAuditLogString(
    const std::string line, std::string& error_str) {
  std::smatch matches;
  if (std::regex_search(line, matches, kAuditLogRegex) && matches.size() == 6) {
    return AuditChecker::AuditLogEntry{.denied = matches[1].str() == "denied",
                                       .permission = matches[2].str(),
                                       .scontext = matches[3].str(),
                                       .tcontext = matches[4].str(),
                                       .tclass = matches[5].str()};
  } else {
    error_str = "Failed to parse audit log string: " + line;
    return std::nullopt;
  }
}

bool AuditChecker::ParseExpectationsFile(const std::string_view file_path) {
  json_parser::JSONParser parser;
  rapidjson::Document doc = parser.ParseFromFile(file_path.data());

  if (parser.HasError()) {
    return false;
  }
  if (!doc.IsObject() || !doc.HasMember(kTestsKey.data()) || !doc[kTestsKey.data()].IsArray()) {
    return false;
  }

  for (const auto& test_obj : doc[kTestsKey.data()].GetArray()) {
    if (!test_obj.IsObject() || !test_obj.HasMember(kTestNameKey.data()) ||
        !test_obj[kTestNameKey.data()].IsString() ||
        !test_obj.HasMember(kTestAuditExpectationsKey.data()) ||
        !test_obj[kTestAuditExpectationsKey.data()].IsArray()) {
      return false;
    }

    std::string test_name = test_obj[kTestNameKey.data()].GetString();
    std::vector<AuditLogEntry> expected_logs;

    for (const auto& log_str_val : test_obj[kTestAuditExpectationsKey.data()].GetArray()) {
      if (!log_str_val.IsString()) {
        return false;
      }
      std::string error_str;
      std::string log_str = log_str_val.GetString();
      if (auto entry = ParseAuditLogString(log_str, error_str)) {
        expected_logs.push_back(*entry);
      } else {
        return false;
      }
    }
    expectations_map_[test_name] = std::move(expected_logs);
  }
  return true;
}

bool AuditChecker::AuditLogEntry::operator==(const AuditChecker::AuditLogEntry& other) const {
  if (permission != other.permission) {
    printf("Notice: 'permission' field differs: '%s' vs '%s'\n", permission.c_str(),
           other.permission.c_str());
  }
  return scontext == other.scontext && tcontext == other.tcontext && tclass == other.tclass;
}

std::vector<AuditChecker::AuditLogEntry> AuditChecker::ReadAuditLogs(std::string_view test_name) {
  std::vector<std::string> raw_logs;
  std::vector<AuditChecker::AuditLogEntry> parsed_logs;
  fbl::unique_fd fd = OpenNetlinkAuditSocket();
  if (!fd.is_valid() || RegisterAsAuditDaemon(fd.get(), getpid()).is_error()) {
    return parsed_logs;
  }
  if (debug_) {
    printf("audit_listener: started\n");
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
    std::string error_str;
    if (auto entry = ParseAuditLogString(message, error_str)) {
      if (!entry->denied) {
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
      if (debug_) {
        raw_logs.push_back(message);
      }
      parsed_logs.push_back(*entry);
    } else {
      ADD_FAILURE() << "Failed to parse AUDIT_AVC message: " << error_str
                    << "\nMessage: " << message;
    }
  }
  if (UnregisterAuditDaemon(fd.get()).is_error()) {
    printf("Failed to unregister\n");
    return parsed_logs;
  }
  if (!checked_start) {
    fprintf(stderr, "Did not find start sentinel\n");
  }
  if (debug_) {
    DebugExpectationsToJSON(raw_logs, test_name);
    printf("\naudit_listener: stopped\n");
  }
  return parsed_logs;
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

bool AuditChecker::ShouldCheckAudits(std::string_view test_name) {
  return expectations_map_.count(test_name.data()) > 0;
}

void AuditChecker::OnTestSuiteStart(const testing::TestSuite& test_suite) {
  current_test_suite_name_ = std::string(test_suite.name());
}

void AuditChecker::OnTestStart(const testing::TestInfo& test_info) {
  if (!ShouldCheckAudits(std::string_view(current_test_suite_name_ + "/" + test_info.name())) &&
      !debug_) {
    return;
  }
  SendStartSentinel();
}

void AuditChecker::OnTestEnd(const testing::TestInfo& test_info) {
  if (!ShouldCheckAudits(std::string_view(current_test_suite_name_ + "/" + test_info.name())) &&
      !debug_) {
    return;
  }
  SendEndSentinel();
  CheckAuditExpectations(std::string_view(current_test_suite_name_ + "/" + test_info.name()));
}

void AuditChecker::DebugPrintWithTab(int multiplier, const char* format, ...) {
  printf("%*s", multiplier * kTabSize, "");
  va_list args;
  va_start(args, format);
  vprintf(format, args);
  va_end(args);
}

void AuditChecker::DebugExpectationsToJSON(std::vector<std::string> logs,
                                           std::string_view test_name) {
  if (!logs.size()) {
    return;
  }

  printf("\n{\n");
  DebugPrintWithTab(1, "\"%s\": \"%s\",\n", kTestNameKey.data(), test_name.data());
  DebugPrintWithTab(1, "\"%s\": [\n", kTestAuditExpectationsKey.data());
  for (int i = 0; i < (int)logs.size(); i++) {
    DebugPrintWithTab(2, "\"%s\"", logs[i].c_str());
    if (i != (int)logs.size() - 1) {
      printf(",");
    }
    printf("\n");
  }
  DebugPrintWithTab(1, "]\n");
  printf("}\n");
}

std::string AuditChecker::StringifyAudit(const AuditChecker::AuditLogEntry entry) {
  return "permission: " + entry.permission + " | " + "scontext: " + entry.scontext + " | " +
         "tcontext: " + entry.tcontext + " | " + "tclass: " + entry.tclass;
}

void AuditChecker::CheckAuditExpectations(std::string_view test_name) {
  std::vector<AuditChecker::AuditLogEntry> actual_logs = ReadAuditLogs(test_name);
  if (debug_) {
    return;
  }
  std::vector<AuditChecker::AuditLogEntry> expected_logs = expectations_map_.at(test_name.data());

  if (expected_logs.size() != actual_logs.size()) {
    ADD_FAILURE() << "Audit log count mismatch. Expected: " << expected_logs.size()
                  << ", Actual: " << actual_logs.size();
  }

  size_t min_size = std::min(expected_logs.size(), actual_logs.size());
  // Check each audit log against the expectation
  for (size_t i = 0; i < min_size; ++i) {
    const auto actual = actual_logs[i];
    const auto expected = expected_logs[i];
    if (actual != expected) {
      ADD_FAILURE() << "Audit log mismatch at index " << i
                    << ":\nExpected: " << StringifyAudit(expected)
                    << "\nActual: " << StringifyAudit(actual);
    }
  }
}
