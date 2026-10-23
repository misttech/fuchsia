// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/critical-services/storage-watchdog/diagnostics_text.h"

#include <string>
#include <vector>

#include <gtest/gtest.h>

namespace storage_watchdog {
namespace {

constexpr int64_t kSec = 1'000'000'000;

TEST(LogTailTest, KeepsWarningsAndStorageComponents) {
  LogTail tail(100, 50, 100'000);
  tail.Add(1 * kSec, "core/foo", "INFO", "boring");
  tail.Add(2 * kSec, "core/foo", "WARN", "not great");
  tail.Add(3 * kSec, "bootstrap/fshost", "INFO", "mounted data");
  tail.Add(4 * kSec, "bootstrap/fshost/fxfs", "INFO", "flushed");
  tail.Add(5 * kSec, "bootstrap/boot-drivers:ufs-3c400000", "INFO", "link up");
  tail.Add(6 * kSec, "core/starnix_runner", "INFO", "suspending container");
  // The Starnix kernel isn't a storage component, even though it lives under starnix_runner.
  tail.Add(7 * kSec, "core/starnix_runner/kernels:abc", "INFO", "android noise");
  EXPECT_EQ(tail.Lines(), (std::vector<std::string>{
                              "[00002.000][foo][W] not great",
                              "[00003.000][fshost][I] mounted data",
                              "[00004.000][fxfs][I] flushed",
                              "[00005.000][boot-drivers:ufs-3c400000][I] link up",
                              "[00006.000][starnix_runner][I] suspending container",
                          }));
}

TEST(LogTailTest, StorageLinesHaveTheirOwnShareOfTheTail) {
  LogTail tail(4, 2, 100'000);
  for (int i = 0; i < 3; i++) {
    tail.Add(i * kSec, "bootstrap/fshost", "INFO", std::string(1, static_cast<char>('a' + i)));
  }
  for (int i = 3; i < 10; i++) {
    tail.Add(i * kSec, "core/foo", "WARN", std::string(1, static_cast<char>('a' + i)));
  }
  EXPECT_EQ(tail.Lines(), (std::vector<std::string>{
                              "[00001.000][fshost][I] b",
                              "[00002.000][fshost][I] c",
                              "[00008.000][foo][W] i",
                              "[00009.000][foo][W] j",
                          }));
}

TEST(LogTailTest, FoldsInterleavedMessagesThatOnlyDifferInNumbers) {
  LogTail tail(100, 50, 100'000);
  for (int64_t i = 0; i < 4; i++) {
    tail.Add((2 * i) * kSec, "core/starnix_runner/kernels:abc", "ERROR",
             "pixel-thermal: Failed to update cdev cpufreq-cpu" + std::to_string(i * 2));
    tail.Add(((2 * i) + 1) * kSec, "core/starnix_runner/kernels:abc", "WARN",
             "StatusFile::generate for task " + std::to_string(100 + i) + " took 1" +
                 std::to_string(i) + " ms");
  }
  EXPECT_EQ(tail.Lines(),
            (std::vector<std::string>{
                "[00000.000][kernels:abc][E] pixel-thermal: Failed to update cdev cpufreq-cpu0 (+3 "
                "similar until 00006.000)",
                "[00001.000][kernels:abc][W] StatusFile::generate for task 100 took 10 ms (+3 "
                "similar until 00007.000)",
            }));
}

TEST(LogTailTest, FoldsMessagesThatOnlyDifferInUrlsWithHashes) {
  LogTail tail(100, 50, 100'000);
  for (const char* package : {"paver", "vsock_service", "battery-manager"}) {
    tail.Add(1 * kSec, "core/pkg-resolver", "WARN",
             std::string("failed to resolve fuchsia-pkg://fuchsia.com/") + package +
                 "?hash=347377eb4f11d8848c7c4f5eb99692a7f6a134d6473eed949b5f4c3b73874d4c as "
                 "fuchsia-pkg://fuchsia.com/" +
                 package);
  }
  std::vector<std::string> lines = tail.Lines();
  ASSERT_EQ(lines.size(), 1u);
  EXPECT_TRUE(lines[0].ends_with("(+2 similar until 00001.000)"));
}

TEST(LogTailTest, LinesFitInOneDebuglogRecord) {
  LogTail tail(100, 50, 100'000);
  tail.Add(1 * kSec, "core/foo", "WARN", std::string(300, 'x') + "1");
  tail.Add(2 * kSec, "core/foo", "WARN", std::string(300, 'x') + "2");
  std::vector<std::string> lines = tail.Lines();
  ASSERT_EQ(lines.size(), 1u);
  EXPECT_EQ(lines[0].size(), kMaxLineLength);
  EXPECT_TRUE(lines[0].ends_with("xx (+1 similar until 00002.000)"));
}

TEST(LogTailTest, ParsesArchivistJson) {
  LogTail tail(100, 50, 100'000);
  tail.AddJson(R"([{"moniker": "core/foo", "metadata": {"timestamp": 1500000000,
      "severity": "ERROR"}, "payload": {"root": {"message": {"value": "oops"}}}},
      {"bad": true}])");
  tail.AddJson("not json");
  EXPECT_EQ(tail.Lines(), (std::vector<std::string>{"[00001.500][foo][E] oops"}));
}

TEST(InspectTest, FormatsStripsAndCaps) {
  const std::vector<std::string> batches = {
      R"([{"moniker": "bootstrap/power-broker", "payload": {"root": {"p": 1}}},
          {"moniker": "bootstrap/fshost", "payload": {"root": {"x": 1,
              "latency": {"floor": 0, "step": 1, "counts": [1, 2], "indexes": [0, 1]}}}},
          {"moniker": "bootstrap/fshost/fs-collection:a", "payload": {"root": {}}},
          {"moniker": "gone", "payload": null}])",
      R"({"moniker": "big", "payload": {"ufs": {"s": ")" + std::string(500, 'a') + R"("}}})",
  };
  std::vector<std::string> lines = InspectLines(batches, 100, 1000);
  ASSERT_EQ(lines.size(), 3u);
  EXPECT_EQ(lines[0], R"(inspect bootstrap/fshost: {"root":{"x":1}})");
  EXPECT_EQ(lines[1].size(), 100u);
  EXPECT_TRUE(lines[1].starts_with(R"(inspect big: {"ufs":{"s":"aaa)"));
  EXPECT_TRUE(lines[1].ends_with(" ...(529 bytes total)"));
  EXPECT_EQ(lines[2], R"(inspect bootstrap/power-broker: {"elements":[]})");
}

TEST(InspectTest, CompactsPowerBroker) {
  // The shape of power-broker's Inspect data for the watchdog's selectors.
  const std::vector<std::string> batches = {
      R"([{"moniker": "bootstrap/power-broker",
      "metadata": {"name": "root"}, "payload": {"root": {"broker": {
        "leases": {"45943": "Satisfied"},
        "topology": {"fuchsia.inspect.Graph": {"topology": {
          "54623706": {"meta": {"current_level": 1, "name": "spi-db31000", "required_level": 1,
                                "valid_levels": [0, 1], "leases": {}}},
          "2994881255": {"meta": {"current_level": 2, "name": "execution_state",
                                  "required_level": 2, "valid_levels": [0, 1, 2],
                                  "leases": {"45943": {"level": 1, "status": 2}}}},
          "77": {"meta": {"current_level": 0, "name": "ufs", "required_level": 1,
                          "valid_levels": [0, 1], "leases": {}}}}}}}}}}])",
  };
  EXPECT_EQ(
      InspectLines(batches, 10'000, 10'000),
      (std::vector<std::string>{
          R"(inspect bootstrap/power-broker: {"leases":{"45943":"Satisfied"},"elements":[{"name":"execution_state","current_level":2,"required_level":2,"leases":{"45943":{"level":1,"status":2}}},{"name":"ufs","cur)",
          R"(rent_level":0,"required_level":1,"leases":{}}]})",
      }));
}

