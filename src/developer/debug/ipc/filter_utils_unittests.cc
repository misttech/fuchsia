// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <gtest/gtest.h>

#include "src/developer/debug/ipc/filter_utils.h"
#include "src/developer/debug/ipc/records.h"

namespace debug_ipc {

TEST(FilterUtils, FilterMatches) {
  Filter filter{.type = debug_ipc::Filter::Type::kProcessName, .pattern = "foo"};
  EXPECT_TRUE(FilterMatches(filter, "foo", {}));
  EXPECT_FALSE(FilterMatches(filter, "foobar", {}));

  filter = {.type = debug_ipc::Filter::Type::kProcessNameSubstr, .pattern = "foo"};
  EXPECT_TRUE(FilterMatches(filter, "foo", {}));
  EXPECT_TRUE(FilterMatches(filter, "foobar", {}));

  filter = {.type = debug_ipc::Filter::Type::kComponentMoniker, .pattern = "/core/abc"};
  EXPECT_TRUE(FilterMatches(filter, "", {ComponentInfo{.moniker = "/core/abc"}}));
  EXPECT_FALSE(FilterMatches(filter, "", {ComponentInfo{.moniker = "/core/abc/def"}}));

  filter = {.type = debug_ipc::Filter::Type::kComponentMonikerSuffix, .pattern = "abc/def"};
  EXPECT_TRUE(FilterMatches(filter, "", {ComponentInfo{.moniker = "/core/abc/def"}}));
  EXPECT_FALSE(FilterMatches(filter, "", {ComponentInfo{.moniker = "/core/abc"}}));

  filter = {.type = debug_ipc::Filter::Type::kComponentName, .pattern = "foo.cm"};
  EXPECT_TRUE(FilterMatches(filter, "", {ComponentInfo{.url = "pkg://host#meta/foo.cm"}}));

  filter = {.type = debug_ipc::Filter::Type::kComponentUrl, .pattern = "pkg://host#meta/foo.cm"};
  EXPECT_TRUE(
      FilterMatches(filter, "", {ComponentInfo{.url = "pkg://host?hash=abcd#meta/foo.cm"}}));
}

TEST(FilterUtils, GetAttachConfigsForFilterMatches) {
  // None of the filters need patterns, because they've already been determined to be a match.
  Filter filter1 = {
      .id = Filter::Identifier(1, Filter::Originator::kUnknown),
      .config =
          {
              .weak = true,
          },
  };

  Filter filter2 = {
      .id = Filter::Identifier(2, Filter::Originator::kUnknown),
      .config = {},
  };

  Filter filter3 = {
      .id = Filter::Identifier(3, Filter::Originator::kUnknown),
      .config =
          {
              .job_only = true,
          },
  };

  Filter filter4 = {
      .id = Filter::Identifier(4, Filter::Originator::kUnknown),
      .config =
          {
              .never_attach = true,
          },
  };

  std::vector<Filter> filters = {filter1, filter2, filter3, filter4};

  constexpr uint64_t kWeakPid1 = 0x1234;
  constexpr uint64_t kWeakPid2 = 0x1235;
  constexpr uint64_t kStrongProcessPid = 0x1236;
  constexpr uint64_t kJobPid = 0x1237;

  const std::vector<FilterMatch> kMatches = {
      FilterMatch(filter1.id, {kWeakPid1, kWeakPid2, kStrongProcessPid}),
      FilterMatch(filter2.id, {kStrongProcessPid}), FilterMatch(filter3.id, {kJobPid}),
      FilterMatch(filter4.id, {kWeakPid2})};

  auto result = GetAttachConfigsForFilterMatches(kMatches, filters);

  // There are 4 unique pids.
  EXPECT_EQ(result.size(), 4u);

  // kStrongProcessPid is matched by both a strong a weak filter, the attach request should be
  // strong.
  auto strong_attach = result.find(kStrongProcessPid);
  ASSERT_NE(strong_attach, result.end());
  EXPECT_FALSE(strong_attach->second.weak);
  EXPECT_EQ(strong_attach->second.target, AttachConfig::Target::kProcess);

  auto job_attach = result.find(kJobPid);
  ASSERT_NE(job_attach, result.end());
  EXPECT_EQ(job_attach->second.target, AttachConfig::Target::kJob);
  EXPECT_FALSE(job_attach->second.weak);

  auto weak_attach1 = result.find(kWeakPid1);
  ASSERT_NE(weak_attach1, result.end());
  EXPECT_TRUE(weak_attach1->second.weak);
  EXPECT_EQ(weak_attach1->second.target, AttachConfig::Target::kProcess);

  auto weak_attach2 = result.find(kWeakPid2);
  ASSERT_NE(weak_attach2, result.end());
  EXPECT_TRUE(weak_attach2->second.weak);
  EXPECT_EQ(weak_attach2->second.target, AttachConfig::Target::kProcess);
}

}  // namespace debug_ipc
