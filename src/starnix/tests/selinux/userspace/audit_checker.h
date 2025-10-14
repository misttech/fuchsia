// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STARNIX_TESTS_SELINUX_USERSPACE_AUDIT_CHECKER_H_
#define SRC_STARNIX_TESTS_SELINUX_USERSPACE_AUDIT_CHECKER_H_

#include <memory>
#include <optional>
#include <regex>
#include <string>
#include <unordered_map>
#include <vector>

#include <gtest/gtest.h>

class AuditChecker : public testing::EmptyTestEventListener {
 public:
  // Constructor: Uses the path of the JSON file containing expected audit logs.
  AuditChecker();

  // Creates an `AuditChecker` with debug prints enabled.
  static AuditChecker* for_debug();
  // Creates an `AuditChecker` with audit log JSON generation.
  static AuditChecker* with_json_generation();

  void OnTestSuiteStart(const testing::TestSuite& test_suite) override;
  void OnTestStart(const testing::TestInfo& test_info) override;
  void OnTestEnd(const testing::TestInfo& test_info) override;

 private:
  // Buffer size for reading from the netlink socket.
  static constexpr int kNetlinkBufSize = 4096;
  static constexpr int kTabSize = 4;

  static constexpr char kSuccessKey[] = "audit_success";
  static constexpr char kExpectedFailureKey[] = "audit_failure";
  static constexpr char kSkipKey[] = "audit_skip";
  static constexpr char kTestNameKey[] = "name";
  static constexpr char kTestAuditExpectationsKey[] = "audit_expectations";
  static constexpr char kExpectationsFile[] = "data/audit_expectations/audit_expectations.json";

  const std::regex kAuditLogRegex = std::regex(
      R"(avc:\s+(denied|granted)\s+\{\s*([^}]+)\s*\}.*scontext=([^ ]+)\s+tcontext=([^ ]+)\s+tclass=([^ ]+).*)");

  struct AuditLogEntry {
    bool denied;
    std::string permission;
    std::string scontext;
    std::string tcontext;
    std::string tclass;

    bool operator==(const AuditLogEntry& other) const;
  };

  // Parses the JSON expectations file.
  bool ParseExpectationsFile(const std::string& file_path);

  // Reads and parses all audit logs between the start and end sentinels.
  std::vector<AuditChecker::AuditLogEntry> ReadAuditLogs(const std::string& test_name);

  // Parses a single audit log.
  std::optional<AuditChecker::AuditLogEntry> ParseAuditLogString(const std::string& line,
                                                                 std::string& error_str);

  // Creates a string representation of an AuditLogEntry for logging.
  std::string StringifyAudit(const AuditChecker::AuditLogEntry entry);

  // Checks if a given test should be audited based on its name.
  bool ShouldCheckAudits(const std::string& test_name);
  // Checks if a given test should be skipped based on its name.
  bool ShouldOnlyDrainAudits(const std::string& test_name);
  // Checks if a give test is in the expected failures.
  bool IsExpectedToFail(const std::string& test_name);
  // Drains all audit logs without any checks.
  void DrainAuditLog();

  // Sends USER_AVC sentinel messages to mark the beginning and end of a test section
  // in the audit log. If the test reads the audit logs before finishing the test,
  // it must be skipped, because it will consume the start sentinel.
  void SendStartSentinel();
  void SendEndSentinel();

  // The main method to perform the audit check against the expectations file
  // provided in the constructor.
  void CheckAuditExpectations(const std::string& test_name);

  // Escapes the quotes in an audit log.
  void EscapeAuditLog(std::string& audit_log);
  // Printing functions to format audit expectations.
  void PrintWithTab(int multiplier, const char* format, ...);
  void ExpectationsToJSON(std::vector<std::string> logs, const std::string& test_name);
  void AddAuditFailure(const std::string& failure, bool expected);

  std::unordered_map<std::string, std::vector<AuditChecker::AuditLogEntry>> expectations_map_;
  std::vector<std::string> skipped_tests_;
  std::vector<std::string> expected_failure_tests_;
  std::string current_test_suite_name_;
  // Set to true to generate audit log JSON objects without audit checks.
  bool generate_json_ = false;
};

#endif  // SRC_STARNIX_TESTS_SELINUX_USERSPACE_AUDIT_CHECKER_H_