TEST(InspectTest, TotalCapAndLineSplitting) {
  const std::vector<std::string> batches = {
      R"([{"moniker": "a", "payload": {"root": {"s": ")" + std::string(300, 'a') + R"("}}},
          {"moniker": "b", "payload": {"root": {"s": "b"}}}])",
  };
  std::vector<std::string> lines = InspectLines(batches, 1000, 350);
  ASSERT_EQ(lines.size(), 3u);
  EXPECT_EQ(lines[0].size(), kMaxLineLength);
  EXPECT_EQ(lines[0].size() + lines[1].size(), 328u);
  // "b" only gets what's left of the total.
  EXPECT_EQ(lines[2].size(), 22u);
}

TEST(BudgetTest, NewestWithinBudget) {
  const std::vector<std::string> lines = {
      std::string(60, 'a'),
      std::string(60, 'b'),
      std::string(60, 'c'),
  };
  EXPECT_EQ(DebuglogCost(lines), 3 * (60 + kDebuglogRecordHeader));
  EXPECT_EQ(NewestWithinBudget(lines, 1000).size(), 3u);
  auto newest = NewestWithinBudget(lines, (2 * (60 + kDebuglogRecordHeader)) + 10);
  ASSERT_EQ(newest.size(), 2u);
  EXPECT_EQ(newest[0], lines[1]);
  EXPECT_EQ(NewestWithinBudget(lines, 10).size(), 0u);
}

}  // namespace
}  // namespace storage_watchdog
