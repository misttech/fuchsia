// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STARNIX_TESTS_SELINUX_USERSPACE_AUDIT_CHECKER_H_
#define SRC_STARNIX_TESTS_SELINUX_USERSPACE_AUDIT_CHECKER_H_

#include <lib/fit/result.h>

#include <set>
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

  void OnTestSuiteEnd(const testing::TestSuite& test_suite) override;
  void OnTestStart(const testing::TestInfo& test_info) override;
  void OnTestEnd(const testing::TestInfo& test_info) override;

 private:
  struct AuditLogEntry {
    bool denied;
    std::set<std::string> permission;
    std::string scontext;
    std::string tcontext;
    std::string tclass;
    bool permissive;

    bool operator==(const AuditLogEntry& other) const;

    // Returns true if `other` matches scontext/tcontext/tclass, and the permission is one of those
    // described by this entry.
    bool contains(const AuditLogEntry& other) const;

    std::string ToString() const;
  };

  // Parses the JSON expectations file.
  bool ParseExpectationsFile(const std::string& file_path);

  // Reads and parses all audit logs between the start and end sentinels.
  fit::result<std::string, std::vector<AuditChecker::AuditLogEntry>> ReadAuditLogs(
      const std::string& test_name);

  // Parses a single audit log.
  fit::result<std::string, AuditChecker::AuditLogEntry> ParseAuditLogString(
      const std::string& line);

  // Checks if a given test should be skipped based on its name.
  bool ShouldOnlyDrainAudits(const std::string& test_name);

  // Checks if a give test is in the expected failures.
  bool IsExpectedToFail(const std::string& test_name);

  // The main method to perform the audit check against the expectations file
  // provided in the constructor.
  void CheckAuditExpectations(const std::string& test_name);

  std::unordered_map<std::string, std::vector<AuditChecker::AuditLogEntry>> expectations_map_;
  std::vector<std::string> skipped_tests_;
  std::vector<std::string> expected_failure_tests_;

  std::vector<std::tuple<std::vector<std::string>, std::string>> current_test_suite_raw_logs_;

  // Set to true to generate audit log JSON objects without audit checks.
  bool generate_json_ = false;
};

#endif  // SRC_STARNIX_TESTS_SELINUX_USERSPACE_AUDIT_CHECKER_H_
